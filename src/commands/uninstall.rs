//! The `uninstall` command: remove the station service and, unless
//! `--keep-data`, wipe credentials, deployments, and local state.

use std::fs;
use std::path::Path;

use super::auth::credentials;
use super::config;
use super::db;
use serde::Serialize;

#[derive(Serialize)]
struct UninstallOutput {
    removed: Vec<String>,
    kept: Vec<String>,
    errors: Vec<String>,
}

/// Run the uninstall command.
pub async fn run_cmd(keep_data: bool, yes: bool, json_mode: bool) -> i32 {
    let data_dir = match db::tofupilot_dir() {
        Ok(d) => d,
        Err(e) => {
            if json_mode {
                println!("{}", serde_json::json!({ "error": e.to_string() }));
            } else {
                crate::log::error(&format!("Cannot locate data directory: {e}"));
            }
            return 1;
        }
    };

    let exe_path = std::env::current_exe().ok();

    // Short summary -- paths omitted, they live under the documented
    // ~/.tofupilot data dir and noise here just makes the prompt scroll.
    if !json_mode {
        if keep_data {
            crate::log::info("This will remove TofuPilot, credentials, and cache.");
            crate::log::info("Run data and deployments will be kept.");
        } else {
            crate::log::info("This will remove TofuPilot, credentials, cache, and all data.");
            crate::log::info("Pass --keep-data to preserve runs and deployments.");
        }
    }

    // Confirm unless --yes
    if !yes {
        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Proceed with uninstall?")
            .default(false)
            .interact()
            .unwrap_or(false);

        if !confirmed {
            if json_mode {
                println!("{}", serde_json::json!({ "cancelled": true }));
            } else {
                crate::log::info("Cancelled.");
            }
            return 0;
        }
    }

    // Notify server before destroying credentials. Best-effort: if the key
    // was already revoked (e.g. another install replaced us), the server
    // logged_out event stays at its existing reason and we just continue
    // with local cleanup. Local uninstall always completes regardless.
    if let Some(creds) = credentials::load() {
        crate::commands::auth::notify_server_logout(&creds, true).await;
    }

    let mut output = UninstallOutput {
        removed: Vec::new(),
        kept: Vec::new(),
        errors: Vec::new(),
    };

    // 1. Turn off launch-on-boot + desktop icon via the same helpers that manage
    //    them in normal operation. Keeps labels/paths in sync with config.rs.
    let lob = launch_on_boot_artifact_exists();
    let icon = desktop_icon_exists();
    if !json_mode {
        crate::log::info(&format!(
            "Launch-on-boot: {}",
            if lob { "found" } else { "not found" }
        ));
        crate::log::info(&format!(
            "Desktop icon: {}",
            if icon { "found" } else { "not found" }
        ));
    }
    // launch_on_boot is no longer a config key driven from the
    // server; it's owned by the explicit `tofupilot install`
    // subcommand. Call the same low-level remover here.
    match config::apply_launch_on_boot(false) {
        Ok(()) if lob => {
            if !json_mode {
                crate::log::success("Removed: launch-on-boot service");
            }
            output.removed.push("launch-on-boot service".into());
        }
        Ok(()) => {}
        Err(e) => output.errors.push(format!("launch_on_boot=off: {e}")),
    }
    turn_off("desktop_icon", icon, "desktop icon", json_mode, &mut output);

    // 2. Remove credentials
    remove_file(
        &crate::commands::auth::credentials::credentials_path(),
        &mut output,
    );

    // 3. Remove update cache
    remove_dir(&data_dir.join("update"), &mut output);

    // 3b. Remove the engine's extracted Python helper scripts
    // (`tp_worker.py`, `tp_plug.py`). These are reproduced from the
    // binary on next run, so removing them is always safe regardless
    // of `--keep-data` (which protects user data, not cache).
    remove_dir(&data_dir.join("runtime"), &mut output);

    if keep_data {
        // Keep state.redb and deployments
        if let Ok(state_path) = db::state_path() {
            if state_path.exists() {
                output.kept.push(state_path.display().to_string());
            }
        }
        if let Ok(deployments) = db::deployments_dir() {
            if deployments.exists() {
                output.kept.push(deployments.display().to_string());
            }
        }
    } else {
        // Clear deployments via the shared helper so DB pull state is wiped
        // alongside the directory; otherwise state.redb keeps stale manifest
        // references that would resurface if the user reinstalls and reuses
        // the same data dir before remove_dir lands.
        if let Err(e) = db::clear_deployments() {
            output.errors.push(format!("clear deployments: {e}"));
        }
        remove_dir(&data_dir, &mut output);
    }

    // 4. Remove binary (last -- we're running from it). self_delete handles
    //    the Windows case where a running .exe cannot be unlinked directly.
    if let Some(ref exe) = exe_path {
        match self_replace::self_delete() {
            Ok(()) => output.removed.push(exe.display().to_string()),
            Err(e) => output.errors.push(format!("{}: {e}", exe.display())),
        }
    }

    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&output).unwrap_or_default()
        );
    } else {
        if !output.errors.is_empty() {
            for err in &output.errors {
                crate::log::warn(err);
            }
        }
        if !output.kept.is_empty() {
            crate::log::info(&format!("Kept: {}", output.kept.join(", ")));
        }
        crate::log::success("TofuPilot uninstalled.");
    }

    0
}

