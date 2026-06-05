//! Update-check cache in redb: throttles version checks and records the staged
//! binary and its checksum.

use std::path::PathBuf;

use super::version::VERSION;
use crate::commands::db;

fn update_dir() -> Option<PathBuf> {
    let dir = db::tofupilot_dir().ok()?.join("update");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir)
}

/// Path for the staged binary.
pub fn staged_path() -> Option<PathBuf> {
    Some(update_dir()?.join("tofupilot.staged"))
}

/// Path for the previous binary.
pub fn previous_path() -> Option<PathBuf> {
    Some(update_dir()?.join("tofupilot.previous"))
}

/// Last-known minimum supported version, used by `enforce_min_version`
/// at process boot. Survives across restarts so an offline boot still
/// honours the floor recorded on the previous online check.
pub fn cached_min() -> Option<String> {
    db::open().ok()?.get_update_cache().ok()??.min
}

/// True when the last update *attempt* is more recent than `within`.
/// Used to throttle the one-shot CLI's background check so rapid
/// back-to-back commands don't each hit the network. A missing or
/// unreadable cache returns `false` (treat as "never checked" → allow a
/// check), so the first run after install still updates.
pub fn checked_recently(within: std::time::Duration) -> bool {
    let Some(checked_at) = db::open()
        .ok()
        .and_then(|db| db.get_update_cache().ok().flatten())
        .map(|c| c.checked_at)
    else {
        return false;
    };
    is_within(checked_at, chrono::Utc::now(), within)
}

/// Stamp `checked_at = now` without touching the other cache fields,
/// creating a minimal row if none exists. Called *before* the background
/// fetch is spawned so the throttle records the *attempt*, not just a
/// successful fetch. Without this, a fast command that `process::exit`s
/// before the detached fetch lands — or any offline command whose fetch
/// fails — never advances `checked_at`, so the throttle never engages.
/// Best-effort: a write failure just means the next command rechecks.
pub fn touch_checked_at() {
    let Ok(state) = db::open() else { return };
    let now = chrono::Utc::now();
    let cache = match state.get_update_cache().ok().flatten() {
        Some(mut c) => {
            c.checked_at = now;
            c
        }
        None => db::UpdateCache {
            checked_at: now,
            latest: VERSION.to_string(),
            min: None,
            poisoned_version: None,
            staged_sha256: None,
            staged_version: None,
        },
    };
    let _ = state.set_update_cache(&cache);
}

/// Pure freshness test: is `checked_at` within `within` of `now`,
/// counting both directions? Extracted from `checked_recently` so the
/// throttle window logic is unit-testable without touching the on-disk
/// cache.
///
/// Fresh means `-window < age < window`. The lower bound matters: a clock
/// that steps backward (NTP correction, VM resume, dual-boot RTC) leaves
/// `checked_at` far in the future, giving a large negative age. Without
/// the lower bound that would read as "always fresh" and pin the throttle
/// on forever, silently halting update checks. A small forward skew
/// (within the window) is still tolerated so NTP jitter doesn't force a
/// recheck every command.
fn is_within(
    checked_at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
    within: std::time::Duration,
) -> bool {
    match chrono::Duration::from_std(within) {
        Ok(window) => {
            let age = now - checked_at;
            age < window && age > -window
        }
        Err(_) => false,
    }
}

pub fn write(latest: &str, min: Option<&str>) -> crate::error::CliResult<()> {
    let state = db::open()?;
    // Carry the poisoned-version marker forward, but clear it once the
    // server advertises a different `latest` — a new release re-arms
    // auto-update; only the previously-failed version stays skipped.
    let prior = state.get_update_cache().ok().flatten();
    let poisoned = prior
        .as_ref()
        .and_then(|c| c.poisoned_version.clone())
        .filter(|v| v == latest);
    // Carry staged fields only while they still describe the same
    // binary version we're about to advertise. `latest` moving means
    // the staged file (if any) is for a now-superseded version; let
    // the caller restage and overwrite.
    let (staged_sha256, staged_version) = match prior {
        Some(c) if c.staged_version.as_deref() == Some(latest) => {
            (c.staged_sha256, c.staged_version)
        }
        _ => (None, None),
    };
    let cache = db::UpdateCache {
        checked_at: chrono::Utc::now(),
        latest: latest.to_string(),
        min: min.map(str::to_string),
        poisoned_version: poisoned,
        staged_sha256,
        staged_version,
    };
    state.set_update_cache(&cache)?;
    Ok(())
}

