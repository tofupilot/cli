use command_group::AsyncGroupChild;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::ChildStderr;

/// Drain whatever the python child has already written to stderr without
/// blocking on a healthy worker. Used only on the failure path of the
/// port-line handshake, so a few-KB cap and a short timeout are fine —
/// we just want enough text to surface a traceback to the operator.
const STDERR_DRAIN_CAP_BYTES: usize = 8 * 1024;
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

async fn drain_stderr(stderr: ChildStderr) -> String {
    let mut reader = BufReader::new(stderr).take(STDERR_DRAIN_CAP_BYTES as u64);
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(STDERR_DRAIN_TIMEOUT, reader.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).trim().to_string()
}

#[cfg(unix)]
use nix::sys::signal::{self, Signal};
#[cfg(unix)]
use nix::unistd::Pid;

/// Kill a process group by its PID. Cross-platform.
fn kill_process_group(id: u32) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(id) {
            let _ = signal::kill(Pid::from_raw(-pid), Signal::SIGKILL);
        } else {
            log::error!("PID {} exceeds i32::MAX, cannot kill process group", id);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &id.to_string(), "/T", "/F"])
            .output();
    }
}

/// A managed child process that advertises an NDJSON TCP port on stdout.
#[derive(Debug)]
pub struct ChildProcess {
    pub port: u16,
    pub process: AsyncGroupChild,
}

impl ChildProcess {
    pub async fn spawn(
        python_path: &str,
        script_path: PathBuf,
        args: Vec<String>,
        working_dir: Option<&PathBuf>,
        env_vars: Vec<(String, String)>,
        stderr_handler: Option<Box<dyn FnOnce(ChildStderr) + Send>>,
    ) -> Result<Self, String> {
        let mut cmd = crate::worker::runtime::python::PythonCommandBuilder::new(python_path)
            .unbuffered()
            .with_stdio(Stdio::null(), Stdio::piped(), Stdio::piped())
            .arg(&script_path);

        for arg in args {
            cmd = cmd.arg(&arg);
        }

        for (key, value) in env_vars {
            cmd = cmd.env(&key, &value);
        }

        if let Some(dir) = working_dir {
            cmd = cmd.working_dir(dir);
        }

        let mut process = match cmd.spawn() {
            Ok(p) => p,
            Err(e) => {
                // Replace the generic `os error 5` / `os error 13` text
                // with an actionable diagnostic when we can name the
                // cause (missing exec bit, broken symlink, dir-not-
                // file, Windows AV/EDR refusal).
                let diag = crate::path_utils::diagnose_interpreter(
                    std::path::Path::new(python_path),
                );
                return Err(match diag {
                    Some(d) => format!("Failed to spawn process: {} — {}", e, d),
                    None => format!("Failed to spawn process: {}", e),
                });
            }
        };

        let kill_on_err = |mut p: AsyncGroupChild| {
            if let Some(id) = p.inner().id() {
                kill_process_group(id);
            }
        };

        let stdout = match process.inner().stdout.take() {
            Some(s) => s,
            None => {
                kill_on_err(process);
                return Err("Failed to get stdout from process".to_string());
            }
        };

        // Hold stderr locally until the port line is read. Handing it to
        // the stderr_handler (which logs via the `log` crate) before the
        // handshake means a python crash on startup writes its traceback
        // to a logger that may not be configured (CLI never installs a
        // log impl), and the user sees "Invalid port line from process: "
        // with no diagnostic. Read stderr inline on failure so the
        // traceback is embedded in the returned error.
        let mut stderr = process.inner().stderr.take();

        let mut stdout_reader = BufReader::new(stdout);
        let mut port_line = String::new();
        let read_result = stdout_reader.read_line(&mut port_line).await;

        let parsed_port = port_line
            .trim()
            .strip_prefix("NDJSON_PORT:")
            .and_then(|s| s.parse::<u16>().ok());

        match (read_result, parsed_port) {
            (Ok(_), Some(port)) => {
                if let (Some(stderr_handle), Some(handler)) = (stderr.take(), stderr_handler) {
                    handler(stderr_handle);
                }
                Ok(Self { port, process })
            }
            (read_result, _) => {
                let stderr_text = match stderr.take() {
                    Some(s) => drain_stderr(s).await,
                    None => String::new(),
                };
                kill_on_err(process);
                let prefix = match read_result {
                    Err(e) => format!("Failed to read port from process: {}", e),
                    Ok(_) => format!(
                        "Invalid port line from process: {:?}\nPython worker may have crashed during startup.",
                        port_line.trim()
                    ),
                };
                if stderr_text.is_empty() {
                    Err(format!("{prefix}\n(no stderr captured)"))
                } else {
                    Err(format!("{prefix}\n--- python stderr ---\n{stderr_text}"))
                }
            }
        }
    }

