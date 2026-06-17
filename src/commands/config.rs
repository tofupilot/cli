//! The `config` command: read and apply local station configuration stored in
//! the redb `station.config` table.

use crate::commands::auth::credentials::Credentials;
use crate::commands::db;
use station_protocol::StationEvent;

/// True when the CLI runs as root (effective uid 0). On Linux this means
/// launch-on-boot installs a *system* service (`/etc/systemd/system`,
/// `systemctl` with no `--user`) rather than a user service, because root
/// via sudo/su/non-login-SSH has no session bus for `systemctl --user`.
/// It also derives the station's `run_mode` reported to the dashboard.
/// Always false on non-unix.
pub(crate) fn is_root_system() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid() is always safe — no args, no shared state.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// The station `run_mode` string reported on the Hardware event.
/// `"root_system"` when running as root, else `"user"`.
pub(crate) fn run_mode() -> &'static str {
    if is_root_system() {
        "root_system"
    } else {
        "user"
    }
}

/// systemctl scope prefix matching how launch-on-boot installs the unit:
/// system scope (empty) as root, `--user` otherwise. Single source of
/// truth for the scope decision, shared by config.rs and service.rs so the
/// "stop/status/enable" commands always target the installed unit.
#[cfg(target_os = "linux")]
pub(crate) fn systemctl_scope() -> &'static [&'static str] {
    if is_root_system() {
        &[]
    } else {
        &["--user"]
    }
}

/// Print an actionable status block after launch-on-boot is enabled, in
/// the interactive foreground login. Explains what was installed and —
/// for a headless root system service — that the on-screen kiosk is off
/// and the station is driven from the dashboard. Skipped on non-Linux
/// (other platforms have their own supervisors and no root/system split).
#[cfg(target_os = "linux")]
pub(crate) fn print_launch_on_boot_status(creds: &Credentials) {
    if is_root_system() {
        crate::log::success("Launch on boot enabled. System service, starts at every boot.");
        crate::log::info(&format!("Unit: {SYSTEM_UNIT_DIR}/tofupilot.service"));
        crate::log::info("Logs: journalctl -u tofupilot -f");
        crate::log::info(
            "On-screen kiosk is off (a root system service has no display). \
             Control this station from the dashboard:",
        );
        crate::log::info(&format!(
            "  {}/{}/stations",
            creds.base(),
            creds.organization_slug
        ));
        crate::log::info(
            "Tip: link a procedure to this station and deploy it so it appears in the picker.",
        );
    } else {
        crate::log::success("Launch on boot enabled. Starts at next login.");
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn print_launch_on_boot_status(_creds: &Credentials) {
    crate::log::success("Launch on boot enabled.");
}

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
        // unit / shortcut could have been removed by a plain
        // `tofupilot login` (return-to-development), by a manual
        // `systemctl --user disable`, or by uninstall. The DB still says
        // "on" because the dashboard toggle was never flipped, and a
        // strict "skip on no
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

/// Run a systemctl command at the scope implied by the current uid:
/// system scope (no `--user`) when running as root, user scope otherwise.
/// Returns a clear error on failure.
///
/// Root has no session bus, so `systemctl --user` fails with
/// "Failed to connect to bus" — the system scope is mandatory there.
#[cfg(target_os = "linux")]
fn systemctl(args: &[&str]) -> crate::error::CliResult<()> {
    let mut cmd_args: Vec<&str> = systemctl_scope().to_vec();
    cmd_args.extend_from_slice(args);
    let output = std::process::Command::new("systemctl")
        .args(&cmd_args)
        .output()
        .map_err(|e| format!("systemctl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        if !trimmed.is_empty() {
            // Translate the most common confusing failure: a user-scope
            // call with no session bus (e.g. running under sudo/su without
            // a login session). Map it to an actionable message instead of
            // the raw D-Bus error. Root never hits this — it uses system
            // scope — so reaching it means a non-root context lost its bus.
            if trimmed.contains("Failed to connect to bus") {
                return Err(
                    "no user systemd session bus (running without a login session?). \
                     Run as root for a system service (`sudo tofupilot ...`), or enable \
                     lingering for the station user: `sudo loginctl enable-linger <user>`."
                        .into(),
                );
            }
            return Err(format!("systemctl failed: {trimmed}").into());
        }
    }
    Ok(())
}

/// Is the unit already enabled at the current scope? Parses `systemctl
/// is-enabled` STDOUT (`enabled` / `enabled-runtime`) rather than the
/// exit code — the `systemctl()` helper masks nonzero exits when stderr
/// is empty, and `is-enabled` returns nonzero for the common
/// `disabled`/`static` states. Best-effort: any error → `false` so the
/// caller falls through to an `enable`, which is idempotent.
#[cfg(target_os = "linux")]
fn systemctl_is_enabled(unit: &str) -> bool {
    let mut cmd_args: Vec<&str> = systemctl_scope().to_vec();
    cmd_args.push("is-enabled");
    cmd_args.push(unit);
    std::process::Command::new("systemctl")
        .args(&cmd_args)
        .output()
        .ok()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            let first = s.trim();
            first == "enabled" || first == "enabled-runtime"
        })
        .unwrap_or(false)
}

