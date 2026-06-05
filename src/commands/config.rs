//! The `config` command: read and apply local station configuration stored in
//! the redb `station.config` table.

use crate::commands::auth::credentials::Credentials;
use crate::commands::db;
use station_protocol::StationEvent;

// Shared brand assets, reused from the Studio Tauri bundle. Embedded so the
// single-file CLI can place them on disk as needed without an installer step.
#[cfg(target_os = "linux")]
const ICON_PNG: &[u8] = include_bytes!("../../assets/icons/icon.png");
#[cfg(target_os = "macos")]
const ICON_ICNS: &[u8] = include_bytes!("../../assets/icons/icon.icns");
#[cfg(target_os = "windows")]
const ICON_ICO: &[u8] = include_bytes!("../../assets/icons/icon.ico");

/// Fetch desired config from server and apply any differences.
pub async fn sync_config(creds: &Credentials, installation_id: &str) -> Vec<StationEvent> {
    let mut events = Vec::new();

    let server_config = match fetch_server_config(creds).await {
        Ok(c) => c,
        Err(e) => {
            crate::log::warn(&format!("Failed to sync config: {e}"));
            return events;
        }
    };

    let db = match db::open() {
        Ok(d) => d,
        Err(e) => {
            crate::log::warn(&format!("Failed to open config db: {e}"));
            return events;
        }
    };

    for (key, value) in &server_config {
        let current = db.get_config(key).ok().flatten();
        let unchanged = current.as_deref() == Some(value.as_str());
        // `launch_on_boot` and `desktop_icon` are reapplied every sync
        // even when the DB value hasn't changed: the on-disk supervisor
        // unit / shortcut could have been removed by a prior
        // `tofupilot install --disable`, by a manual `systemctl --user
        // disable`, or by uninstall. The DB still says "on" because the
        // dashboard toggle was never flipped, and a strict "skip on no
        // change" path would silently leave the station unable to
        // auto-restart at next reboot. Reapplying also rewrites
        // shortcut Arguments / unit ExecStart whenever the CLI upgrades
        // and changes them. `apply_launch_on_boot` and
        // `apply_desktop_icon` are idempotent.
        let always_reapply = matches!(key.as_str(), "launch_on_boot" | "desktop_icon");
        if unchanged && !always_reapply {
            continue;
        }
        // `apply_and_event` shells out to launchctl / systemctl /
        // desktop tools synchronously; off-load to `spawn_blocking` so
        // the boot/login flow doesn't stall on a wedged supervisor.
        let key_owned = key.clone();
        let value_owned = value.clone();
        let installation_id_owned = installation_id.to_string();
        let event = tokio::task::spawn_blocking(move || {
            apply_and_event(&key_owned, &value_owned, &installation_id_owned, false)
        })
        .await
        .unwrap_or_else(|_| StationEvent::ConfigApplied {
            installation_id: installation_id.to_string(),
            key: key.clone(),
            value: value.clone(),
            success: false,
            error: Some("apply task panicked".to_string()),
        });
        let _ = db.set_config(key, value);
        events.push(event);
    }

    events
}

/// Apply a config key=value to the local system. Side effects only:
/// writes/removes the supervisor unit, the desktop shortcut, etc.
/// Keys without system-level side effects are no-ops.
pub fn apply(key: &str, value: &str) -> crate::error::CliResult<()> {
    match key {
        "launch_on_boot" => apply_launch_on_boot(value == "on"),
        "desktop_icon" => apply_desktop_icon(value == "on"),
        // terminal_ui, auto_update, kiosk_ui, and any unknown key are
        // stored locally with no OS-level side effect.
        _ => Ok(()),
    }
}

/// Apply + log + build a ConfigApplied StationEvent for the stream.
/// Used by the station event loop and startup sync.
pub fn apply_and_event(key: &str, value: &str, installation_id: &str, quiet: bool) -> StationEvent {
    let result = apply(key, value);
    log_result(key, value, &result, quiet);
    match result {
        Ok(()) => StationEvent::ConfigApplied {
            installation_id: installation_id.to_string(),
            key: key.to_string(),
            value: value.to_string(),
            success: true,
            error: None,
        },
        Err(e) => StationEvent::ConfigApplied {
            installation_id: installation_id.to_string(),
            key: key.to_string(),
            value: value.to_string(),
            success: false,
            error: Some(e.to_string()),
        },
    }
}

