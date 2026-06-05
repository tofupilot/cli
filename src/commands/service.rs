//! `tofupilot service` — supervisor lifecycle helpers.
//!
//! `service start` is invoked by systemd / launchd as the unit's
//! ExecStart and just runs the station daemon foreground.
//!
//! Supervisors are configured `Restart=no` (Linux) / no `KeepAlive`
//! (macOS), so a daemon that dies stays dead until next login.
//! That's the whole lifecycle contract — no in-flight coordination
//! between the CLI and the supervisor at runtime.
//!
//! `service stop` and `service status` shell out to the supervisor
//! for visibility; they're operator commands. The daemon itself
//! never coordinates with the supervisor at runtime — when the
//! operator clicks "Close CLI" in the kiosk, the daemon just exits
//! and stays dead until next login.

use crate::log;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

#[cfg(target_os = "linux")]
const UNIT: &str = "tofupilot.service";

#[cfg(target_os = "macos")]
const LABEL: &str = "com.tofupilot.station";

// Absolute paths for shelled-out tools. Using `Command::new("lsof")`
// would resolve via the inherited PATH; on a CM host where a
// low-privilege user controls early PATH entries (e.g.
// `~/bin:/usr/bin`), they can drop a malicious `lsof` to hijack
// `tofupilot service status`. Same daemon UID, so it's self-pwn not
// privilege escalation, but still ACE via an operator command.
//
// Try the canonical path first, fall back to PATH lookup so we don't
// hard-fail on hardened distros that move binaries (Alpine puts
// `lsof` under `/usr/bin`, NixOS uses store paths). The fallback
// keeps existing behavior; the canonical-first lookup hardens the
// common case.
// Only the macOS bin-resolvers call this; Linux uses its own inline
// candidate lists.
#[cfg(target_os = "macos")]
fn resolved(canonical: &str, name: &str) -> String {
    if std::path::Path::new(canonical).is_file() {
        canonical.to_string()
    } else {
        name.to_string()
    }
}

#[cfg(target_os = "macos")]
fn lsof_bin() -> String {
    resolved("/usr/sbin/lsof", "lsof")
}
#[cfg(target_os = "linux")]
fn lsof_bin() -> String {
    let candidates = ["/usr/bin/lsof", "/bin/lsof", "/usr/sbin/lsof"];
    for c in candidates {
        if std::path::Path::new(c).is_file() {
            return c.to_string();
        }
    }
    "lsof".to_string()
}

#[cfg(target_os = "linux")]
fn systemctl_bin() -> String {
    let candidates = ["/usr/bin/systemctl", "/bin/systemctl"];
    for c in candidates {
        if std::path::Path::new(c).is_file() {
            return c.to_string();
        }
    }
    "systemctl".to_string()
}

#[cfg(target_os = "macos")]
fn launchctl_bin() -> String {
    resolved("/bin/launchctl", "launchctl")
}

#[cfg(target_os = "windows")]
fn powershell_bin() -> String {
    // `%SystemRoot%` is set by the OS; only honor an absolute path
    // under a Windows directory we trust. Fall back to the default
    // install path, then to PATH lookup.
    if let Ok(sysroot) = std::env::var("SystemRoot") {
        let p = format!("{sysroot}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
        if std::path::Path::new(&p).is_file() {
            return p;
        }
    }
    let p = "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe";
    if std::path::Path::new(p).is_file() {
        return p.to_string();
    }
    "powershell".to_string()
}

/// Default loopback port for the operator UI. Single source of truth —
/// `local_ws::Server::start` reads this when `TOFUPILOT_LOCAL_UI_PORT`
/// is unset, and `local_port()` falls back to it for probes.
pub const DEFAULT_LOCAL_PORT: u16 = 7321;

/// Cache: env-var parse + warn happens once per process, not on
/// every caller. Without this, a typo in `TOFUPILOT_LOCAL_UI_PORT`
/// emits the same warn 3-4× per CLI invocation (status, is_running,
/// no-args path, attach_kiosk).
static LOCAL_PORT_CACHE: OnceLock<u16> = OnceLock::new();

/// Resolved loopback port for the operator UI. Honors
/// `TOFUPILOT_LOCAL_UI_PORT` so probes match what the daemon binds in
/// `local_ws::Server::start`. Warns once on unparseable input so a
/// typo (`8O80` with a letter O) is visible at the source rather
/// than silently falling back. Cached via `OnceLock` for the rest
/// of the process lifetime.
pub fn local_port() -> u16 {
    *LOCAL_PORT_CACHE.get_or_init(|| match std::env::var("TOFUPILOT_LOCAL_UI_PORT") {
        Ok(s) => match s.parse::<u16>() {
            Ok(p) => p,
            Err(_) => {
                log::warn(&format!(
                    "TOFUPILOT_LOCAL_UI_PORT={s:?} is not a valid u16; \
                         falling back to default port {DEFAULT_LOCAL_PORT}."
                ));
                DEFAULT_LOCAL_PORT
            }
        },
        Err(_) => DEFAULT_LOCAL_PORT,
    })
}

/// Best-effort check: is the daemon currently running on this host?
/// Probes the loopback bind because that's the daemon's actual
/// single-instance gate. Honors `TOFUPILOT_LOCAL_UI_PORT`.
///
/// 500ms timeout: the no-args path at `main.rs` uses this to decide
/// whether to short-circuit vs spawn a daemon. A 200ms budget was
/// too tight under cold-launch macOS load — the accept loop can be
/// briefly starved while launchd is still wiring stdio, leading to
/// false negatives that then bind-fail with EADDRINUSE.
pub fn is_running() -> bool {
    let port = local_port();
    let addr = format!("127.0.0.1:{port}");
    match addr.parse() {
        Ok(sa) => std::net::TcpStream::connect_timeout(&sa, Duration::from_millis(500)).is_ok(),
        Err(_) => false,
    }
}

/// Outcome of a loopback probe, carried through `status_cmd` so we
/// can present a consistent diagnosis regardless of supervisor state.
enum ProbeResult {
    Listening,
    Refused,
    Timeout,
    Other(String),
}

fn probe_loopback(port: u16) -> ProbeResult {
    let addr = match format!("127.0.0.1:{port}").parse() {
        Ok(sa) => sa,
        Err(e) => return ProbeResult::Other(format!("address parse failed: {e}")),
    };
    match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Ok(_) => ProbeResult::Listening,
        Err(e) => match e.kind() {
            std::io::ErrorKind::ConnectionRefused => ProbeResult::Refused,
            std::io::ErrorKind::TimedOut => ProbeResult::Timeout,
            _ => ProbeResult::Other(e.to_string()),
        },
    }
}