/// Canonical unit name and the legacy name still cleaned up on teardown.
/// Single source of truth, shared by config.rs and uninstall.rs.
#[cfg(target_os = "linux")]
pub(crate) const UNIT: &str = "tofupilot.service";
#[cfg(target_os = "linux")]
pub(crate) const LEGACY_UNIT: &str = "tofupilot-stream.service";

/// Every launch-on-boot unit path that could exist on disk, paired with
/// whether it is a system-scope unit. Teardown iterates this so we clean
/// up whichever scope is actually installed, independent of the current
/// uid (an enable as root then a plain login as the user must still find
/// and remove the right artifact). Both current and legacy names included.
#[cfg(target_os = "linux")]
pub(crate) fn unit_candidates() -> crate::error::CliResult<Vec<(bool, std::path::PathBuf)>> {
    let user_dir = db::home_dir()?.join(".config/systemd/user");
    let sys_dir = std::path::PathBuf::from(SYSTEM_UNIT_DIR);
    Ok(vec![
        (true, sys_dir.join(UNIT)),
        (true, sys_dir.join(LEGACY_UNIT)),
        (false, user_dir.join(UNIT)),
        (false, user_dir.join(LEGACY_UNIT)),
    ])
}

/// Directory for the root system-scope unit. Single source of truth for
/// the unit path, the teardown symlink base, and the status message.
#[cfg(target_os = "linux")]
const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

/// systemd target the unit installs into. Single source of truth shared by
/// the unit's `WantedBy=` line and the `.wants` symlink path that teardown
/// removes — they must match or an autostart symlink is orphaned.
#[cfg(target_os = "linux")]
const SYSTEM_TARGET: &str = "multi-user.target";
#[cfg(target_os = "linux")]
const USER_TARGET: &str = "default.target";

/// Unit file name as a `String` for systemctl args, falling back to the
/// canonical unit name if the path has no file component.
#[cfg(target_os = "linux")]
fn unit_file_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(UNIT)
        .to_string()
}

/// Remove a unit file + its WantedBy symlink directly on disk, scoped by
/// `system`. We do NOT rely on `systemctl --user disable` for the user
/// scope when running as root: root has no user session bus, so that
/// call no-ops and the symlink survives — the orphan unit would then
/// autostart at next boot and race the system daemon for the loopback
/// port. Direct file removal is bus-independent; the systemctl disable
/// stays best-effort for the in-memory state.
#[cfg(target_os = "linux")]
fn remove_unit_on_disk(system: bool, unit_path: &std::path::Path) {
    let Some(unit_name) = unit_path.file_name() else {
        return;
    };
    // WantedBy symlink location for each scope's target. Built from the
    // same TARGET consts as the unit body's WantedBy= line so they stay
    // in sync.
    let wants_link = if system {
        std::path::PathBuf::from(SYSTEM_UNIT_DIR)
            .join(format!("{SYSTEM_TARGET}.wants"))
            .join(unit_name)
    } else {
        match db::home_dir() {
            Ok(h) => h
                .join(".config/systemd/user")
                .join(format!("{USER_TARGET}.wants"))
                .join(unit_name),
            Err(_) => return,
        }
    };
    let _ = std::fs::remove_file(&wants_link);
    let _ = std::fs::remove_file(unit_path);
}

