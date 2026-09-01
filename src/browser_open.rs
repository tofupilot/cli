//! Browser opener with macOS Chromium-family tab dedup. `/usr/bin/open
//! <url>` doesn't dedup tabs on Chrome / Brave / Arc / Edge / Firefox —
//! only Safari does. To avoid spawning a duplicate tab on every CLI
//! invocation we run an AppleScript that walks the browser's windows,
//! focuses an existing tab whose URL matches, and falls back to
//! opening a fresh one. Each Chromium-family browser is probed in
//! turn; the first one that's running answers the script. If none
//! are running we fall through to the cross-platform launcher.
//!
//! Linux / Windows / Safari / Firefox have no reliable tab-dedup
//! primitive; the browser's own history / pinned-tab dedup handles
//! repeated runs there.

// `Write` is only used by the macOS AppleScript path; gate the import so
// other platforms don't see it as unused.
#[cfg(target_os = "macos")]
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
const OPEN_CHROME_APPLESCRIPT: &str = include_str!("../assets/open-chrome.applescript");

#[cfg(target_os = "macos")]
const CHROMIUM_BROWSERS: &[&str] = &[
    "Google Chrome",
    "Google Chrome Canary",
    "Brave Browser",
    "Microsoft Edge",
    "Vivaldi",
    "Arc",
];

