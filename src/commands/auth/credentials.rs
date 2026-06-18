//! Persisted credentials with restrictive file permissions (API key, base
//! URL, org, optional installation id).
//!
//! Two slots, routed by identity so neither clobbers the other:
//!   * `credentials.json` — the *user* login (`tofupilot login`, browser
//!     device flow). `installation_id` is `None`.
//!   * `station.json` — the *station* login (`tofupilot login --token`).
//!     `installation_id` is `Some`.
//!
//! Both can coexist on one machine: a station that also runs `tofupilot
//! deploy` (which requires a user key) keeps its station identity intact.
//! [`save`] picks the slot from `installation_id`; [`load`] resolves the
//! user identity (the default for API/deploy/run); [`load_station`] reads
//! only the station slot (service start, daemon).

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub api_key: String,
    pub base_url: String,
    pub organization_slug: String,
    #[serde(default)]
    pub installation_id: Option<String>,
}

impl Credentials {
    /// `base_url` with all trailing `/` stripped, so a stray `//` in
    /// stored creds collapses cleanly when joined to a path.
    pub fn base(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }
}

/// Filesystem path to the *user* credentials JSON. Public so the
/// uninstaller can target it without re-deriving the path layout.
/// Falls back to `~/.tofupilot/credentials.json` when `tofupilot_dir`
/// can't be created (read-only home, no `$HOME`); the surrounding
/// load/save calls already tolerate IO failures, so a non-canonical
/// path here just means they no-op rather than panic.
pub fn credentials_path() -> PathBuf {
    super::super::db::tofupilot_dir()
        .unwrap_or_else(|_| PathBuf::from(".tofupilot"))
        .join("credentials.json")
}

/// Filesystem path to the *station* credentials JSON. Separate slot so a
/// user login (`tofupilot login`) never overwrites the station identity a
/// previous `tofupilot login --token` established.
pub fn station_credentials_path() -> PathBuf {
    super::super::db::tofupilot_dir()
        .unwrap_or_else(|_| PathBuf::from(".tofupilot"))
        .join("station.json")
}

/// Save credentials to the slot that matches their identity: a station
/// login (`installation_id: Some`) lands in `station.json`, a user login
/// (`installation_id: None`) in `credentials.json`. The two never collide,
/// so deploying as a user from a station no longer evicts the station token.
pub fn save(creds: &Credentials) -> crate::error::CliResult<()> {
    let path = if creds.installation_id.is_some() {
        station_credentials_path()
    } else {
        // Pre-split installs stored a station identity in the single
        // `credentials.json`. Writing this user login there would silently
        // destroy it (the exact clobber this split exists to prevent), so
        // migrate that legacy station identity into `station.json` first.
        // Idempotent: once `station.json` exists the legacy file no longer
        // carries a station id, so this is a one-time copy.
        // `!station_path.exists()` guard: only migrate into an empty
        // station slot. A populated `station.json` is always at least as
        // fresh as a station id still lingering in the legacy combined
        // file, so never let the legacy copy overwrite it.
        let station_path = station_credentials_path();
        if !station_path.exists() {
            if let Some(legacy_station) =
                read_file(&credentials_path()).filter(|c| c.installation_id.is_some())
            {
                save_to(&legacy_station, &station_path)?;
            }
        }
        credentials_path()
    };
    save_to(creds, &path)
}

fn save_to(creds: &Credentials, path: &std::path::Path) -> crate::error::CliResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(creds)?)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

    // Windows: file inherits the parent's ACL. On a default home dir
    // that's usually `Users:R` (read-only to peers), but a domain-
    // joined machine or an SMB-redirected profile can expose the
    // credentials file fleet-wide. Lock the ACL to the current user
    // only via icacls, mirroring chmod 0600 on Unix.
    #[cfg(windows)]
    {
        let _ = restrict_windows_acl(path);
    }

    Ok(())
}