/// Turn off a config key via the shared config::apply path. Only logs and
/// records "removed" when the underlying artifact actually existed on disk;
/// apply() is still called unconditionally so logged-out-but-disk-absent
/// state converges cleanly.
fn turn_off(key: &str, existed: bool, label: &str, json_mode: bool, output: &mut UninstallOutput) {
    match config::apply(key, "off") {
        Ok(()) if existed => {
            if !json_mode {
                crate::log::success(&format!("Config applied: {key}=off"));
            }
            output.removed.push(label.into());
        }
        Ok(()) => {}
        Err(e) => output.errors.push(format!("{key}=off: {e}")),
    }
}

fn remove_file(path: &Path, output: &mut UninstallOutput) {
    if !path.exists() {
        return;
    }
    match fs::remove_file(path) {
        Ok(()) => output.removed.push(path.display().to_string()),
        Err(e) => output.errors.push(format!("{}: {e}", path.display())),
    }
}

fn remove_dir(path: &Path, output: &mut UninstallOutput) {
    if !path.exists() {
        return;
    }
    match fs::remove_dir_all(path) {
        Ok(()) => output.removed.push(path.display().to_string()),
        Err(e) => output.errors.push(format!("{}: {e}", path.display())),
    }
}

fn launch_on_boot_artifact_exists() -> bool {
    let Some(base) = directories::BaseDirs::new() else {
        return false;
    };
    #[cfg(target_os = "macos")]
    {
        let dir = base.home_dir().join("Library/LaunchAgents");
        // Current label + legacy label; apply_launch_on_boot_macos cleans
        // both paths up under the covers, so we should report "removed"
        // when either existed on disk at the start.
        dir.join("com.tofupilot.station.plist").exists()
            || dir.join("com.tofupilot.stream.plist").exists()
    }
    #[cfg(target_os = "linux")]
    {
        let dir = base.home_dir().join(".config/systemd/user");
        dir.join("tofupilot.service").exists() || dir.join("tofupilot-stream.service").exists()
    }
    #[cfg(target_os = "windows")]
    {
        let _ = base;
        // Probe the per-user Run key. `reg query` exits 0 if the value exists.
        std::process::Command::new("reg")
            .args([
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "TofuPilot",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = base;
        false
    }
}

fn desktop_icon_exists() -> bool {
    let Some(base) = directories::BaseDirs::new() else {
        return false;
    };
    #[cfg(target_os = "macos")]
    {
        // Current .app bundle + legacy .command script.
        base.home_dir().join("Desktop/TofuPilot.app").exists()
            || base.home_dir().join("Desktop/TofuPilot.command").exists()
    }
    #[cfg(target_os = "linux")]
    {
        base.home_dir().join("Desktop/tofupilot.desktop").exists()
    }
    #[cfg(target_os = "windows")]
    {
        let _ = base;
        let desktop = std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .map(|p| p.join("Desktop/TofuPilot.lnk").exists())
            .unwrap_or(false);
        let start = std::env::var_os("APPDATA")
            .map(std::path::PathBuf::from)
            .map(|p| {
                p.join("Microsoft/Windows/Start Menu/Programs/TofuPilot.lnk")
                    .exists()
            })
            .unwrap_or(false);
        desktop || start
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = base;
        false
    }
}