/// Record the sha256 + version of the staged binary so `apply_staged`
/// can verify integrity before exec.
///
/// Requires a prior `cache::write` to have populated `latest`/`min`.
/// Both `background_check` and `run_update` call `write()` immediately
/// before `download_and_stage`, so this is satisfied on every real
/// path; erroring on a missing prior keeps us from silently dropping
/// the server's `min` floor when staging.
pub fn set_staged(version: &str, sha256: &str) -> crate::error::CliResult<()> {
    let state = db::open()?;
    let mut cache = state
        .get_update_cache()?
        .ok_or("set_staged called with no prior update cache (cache::write must run first)")?;
    cache.staged_sha256 = Some(sha256.to_string());
    cache.staged_version = Some(version.to_string());
    state.set_update_cache(&cache)?;
    Ok(())
}

/// Drop staged sha/version after a successful or failed apply so a
/// stale checksum can't haunt the next check.
pub fn clear_staged() {
    let Ok(state) = db::open() else { return };
    let Ok(Some(mut cache)) = state.get_update_cache() else {
        return;
    };
    cache.staged_sha256 = None;
    cache.staged_version = None;
    let _ = state.set_update_cache(&cache);
}

pub fn staged_sha256() -> Option<String> {
    db::open().ok()?.get_update_cache().ok()??.staged_sha256
}

/// Mark a target version as known-bad on this host. Stops `background_check`
/// from re-staging the same binary every tick. Cleared automatically by
/// `write()` once the server advertises a different `latest`.
pub fn poison(version: &str) -> crate::error::CliResult<()> {
    let state = db::open()?;
    let mut cache = state.get_update_cache()?.unwrap_or(db::UpdateCache {
        checked_at: chrono::Utc::now(),
        latest: version.to_string(),
        min: None,
        poisoned_version: None,
        staged_sha256: None,
        staged_version: None,
    });
    cache.poisoned_version = Some(version.to_string());
    state.set_update_cache(&cache)?;
    Ok(())
}

pub fn poisoned_version() -> Option<String> {
    db::open().ok()?.get_update_cache().ok()??.poisoned_version
}

#[cfg(test)]
mod tests {
    use super::is_within;
    use std::time::Duration;

    fn at(secs_ago: i64) -> chrono::DateTime<chrono::Utc> {
        // Fixed `now` minus an offset; both passed explicitly so the test
        // never depends on wall-clock drift.
        now() - chrono::Duration::seconds(secs_ago)
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn recent_check_is_within_window() {
        // Checked 1h ago, 6h window → still fresh.
        assert!(is_within(at(3600), now(), Duration::from_secs(6 * 3600)));
    }

    #[test]
    fn old_check_is_outside_window() {
        // Checked 7h ago, 6h window → stale, a new check is due.
        assert!(!is_within(
            at(7 * 3600),
            now(),
            Duration::from_secs(6 * 3600)
        ));
    }

    #[test]
    fn boundary_is_treated_as_stale() {
        // Exactly at the window edge is not "within" (strict `<`), so the
        // check runs rather than being skipped one tick too long.
        assert!(!is_within(
            at(6 * 3600),
            now(),
            Duration::from_secs(6 * 3600)
        ));
    }

    #[test]
    fn small_future_skew_is_within() {
        // A checked_at slightly ahead of now (NTP jitter) must not force a
        // recheck every invocation — a small negative age is still "within".
        assert!(is_within(
            now() + chrono::Duration::seconds(60),
            now(),
            Duration::from_secs(6 * 3600)
        ));
    }

    #[test]
    fn far_future_checked_at_is_stale() {
        // A backward clock step leaves checked_at far in the future. That
        // must read as stale (force a recheck), not pin the throttle on
        // forever.
        assert!(!is_within(
            now() + chrono::Duration::hours(48),
            now(),
            Duration::from_secs(6 * 3600)
        ));
    }

    #[test]
    fn future_boundary_is_treated_as_stale() {
        // Mirror of `boundary_is_treated_as_stale` on the future side:
        // exactly one window ahead is the strict `> -window` edge, so it
        // reads as stale rather than fresh.
        assert!(!is_within(
            now() + chrono::Duration::hours(6),
            now(),
            Duration::from_secs(6 * 3600)
        ));
    }
}