#[cfg(windows)]
fn restrict_windows_acl(path: &std::path::Path) -> crate::error::CliResult<()> {
    use std::process::Command;
    let user = std::env::var("USERNAME").map_err(|e| format!("USERNAME env not set: {e}"))?;
    // /inheritance:r drops inherited ACEs; /grant grants the current
    // user full control. Combined: only the current user can read.
    let output = Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant")
        .arg(format!("{user}:F"))
        .output()
        .map_err(|e| format!("Run icacls: {e}"))?;
    if !output.status.success() {
        return Err(format!("icacls failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(())
}

fn read_file(path: &std::path::Path) -> Option<Credentials> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Load the *user* identity (the default for API calls, deploy, run, the
/// SDK). Prefers `credentials.json`; falls back to `station.json` so a
/// pure-station machine (one that has never run a browser `login`) still
/// resolves an API key for read-only commands. Pre-split installs wrote a
/// single `credentials.json` that may carry a station `installation_id` —
/// that file is read here unchanged.
pub fn load() -> Option<Credentials> {
    read_file(&credentials_path()).or_else(|| read_file(&station_credentials_path()))
}

/// Load the *station* identity for the daemon / service start. Reads
/// `station.json`; falls back to a legacy single-file `credentials.json`
/// that still carries an `installation_id` (pre-split installs that logged
/// in with `--token` and never re-ran setup), so existing stations keep
/// booting without a re-login.
pub fn load_station() -> Option<Credentials> {
    // Filter the station slot on `installation_id` too (not just the legacy
    // fallback): `save()` only routes id-bearing records here, so a record
    // without one means a corrupt or hand-edited file. Rejecting it keeps
    // the daemon's `Some(creds) => run` arm sound — it never boots with a
    // non-station identity.
    read_file(&station_credentials_path())
        .filter(|c| c.installation_id.is_some())
        .or_else(|| read_file(&credentials_path()).filter(|c| c.installation_id.is_some()))
}

/// Canonical "not logged in" message. Centralized so a UX tweak lands
/// in one place across every command that requires auth.
pub const NOT_LOGGED_IN: &str = "Not logged in. Run `tofupilot login` first.";

/// `load()` + canonical error for commands that strictly require auth. The
/// error carries [`NOT_LOGGED_IN`]; callers typically log it and exit.
pub fn require() -> crate::error::CliResult<Credentials> {
    load().ok_or_else(|| crate::error::CliError::msg(NOT_LOGGED_IN))
}

/// Clear both credential slots. Logout is a full reset, so it removes the
/// user and station files regardless of which identity is active.
pub fn clear() -> crate::error::CliResult<()> {
    for path in [credentials_path(), station_credentials_path()] {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(base_url: &str) -> Credentials {
        Credentials {
            api_key: "k".into(),
            base_url: base_url.into(),
            organization_slug: "org".into(),
            installation_id: None,
        }
    }

    #[test]
    fn base_strips_single_trailing_slash() {
        assert_eq!(creds("https://x.app/").base(), "https://x.app");
    }

    #[test]
    fn base_collapses_repeated_trailing_slashes() {
        assert_eq!(creds("https://x.app///").base(), "https://x.app");
    }

    #[test]
    fn base_leaves_clean_url_untouched() {
        assert_eq!(creds("https://x.app").base(), "https://x.app");
    }

    #[test]
    fn installation_id_defaults_to_none_when_absent() {
        let json = r#"{"api_key":"k","base_url":"https://x.app","organization_slug":"org"}"#;
        let parsed: Credentials = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.installation_id, None);
    }

    #[test]
    fn credentials_round_trip_through_json() {
        let c = Credentials {
            installation_id: Some("inst_1".into()),
            ..creds("https://x.app")
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: Credentials = serde_json::from_str(&json).unwrap();
        assert_eq!(back.installation_id.as_deref(), Some("inst_1"));
        assert_eq!(back.base(), "https://x.app");
    }

    fn station(id: &str) -> Credentials {
        Credentials {
            installation_id: Some(id.into()),
            ..creds("https://x.app")
        }
    }

    // The split-slot routing is exercised through `save_to`/`read_file`
    // against tmp paths so tests never touch the real `~/.tofupilot`.

    #[test]
    fn save_to_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("tp-creds-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let user_path = dir.join("credentials.json");
        let station_path = dir.join("station.json");

        // A user login and a station login written to their own slots.
        save_to(&creds("https://x.app"), &user_path).unwrap();
        save_to(&station("inst_1"), &station_path).unwrap();

        // Neither clobbered the other: both files survive independently.
        assert_eq!(read_file(&user_path).unwrap().installation_id, None);
        assert_eq!(
            read_file(&station_path).unwrap().installation_id.as_deref(),
            Some("inst_1"),
        );

        // Both slots are locked to the owner (0600) — a station API key in
        // a world-readable file would be a leak. Guards against a future
        // change that skips perms for one slot.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&user_path, &station_path] {
                let mode = fs::metadata(p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{p:?} not chmod 0600");
            }
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_routes_by_identity_to_distinct_paths() {
        // The whole point of the split: a station login and a user login
        // resolve to different on-disk paths, so one can never evict the
        // other.
        assert_ne!(credentials_path(), station_credentials_path());
    }

    // Reproduces the legacy-migration path in `save()` against tmp files:
    // a pre-split combined `credentials.json` carrying a station identity,
    // then a user login. The user login must NOT destroy the station id —
    // it gets migrated to `station.json` first.
    #[test]
    fn legacy_combined_station_id_survives_a_user_login() {
        let dir = std::env::temp_dir().join(format!("tp-migrate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let user_path = dir.join("credentials.json");
        let station_path = dir.join("station.json");

        // Pre-split state: a single combined file with a station id.
        save_to(&station("inst_legacy"), &user_path).unwrap();

        // Simulate save()'s migration branch for a user login: copy any
        // legacy station id out before overwriting credentials.json.
        if let Some(legacy) = read_file(&user_path).filter(|c| c.installation_id.is_some()) {
            save_to(&legacy, &station_path).unwrap();
        }
        save_to(&creds("https://x.app"), &user_path).unwrap();

        // User identity now in credentials.json, station id preserved in
        // station.json — nothing lost.
        assert_eq!(read_file(&user_path).unwrap().installation_id, None);
        assert_eq!(
            read_file(&station_path).unwrap().installation_id.as_deref(),
            Some("inst_legacy"),
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
