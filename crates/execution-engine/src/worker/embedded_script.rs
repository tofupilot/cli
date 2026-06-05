//! Resolution + extraction for the embedded `tp_worker.py` / `tp_plug.py`
//! Python scripts the engine spawns.
//!
//! Resolution order:
//! 1. `<exe_dir>/python/<name>` — Studio/packaged layout that ships the
//!    script next to the binary.
//! 2. `<runtime_dir>/<name>` — extracted from the embedded constant on
//!    first use. Lives under `~/.tofupilot/runtime/`, NOT `%TEMP%` —
//!    Windows Defender ASR rules ("Block executable content from
//!    email/webmail" et al.) and several enterprise EDR policies block
//!    script execution from `AppData\Local\Temp`, surfacing as
//!    `Access is denied (os error 5)` on the first spawn. A per-user,
//!    persistent location lets AV scan once and stay quiet. Sharing
//!    `~/.tofupilot/` with the rest of CLI state also means uninstall
//!    sweeps the runtime dir along with creds / redb in one shot.
//!
//! Writes are guarded by a content hash so concurrent CLI invocations
//! don't truncate each other's mid-spawn copy.

use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Subdirectory under `~/.tofupilot/` for extracted helper scripts.
/// Public so the CLI's uninstaller can target it without re-deriving
/// the layout.
pub const RUNTIME_SUBDIR: &str = "runtime";

fn runtime_dir() -> PathBuf {
    // Engine can't depend on CLI, so we resolve `~/.tofupilot/runtime`
    // here. Fallback to `std::env::temp_dir()/tofupilot-runtime` only
    // when no home dir resolves (CI, sandboxes); in that pathological
    // case AV friction is unavoidable.
    let base = dirs::home_dir()
        .map(|h| h.join(".tofupilot"))
        .unwrap_or_else(|| std::env::temp_dir().join("tofupilot-fallback"));
    base.join(RUNTIME_SUBDIR)
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for byte in out.iter() {
        use std::fmt::Write;
        let _ = write!(&mut s, "{:02x}", byte);
    }
    s
}

/// Resolve `<exe_dir>/python/<name>` if it exists.
pub fn next_to_exe(name: &str) -> Option<PathBuf> {
    let exe_path = std::env::current_exe().ok()?;
    let exe_dir = exe_path.parent()?;
    let script_path = exe_dir.join("python").join(name);
    if script_path.exists() {
        Some(script_path)
    } else {
        None
    }
}

/// Extract `script_bytes` to `<runtime_dir>/<name>` if not already
/// present with matching content. Returns the script path.
///
/// Concurrent callers may race here; the worst case is two processes
/// writing identical bytes. We compare hashes before overwriting so a
/// process mid-spawn doesn't get its `tp_worker.py` truncated by a
/// parallel CLI invocation. On Windows, `rename` over a file opened
/// without `FILE_SHARE_DELETE` returns `os error 5`; if the rename
/// fails but the existing file already has the expected hash, treat
/// it as a win-by-another-process and return success.
///
/// The hash gate also means an upgraded CLI with a new embedded script
/// rewrites the on-disk copy on next run — no version-keyed subdir
/// needed, no stale-version cleanup.
pub fn extract_to_runtime_dir(name: &str, script_bytes: &str) -> Result<PathBuf, String> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create runtime dir {}: {}", dir.display(), e))?;
    let script_path = dir.join(name);
    let expected = hex_digest(script_bytes.as_bytes());

    if hash_matches(&script_path, &expected) {
        prune_once(&dir);
        return Ok(script_path);
    }

    // Write to a temp neighbor then rename. Tmp name is per-process so
    // two concurrent CLIs don't fight over the same file.
    let tmp = script_path.with_extension(format!("py.tmp.{}", std::process::id()));
    std::fs::write(&tmp, script_bytes.as_bytes())
        .map_err(|e| format!("Failed to write script {}: {}", tmp.display(), e))?;
    match std::fs::rename(&tmp, &script_path) {
        Ok(()) => {
            prune_once(&dir);
            Ok(script_path)
        }
        Err(e) => {
            // Another process may have installed an identical file
            // first and is now holding it open (Windows: no
            // FILE_SHARE_DELETE on the python read). If the on-disk
            // bytes match what we'd have written, we're done.
            let _ = std::fs::remove_file(&tmp);
            if hash_matches(&script_path, &expected) {
                prune_once(&dir);
                Ok(script_path)
            } else {
                Err(format!(
                    "Failed to install script {}: {}",
                    script_path.display(),
                    e
                ))
            }
        }
    }
}

/// Gate pruning behind `Once` so a long-running station that spawns
/// many workers / plug services doesn't `read_dir` the runtime layout
/// on every extract call. Cleanup is opportunistic — running it once
/// per process is enough to keep the layout tidy.
fn prune_once(dir: &std::path::Path) {
    static PRUNE: std::sync::Once = std::sync::Once::new();
    let dir = dir.to_path_buf();
    PRUNE.call_once(move || {
        prune_stale_tmps(&dir);
    });
}

fn hash_matches(path: &std::path::Path, expected: &str) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => hex_digest(&bytes) == expected,
        Err(_) => false,
    }
}

/// Best-effort cleanup of orphaned `*.py.tmp.<pid>` files left behind
/// when a previous extract crashed mid-write. Skips files owned by
/// running PIDs to avoid stomping a concurrent extract.
fn prune_stale_tmps(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        // Match `<anything>.py.tmp.<pid>`.
        let Some((_, pid_str)) = name_str.rsplit_once(".tmp.") else { continue };
        let Ok(pid) = pid_str.parse::<u32>() else { continue };
        if pid != std::process::id() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