/// Open the URL in the user's default browser. On macOS, probe each
/// known Chromium-family browser in turn — the first one that's
/// already running gets the URL handled by our dedup AppleScript.
/// If none are running, fall through to plain `open`.
pub fn open_or_focus(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        for browser in CHROMIUM_BROWSERS {
            match macos::run_applescript(url, browser) {
                Ok(macos::ScriptOutcome::Focused | macos::ScriptOutcome::Opened) => return Ok(()),
                // Browser isn't running, or scripting permission was
                // denied — try the next one.
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
    }
    open::that(url).map(|_| ())
}

/// Open the URL in a Chromium / Firefox **kiosk-mode** window — full
/// screen, no chrome, no tab strip. It is a presentation mode, not a
/// lockdown: see `KioskCandidate::kiosk_args` for what is and is not
/// actually blocked. Chromium browsers get `--kiosk URL` plus
/// a per-call temp `--user-data-dir` so the flag isn't ignored when a
/// regular browser session is already running. On Windows, Edge
/// additionally gets `--edge-kiosk-type=fullscreen`, without which it
/// stays in a normal profile session that an operator can tab out of.
/// Firefox uses `-kiosk`.
///
/// Detection chain (first hit wins):
///   * Linux: chromium-browser, chromium, google-chrome,
///     google-chrome-stable, microsoft-edge, brave-browser, firefox.
///   * macOS: Google Chrome, Microsoft Edge, Brave, Firefox (resolved
///     via `/Applications` paths). Safari has no CLI kiosk flag and is
///     skipped — operators on Safari rely on the in-page Maximize
///     button (Fullscreen API) instead.
///   * Windows: msedge.exe, chrome.exe, firefox.exe (Program Files).
///
/// Returns the spawned kiosk window as a `KioskHandle` whose Drop
/// kills the browser process. Caller must keep it alive for the
/// lifetime of the kiosk session — drop = window closes. Pass to
/// `forget()` if you want a leaking-but-permanent window.
///
/// Returns `Ok(handle)` even when no kiosk-capable browser was found
/// (`brand == Fallback`); in that case the default browser was
/// launched via `open::that` and we don't have a child handle to
/// kill, so the window stays after Drop. The caller logs the brand
/// so the operator knows whether they got a true kiosk window.
pub fn open_kiosk(url: &str) -> std::io::Result<KioskHandle> {
    let temp_profile = temp_profile_dir();

    for cand in kiosk_candidates() {
        if let Some(args) = cand.kiosk_args(url, temp_profile.as_deref()) {
            let mut cmd = Command::new(&cand.bin);
            cmd.args(args).stdout(Stdio::null()).stderr(Stdio::null());
            // Unix: put the kiosk in its own process group so we can
            // killpg() the entire tree (Chromium spawns renderer / GPU
            // / zygote subprocesses) on Drop. Without the group, our
            // kill() only targets the launcher PID and the renderers
            // survive as orphans.
            #[cfg(unix)]
            unsafe {
                use std::os::unix::process::CommandExt;
                cmd.pre_exec(|| {
                    if libc::setpgid(0, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            match cmd.spawn() {
                Ok(child) => {
                    return Ok(KioskHandle {
                        brand: cand.brand,
                        child: Some(child),
                    });
                }
                // Binary present but exec failed (permissions,
                // sandbox, etc.). Try the next candidate.
                Err(_) => continue,
            }
        }
    }

    open::that(url).map(|_| KioskHandle {
        brand: KioskBrowser::Fallback,
        child: None,
    })
}

#[derive(Debug, Clone, Copy)]
pub enum KioskBrowser {
    Chromium,
    /// Chromium-family too, but Edge needs two switches of its own on top
    /// of the Chromium ones to reach a real kiosk session. See
    /// `KioskCandidate::kiosk_args`.
    ///
    /// Only constructed on Windows, the one platform where Microsoft
    /// implements Edge kiosk mode: `kiosk_candidates` keeps the Linux and
    /// macOS `microsoft-edge` entries on `Chromium` on purpose (the reasons,
    /// one documented and one measured, are at those entries), so elsewhere
    /// this variant is dead by design and `-D warnings` would reject the
    /// build. The exception is scoped rather than a blanket
    /// `allow(dead_code)`, so dropping the Edge routing on Windows still
    /// fails the build the way it should.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Edge,
    Firefox,
    /// No kiosk-capable browser found — default browser was opened
    /// instead. Operator should expect a normal window with chrome.
    Fallback,
}

/// RAII guard around the spawned browser process. Dropping kills
/// the kiosk window; the caller must hold this for the lifetime of
/// the session. `child` is None on the `Fallback` path because
/// `open::that` doesn't expose a handle.
pub struct KioskHandle {
    pub brand: KioskBrowser,
    child: Option<std::process::Child>,
}

impl KioskHandle {
    /// PID of the launcher process, if we have one. `Fallback` (no
    /// kiosk-capable browser) returns `None`. Used by callers to
    /// spawn a watcher that detects the kiosk window exiting
    /// unexpectedly (Chrome profile lock, immediate crash, etc.).
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }
}

impl Drop for KioskHandle {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Unix: signal the whole process group so Chromium's
            // renderer / GPU / zygote subprocesses die alongside the
            // launcher. Without this `child.kill()` only targets the
            // launcher PID and renderers linger as orphans.
            #[cfg(unix)]
            unsafe {
                let pid = child.id() as libc::pid_t;
                libc::killpg(pid, libc::SIGTERM);
            }
            // Windows: `child.kill()` calls `TerminateProcess` on the
            // launcher only. Chromium spawns 5-20+ renderer / GPU /
            // utility / zygote subprocesses; they survive the
            // launcher's death and keep the user-data dir locked,
            // which makes the next kiosk launch fail silently with
            // a profile-lock error. `taskkill /F /T /PID` walks the
            // tree and forces them all down. Spawned and detached so
            // a hung taskkill can't block the runtime drop path.
            #[cfg(windows)]
            {
                let pid = child.id();
                let _ = std::process::Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
            // Best-effort kill on the launcher itself.
            let _ = child.kill();
            // Reap on a detached thread so a hung browser (Chromium
            // GPU pinned, X server frozen on a Pi) can't block the
            // async runtime that's driving us. `child.wait()` is
            // synchronous and unbounded; if it never returns, the
            // detached thread leaks until process exit — which is
            // bounded by the OS finally reaping us. Previous version
            // ran `child.wait()` inline and froze station shutdown
            // on flaky kiosks.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

struct KioskCandidate {
    bin: String,
    brand: KioskBrowser,
}

impl KioskCandidate {
    fn kiosk_args(&self, url: &str, profile: Option<&std::path::Path>) -> Option<Vec<String>> {
        match self.brand {
            KioskBrowser::Chromium | KioskBrowser::Edge => {
                // Minimal flag set. `--start-maximized` /
                // `--start-fullscreen` / `--ozone-platform=wayland`
                // were tried earlier to coax Pi OS Bookworm/labwc into
                // true fullscreen, but each combination introduced
                // regressions (silent exits, decoration leaks). Modern
                // Chromium (>= 120) auto-detects Ozone correctly and
                // `--kiosk` alone produces fullscreen on labwc, X11,
                // macOS, and Windows.
                let mut v: Vec<String> = vec![
                    "--kiosk".to_string(),
                    "--noerrdialogs".to_string(),
                    "--disable-infobars".to_string(),
                    "--no-first-run".to_string(),
                    "--no-default-browser-check".to_string(),
                    // Skip OS keyring (gnome-keyring / kwallet / macOS
                    // Keychain / Windows Credential Vault). On headless
                    // RPi kiosks the keyring is locked at boot and
                    // Chromium prompts for the login password before
                    // showing the page. `basic` stores creds in the
                    // profile dir, fine for a disposable kiosk
                    // profile, and we don't save creds anyway.
                    // `--use-mock-keyring` belt-and-suspenders for
                    // builds that ignore `--password-store=basic`.
                    "--password-store=basic".to_string(),
                    "--use-mock-keyring".to_string(),
                ];
                if let Some(dir) = profile {
                    // A per-call profile dir means `--kiosk` takes
                    // effect even when a regular Chrome window is
                    // already open; otherwise Chrome forwards the URL
                    // to the existing process and ignores the flag.
                    //
                    // The temp dir is freshly created per launch so
                    // there's no stale `SingletonLock` to clear.
                    v.push(format!("--user-data-dir={}", dir.display()));
                }
                if matches!(self.brand, KioskBrowser::Edge) {
                    // Not an Edge defect: Chromium's `--kiosk` is a
                    // presentation mode, full screen with no chrome, and it
                    // locks nothing down. Ctrl+T still opens a tab in Chrome,
                    // Brave and Chromium too, on every platform (observed on
                    // Chrome for macOS). Edge on Windows is simply the only
                    // combination where a real lockdown is reachable from the
                    // command line, so it is the only one fixed here.
                    //
                    // Concretely, `--kiosk` alone leaves Edge in a NORMAL
                    // profile session. Measured on
                    // Windows (Edge 151): the window title reads
                    // "... - Profile 1 - Microsoft Edge", not
                    // "... - [InPrivate] - ...". Edge's own shortcuts stay
                    // live in that state, so an operator can still open a
                    // tab, and a new tab in Edge is the MSN page
                    // (ntp.msn.cn in China), which is how a customer
                    // found this. `fullscreen` selects Edge's digital-signage
                    // kiosk instead: single site, InPrivate, Ctrl+T
                    // blocked. In the same measurement it also removed
                    // every Microsoft-domain DNS lookup the browser made
                    // at startup, which matters on the allowlisted proxies
                    // several of our customers run.
                    v.push("--edge-kiosk-type=fullscreen".to_string());
                    // `fullscreen` already defaults to no idle reset; the
                    // other kiosk type defaults to 5 minutes and closes the
                    // window when it fires. Pinned so a Microsoft-side
                    // default change can't start killing station windows
                    // between runs.
                    v.push("--kiosk-idle-timeout-minutes=0".to_string());
                }
                v.push(url.to_string());
                Some(v)
            }
            KioskBrowser::Firefox => Some(vec!["-kiosk".to_string(), url.to_string()]),
            KioskBrowser::Fallback => None,
        }
    }
}

#[cfg(target_os = "linux")]
fn kiosk_candidates() -> Vec<KioskCandidate> {
    [
        ("chromium-browser", KioskBrowser::Chromium),
        ("chromium", KioskBrowser::Chromium),
        ("google-chrome", KioskBrowser::Chromium),
        ("google-chrome-stable", KioskBrowser::Chromium),
        // Deliberately NOT `KioskBrowser::Edge`: Microsoft documents
        // kiosk mode as unsupported on Linux, so `--edge-kiosk-type` is
        // untested there and the plain Chromium flags already work.
        ("microsoft-edge", KioskBrowser::Chromium),
        ("brave-browser", KioskBrowser::Chromium),
        ("firefox", KioskBrowser::Firefox),
    ]
    .into_iter()
    .filter_map(|(name, brand)| {
        which(name).map(|p| KioskCandidate {
            bin: p.to_string_lossy().into_owned(),
            brand,
        })
    })
    .collect()
}

#[cfg(target_os = "macos")]
fn kiosk_candidates() -> Vec<KioskCandidate> {
    // Talk to the binary inside `Contents/MacOS/` directly. `open -a`
    // doesn't pass CLI flags through reliably (Chrome inherits some,
    // Firefox drops `-kiosk` entirely).
    [
        (
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            KioskBrowser::Chromium,
        ),
        // Deliberately NOT `KioskBrowser::Edge`, and this one is measured:
        // Edge 152.0.4191.53 on macOS 26 ignores `--edge-kiosk-type=fullscreen`
        // outright. AppleScript `mode of window 1` reports `normal` with and
        // without the switch, where a real InPrivate window of the same build
        // reports `incognito` (verified by launching it with `--inprivate`).
        // Microsoft documents Edge kiosk mode for Windows only. Chromium drops
        // the unknown switch silently, so passing it here would cost nothing
        // and buy nothing, while the docs would promise macOS stations a
        // lockdown they do not get.
        (
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            KioskBrowser::Chromium,
        ),
        (
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            KioskBrowser::Chromium,
        ),
        (
            "/Applications/Firefox.app/Contents/MacOS/firefox",
            KioskBrowser::Firefox,
        ),
    ]
    .into_iter()
    .filter_map(|(path, brand)| {
        if std::path::Path::new(path).is_file() {
            Some(KioskCandidate {
                bin: path.to_string(),
                brand,
            })
        } else {
            None
        }
    })
    .collect()
}

#[cfg(target_os = "windows")]
fn kiosk_candidates() -> Vec<KioskCandidate> {
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
    let pfx86 =
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into());
    let candidates: Vec<(String, KioskBrowser)> = vec![
        (
            format!("{pf}\\Microsoft\\Edge\\Application\\msedge.exe"),
            KioskBrowser::Edge,
        ),
        (
            format!("{pfx86}\\Microsoft\\Edge\\Application\\msedge.exe"),
            KioskBrowser::Edge,
        ),
        (
            format!("{pf}\\Google\\Chrome\\Application\\chrome.exe"),
            KioskBrowser::Chromium,
        ),
        (
            format!("{pfx86}\\Google\\Chrome\\Application\\chrome.exe"),
            KioskBrowser::Chromium,
        ),
        (
            format!("{pf}\\Mozilla Firefox\\firefox.exe"),
            KioskBrowser::Firefox,
        ),
        (
            format!("{pfx86}\\Mozilla Firefox\\firefox.exe"),
            KioskBrowser::Firefox,
        ),
    ];
    candidates
        .into_iter()
        .filter_map(|(path, brand)| {
            if std::path::Path::new(&path).is_file() {
                Some(KioskCandidate { bin: path, brand })
            } else {
                None
            }
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Disposable Chrome profile dir for kiosk launches. Unique per call
/// so concurrent kiosk windows don't collide. Returns `None` if the
/// system temp dir isn't writable — callers fall through to launching
/// without a profile dir, which Chrome handles by attaching to the
/// existing process and ignoring `--kiosk`. Acceptable degradation.
fn temp_profile_dir() -> Option<PathBuf> {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = base.join(format!("tofupilot-kiosk-{pid}-{nanos}"));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    pub enum ScriptOutcome {
        Focused,
        Opened,
        NotRunning,
    }

    pub fn run_applescript(url: &str, app_name: &str) -> std::io::Result<ScriptOutcome> {
        // `osascript -` reads the script body from stdin so we don't
        // need a temp file. Args after `-` are passed to the script's
        // `on run argv` handler.
        let mut child = Command::new("/usr/bin/osascript")
            .arg("-")
            .arg(url)
            .arg(app_name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(OPEN_CHROME_APPLESCRIPT.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(std::io::Error::other("osascript failed"));
        }
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // Unknown stdout (script crashed, AppleScript syntax regression,
        // permission prompt declined) is treated as "couldn't help" so
        // the caller advances to the next browser or the cross-platform
        // fallback. Bubbling an explicit error keeps the matcher tight.
        match stdout.as_str() {
            "focused" => Ok(ScriptOutcome::Focused),
            "opened" => Ok(ScriptOutcome::Opened),
            "not_running" => Ok(ScriptOutcome::NotRunning),
            _ => Err(std::io::Error::other(format!(
                "unexpected osascript stdout: {stdout:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_for(brand: KioskBrowser) -> Vec<String> {
        KioskCandidate {
            bin: "browser".to_string(),
            brand,
        }
        .kiosk_args("http://127.0.0.1:7321/", None)
        .expect("kiosk-capable brand yields args")
    }

    /// Edge ignores nothing here, it just needs more: with only Chromium's
    /// `--kiosk` it stays in a normal profile session the operator can open
    /// a tab out of, and that tab is the MSN new tab page.
    #[test]
    fn edge_gets_its_own_kiosk_switches() {
        let args = args_for(KioskBrowser::Edge);
        assert!(args.iter().any(|a| a == "--edge-kiosk-type=fullscreen"));
        assert!(args.iter().any(|a| a == "--kiosk-idle-timeout-minutes=0"));
        assert!(args.iter().any(|a| a == "--kiosk"));
    }

    /// The two switches above are Edge-only. Chrome / Chromium / Brave must
    /// not see them.
    #[test]
    fn chromium_gets_only_the_portable_flags() {
        let args = args_for(KioskBrowser::Chromium);
        assert!(!args.iter().any(|a| a.starts_with("--edge-kiosk-type")));
        assert!(!args.iter().any(|a| a.starts_with("--kiosk-idle-timeout")));
        assert!(args.iter().any(|a| a == "--kiosk"));
    }

    /// The URL is positional: anything appended after it would be read as a
    /// second URL and open a second tab.
    #[test]
    fn url_stays_last_for_every_brand() {
        for brand in [
            KioskBrowser::Chromium,
            KioskBrowser::Edge,
            KioskBrowser::Firefox,
        ] {
            let args = args_for(brand);
            assert_eq!(
                args.last().map(String::as_str),
                Some("http://127.0.0.1:7321/")
            );
        }
    }

    #[test]
    fn fallback_has_no_args() {
        let candidate = KioskCandidate {
            bin: "browser".to_string(),
            brand: KioskBrowser::Fallback,
        };
        assert!(candidate
            .kiosk_args("http://127.0.0.1:7321/", None)
            .is_none());
    }
}
