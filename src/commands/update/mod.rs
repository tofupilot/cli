//! Self-update: version checks, download + checksum verification, and
//! in-place binary replacement.
//!
//! Checks are throttled and cached (see [`cache`]); a newer binary is
//! downloaded, verified, staged, and applied by re-executing. Also enforces a
//! server-mandated minimum version.

mod cache;
mod config;
mod download;
mod platform;
mod version;

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use cache::{
    clear_staged, poison, poisoned_version, previous_path, staged_path, staged_sha256, write,
};
use download::{download_and_stage, fetch};
use platform::{is_disabled, reexec};
use version::{is_newer, is_same};
pub use version::{version_at_least, VERSION};

/// Why an update attempt failed. Callers branch on this to decide whether the
/// outcome warrants an `UpdateFailed` event (we tried to apply something and
/// it broke) versus a transient log line (we couldn't even reach the version
/// endpoint, so no upgrade was attempted in the first place).
#[derive(Debug)]
pub enum UpdateError {
    /// Couldn't reach or parse the version endpoint. No new version was
    /// staged or applied; treat as a transient connectivity issue.
    Fetch(String),
    /// Download/extract/checksum failed for an upgrade we'd already decided
    /// to attempt. The user-facing impact is real (upgrade didn't land), so
    /// callers should report this as a failed update.
    Stage(String),
    /// Replacing the current binary failed after staging succeeded. Same
    /// reporting treatment as Stage.
    Apply(String),
}

