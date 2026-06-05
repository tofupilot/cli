//! Python runtime resolution for a pulled deployment.
//!
//! The deployment bundle produced by the build worker ships a
//! pre-installed venv and the procedure source tree. This module
//! locates the prebuilt interpreter and the user's entry file.
//!
//! Contract between build worker and CLI:
//!
//! ```text
//! <deployment>/
//!   manifest.json                     (bundle metadata)
//!   wheels/, vendor/                  (consumed by the installer)
//!   <root_directory>/                 (= deployment root for single-package bundles)
//!     main.py / procedure.yaml        (user-committed entry)
//!     venv/bin/python                 (venv interpreter, written by sync.rs inside
//!                                      the package dir for single-package AND
//!                                      monorepo bundles)
//! ```

use command_group::AsyncCommandGroup;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

// ---------------------------------------------------------------------------
// Entry point detection
// ---------------------------------------------------------------------------

/// Locate the user's Python entry file in a package dir. Source-shipped
/// bundles always commit `main.py`; that's the single contract.
pub fn find_entry_point(dir: &Path) -> Option<PathBuf> {
    let p = dir.join("main.py");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Python resolution
// ---------------------------------------------------------------------------

/// Compose `<dir>/<name>/{bin,Scripts}/python(.exe)`. The two-layer
/// shape is uv/python's own convention; we just join.
fn venv_python_path(dir: &Path, name: &str) -> PathBuf {
    if cfg!(target_os = "windows") {
        dir.join(name).join("Scripts").join("python.exe")
    } else {
        dir.join(name).join("bin").join("python")
    }
}

/// Resolve the Python interpreter for a pulled deployment. `pull/sync.rs`
/// writes the venv at `<package_dir>/venv` — directly inside the
/// procedure's package dir for both single-package and monorepo bundles
/// (verify the comment block at sync.rs:518-526 if updating this).
pub fn deployment_python(package_dir: &Path) -> crate::error::CliResult<PathBuf> {
    let python = venv_python_path(package_dir, "venv");
    if python.exists() {
        Ok(python)
    } else {
        Err(format!(
            "No Python venv found at {}. Run `tofupilot pull` to provision the deployment.",
            python.display()
        )
        .into())
    }
}

// ---------------------------------------------------------------------------
// Process spawn helpers
// ---------------------------------------------------------------------------

/// Build a Python command with standard env vars.
///
/// The CLI is the orchestrator: it reads the Python process's NDJSON events
/// and owns persistence, upload, and WebSocket streaming. Python subprocesses
/// inherit no credentials, no TofuPilot URLs, nothing from the user's shell
/// beyond `PATH` — procedure code has zero awareness of where results go.
///
/// `queue_id` is the only CLI→Python handshake: it names a scratch dir under
/// `~/.tofupilot/attachments/` where the connector can park file attachments
/// that the CLI then picks up when building the `QueuedRun`.
pub fn build_command(
    python_path: &Path,
    args: &[&Path],
    working_dir: &Path,
    queue_id: &str,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(python_path);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.current_dir(working_dir)
        .env_remove("PYTHONHOME")
        .env_remove("PYTHONPATH")
        .env_remove("TOFUPILOT_API_KEY")
        .env_remove("TOFUPILOT_URL")
        .env("PYTHONUNBUFFERED", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("TOFUPILOT_QUEUE_ID", queue_id)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Format a spawn-failure message with an interpreter diagnostic
/// appended when one applies. Replaces the generic `os error 5` /
/// `os error 13` text operators normally see with an actionable
/// hint (missing exec bit, broken venv, AV/EDR refusal).
pub fn spawn_error_message(python_path: &Path, source: &std::io::Error) -> String {
    let diag = execution_engine::path_utils::diagnose_interpreter(python_path);
    match diag {
        Some(d) => format!("Failed to spawn Python: {source} — {d}"),
        None => format!("Failed to spawn Python: {source}"),
    }
}

/// Graceful shutdown: SIGTERM the group, wait 5s, SIGKILL.
pub async fn graceful_shutdown(child: &mut command_group::AsyncGroupChild) -> i32 {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    {
        let _ = child.kill().await;
    }

    match tokio::time::timeout(
        crate::config::timeouts::PYTHON_GRACEFUL_SHUTDOWN,
        child.wait(),
    )
    .await
    {
        Ok(Ok(s)) => s.code().unwrap_or(130),
        // SIGTERM didn't land in time — escalate to SIGKILL. `kill()` is async on
        // `command_group::AsyncGroupChild`; awaiting it ensures the kill actually
        // issues before we wait for the corpse, otherwise `wait()` blocks on a
        // process that's still very much alive.
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            130
        }
    }
}

/// Stream the Python subprocess's stderr to this process's stderr.
///
/// We don't wrap stderr in the agent protocol: stdout is the protocol stream
/// and stderr is a separate channel by convention. Agents that want to capture
/// stderr can redirect it on their side (`2>&1` or a pipe).
pub fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    _json_mode: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            eprintln!("{line}");
        }
    })
}