fn log_result(key: &str, value: &str, result: &crate::error::CliResult<()>, quiet: bool) {
    match result {
        Ok(()) => {
            if !quiet {
                crate::log::success(&format!("Config applied: {key}={value}"));
            }
        }
        Err(e) => crate::log::error(&format!("Config failed: {key}={value} ({e})")),
    }
}

async fn fetch_server_config(
    creds: &Credentials,
) -> crate::error::CliResult<Vec<(String, String)>> {
    let base = creds.base();
    use crate::http::RequestBuilderExt;
    let res = crate::http::client()
        .get(format!("{base}/api/cli/config"))
        .bearer(&creds.api_key)
        .send()
        .await
        .map_err(|e| format!("Fetch config: {e}"))?;

    let res = crate::commands::http::ok_or_describe(res)
        .await
        .map_err(|e| format!("Fetch config: {}", e.body()))?;

    let map: std::collections::HashMap<String, String> =
        res.json().await.map_err(|e| format!("Parse config: {e}"))?;

    Ok(map.into_iter().collect())
}

#[cfg(target_os = "macos")]
mod launchctl {
    use std::process::{Command, Stdio};

    fn domain() -> String {
        format!("gui/{}", unsafe { libc::getuid() })
    }

    pub fn target(label: &str) -> String {
        format!("{}/{label}", domain())
    }

    fn run(args: &[&str], action: &str) -> crate::error::CliResult<()> {
        let output = Command::new("launchctl")
            .args(args)
            .output()
            .map_err(|e| format!("launchctl {action}: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        Err(format!(
            "launchctl {action} failed: {}",
            if msg.is_empty() { "non-zero exit" } else { msg }
        )
        .into())
    }

    /// Clear the persistent disabled flag so launchd will load the service.
    pub fn enable(label: &str) -> crate::error::CliResult<()> {
        run(&["enable", &target(label)], "enable")
    }

    /// Set the persistent disabled flag. Safe from within the managed process:
    /// launchd records the flag before any SIGTERM, so the service won't
    /// respawn after the subsequent bootout.
    pub fn disable(label: &str) -> crate::error::CliResult<()> {
        run(&["disable", &target(label)], "disable")
    }

