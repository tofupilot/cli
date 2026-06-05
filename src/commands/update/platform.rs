//! Platform-specific update bits: the release platform key and the binary
//! self-replace + re-exec on each OS.

use std::path::Path;

#[cfg(target_os = "macos")]
const OS: &str = "darwin";
#[cfg(target_os = "linux")]
const OS: &str = "linux";
#[cfg(target_os = "windows")]
const OS: &str = "windows";
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const OS: &str = "unknown";

#[cfg(target_arch = "x86_64")]
const ARCH: &str = "amd64";
#[cfg(target_arch = "aarch64")]
const ARCH: &str = "arm64";
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const ARCH: &str = "unknown";

pub fn platform_key() -> String {
    format!("{OS}-{ARCH}")
}

pub fn is_disabled() -> bool {
    std::env::var("TOFUPILOT_NO_UPDATE").is_ok_and(|v| v == "1" || v == "true")
}

#[cfg(unix)]
pub fn reexec(exe: &Path, args: &[String]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    // execvp replaces the process image — same pid, lock automatically
    // transfers, no parent/child collision.
    std::process::Command::new(exe).args(&args[1..]).exec()
}

#[cfg(windows)]
pub fn reexec(exe: &Path, args: &[String]) -> std::io::Error {
    use std::os::windows::process::CommandExt;
    // Windows has no exec(): we spawn the new binary, then exit. The
    // parent must release the redb lock first (caller responsibility via
    // db::close) so the child can reopen state on startup. CREATE_NO_WINDOW
    // keeps the inherited console; we don't want a flashing second window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    match std::process::Command::new(exe)
        .args(&args[1..])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(_) => std::process::exit(0),
        Err(e) => e,
    }
}

#[cfg(not(any(unix, windows)))]
pub fn reexec(exe: &Path, args: &[String]) -> std::io::Error {
    match std::process::Command::new(exe).args(&args[1..]).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => e,
    }
}