/// Install (or remove) the OS-level station service. Called on a token
/// login (enable) and on a plain browser login (remove), and on every
/// config sync. Writes the systemd unit / launchd plist / Windows Run key
/// with the current binary path, then enables the supervisor so the
/// daemon starts at next login / reboot.
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
        // Plain `tofupilot login` calls this on every dev login, including
        // machines that were never a station. The plist is our marker for
        // "this was a station": if it's absent there's nothing registered,
        // so skip `disable`/`bootout` to avoid spawning launchctl and
        // emitting stderr noise about an unknown label on the hot path.
        if !plist_path.exists() {
            return Ok(());
        }
        // Order matters when called from within the managed process: the
        // bootout SIGTERMs us, so do everything that must complete first.
        //   1. `disable` records the persistent flag -> launchd won't respawn.
        //   2. Remove the plist on disk so it doesn't auto-load at next login.
        //   3. bootout unloads the in-memory definition (and kills us if
        //      self-managed, which is now safe).
        let _ = launchctl::disable(LABEL);
        std::fs::remove_file(&plist_path).map_err(|e| format!("Remove plist: {e}"))?;
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
    let root = is_root_system();

    // --- Disable / uninstall: scope-agnostic, derived from disk. ---
    // Tear down EVERY candidate unit that exists, regardless of the
    // current uid: an enable as root then a plain login as the user
    // must still find and remove the system unit (and vice versa).
    // Removal is on-disk (file + WantedBy symlink) so it works even
    // when the opposite scope's bus is unreachable; systemctl calls are
    // best-effort for in-memory state. System-scope removal is only
    // attempted when running as root — a normal user can't write
    // /etc/systemd/system, and silently skipping avoids EACCES noise on
    // every plain login.
    if !enable {
        for (system, path) in unit_candidates()? {
            if !path.exists() {
                continue;
            }
            if system && !root {
                // Can't manage the system unit without root. Surface it
                // once so an orphan doesn't silently survive.
                crate::log::warn(
                    "A system launch-on-boot service exists but removing it needs root. \
                     Run `sudo tofupilot ...` to remove it.",
                );
                continue;
            }
            let unit_name = unit_file_name(&path);
            // disable (no --now) writes the symlink removal before the
            // stop, so a self-SIGTERM can't abort it; then on-disk
            // removal guarantees the symlink is gone even with no bus.
            let _ = systemctl(&["disable", &unit_name]);
            remove_unit_on_disk(system, &path);
            let _ = systemctl(&["daemon-reload"]);
            let _ = systemctl(&["stop", "--no-block", &unit_name]);
        }
        return Ok(());
    }

    // --- Enable: profile chosen by uid. ---
    // Before writing the chosen unit, tear down the OPPOSITE scope's unit
    // so a mode switch (user<->root) doesn't leave two units enabled and
    // racing for the loopback port. Best-effort: the opposite scope may be
    // unmanageable from here — e.g. a root enable can't reach the seat
    // user's ~/.config (db::home_dir resolves to /root, not the SUDO_USER
    // home), so a pre-existing user unit can survive. The EADDRINUSE
    // single-instance gate means only one daemon wins at boot; the cost is
    // log noise, not corruption. Warn so the leftover is visible.
    let opposite_system = !root;
    for (system, path) in unit_candidates()? {
        if system == opposite_system && path.exists() {
            let unit_name = unit_file_name(&path);
            let _ = systemctl(&["disable", &unit_name]);
            // Can remove on disk unless it's a system unit and we aren't
            // root (no write access to /etc/systemd/system).
            if !system || root {
                remove_unit_on_disk(system, &path);
            } else {
                // Opposite unit is a system unit we lack root to remove.
                crate::log::warn(
                    "A system launch-on-boot service from a previous setup \
                     still exists. Run `sudo tofupilot login` (or uninstall) \
                     to remove it so it doesn't start alongside this one.",
                );
            }
        }
    }

    let (unit_dir, unit_body) = if root {
        // System service: runs as root at boot, no graphical session, so
        // no DISPLAY/XAUTHORITY and no graphical-session.target ordering
        // (meaningless in the system manager). Logs to the journal so
        // exit-75 (revoked creds) is visible via `journalctl -u tofupilot`.
        // WantedBy=multi-user.target fires at boot without any login.
        let body = format!(
            "[Unit]\nDescription=TofuPilot Station\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={exe} service start\nRestart=on-failure\nRestartSec=10\nRestartPreventExitStatus=75 130\nStandardOutput=journal\nStandardError=journal\n\n[Install]\nWantedBy={target}\n",
            exe = exe.display(),
            target = SYSTEM_TARGET,
        );
        (std::path::PathBuf::from(SYSTEM_UNIT_DIR), body)
    } else {
        // User service (Raspberry-Pi-style auto-login kiosk). The user's
        // systemd instance does not inherit GUI env, so DISPLAY/XAUTHORITY
        // are injected for the kiosk browser; graphical-session.target
        // ordering ties the launch to X being up. RestartPreventExitStatus
        // excludes 75 (revoked creds) and 130 (SIGINT) so a deliberate
        // quit / reauth-needed state isn't fought by respawns.
        let display = sanitize_display(&std::env::var("DISPLAY").unwrap_or_default());
        let body = format!(
            "[Unit]\nDescription=TofuPilot Station\nAfter=network-online.target graphical-session.target\nWants=network-online.target\n\n[Service]\nType=simple\nEnvironment=DISPLAY={display}\nEnvironment=XAUTHORITY=%h/.Xauthority\nExecStart={exe} service start\nRestart=on-failure\nRestartSec=10\nRestartPreventExitStatus=75 130\n\n[Install]\nWantedBy={target}\n",
            exe = exe.display(),
            target = USER_TARGET,
        );
        (db::home_dir()?.join(".config/systemd/user"), body)
    };

    // System services must not exec a binary from a user-writable
    // location: a non-root user who can replace the binary gets root
    // code execution at next boot. Refuse with an actionable message
    // rather than installing an escalation vector.
    if root {
        guard_system_exe_location(exe)?;
    }

    let unit_path = unit_dir.join(UNIT);
    std::fs::create_dir_all(&unit_dir).map_err(|e| format!("Create systemd dir: {e}"))?;

    let current = std::fs::read_to_string(&unit_path).ok();
    let unit_changed = current.as_deref() != Some(unit_body.as_str());
    if unit_changed {
        std::fs::write(&unit_path, &unit_body).map_err(|e| format!("Write unit: {e}"))?;
        let _ = systemctl(&["daemon-reload"]);
    }

    // Idempotency: only shell out to `enable` when not already enabled.
    // `apply_launch_on_boot` is reapplied on every config sync to
    // self-heal a unit removed out-of-band; the is-enabled gate keeps
    // the steady state to one cheap probe with no symlink churn.
    if !systemctl_is_enabled(UNIT) {
        systemctl(&["enable", UNIT])?;
    }

    if root {
        // Root system service: authenticated from /root/.tofupilot. If
        // creds aren't there the station boots unauthenticated with no
        // obvious cause — warn so the operator runs `sudo tofupilot login`.
        warn_if_root_creds_missing();
    } else {
        // User instance only runs while the user is logged in. Without
        // lingering the unit never fires on a headless / no-auto-login
        // box. Detect + warn rather than silently failing at next reboot.
        warn_if_linger_disabled();
    }
    Ok(())
}