    /// Unload the service. Best-effort; failures are expected when the service
    /// was already unloaded or when this process is the service target.
    pub fn bootout(label: &str) {
        let _ = Command::new("launchctl")
            .args(["bootout", &target(label)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Run a user-level systemctl command, returning a clear error on failure.
#[cfg(target_os = "linux")]
fn systemctl(args: &[&str]) -> crate::error::CliResult<()> {
    let mut cmd_args = vec!["--user"];
    cmd_args.extend_from_slice(args);
    let output = std::process::Command::new("systemctl")
        .args(&cmd_args)
        .output()
        .map_err(|e| format!("systemctl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        if !trimmed.is_empty() {
            return Err(format!("systemctl failed: {trimmed}").into());
        }
    }
    Ok(())
}

/// Install (or remove) the OS-level station service. Called by the
/// `tofupilot install` subcommand. Writes the systemd unit / launchd
/// plist / Windows Run key with the current binary path, then enables
/// the supervisor so the daemon starts at next login / reboot.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub(crate) fn apply_launch_on_boot(enable: bool) -> crate::error::CliResult<()> {
    let exe = std::env::current_exe().map_err(|e| format!("Current exe: {e}"))?;
    #[cfg(target_os = "macos")]
    {
        apply_launch_on_boot_macos(enable, &exe)
    }
    #[cfg(target_os = "linux")]
    {
        apply_launch_on_boot_linux(enable, &exe)
    }
    #[cfg(target_os = "windows")]
    {
        apply_launch_on_boot_windows(enable, &exe)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub(crate) fn apply_launch_on_boot(_enable: bool) -> crate::error::CliResult<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn apply_launch_on_boot_macos(enable: bool, exe: &std::path::Path) -> crate::error::CliResult<()> {
    const LABEL: &str = "com.tofupilot.station";
    const LEGACY_LABEL: &str = "com.tofupilot.stream";

    let plist_dir = db::home_dir()?.join("Library/LaunchAgents");
    let plist_path = plist_dir.join(format!("{LABEL}.plist"));
    let legacy_plist = plist_dir.join(format!("{LEGACY_LABEL}.plist"));

    // Clean up any legacy service plist regardless of direction.
    if legacy_plist.exists() {
        launchctl::bootout(LEGACY_LABEL);
        let _ = std::fs::remove_file(&legacy_plist);
    }

    if !enable {
        // Order matters when called from within the managed process: the
        // bootout SIGTERMs us, so do everything that must complete first.
        //   1. `disable` records the persistent flag -> launchd won't respawn.
        //   2. Remove the plist on disk so it doesn't auto-load at next login.
        //   3. bootout unloads the in-memory definition (and kills us if
        //      self-managed, which is now safe).
        let _ = launchctl::disable(LABEL);
        if plist_path.exists() {
            std::fs::remove_file(&plist_path).map_err(|e| format!("Remove plist: {e}"))?;
        }
        launchctl::bootout(LABEL);
        return Ok(());
    }

    let log_dir = plist_dir
        .parent()
        .unwrap_or(std::path::Path::new("/tmp"))
        .join("Logs/TofuPilot");
    let _ = std::fs::create_dir_all(&log_dir);

    // `RunAtLoad=true` only — no `KeepAlive`. launchd starts the
    // daemon once at login; if it dies (clean, crash, kill) it stays
    // dead until next login. Matches Linux's `Restart=no`.
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key><array><string>{exe}</string><string>service</string><string>start</string></array>
    <key>RunAtLoad</key><true/>
    <key>ProcessType</key><string>Background</string>
    <key>StandardOutPath</key><string>{log_dir}/stdout.log</string>
    <key>StandardErrorPath</key><string>{log_dir}/stderr.log</string>
</dict>
</plist>"#,
        exe = exe.display(),
        log_dir = log_dir.display(),
    );

    std::fs::create_dir_all(&plist_dir).map_err(|e| format!("Create LaunchAgents: {e}"))?;

    let current = std::fs::read_to_string(&plist_path).ok();
    let plist_changed = current.as_deref() != Some(plist.as_str());
    if plist_changed {
        std::fs::write(&plist_path, &plist).map_err(|e| format!("Write plist: {e}"))?;
    }

    // `enable` clears any stale persistent-disabled flag. Just a flag
    // write — no `bootstrap`, no `bootout`. Plist on disk + enable is
    // enough; launchd loads it at next login. Skipping the bootstrap
    // means we never spawn a second daemon while this one is running,
    // matching the Linux path.
    let _ = launchctl::enable(LABEL);
    Ok(())
}

#[cfg(target_os = "linux")]
fn apply_launch_on_boot_linux(enable: bool, exe: &std::path::Path) -> crate::error::CliResult<()> {
    const UNIT: &str = "tofupilot.service";
    const LEGACY_UNIT: &str = "tofupilot-stream.service";

    let unit_dir = db::home_dir()?.join(".config/systemd/user");
    let unit_path = unit_dir.join(UNIT);
    let legacy_path = unit_dir.join(LEGACY_UNIT);

    if legacy_path.exists() {
        let _ = systemctl(&["disable", "--now", LEGACY_UNIT]);
        let _ = std::fs::remove_file(&legacy_path);
    }

    if !enable {
        if unit_path.exists() {
            // Do work that must survive a self-SIGTERM before the stop command:
            //   1. `disable` (no --now) writes the symlink removal, preventing
            //      launch-on-boot at next login; doesn't touch the running unit.
            //   2. Remove the unit file from disk.
            //   3. `stop --no-block` asks systemd to stop us; the call returns
            //      before SIGTERM fires, letting daemon-reload complete.
            let _ = systemctl(&["disable", UNIT]);
            let _ = std::fs::remove_file(&unit_path);
            let _ = systemctl(&["daemon-reload"]);
            let _ = systemctl(&["stop", "--no-block", UNIT]);
        }
        return Ok(());
    }

    // `Restart=on-failure`: a clean exit (operator hit Exit, exit 0)
    // stays dead — no surprise respawn, no fight against the operator.
    // A failure exit (network not yet reachable at boot, transient
    // crash) gets retried every 10s. `network-online.target` is a
    // soft target on a Pi (often satisfied before DHCP completes), so
    // without retry the unit dies on first boot when the connection
    // hasn't stabilized yet.
    //
    // `Environment=DISPLAY/XAUTHORITY`: the user's systemd instance
    // does NOT inherit GUI env from the X session, so without these
    // the kiosk launcher can't open a browser window. `:0` is the
    // standard single-monitor display on a Pi with auto-login.
    // `XAUTHORITY=%h/.Xauthority` is the default location lightdm /
    // raspi-config writes to.
    //
    // `After=graphical-session.target` ties unit ordering to the GUI
    // session so the kiosk launches with X already up.
    // RestartPreventExitStatus excludes:
    //  - 75 (EX_TEMPFAIL): credentials revoked. Restarting just spins
    //    against the same revoked key; operator must reauth.
    //  - 130: SIGINT (Ctrl+C). Operator deliberately quit.
    // Honor the operator's current `$DISPLAY` if set so multi-monitor
    // / non-standard setups install with the right value. Fall back to
    // `:0` which matches the standard Pi single-monitor auto-login.
    // Wayland-only hosts will still get `:0` here; the kiosk launcher
    // detects WAYLAND_DISPLAY at runtime and adapts.
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    let unit = format!(
        "[Unit]\nDescription=TofuPilot Station\nAfter=network-online.target graphical-session.target\nWants=network-online.target\n\n[Service]\nType=simple\nEnvironment=DISPLAY={display}\nEnvironment=XAUTHORITY=%h/.Xauthority\nExecStart={exe} service start\nRestart=on-failure\nRestartSec=10\nRestartPreventExitStatus=75 130\n\n[Install]\nWantedBy=default.target\n",
        exe = exe.display()
    );
    std::fs::create_dir_all(&unit_dir).map_err(|e| format!("Create systemd dir: {e}"))?;

    let current = std::fs::read_to_string(&unit_path).ok();
    let unit_changed = current.as_deref() != Some(unit.as_str());
    if unit_changed {
        std::fs::write(&unit_path, &unit).map_err(|e| format!("Write unit: {e}"))?;
        let _ = systemctl(&["daemon-reload"]);
    }

    // `enable` (no `--now`) just writes the WantedBy symlink. It does
    // NOT spawn a second daemon, so it's safe to call from inside the
    // already-running CLI. The change takes effect at next reboot;
    // the current foreground session keeps running unchanged.
    systemctl(&["enable", UNIT])?;

    // The user's systemd instance only runs while the user is logged
    // in (no SSH session, no auto-login). Without lingering, the unit
    // never fires at boot on a fresh-boot-then-no-login machine. On a
    // typical Pi with auto-login this isn't a problem — the user logs
    // in automatically — but for kiosks running headless or via SSH
    // we need linger. Detect + warn rather than silently fail at next
    // reboot.
    warn_if_linger_disabled();
    Ok(())
}

#[cfg(target_os = "linux")]
fn warn_if_linger_disabled() {
    let user = std::env::var("USER").unwrap_or_default();
    if user.is_empty() {
        return;
    }
    let output = std::process::Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .output();
    let lingering = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .eq_ignore_ascii_case("yes"),
        // `loginctl` missing or user not known to logind — can't tell, stay quiet.
        _ => return,
    };
    if !lingering {
        crate::log::warn(&format!(
            "User systemd lingering is disabled. Without it, the station won't \
             start on boot unless someone logs in (SSH or auto-login). Enable with: \
             sudo loginctl enable-linger {user}"
        ));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn apply_desktop_icon(enable: bool) -> crate::error::CliResult<()> {
    #[cfg(target_os = "macos")]
    {
        apply_desktop_icon_macos(enable)
    }
    #[cfg(target_os = "linux")]
    {
        apply_desktop_icon_linux(enable)
    }
    #[cfg(target_os = "windows")]
    {
        apply_desktop_icon_windows(enable)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn apply_desktop_icon(_enable: bool) -> crate::error::CliResult<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_app_bundle() -> crate::error::CliResult<std::path::PathBuf> {
    Ok(db::home_dir()?.join("Desktop/TofuPilot.app"))
}

#[cfg(target_os = "macos")]
fn apply_desktop_icon_macos(enable: bool) -> crate::error::CliResult<()> {
    use std::os::unix::fs::PermissionsExt;

    let bundle = macos_app_bundle()?;
    // Legacy .command script artifact; remove unconditionally so upgrades
    // don't leave two icons on the user's desktop.
    let legacy = db::home_dir()?.join("Desktop/TofuPilot.command");
    if legacy.exists() {
        let _ = std::fs::remove_file(&legacy);
    }

    if !enable {
        if bundle.exists() {
            std::fs::remove_dir_all(&bundle).map_err(|e| format!("Remove .app: {e}"))?;
        }
        return Ok(());
    }

    let exe = std::env::current_exe().map_err(|e| format!("Current exe: {e}"))?;
    let macos_dir = bundle.join("Contents/MacOS");
    let resources_dir = bundle.join("Contents/Resources");
    std::fs::create_dir_all(&macos_dir).map_err(|e| format!("Create .app/MacOS: {e}"))?;
    std::fs::create_dir_all(&resources_dir).map_err(|e| format!("Create .app/Resources: {e}"))?;

    // Launcher script. We need a Terminal window for the run UI, so the
    // bundle's main exec opens Terminal pointed at the real binary instead of
    // attaching the binary directly (which would give Finder a hidden GUI app
    // with no stdout). osascript keeps activation focus on Terminal so the
    // user lands in the new window.
    //
    // Invoke bare `tofupilot` (no subcommand), not `tofupilot run`. The
    // no-args path runs the station daemon, which subscribes to the
    // dashboard broker and reacts to procedure pulls and station-config
    // changes (kiosk_ui, terminal_ui). `tofupilot run` is the
    // single-procedure runner and reads only the local deployment cache,
    // so launches from this bundle would never pick up dashboard changes.
    let launcher = macos_dir.join("tofupilot");
    let launcher_script = format!(
        "#!/bin/bash\nexec /usr/bin/osascript -e 'tell application \"Terminal\" to do script \"{exe}\"' -e 'tell application \"Terminal\" to activate'\n",
        exe = exe.display(),
    );
    std::fs::write(&launcher, &launcher_script).map_err(|e| format!("Write launcher: {e}"))?;
    let _ = std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755));

    let icon_path = resources_dir.join("icon.icns");
    if std::fs::read(&icon_path).ok().as_deref() != Some(ICON_ICNS) {
        std::fs::write(&icon_path, ICON_ICNS).map_err(|e| format!("Write icns: {e}"))?;
    }

    let info_plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>TofuPilot</string>
    <key>CFBundleDisplayName</key><string>TofuPilot</string>
    <key>CFBundleIdentifier</key><string>com.tofupilot.station</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleExecutable</key><string>tofupilot</string>
    <key>CFBundleIconFile</key><string>icon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSUIElement</key><false/>
</dict>
</plist>"#,
        version = env!("CARGO_PKG_VERSION"),
    );
    let info_path = bundle.join("Contents/Info.plist");
    if std::fs::read_to_string(&info_path).ok().as_deref() != Some(info_plist.as_str()) {
        std::fs::write(&info_path, &info_plist).map_err(|e| format!("Write Info.plist: {e}"))?;
    }

    // Bump the bundle's mtime so Finder/LaunchServices invalidates its icon
    // cache and picks up icon changes on upgrade. `touch` is the cheapest
    // portable way; a failure here is purely cosmetic.
    let _ = std::process::Command::new("touch").arg(&bundle).status();
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_icon_path() -> crate::error::CliResult<std::path::PathBuf> {
    Ok(db::home_dir()?.join(".local/share/icons/tofupilot.png"))
}

#[cfg(target_os = "linux")]
fn apply_desktop_icon_linux(enable: bool) -> crate::error::CliResult<()> {
    use std::os::unix::fs::PermissionsExt;

    let desktop_dir = db::home_dir()?.join("Desktop");
    let desktop_file = desktop_dir.join("tofupilot.desktop");
    let icon_path = linux_icon_path()?;

    if !enable {
        if desktop_file.exists() {
            std::fs::remove_file(&desktop_file).map_err(|e| format!("Remove .desktop: {e}"))?;
        }
        if icon_path.exists() {
            let _ = std::fs::remove_file(&icon_path);
        }
        return Ok(());
    }

    // Place the icon under XDG_DATA_HOME/icons so the desktop file's absolute
    // Icon= path resolves on every freedesktop-compliant DE without touching
    // the hicolor theme cache.
    if let Some(parent) = icon_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Create icon dir: {e}"))?;
    }
    if std::fs::read(&icon_path).ok().as_deref() != Some(ICON_PNG) {
        std::fs::write(&icon_path, ICON_PNG).map_err(|e| format!("Write icon: {e}"))?;
    }

    // Exec invokes bare `tofupilot` (no subcommand), not `tofupilot run`.
    // Bare = station daemon path, which subscribes to the broker and
    // applies dashboard-pushed pulls and config changes (kiosk_ui,
    // terminal_ui). `tofupilot run` is the single-procedure runner and
    // reads only the local deployment cache, so .desktop launches would
    // never pick up dashboard changes.
    let exe = std::env::current_exe().map_err(|e| format!("Current exe: {e}"))?;
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=TofuPilot\nComment=TofuPilot Station\nExec={exe}\nIcon={icon}\nTerminal=true\nCategories=Development;\nStartupWMClass=tofupilot\n",
        exe = exe.display(),
        icon = icon_path.display(),
    );
    std::fs::create_dir_all(&desktop_dir).map_err(|e| format!("Create Desktop: {e}"))?;
    std::fs::write(&desktop_file, entry).map_err(|e| format!("Write .desktop: {e}"))?;
    // Mark the launcher executable; required by Nautilus/GNOME to even
    // consider trusting it.
    let _ = std::fs::set_permissions(&desktop_file, std::fs::Permissions::from_mode(0o755));
    // GNOME 42+ also requires a per-file `metadata::trusted` attribute before
    // double-click works without a right-click "Allow Launching" prompt.
    // gio is available on every modern desktop install; failure is non-fatal
    // since the launcher still works from menus / file manager double-click
    // after the prompt.
    let _ = std::process::Command::new("gio")
        .args([
            "set",
            desktop_file.to_str().unwrap_or(""),
            "metadata::trusted",
            "true",
        ])
        .status();
    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_local_appdata() -> crate::error::CliResult<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA not set".into())
}

#[cfg(target_os = "windows")]
fn windows_icon_path() -> crate::error::CliResult<std::path::PathBuf> {
    Ok(windows_local_appdata()?.join("TofuPilot").join("icon.ico"))
}

#[cfg(target_os = "windows")]
fn windows_desktop_dir() -> crate::error::CliResult<std::path::PathBuf> {
    // Resolve via SHGetKnownFolderPath(FOLDERID_Desktop) so we follow
    // OneDrive Known Folder Move redirects. The naive `USERPROFILE\Desktop`
    // path is stale on any machine where the user moved Desktop into
    // OneDrive, and `WshShell.SaveAs` then throws DirectoryNotFoundException.
    directories::UserDirs::new()
        .and_then(|d| d.desktop_dir().map(std::path::Path::to_path_buf))
        .ok_or_else(|| "Could not resolve Desktop folder".into())
}

#[cfg(target_os = "windows")]
fn windows_start_menu_dir() -> crate::error::CliResult<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "APPDATA not set".to_string())?;
    Ok(appdata.join("Microsoft/Windows/Start Menu/Programs"))
}

#[cfg(target_os = "windows")]
fn apply_launch_on_boot_windows(
    enable: bool,
    exe: &std::path::Path,
) -> crate::error::CliResult<()> {
    // Per-user Run key: simplest reliable autostart, no admin rights, no
    // Task Scheduler XML wrangling. Trade-off: only fires at interactive
    // logon, not at boot before login. That matches the "station runs while
    // operator is logged in" model the macOS LaunchAgent / Linux user-systemd
    // setups also use, so behavior stays consistent across OSes.
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "TofuPilot";

    if !enable {
        let _ = std::process::Command::new("reg")
            .args(["delete", KEY, "/v", VALUE, "/f"])
            .output();
        return Ok(());
    }

    // Quote the exe path so spaces survive the registry round-trip.
    let data = format!("\"{}\" service start", exe.display());
    let output = std::process::Command::new("reg")
        .args(["add", KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &data, "/f"])
        .output()
        .map_err(|e| format!("reg add: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "reg add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn apply_desktop_icon_windows(enable: bool) -> crate::error::CliResult<()> {
    let icon_path = windows_icon_path()?;
    let desktop_lnk = windows_desktop_dir()?.join("TofuPilot.lnk");
    let start_lnk = windows_start_menu_dir()?.join("TofuPilot.lnk");

    if !enable {
        for p in [&desktop_lnk, &start_lnk] {
            if p.exists() {
                let _ = std::fs::remove_file(p);
            }
        }
        if icon_path.exists() {
            let _ = std::fs::remove_file(&icon_path);
        }
        return Ok(());
    }

    if let Some(parent) = icon_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Create icon dir: {e}"))?;
    }
    if std::fs::read(&icon_path).ok().as_deref() != Some(ICON_ICO) {
        std::fs::write(&icon_path, ICON_ICO).map_err(|e| format!("Write ico: {e}"))?;
    }

    let exe = std::env::current_exe().map_err(|e| format!("Current exe: {e}"))?;
    for parent in [desktop_lnk.parent(), start_lnk.parent()]
        .into_iter()
        .flatten()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    crate::log::info(&format!("Desktop shortcut: {}", desktop_lnk.display()));
    crate::log::info(&format!("Start menu shortcut: {}", start_lnk.display()));
    create_windows_shortcut(&desktop_lnk, &exe, &icon_path)?;
    create_windows_shortcut(&start_lnk, &exe, &icon_path)?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn create_windows_shortcut(
    link: &std::path::Path,
    target: &std::path::Path,
    icon: &std::path::Path,
) -> crate::error::CliResult<()> {
    // Build via WScript.Shell COM object exposed by PowerShell. No external
    // dependency, ships with every supported Windows. Single-quoted PS strings
    // need '' to escape an embedded apostrophe; paths from std should not
    // contain one in practice, but escape defensively.
    fn escape(p: &std::path::Path) -> String {
        p.display().to_string().replace('\'', "''")
    }
    // Arguments empty so the shortcut invokes the same code path as
    // bare `tofupilot.exe` from a shell: the station daemon. Previously
    // `'run'` was passed, which routes to the single-procedure runner
    // and reads only the local deployment cache, so operators
    // double-clicking the shortcut never picked up new procedures or
    // station-config changes (kiosk_ui, terminal_ui, ...) pushed from
    // the dashboard. With no Arguments, the no-args main handler
    // either attaches to the already-running daemon (logon Run key) or
    // starts one in the foreground.
    let script = format!(
        "$ws = New-Object -ComObject WScript.Shell; \
         $s = $ws.CreateShortcut('{link}'); \
         $s.TargetPath = '{target}'; \
         $s.Arguments = ''; \
         $s.IconLocation = '{icon}'; \
         $s.Description = 'TofuPilot Station'; \
         $s.Save()",
        link = escape(link),
        target = escape(target),
        icon = escape(icon),
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|e| format!("powershell: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "create .lnk failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}