impl UpdateError {
    /// True for errors that mean the update never got past the version
    /// check. Used by callers to suppress `UpdateFailed` events when
    /// nothing was actually attempted.
    pub fn is_fetch(&self) -> bool {
        matches!(self, UpdateError::Fetch(_))
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateError::Fetch(s) | UpdateError::Stage(s) | UpdateError::Apply(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for UpdateError {}

/// Block execution if current version is below server's minimum.
pub fn enforce_min_version() {
    if let Some(min) = cache::cached_min() {
        if is_newer(&min, VERSION) {
            crate::log::error(&format!(
                "Version {VERSION} is no longer supported (minimum: {min})."
            ));
            crate::log::info("Run `tofupilot update` to upgrade.");
            std::process::exit(1);
        }
    }
}

/// Apply a previously staged update by replacing the current binary.
/// Records a pending-update marker (from → to) before re-exec so the new
/// process can publish a matching UpdateApplied event.
pub fn apply_staged() -> crate::error::CliResult<()> {
    if is_disabled() {
        return Ok(());
    }
    let staged = require_path(staged_path())?;
    if !staged.exists() {
        return Ok(());
    }
    // Re-hash the staged binary against the sha recorded at download
    // time. Catches torn writes, partial copies, on-disk bit rot, and
    // any tampering between stage and apply. Without this check a
    // half-written staged file would self_replace + reexec into a
    // SIGSEGV/SIGBUS at the new process's first faulted code page.
    if let Err(e) = verify_staged(&staged) {
        let _ = fs::remove_file(&staged);
        clear_staged();
        return Err(e);
    }
    let to = staged_version().unwrap_or_else(|| "unknown".to_string());
    // Attribute each failure point so a generic ENOENT no longer hides
    // which call broke. Non-fatal `backup_current` failures used to mask
    // a missing `current_exe()` path — now they surface explicitly.
    backup_current().map_err(|e| format!("backup_current failed: {e}"))?;
    replace_and_reexec(&staged, VERSION, &to)
}

/// Re-compute the staged binary's sha256 and compare against what
/// `download_and_stage` recorded. Missing recorded sha is itself a
/// failure: an old (pre-fix) staged file or a stage that didn't write
/// the cache must not be trusted.
fn verify_staged(staged: &Path) -> crate::error::CliResult<()> {
    let expected =
        staged_sha256().ok_or("staged binary has no recorded checksum; refusing to apply")?;
    let mut file = fs::File::open(staged).map_err(|e| format!("open staged: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read staged: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        return Err(
            format!("staged binary checksum mismatch (expected {expected}, got {actual})").into(),
        );
    }
    Ok(())
}

/// True if a staged update binary is waiting to be applied.
pub fn has_staged() -> bool {
    if is_disabled() {
        return false;
    }
    staged_path().is_some_and(|p| p.exists())
}

/// Best-effort delete of the staged binary. Used after an apply failure
/// so we don't retry the same broken state every run-end.
pub fn discard_staged() {
    if let Some(p) = staged_path() {
        let _ = fs::remove_file(p);
    }
    clear_staged();
}

/// Strip macOS xattrs from a freshly written binary so Gatekeeper
/// doesn't kill the next exec with SIGKILL (or, on Apple Silicon,
/// surface as SIGSEGV at first faulted code page when page-validation
/// rejects the unsigned overwritten binary).
///
/// Uses `xattr -c` (clear all) rather than `-d com.apple.quarantine`:
/// `-d` returns non-zero when the attr is absent (every non-quarantined
/// binary), and `-c` is idempotent in one syscall. Best-effort —
/// failure is non-fatal.
#[cfg(target_os = "macos")]
fn strip_quarantine(path: &Path) {
    let _ = std::process::Command::new("xattr")
        .arg("-c")
        .arg(path)
        .output();
}

#[cfg(not(target_os = "macos"))]
fn strip_quarantine(_path: &Path) {}

/// Mark a target version as known-bad on this host so `background_check`
/// stops re-staging it every tick. Cleared automatically once the server
/// advertises a different `latest` (i.e. a real new release re-arms).
pub fn mark_poisoned(version: &str) {
    let _ = poison(version);
}

/// Version that a staged update would upgrade to. Prefer the
/// `staged_version` field (authoritative — set by `set_staged` at
/// download time and cleared when the staged file no longer matches
/// `latest`). Fall back to `latest` for legacy cache rows written by
/// pre-fix CLIs that didn't persist `staged_version`.
pub fn staged_version() -> Option<String> {
    let cache = crate::commands::db::open()
        .ok()?
        .get_update_cache()
        .ok()??;
    cache.staged_version.or(Some(cache.latest))
}

/// Write a pending-update marker so the post-reexec process can report the
/// outcome. Best-effort: failure to persist is non-fatal.
fn record_pending(from: &str, to: &str) {
    if let Ok(db) = crate::commands::db::open() {
        let _ = db.set_pending_update(&crate::commands::db::PendingUpdate {
            from_version: from.to_string(),
            to_version: to.to_string(),
            started_at: chrono::Utc::now(),
        });
    }
}

/// Clear a previously written pending-update marker. Called when an apply
/// fails after `record_pending` has already run, so the next boot doesn't
/// double-report a failure the caller already published as `UpdateFailed`.
pub(crate) fn clear_pending_marker() {
    if let Ok(db) = crate::commands::db::open() {
        let _ = db.clear_pending_update();
    }
}

/// Whether automatic updates are enabled. Reads the local `auto_update` config
/// (set by the station UI, synced via stream). Defaults to on when unset so
/// fresh installs and non-station CLI invocations keep the current behavior.
pub fn auto_update_enabled() -> bool {
    crate::commands::db::open()
        .ok()
        .and_then(|db| db.get_config("auto_update").ok().flatten())
        .is_none_or(|v| v == "on")
}

/// Whether a one-shot CLI invocation should run its background update
/// check now, or skip it because a check ran within
/// `CLI_UPDATE_CHECK_INTERVAL`. The station daemon does NOT use this — it
/// paces itself on `STATION_UPDATE_CHECK_INTERVAL` and always checks on
/// its own tick. Only the per-command startup path consults this so a
/// burst of commands collapses to one network call.
pub fn cli_check_due() -> bool {
    !cache::checked_recently(crate::config::timeouts::CLI_UPDATE_CHECK_INTERVAL)
}

/// Record that a one-shot CLI invocation is *attempting* a check now, so
/// the throttle counts the attempt even when the spawned `background_check`
/// is killed by `process::exit` before it finishes, or its `fetch()` fails
/// offline. Call this synchronously right before spawning the check.
pub fn mark_cli_checked() {
    cache::touch_checked_at();
}

/// Background update check — stage new version for next launch.
/// Rate-limited externally by the caller's tick interval (see
/// `STATION_UPDATE_CHECK_INTERVAL`).
pub async fn background_check() -> Result<(), UpdateError> {
    if is_disabled() {
        return Ok(());
    }
    crate::log::info("Checking for updates...");
    let info = fetch()
        .await
        .map_err(|e| UpdateError::Fetch(e.to_string()))?;
    write(&info.latest, info.min.as_deref()).map_err(|e| UpdateError::Stage(e.to_string()))?;
    if is_same(&info.latest, VERSION) {
        crate::log::info(&format!("Already on latest (v{}).", VERSION));
        return Ok(());
    }
    // Don't stage a downgrade. If the running binary is already newer than
    // the server's `latest` (manual install, dev build, rollback), there's
    // nothing to do — and staging would queue a pending-update marker that
    // the post-restart check would later flag as a failure.
    if !is_newer(&info.latest, VERSION) {
        return Ok(());
    }
    // Skip versions we already proved we can't apply on this host.
    // `cache::write` clears the marker as soon as the server advertises
    // a different `latest`, so a real new release re-arms auto-update.
    if poisoned_version().as_deref() == Some(info.latest.as_str()) {
        return Ok(());
    }
    crate::log::info(&format!(
        "Update available: v{} → v{}, downloading...",
        VERSION, info.latest,
    ));
    let staged = require_path(staged_path()).map_err(|e| UpdateError::Stage(e.to_string()))?;
    ensure_parent(&staged).map_err(|e| UpdateError::Stage(e.to_string()))?;
    download_and_stage(&info, &staged)
        .await
        .map_err(|e| UpdateError::Stage(e.to_string()))?;
    crate::log::info(&format!(
        "Staged v{}; will apply between runs.",
        info.latest,
    ));
    Ok(())
}

/// Explicit `tofupilot update` -- check, download, and replace the binary.
/// Does not restart: long-running modes pick up the new binary on next launch
/// via `apply_staged`.
pub async fn run_update() -> Result<bool, UpdateError> {
    run_update_with_publisher(None).await
}

/// Variant that announces the upgrade attempt on the wire when a
/// publisher is available. Symmetric with `try_apply_staged_update`'s
/// `UpdateStarted` emit so a dashboard tab observing both paths sees
/// the same lifecycle (`UpdateStarted` → `UpdateApplied` /
/// `UpdateFailed`). The single-arg `run_update` skips the publish so
/// callers in tests / the standalone `tofupilot update` command don't
/// need a wire stack.
pub async fn run_update_with_publisher(
    publisher: Option<&crate::commands::station::client::PublishHandle>,
) -> Result<bool, UpdateError> {
    crate::log::info("Checking for updates...");
    let info = fetch()
        .await
        .map_err(|e| UpdateError::Fetch(e.to_string()))?;
    write(&info.latest, info.min.as_deref()).map_err(|e| UpdateError::Stage(e.to_string()))?;
    if is_same(&info.latest, VERSION) {
        crate::log::info(&format!("Already on latest (v{}).", VERSION));
        return Ok(false);
    }
    if !is_newer(&info.latest, VERSION) {
        crate::log::info(&format!(
            "Already on v{} (newer than server latest v{}); nothing to do.",
            VERSION, info.latest,
        ));
        return Ok(false);
    }
    if let Some(pub_handle) = publisher {
        let inst_id = crate::commands::auth::credentials::require()
            .ok()
            .and_then(|c| c.installation_id)
            .unwrap_or_default();
        let _ = pub_handle
            .publish(&station_protocol::StationEvent::UpdateStarted {
                installation_id: inst_id,
                from_version: VERSION.to_string(),
                to_version: info.latest.clone(),
            })
            .await;
    }
    crate::log::info(&format!("Downloading v{}...", info.latest));
    let staged = require_path(staged_path()).map_err(|e| UpdateError::Stage(e.to_string()))?;
    ensure_parent(&staged).map_err(|e| UpdateError::Stage(e.to_string()))?;
    download_and_stage(&info, &staged)
        .await
        .map_err(|e| UpdateError::Stage(e.to_string()))?;
    crate::log::info("Applying update...");
    // No re-verify here: `download_and_stage` validated the archive
    // sha and persisted the bin sha just above, on a file we haven't
    // closed long enough for bit rot. `apply_staged` (cross-reboot
    // path) is the one that needs to re-hash.
    backup_current().map_err(|e| UpdateError::Apply(e.to_string()))?;
    record_pending(VERSION, &info.latest);
    if let Err(e) = self_replace::self_replace(&staged) {
        clear_pending_marker();
        return Err(UpdateError::Apply(e.to_string()));
    }
    let exe_path = std::env::current_exe().ok();
    if let Some(p) = exe_path.as_deref() {
        strip_quarantine(p);
    }
    // Post-`self_replace` cleanup. The binary swap already happened, so
    // a `remove_file` failure here doesn't undo the update — degrade
    // the staged tmp into orphaned bytes and proceed. Bubbling
    // `UpdateError::Apply` without clearing the pending marker would
    // also publish a duplicate `UpdateApplied` on next boot (the new
    // binary IS running) even though this call returned an error.
    if let Err(e) = fs::remove_file(&staged) {
        crate::log::warn(&format!(
            "Could not delete staged update file {}: {e}. The update has been applied.",
            staged.display()
        ));
    }
    clear_staged();
    crate::log::success(&format!("Updated to v{}.", info.latest));
    // Do not reexec here: `tofupilot update` is a one-shot command and
    // already finished its job. Reexec'ing would re-run the whole update
    // flow in a child (Windows can't swap the process image), which then
    // collides with the parent on the redb lock. Long-running modes pick
    // up the new binary on next launch via `apply_staged()`.
    Ok(true)
}

/// Rollback to the previous version.
pub fn rollback() -> crate::error::CliResult<()> {
    let prev = require_path(previous_path())?;
    if !prev.exists() {
        crate::log::error("No previous version to rollback to.");
        std::process::exit(1);
    }
    crate::log::info("Rolling back...");
    self_replace::self_replace(&prev)?;
    if let Ok(exe) = std::env::current_exe() {
        strip_quarantine(&exe);
    }
    fs::remove_file(&prev)?;
    crate::log::success("Rolled back. Restart tofupilot to use the previous version.");
    Ok(())
}

fn backup_current() -> crate::error::CliResult<()> {
    let prev = require_path(previous_path())?;
    ensure_parent(&prev)?;
    let exe = std::env::current_exe().map_err(|e| format!("current_exe() failed: {e}"))?;
    // `current_exe()` can resolve to a path that no longer exists if the
    // file on disk was moved/deleted while this process kept running
    // (kernel keeps the inode mmap'd). `self_replace` will then ENOENT on
    // its own `metadata()` call too, so skip the backup rather than fail
    // — the previous-version backup is best-effort anyway.
    if !exe.exists() {
        crate::log::warn(&format!(
            "current_exe() points to a missing path ({}); skipping backup",
            exe.display()
        ));
        return Ok(());
    }
    fs::copy(&exe, &prev).map_err(|e| {
        format!(
            "fs::copy({} -> {}) failed: {e}",
            exe.display(),
            prev.display()
        )
    })?;
    Ok(())
}

fn replace_and_reexec(path: &Path, from: &str, to: &str) -> crate::error::CliResult<()> {
    // Resolve exe path before self_replace -- on Linux /proc/self/exe becomes invalid after swap
    let exe = std::env::current_exe().map_err(|e| format!("current_exe() failed: {e}"))?;
    if !exe.exists() {
        return Err(format!(
            "current_exe() resolves to missing path: {}. Reinstall with: curl -fsSL https://tofupilot.sh/install | sh",
            exe.display()
        ).into());
    }
    // Write the pending-update marker only at the point of no return: if
    // anything before this fails, we don't want a stale marker triggering
    // a spurious "version after restart is X, expected Y" report on the
    // next boot — that failure was already published as `UpdateFailed` by
    // the caller's Err arm.
    record_pending(from, to);
    if let Err(e) = self_replace::self_replace(path) {
        // self_replace failed after we wrote the marker: roll it back so
        // the next boot doesn't double-report this failure.
        clear_pending_marker();
        return Err(format!("self_replace({}) failed: {e}", path.display()).into());
    }
    // Strip macOS quarantine on the post-replace binary so Gatekeeper
    // doesn't kill the next exec with SIGKILL or surface a SIGSEGV via
    // page-validation on Apple Silicon.
    strip_quarantine(&exe);
    // Best-effort cleanup of the staged binary. self_replace::self_replace
    // copies (not moves) on Unix, so the staged file should still exist;
    // if it's already gone we still want to proceed to reexec.
    //
    // Only clear the cached sha/version when the file is actually gone.
    // On Windows, AV can hold a handle on the staged file past
    // self_replace, making remove_file fail with EBUSY/EACCES — clearing
    // the sha unconditionally there leaves a file on disk with no
    // recorded checksum, which `apply_staged` would later flag as
    // "staged binary has no recorded checksum" on the very next boot
    // (re-publishing UpdateFailed for a same-version "upgrade"). Keeping
    // the sha when the file persists lets the lazy `has_staged()` orphan
    // detector clean up next boot, or `verify_staged` re-check on retry.
    let removed = fs::remove_file(path).is_ok() || !path.exists();
    if removed {
        clear_staged();
    }
    // Release the redb lock before re-exec. On Unix exec swaps the image
    // and the lock transfers; on Windows we spawn a child + exit, so the
    // child needs the lock free.
    crate::commands::db::close();
    let args: Vec<String> = std::env::args().collect();
    Err(format!("failed to restart: {}", reexec(&exe, &args)).into())
}

fn require_path(p: Option<PathBuf>) -> crate::error::CliResult<PathBuf> {
    p.ok_or_else(|| "cannot determine cache directory".into())
}

fn ensure_parent(path: &Path) -> crate::error::CliResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}
