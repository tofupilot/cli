//! A plug service must not outlive the process that spawned it.
//!
//! SIGKILL of the CLI (crash, force-quit, supervisor) runs no `Drop`:
//! before the parent watchdog in `tp_plug.py`, the plug kept running
//! with its listen socket and its instrument session open, and the
//! next run could not connect. The parent here is a helper binary so
//! the test can SIGKILL it; the plug is spawned through the real
//! `PlugServiceManager` path.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

fn python3() -> Option<PathBuf> {
    let out = std::process::Command::new("which")
        .arg("python3")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    path.exists().then_some(path)
}

/// Pid of the process listening on the loopback port, if any.
fn listener_pid(port: u16) -> Option<i32> {
    let out = std::process::Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}"), "-sTCP:LISTEN"])
        .output()
        .expect("lsof available on dev machines");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.trim().parse().ok())
}

fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

#[test]
fn plug_service_exits_after_its_parent_is_sigkilled() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let dir = std::env::temp_dir().join(format!("tp-plug-orphan-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cleaned = dir.join("cleaned");
    std::fs::write(
        dir.join("plug.py"),
        format!(
            r#"
class Plug:
    def ping(self):
        return "pong"

    def __del__(self):
        with open({cleaned:?}, "w") as f:
            f.write("closed\n")
"#,
            cleaned = cleaned.to_string_lossy()
        ),
    )
    .unwrap();

    let mut helper = std::process::Command::new(env!("CARGO_BIN_EXE_plug-orphan-helper"))
        .arg(&dir)
        .arg(&python)
        .arg(dir.join("plug.py"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("helper spawns");
    let mut line = String::new();
    BufReader::new(helper.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port: u16 = line
        .trim()
        .strip_prefix("PORT:")
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("helper reported {line:?}"));

    let plug_pid = listener_pid(port).expect("plug service is listening");
    assert!(alive(plug_pid));

    kill(Pid::from_raw(helper.id() as i32), Signal::SIGKILL).unwrap();
    let _ = helper.wait();

    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(plug_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let survived = alive(plug_pid);
    if survived {
        // Never leave the orphan behind for the next run of the suite.
        let _ = kill(Pid::from_raw(plug_pid), Signal::SIGKILL);
    }
    let still_listening = listener_pid(port).is_some();
    let cleanup_ran = cleaned.exists();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(!survived, "tp_plug.py (pid {plug_pid}) outlived its SIGKILLed parent by 3 s");
    assert!(!still_listening, "port {port} is still held after the parent died");
    assert!(cleanup_ran, "the plug instance was not released (__del__ did not run) on orphan exit");
}
