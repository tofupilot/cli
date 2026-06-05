//! `tofupilot install` — write the OS-level station service definition
//! (systemd user unit on Linux, launchd LaunchAgent plist on macOS,
//! per-user Run key on Windows) and enable auto-start.
//!
//! This is the single, explicit mutation point for the supervisor
//! config. The station daemon never edits its own unit; it just runs.
//! Operators (or `finalize_station_login`) call this once at install
//! time and again after a major upgrade if the binary moved.

use crate::log;

pub fn run_cmd(enable: bool, json_mode: bool) -> i32 {
    match crate::commands::config::apply_launch_on_boot(enable) {
        Ok(()) => {
            if !json_mode {
                if enable {
                    log::success("Station service installed and started.");
                    print_logs_hint();
                } else {
                    log::success("Station service uninstalled.");
                }
            }
            0
        }
        Err(e) => {
            log::error(&format!("Install failed: {e}"));
            1
        }
    }
}

fn print_logs_hint() {
    #[cfg(target_os = "macos")]
    log::info("Logs: ~/Library/Logs/TofuPilot/stdout.log");
    #[cfg(target_os = "linux")]
    log::info("Logs: journalctl --user -u tofupilot -f");
    #[cfg(target_os = "windows")]
    log::info("Service runs at next interactive login.");
}