/// Validate a `DISPLAY` value before interpolating it into a unit file.
/// Accepts `:N`, `:N.M`, or `host:N(.M)`; anything else (incl. a value
/// with a newline that could inject unit directives) falls back to `:0`.
#[cfg(target_os = "linux")]
fn sanitize_display(raw: &str) -> String {
    let candidate = raw.trim();
    let valid = {
        // host part: no control chars, no whitespace, no colon
        let (host, rest) = match candidate.split_once(':') {
            Some((h, r)) => (h, r),
            None => ("", ""),
        };
        let host_ok = host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        // display(.screen): digits, optional `.digits`
        let nums_ok = !rest.is_empty()
            && rest.split_once('.').map_or_else(
                || rest.chars().all(|c| c.is_ascii_digit()),
                |(d, s)| {
                    !d.is_empty()
                        && d.chars().all(|c| c.is_ascii_digit())
                        && !s.is_empty()
                        && s.chars().all(|c| c.is_ascii_digit())
                },
            );
        candidate.contains(':') && host_ok && nums_ok
    };
    if valid {
        candidate.to_string()
    } else {
        ":0".to_string()
    }
}

/// Refuse to install a system unit whose ExecStart points at a binary in
/// a user-writable directory (user home or world/group-writable). A
/// non-root user able to overwrite that binary would gain root at boot.
#[cfg(target_os = "linux")]
fn guard_system_exe_location(exe: &std::path::Path) -> crate::error::CliResult<()> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let canonical = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let lossy = canonical.to_string_lossy();
    if lossy.starts_with("/home/") || lossy.starts_with("/Users/") {
        return Err(format!(
            "Refusing to install a root system service from a user home ({}). \
             Move the binary to /usr/local/bin first, then re-run.",
            canonical.display()
        )
        .into());
    }
    // Reject group/world-writable binary or parent dir (non-root could
    // replace it). Owner must be root for a root service's ExecStart.
    if let Ok(meta) = std::fs::metadata(&canonical) {
        let mode = meta.permissions().mode();
        if mode & 0o022 != 0 || meta.uid() != 0 {
            return Err(format!(
                "Refusing to install a root system service from a non-root-owned or \
                 writable binary ({}). Install it to /usr/local/bin owned by root first.",
                canonical.display()
            )
            .into());
        }
    }
    Ok(())
}