/// Best-effort holder lookup for diagnostic output. Returns a short
/// human string like "PID 1234 (tofupilot)" or `None` if nothing
/// useful came back. Never errors — the helper is for diagnostics
/// only.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn port_holder(port: u16) -> Option<String> {
    let out = Command::new(lsof_bin())
        .args(["-nP", "-sTCP:LISTEN"])
        .arg(format!("-iTCP:{port}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Skip the header, take the first data row, return "PID N (cmd)".
    if let Some(line) = text.lines().nth(1) {
        let mut cols = line.split_whitespace();
        let cmd = cols.next()?;
        let pid = cols.next()?;
        return Some(format!("PID {pid} ({cmd})"));
    }
    None
}

#[cfg(target_os = "windows")]
fn port_holder(port: u16) -> Option<String> {
    // PowerShell `Get-NetTCPConnection` is locale-stable: output is
    // structured, not translated. `netstat` on a German/French Windows
    // emits `ABHÖREN` / `À L'ÉCOUTE` instead of `LISTENING`, so a
    // string filter silently returns `None` on non-English hosts.
    //
    // Resolve the owning process name via `Get-Process` so the hint
    // matches the macOS/Linux output shape: "PID 1234 (cmd)".
    // Force UTF-8 output so CJK process or path names survive the
    // Rust `String::from_utf8_lossy` decode. Default Console encoding
    // on zh-CN / zh-TW / ja-JP Windows is GBK / Big5 / Shift_JIS,
    // which would mangle bytes before we ever see them.
    //
    // Sort by `OwningProcess` so dual-stack listeners (IPv4 + IPv6
    // bound to the same port) yield a deterministic answer rather
    // than whatever order CIM returns first.
    // Always emit the PID even if `Get-Process` fails (process exited
    // between cmdlets). Without this, a transient race returns `None`
    // and the operator sees no holder hint at all instead of "PID N (?)".
    let script = format!(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; \
         $ErrorActionPreference='SilentlyContinue'; \
         $c=Get-NetTCPConnection -LocalPort {port} -State Listen \
            | Sort-Object OwningProcess | Select-Object -First 1; \
         if($c){{$p=Get-Process -Id $c.OwningProcess; \
         $name=if($p){{$p.ProcessName}}else{{'?'}}; \
         Write-Output (\"{{0}} {{1}}\" -f $c.OwningProcess, $name)}}"
    );
    // CREATE_NO_WINDOW (0x0800_0000) suppresses the console flash
    // that `powershell.exe` would otherwise produce when the parent
    // has no attached console (e.g. Studio launching the CLI).
    use std::os::windows::process::CommandExt;
    let out = Command::new(powershell_bin())
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.split_whitespace();
    let pid = parts.next()?;
    let cmd = parts.next().unwrap_or("?");
    Some(format!("PID {pid} ({cmd})"))
}

/// Operator: `tofupilot service stop`. Asks the supervisor to stop
/// the unit. With `Restart=no` / no `KeepAlive`, this is a one-shot:
/// the daemon exits and stays dead until next login.
pub fn stop_cmd(json_mode: bool) -> i32 {
    #[cfg(target_os = "linux")]
    {
        shell_status(
            Command::new(systemctl_bin())
                .args(["--user", "stop", UNIT])
                .status(),
            "stop",
            json_mode,
        )
    }
    #[cfg(target_os = "macos")]
    {
        let target = format!("gui/{}/{LABEL}", unsafe { libc::getuid() });
        shell_status(
            Command::new(launchctl_bin())
                .args(["bootout", &target])
                .status(),
            "stop",
            json_mode,
        )
    }
    #[cfg(target_os = "windows")]
    {
        if !json_mode {
            log::info("On Windows the station runs as a per-user logon process. Close it from Task Manager.");
        }
        let _ = json_mode;
        0
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = json_mode;
        log::error("`tofupilot service stop` is not supported on this platform.");
        1
    }
}

/// Operator: `tofupilot service status`. Probes the loopback bind
/// first (that's the daemon's actual liveness signal), then surfaces
/// the supervisor's view for context. Reports the resolved port + any
/// `TOFUPILOT_LOCAL_UI_PORT` override + log paths so a remote operator
/// can diagnose without a second round-trip.
///
/// Exit-code contract: returns 0 iff the loopback probe is Listening.
/// On a non-Listening probe, returns the supervisor's exit code
/// (Linux/macOS) or 1 (Windows). Scripts can rely on `exit==0` meaning
/// "the operator UI socket is accepting on the resolved port."
pub fn status_cmd(json_mode: bool) -> i32 {
    let _ = json_mode;
    let port = local_port();
    let env_override = std::env::var("TOFUPILOT_LOCAL_UI_PORT").ok();

    if let Some(v) = env_override.as_deref() {
        log::info(&format!(
            "Local UI port: {port} (TOFUPILOT_LOCAL_UI_PORT={v})"
        ));
    } else {
        log::info(&format!("Local UI port: {port} (default)"));
    }

    let probe = probe_loopback(port);
    match &probe {
        ProbeResult::Listening => {
            log::success(&format!("Daemon listening on 127.0.0.1:{port}"));
            log::info(&format!("Kiosk: http://127.0.0.1:{port}/"));
        }
        ProbeResult::Refused => {
            log::error(&format!(
                "Nothing listening on 127.0.0.1:{port} (connection refused). \
                 Daemon is not running."
            ));
        }
        ProbeResult::Timeout => {
            log::error(&format!(
                "127.0.0.1:{port} timed out. Loopback should never time out — \
                 check host firewall (Little Snitch / LuLu / Windows Defender) \
                 for a rule blocking the tofupilot binary, or another process \
                 holding the socket without accepting."
            ));
        }
        ProbeResult::Other(msg) => {
            log::error(&format!("Probe of 127.0.0.1:{port} failed: {msg}"));
        }
    }

    // If the probe says nothing is listening, name the holder (if any)
    // so the operator can spot a zombie / second daemon that's hogging
    // the port. Skip on Listening — there the holder is us.
    if !matches!(probe, ProbeResult::Listening) {
        if let Some(h) = port_holder(port) {
            log::warn(&format!("Port {port} currently held by {h}"));
        }
    }

    #[cfg(target_os = "linux")]
    {
        log::info("Logs: journalctl --user -u tofupilot -f");
        log::info("Supervisor:");
        let code = Command::new(systemctl_bin())
            .args(["--user", "status", UNIT, "--no-pager"])
            .status()
            .map(|s| s.code().unwrap_or(1))
            .unwrap_or(1);
        // Probe is authoritative: if the daemon is accepting on
        // loopback we exit 0 regardless of what `systemctl status`
        // says. Otherwise propagate the supervisor's exit code so
        // callers can distinguish "inactive" (3) from "service not
        // installed" (4) etc.
        if matches!(probe, ProbeResult::Listening) {
            0
        } else {
            code
        }
    }
    #[cfg(target_os = "macos")]
    {
        log::info("Logs: ~/Library/Logs/TofuPilot/stdout.log");
        log::info("Supervisor:");
        let target = format!("gui/{}/{LABEL}", unsafe { libc::getuid() });
        let code = Command::new(launchctl_bin())
            .args(["print", &target])
            .status()
            .map(|s| s.code().unwrap_or(1))
            .unwrap_or(1);
        if matches!(probe, ProbeResult::Listening) {
            0
        } else {
            code
        }
    }
    #[cfg(target_os = "windows")]
    {
        log::info("On Windows the station runs as a per-user logon process. Use Task Manager to inspect it.");
        if matches!(probe, ProbeResult::Listening) {
            0
        } else {
            1
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        log::error("`tofupilot service status` is not supported on this platform.");
        1
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn shell_status(
    status: std::io::Result<std::process::ExitStatus>,
    action: &str,
    json_mode: bool,
) -> i32 {
    match status {
        Ok(s) if s.success() => {
            if !json_mode {
                log::success(&format!("Service {action} requested."));
            }
            0
        }
        Ok(s) => {
            log::error(&format!(
                "Service {action} returned exit code {}.",
                s.code().unwrap_or(-1)
            ));
            s.code().unwrap_or(1)
        }
        Err(e) => {
            log::error(&format!("Service {action} failed: {e}"));
            1
        }
    }
}