    /// Graceful shutdown by sending SIGTERM-like signal, poll for exit, kill if needed.
    pub async fn graceful_shutdown_signal(
        &mut self,
        timeout_secs: u64,
    ) -> Result<(), String> {
        // Try to kill gracefully first
        let max_wait = Duration::from_secs(timeout_secs);
        let poll_interval = Duration::from_millis(100);
        let mut waited = Duration::ZERO;

        // Send SIGTERM on Unix
        #[cfg(unix)]
        if let Some(id) = self.process.inner().id() {
            if let Ok(pid) = i32::try_from(id) {
                let _ = signal::kill(Pid::from_raw(-pid), Signal::SIGTERM);
            }
        }

        while waited < max_wait {
            match self.process.try_wait() {
                Ok(Some(_status)) => {
                    return Ok(());
                }
                Ok(None) => {
                    tokio::time::sleep(poll_interval).await;
                    waited += poll_interval;
                }
                Err(e) => {
                    log::error!("Error checking process: {}", e);
                    break;
                }
            }
        }

        log::warn!(
            "Process did not exit after {:.1}s, killing process group",
            max_wait.as_secs_f32()
        );

        if let Err(e) = self.process.kill().await {
            log::error!("Failed to kill process group: {}", e);
        }

        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            self.process.wait()
        ).await;

        Ok(())
    }

    /// Graceful shutdown - sends a custom RPC-like shutdown, polls for exit, kills if needed.
    pub async fn graceful_shutdown<F, Fut>(
        &mut self,
        shutdown_fn: F,
        timeout_secs: u64,
    ) -> Result<(), String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
    {
        shutdown_fn().await.ok();

        let max_wait = Duration::from_secs(timeout_secs);
        let poll_interval = Duration::from_millis(100);
        let mut waited = Duration::ZERO;

        while waited < max_wait {
            match self.process.try_wait() {
                Ok(Some(_status)) => {
                    return Ok(());
                }
                Ok(None) => {
                    tokio::time::sleep(poll_interval).await;
                    waited += poll_interval;
                }
                Err(e) => {
                    log::error!("Error checking process: {}", e);
                    break;
                }
            }
        }

        log::warn!(
            "Process did not exit after {:.1}s, killing process group",
            max_wait.as_secs_f32()
        );

        if let Err(e) = self.process.kill().await {
            log::error!("Failed to kill process group: {}", e);
        }

        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            self.process.wait()
        ).await;

        Ok(())
    }

    /// Force kill - immediately kills the process group
    pub async fn force_kill(&mut self) -> Result<(), String> {
        self.process
            .kill()
            .await
            .map_err(|e| format!("Failed to kill process: {}", e))?;
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            self.process.wait()
        ).await;
        Ok(())
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        // Unix: `tokio::Child::kill_on_drop(true)` only kills the
        // immediate child via TerminateProcess; grandchildren keep
        // running. Sending SIGKILL to the negative pid (process group)
        // catches the whole tree. Required.
        //
        // Windows: `command_group`'s Job Object is configured with
        // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Dropping the inner
        // `AsyncGroupChild` closes the job handle, which atomically
        // terminates every process attached to it. An additional
        // `taskkill /PID <id> /T /F` here is redundant and, worse, can
        // hit a recycled PID — Windows reuses PIDs aggressively, and
        // the brief window between job-close and our explicit kill is
        // enough that we may terminate an unrelated process.
        #[cfg(unix)]
        if let Some(id) = self.process.inner().id() {
            kill_process_group(id);
        }
    }
}
