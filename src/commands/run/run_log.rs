//! Per-run NDJSON event log on disk.
//!
//! Every `StationEvent` a run broadcasts is appended to
//! `~/.tofupilot/logs/run-<execution_id>.log`, one JSON object per line
//! with a `logged_at` wall-clock stamp. This is the artifact support
//! asks for when a customer reports "it hangs / it failed" — the full
//! event timeline survives the terminal scrollback, the kiosk tab, and
//! the operator's memory. Best-effort by design: a run must never fail
//! or slow down because its log file couldn't be written.

use std::io::Write;
use tokio::sync::broadcast;

/// Number of run logs kept. Oldest beyond this are deleted on each new
/// run. Stations run thousands of cycles; an unbounded log dir would
/// grow forever on a machine nobody looks at.
const KEEP_LOGS: usize = 200;

/// Directory the run logs live in (`~/.tofupilot/logs`).
pub fn log_dir() -> Option<std::path::PathBuf> {
    crate::commands::db::home_dir()
        .ok()
        .map(|h| h.join(".tofupilot").join("logs"))
}

/// Path of the log file for one run.
pub fn log_path(execution_id: &str) -> Option<std::path::PathBuf> {
    log_dir().map(|d| d.join(format!("run-{execution_id}.log")))
}

/// Subscribe to the run's broadcast and append every event to the run's
/// log file until the channel closes. Returns the log path when the
/// writer could start (callers surface it in error messages), `None`
/// when the home dir / file isn't writable — the run proceeds unlogged.
pub fn spawn_writer(
    execution_id: &str,
    rx: broadcast::Receiver<station_protocol::StationEvent>,
) -> Option<std::path::PathBuf> {
    let dir = log_dir()?;
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    prune_old_logs(&dir);
    let path = dir.join(format!("run-{execution_id}.log"));
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return None,
    };
    let mut rx = rx;
    tokio::spawn(async move {
        let mut file = std::io::BufWriter::new(file);
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let Ok(json) = serde_json::to_string(&ev) else {
                        continue;
                    };
                    let line = format!(
                        "{{\"logged_at\":\"{}\",\"event\":{json}}}\n",
                        chrono::Utc::now().to_rfc3339()
                    );
                    if file.write_all(line.as_bytes()).is_err() {
                        break; // disk full / file yanked: stop, never disturb the run
                    }
                    // Flush per event: the whole point is that the log is
                    // complete at the moment a wedged run gets kill -9'd.
                    let _ = file.flush();
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        let _ = file.flush();
    });
    Some(path)
}

/// Keep only the newest `KEEP_LOGS` run logs (by mtime). Best-effort.
fn prune_old_logs(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?;
            if !(name.starts_with("run-") && name.ends_with(".log")) {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, path))
        })
        .collect();
    if logs.len() < KEEP_LOGS {
        return;
    }
    logs.sort_by_key(|(t, _)| *t);
    let excess = logs.len() + 1 - KEEP_LOGS; // +1: the new run's log is about to be created
    for (_, path) in logs.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}