/// Like `spawn_stderr_reader` but also captures the tail of stderr (last
/// ~1KB) so the caller can include it in a structured crash event when the
/// subprocess exits abnormally.
pub fn spawn_stderr_reader_with_capture(
    stderr: tokio::process::ChildStderr,
) -> (
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Mutex<String>>,
) {
    let buf = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let buf_clone = buf.clone();
    let handle = tokio::spawn(async move {
        const MAX_TAIL_BYTES: usize = 4096;
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            eprintln!("{line}");
            let mut b = buf_clone.lock().await;
            b.push_str(&line);
            b.push('\n');
            if b.len() > MAX_TAIL_BYTES {
                let mut drop_n = b.len() - MAX_TAIL_BYTES;
                // Advance to the next char boundary to avoid slicing a
                // multi-byte UTF-8 sequence. Bumping forward (not back) keeps
                // the tail length <= MAX_TAIL_BYTES in the steady state.
                while drop_n < b.len() && !b.is_char_boundary(drop_n) {
                    drop_n += 1;
                }
                b.drain(..drop_n);
            }
        }
    });
    (handle, buf)
}

/// Identity bundle threaded into [`execute`] for station-mode runs so
/// the operator-UI sees `RunStarted` + a terminal `RunComplete` even
/// though plain Python has no phases to report. `None` for standalone
/// `tofupilot run` against a plain-Python procedure (no operator-UI to
/// strand).
pub struct PlainRunContext {
    pub procedure_id: String,
    pub procedure_name: String,
    pub execution_id: String,
    pub event_tx: tokio::sync::broadcast::Sender<station_protocol::StationEvent>,
}

/// Plain execution (no connector). Passes stdout/stderr through directly.
///
/// `cancel_rx` is the run's single cancel surface. Plain Python has no
/// force/graceful distinction — Stop and Kill both collapse to
/// `graceful_shutdown` (SIGTERM → 5s wait → SIGKILL via process group),
/// matching the existing ctrl-C path.
///
/// When `ctx` is provided we wrap the run in synthetic lifecycle
/// events (`RunStarted` on spawn, `RunComplete` on exit). Standalone
/// runs pass `None` and just emit through stdout / exit code.
pub async fn execute(
    python_path: &Path,
    file: &Path,
    working_dir: &Path,
    json_mode: bool,
    mut cancel_rx: super::cancel::Receiver,
    ctx: Option<PlainRunContext>,
) -> i32 {
    if let Some(ref c) = ctx {
        let _ = c.event_tx.send(station_protocol::StationEvent::RunStarted {
            procedure_id: c.procedure_id.clone(),
            procedure_name: c.procedure_name.clone(),
            execution_id: c.execution_id.clone(),
            phases: Vec::new(),
            slots: Vec::new(),
            plugs: Vec::new(),
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
            run_id: None,
            unit: None,
        });
    }
    let mut cmd = build_command(python_path, &[file], working_dir, "");
    let mut child = match cmd.group().kill_on_drop(true).spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = spawn_error_message(python_path, &e);
            crate::log::error(&msg);
            if let Some(ref c) = ctx {
                super::emit::run_crashed(
                    &c.event_tx,
                    None,
                    &c.procedure_id,
                    &c.execution_id,
                    "spawn_failed",
                    &msg,
                    1,
                );
            }
            return 1;
        }
    };

    let inner = child.inner();
    // `build_command` always pipes both streams, so these are infallible
    // in practice — but match-and-bail keeps us in lockstep with the
    // OpenHTF connector path (and panic-frees the spawn handler).
    let emit_capture_fail = |ctx: &Option<PlainRunContext>, what: &str| {
        if let Some(c) = ctx.as_ref() {
            super::emit::run_crashed(
                &c.event_tx,
                None,
                &c.procedure_id,
                &c.execution_id,
                "spawn_failed",
                &format!("Failed to capture {what} from Python child"),
                1,
            );
        }
    };
    let stdout = match inner.stdout.take() {
        Some(s) => s,
        None => {
            crate::log::error("Failed to capture stdout");
            emit_capture_fail(&ctx, "stdout");
            return 1;
        }
    };
    let stderr = match inner.stderr.take() {
        Some(s) => s,
        None => {
            crate::log::error("Failed to capture stderr");
            emit_capture_fail(&ctx, "stderr");
            return 1;
        }
    };

    let is_json = json_mode;
    let stdout_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if is_json {
                println!("{}", serde_json::json!({"type": "stdout", "line": line}));
            } else {
                println!("{line}");
            }
        }
    });

    let stderr_handle = spawn_stderr_reader(stderr, json_mode);

    let exit_code = tokio::select! {
        status = child.wait() => {
            match status {
                Ok(s) => s.code().unwrap_or(1),
                Err(e) => { crate::log::error(&format!("Process error: {e}")); 1 }
            }
        }
        _ = tokio::signal::ctrl_c() => graceful_shutdown(&mut child).await,
        _ = cancel_rx.wait_any() => graceful_shutdown(&mut child).await,
    };

    let _ = stdout_handle.await;
    let _ = stderr_handle.await;
    if let Some(c) = ctx {
        super::emit::run_complete(
            &c.event_tx,
            super::outcomes::from_exit_code(exit_code),
            &c.execution_id,
            None,
        );
    }
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch_venv_python(dir: &Path, venv_name: &str) -> PathBuf {
        let python = venv_python_path(dir, venv_name);
        fs::create_dir_all(python.parent().unwrap()).unwrap();
        fs::write(&python, b"").unwrap();
        python
    }

    #[test]
    fn deployment_python_finds_venv_in_package_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let expected = touch_venv_python(tmp.path(), "venv");
        let got = deployment_python(tmp.path()).unwrap();
        assert_eq!(got, expected);
    }

    #[test]
    fn deployment_python_errors_when_venv_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = deployment_python(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("No Python venv found"), "got: {err}");
        assert!(err.contains("venv"), "expected venv path in error: {err}");
    }
}