/// Warn when running as a root system service but /root has no
/// credentials — the daemon would boot unauthenticated.
#[cfg(target_os = "linux")]
fn warn_if_root_creds_missing() {
    if crate::commands::auth::credentials::load().is_none() {
        crate::log::warn(
            "No credentials found for the root system service. \
             Run `sudo tofupilot login` so the station authenticates at boot.",
        );
    }
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
        // Plain `tofupilot login` calls this on every dev login. Probe the
        // Run value first; if it's absent (machine was never a station)
        // skip the `reg delete` so we don't shell out and log a failure
        // for a value that was never there.
        let exists = std::process::Command::new("reg")
            .args(["query", KEY, "/v", VALUE])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if exists {
            let _ = std::process::Command::new("reg")
                .args(["delete", KEY, "/v", VALUE, "/f"])
                .output();
        }
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::sanitize_display;

    #[test]
    fn sanitize_display_accepts_valid() {
        assert_eq!(sanitize_display(":0"), ":0");
        assert_eq!(sanitize_display(":1"), ":1");
        assert_eq!(sanitize_display(":0.0"), ":0.0");
        assert_eq!(sanitize_display("localhost:10.0"), "localhost:10.0");
        assert_eq!(sanitize_display(" :0 "), ":0"); // trimmed
    }

    #[test]
    fn sanitize_display_rejects_injection_and_garbage() {
        // Newline injection would add unit directives — must fall back.
        assert_eq!(sanitize_display(":0\nExecStart=/bin/sh"), ":0");
        assert_eq!(sanitize_display(""), ":0");
        assert_eq!(sanitize_display("nonsense"), ":0");
        assert_eq!(sanitize_display(":"), ":0");
        assert_eq!(sanitize_display(":abc"), ":0");
        assert_eq!(sanitize_display(":0."), ":0");
        assert_eq!(sanitize_display("ho st:0"), ":0"); // space in host
    }
}
