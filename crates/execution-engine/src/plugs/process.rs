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

/// Deadline for the child's `NDJSON_PORT:` startup handshake.
///
/// A healthy worker prints the port line within a second of exec — it is
/// the first thing `tp_worker.py` writes, before any framework import or
/// phase work. If the line hasn't arrived by this deadline the child is
/// alive but not executing (an endpoint-protection agent holding the
/// freshly-provisioned interpreter, or a startup environment flooding
/// stderr into the un-drained pipe). Without a deadline this was the
/// only un-timed read in the engine: `initialize()` blocked forever and
/// the operator saw an infinite spinner with no diagnostic. Mirrors the
/// CLI's `PYTHON_STARTUP_STALL` (apps/cli config/timeouts.rs) — this
/// crate can't see that constant, so the value is duplicated on purpose.
const PORT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(90);

/// Outcome of waiting for the child's `NDJSON_PORT:<port>` line.
#[derive(Debug)]
pub(crate) enum PortHandshake {
    Port(u16),
    /// Deadline elapsed with the pipe still open — the child is alive
    /// but never produced its first line of output.
    TimedOut,
    /// The stream ended (child exited) or produced a non-port line;
    /// carries whatever the line was (empty string on clean EOF).
    BadLine(String),
    /// Read failed outright.
    Io(std::io::Error),
}

/// Read one line and classify it. Extracted from `ChildProcess::spawn`
/// so the deadline semantics are unit-testable without spawning real
/// processes: EOF and garbage classify as `BadLine` (crash — the caller
/// surfaces stderr), while an open-but-silent pipe becomes `TimedOut`
/// instead of blocking forever.
pub(crate) async fn read_port_handshake<R>(reader: &mut R, deadline: Duration) -> PortHandshake
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = String::new();
    match tokio::time::timeout(deadline, reader.read_line(&mut line)).await {
        Err(_) => PortHandshake::TimedOut,
        Ok(Err(e)) => PortHandshake::Io(e),
        Ok(Ok(_)) => match line
            .trim()
            .strip_prefix("NDJSON_PORT:")
            .and_then(|s| s.parse::<u16>().ok())
        {
            Some(port) => PortHandshake::Port(port),
            None => PortHandshake::BadLine(line.trim().to_string()),
        },
    }
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
        match read_port_handshake(&mut stdout_reader, PORT_HANDSHAKE_TIMEOUT).await {
            PortHandshake::Port(port) => {
                if let (Some(stderr_handle), Some(handler)) = (stderr.take(), stderr_handler) {
                    handler(stderr_handle);
                }
                Ok(Self { port, process })
            }
            failure => {
                // Drain stderr BEFORE killing: on the timeout path the
                // child is still alive, and if it's wedged writing into
                // the full stderr pipe (nobody read it during the
                // handshake), this drain both captures the diagnostic
                // and is the only way to see what flooded. The 500ms/8KB
                // cap keeps a silent child from stalling the error path.
                let stderr_text = match stderr.take() {
                    Some(s) => drain_stderr(s).await,
                    None => String::new(),
                };
                kill_on_err(process);
                let prefix = match failure {
                    PortHandshake::TimedOut => format!(
                        "Python process started but produced no output for {}s and never \
                         reported its startup handshake. The interpreter is being prevented \
                         from executing — on managed machines this is usually \
                         endpoint-protection/antivirus software holding the freshly-installed \
                         Python. Try running it by hand to confirm:\n    {} -c \"print('ok')\"\n\
                         If that hangs too, allowlist the interpreter in your security software.",
                        PORT_HANDSHAKE_TIMEOUT.as_secs(),
                        python_path,
                    ),
                    PortHandshake::Io(e) => format!("Failed to read port from process: {}", e),
                    PortHandshake::BadLine(line) => format!(
                        "Invalid port line from process: {:?}\nPython worker may have crashed during startup.",
                        line
                    ),
                    PortHandshake::Port(_) => unreachable!("success handled above"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    // These drive `read_port_handshake` over in-memory duplex pipes on
    // tokio's paused clock, so the 90s-class deadlines elapse in virtual
    // time and the tests are instant. The invariant under test: a child
    // that crashes or prints garbage classifies as `BadLine` (diagnosable
    // crash), while a child that stays alive without ever writing — the
    // shape that used to hang `initialize()` forever — becomes `TimedOut`.

    #[tokio::test(start_paused = true)]
    async fn handshake_parses_port_line() {
        let (mut client, server) = tokio::io::duplex(256);
        client.write_all(b"NDJSON_PORT:4321\n").await.unwrap();
        let mut reader = BufReader::new(server);
        match read_port_handshake(&mut reader, Duration::from_secs(90)).await {
            PortHandshake::Port(4321) => {}
            other => panic!("expected Port(4321), got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn handshake_classifies_clean_eof_as_bad_line() {
        let (client, server) = tokio::io::duplex(256);
        drop(client); // child exited without writing anything
        let mut reader = BufReader::new(server);
        match read_port_handshake(&mut reader, Duration::from_secs(90)).await {
            PortHandshake::BadLine(line) => assert_eq!(line, ""),
            other => panic!("expected BadLine(\"\"), got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn handshake_classifies_garbage_as_bad_line() {
        let (mut client, server) = tokio::io::duplex(256);
        client.write_all(b"Traceback (most recent call last):\n").await.unwrap();
        let mut reader = BufReader::new(server);
        match read_port_handshake(&mut reader, Duration::from_secs(90)).await {
            PortHandshake::BadLine(line) => assert!(line.starts_with("Traceback")),
            other => panic!("expected BadLine(traceback), got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn handshake_times_out_on_alive_but_silent_child() {
        // Writer stays alive (held, not dropped) but never writes — the
        // exact shape of the field hang. Must resolve as TimedOut, not
        // block forever.
        let (_client, server) = tokio::io::duplex(256);
        let mut reader = BufReader::new(server);
        match read_port_handshake(&mut reader, Duration::from_secs(90)).await {
            PortHandshake::TimedOut => {}
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn handshake_accepts_slow_but_in_time_port_line() {
        // Port arrives late but inside the deadline: must succeed, never
        // false-timeout a slow-starting interpreter.
        let (mut client, server) = tokio::io::duplex(256);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            client.write_all(b"NDJSON_PORT:9\n").await.unwrap();
            // Keep the writer alive so EOF doesn't race the read.
            std::future::pending::<()>().await;
        });
        let mut reader = BufReader::new(server);
        match read_port_handshake(&mut reader, Duration::from_secs(90)).await {
            PortHandshake::Port(9) => {}
            other => panic!("expected Port(9), got {other:?}"),
        }
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
