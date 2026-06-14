//! In-process WebSocket + static-file server for the kiosk operator UI.
//!
//! Embeds the operator-ui Vite build (`operator-ui/dist`), serves it on
//! loopback, and bridges the engine's `StationEvent` broadcast and
//! `StationCommand` mpsc to the browser. The wire format and lifecycle are
//! documented below.

// Local WebSocket server for the operator UI air-gap mode. Embeds the
// operator-ui Vite build, serves it on loopback, and proxies the
// existing `StationEvent` broadcast / `StationCommand` mpsc that the
// engine already exposes. The browser-side state machine and reducer
// are unchanged — we just swap the transport.
//
// Wire format on the WS:
//   * server → client (first frame): `{type:"hello", station_id, station_name, procedures}`
//     sent immediately on connect so the SPA doesn't need a separate
//     fetch to bootstrap.
//   * server → client: a `StationEvent` JSON wrapped in a thin
//     `{type:"event", seq:N, event:{...}}` envelope, OR a hydration
//     reply `{type:"hydration", id:X, since_seq:N, events:[...]}`.
//     The seq is monotonic across the server's lifetime; clients
//     use it to drop duplicates straddling the hydrate→live cursor.
//   * client → server: a `StationCommand` JSON or the local control
//     envelope `{type:"hydrate", id:X}`. Hydrate is answered with
//     the server's replay buffer.
//
// Lifecycle: ONE `Server` per CLI process. Bound at startup, lives
// until process exit. Each run plugs its `event_tx` into the server
// via `attach_run`, which returns a `RunAttachment` guard. Dropping
// the guard stops pumping that broadcast; the listener stays up so a
// browser tab survives across runs and `attach_run` on the next run
// reuses the same socket.
//
// Loopback bind, Origin header allow-listed to the server's own
// host:port.
//
// Threat model: localhost-only bind + Origin allow-list defends
// against (a) cross-Origin browser CSRF from a hostile page on a
// different local port, (b) curl/python clients without an Origin
// header. It does NOT defend against (a) other local processes that
// can craft Origin headers (any process with `tofupilot` already
// gets full access on this machine — same posture as the rest of
// the CLI), or (b) malicious browser extensions, which can rewrite
// headers via webRequest. LAN-mode (binding to 0.0.0.0) is
// deliberately not exposed — that path needs token auth which is
// out of scope here.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use include_dir::{include_dir, Dir};
use station_protocol::{StationCommand, StationEvent};
use std::collections::VecDeque;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;

// Bundled SPA. `build.rs` ensures the directory exists even when the
// frontend hasn't been built yet so `cargo build` in isolation still
// compiles; an empty dir produces the placeholder fallback at `/`.
static SPA_DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/operator-ui/dist");

/// Recursive file count for diagnostics. `Dir::files()` only walks
/// the top level; the Vite output puts JS/CSS chunks under `assets/`,
/// so a top-level count would underreport and trigger the "JS chunks
/// missing" warning even on a healthy build. Depth-bounded so a
/// future bundler swap with deeper nesting can't blow the stack.
fn count_spa_files(dir: &Dir<'_>) -> usize {
    fn walk(dir: &Dir<'_>, depth: usize) -> usize {
        if depth >= 16 {
            return dir.files().count();
        }
        dir.files().count() + dir.dirs().map(|d| walk(d, depth + 1)).sum::<usize>()
    }
    walk(dir, 0)
}

/// Cross-platform best-effort liveness check for the kiosk watcher.
/// Unix: `kill(pid, 0)` returns 0 if the process exists, -1 with
/// ESRCH if not. Windows: `OpenProcess(SYNCHRONIZE)` + signaled
/// state (signaled = exited).
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    // OpenProcess(SYNCHRONIZE) + WaitForSingleObject(0) is O(1) and
    // ~µs cheap. The earlier sysinfo-based approach enumerated every
    // system process per poll (~50-200ms, 512KB+ alloc), which is
    // unacceptable on RPi-class kiosks polling at 0.2Hz.
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            // Could not open: process likely gone, or ACL denied
            // (rare for same-user Chromium; happens for protected
            // processes). Conflating "gone" with "denied" means the
            // watcher might fire one false positive on ACL, which
            // is acceptable — the warn text mentions both modes.
            return false;
        }
        let r = WaitForSingleObject(h, 0);
        CloseHandle(h);
        // WAIT_TIMEOUT = still running. Signaled = exited.
        r == WAIT_TIMEOUT
    }
}

/// Asset-extension whitelist for the static_handler "asset miss"
/// warning. Checking `path.contains('.')` was too loose — SPA deep
/// links like `/runs/run.123abc` or `/units/SN-1.2.3` are not asset
/// requests. Match on the trailing segment's extension instead.
fn looks_like_asset(path: &str) -> bool {
    let last_segment = path.rsplit('/').next().unwrap_or("");
    let ext = match last_segment.rsplit_once('.') {
        Some((_, ext)) => ext.to_ascii_lowercase(),
        None => return false,
    };
    matches!(
        ext.as_str(),
        "js" | "mjs"
            | "css"
            | "map"
            | "json"
            | "wasm"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "svg"
            | "ico"
            | "webp"
            | "avif"
            | "html"
            | "htm"
            | "txt"
    )
}

/// Per-process dedupe set for noisy log lines. Origin rejects and
/// asset misses both fire per request; without dedupe a bad kiosk
/// URL with browser auto-reconnect spams journalctl at ~1Hz forever.
/// Set never shrinks — each unique offending value is logged once.
static LOGGED_BAD_ORIGINS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::OnceLock::new();
static LOGGED_ASSET_MISSES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::OnceLock::new();

/// Cap on the dedupe set size. A misbehaving local script rotating
/// Origin headers (or paths) could otherwise grow these unboundedly
/// over hours and OOM the daemon. At 256 we still cover every
/// realistic scenario; past the cap, further unique values are
/// dropped silently rather than logged.
const LOG_DEDUP_CAP: usize = 256;

fn log_origin_reject_once(origin: &str, allowed: &[String]) {
    let set = LOGGED_BAD_ORIGINS.get_or_init(Default::default);
    let mut guard = match set.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if guard.len() >= LOG_DEDUP_CAP {
        return;
    }
    if guard.insert(origin.to_string()) {
        crate::log::warn(&format!(
            "local-ui: /ws rejected — origin={origin:?} not in allowed list {allowed:?}. \
             SPA will hang on a blank page (no live data). \
             Check the URL the kiosk opened — must match one of the allowed origins. \
             (Subsequent rejects from this Origin will be silent.)"
        ));
    }
}

fn log_asset_miss_once(path: &str) {
    let set = LOGGED_ASSET_MISSES.get_or_init(Default::default);
    let mut guard = match set.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if guard.len() >= LOG_DEDUP_CAP {
        return;
    }
    if guard.insert(path.to_string()) {
        crate::log::warn(&format!(
            "local-ui: asset miss {path:?}; serving index.html instead. \
             SPA likely loads but a JS chunk is absent — kiosk will render blank. \
             Rebuild operator-ui. (Subsequent misses for this path will be silent.)"
        ));
    }
}

/// Cap on the per-run event ring. The ring holds events since the
/// most recent `RunStarted`; on cap overflow we evict the oldest
/// non-pinned entry. The `RunStarted` itself is pinned outside the
/// ring so eviction never drops the event hydration most depends on.
const HYDRATION_RING_CAP: usize = 4096;

/// Per-connection outbound mailbox depth. WS frames are tiny JSON
/// payloads; a few hundred slots absorbs bursty event runs without
/// back-pressuring the engine. Lagged consumers (slow tab on weak
/// hardware) drop frames at the broadcast layer — same posture as
/// the centrifugo path.
const OUTBOUND_CHAN_CAP: usize = 256;

/// Capacity of the per-connection forward channel: the broadcast
/// receiver feeds into this through a wrapper task that stamps a
/// seq. Sized larger than the upstream broadcast (128) so a brief
/// stall in the writer doesn't cascade into a broadcast lag.
const FORWARD_CHAN_CAP: usize = 256;

#[derive(Clone, serde::Serialize)]
pub struct ProcedureRef {
    pub id: String,
    pub name: String,
}

/// Wraps a `StationEvent` with a monotonically-increasing sequence
/// number assigned by the local server. Clients use seq for two
/// things:
///   * dedupe across hydrate→live straddle (drop live events whose
///     seq is ≤ the seq the hydration response carried),
///   * diagnose dropped frames (gaps in the seq line means the
///     broadcast lagged or the server skipped a frame).
#[derive(Clone)]
struct StampedEvent {
    seq: u64,
    event: StationEvent,
}

#[derive(Clone)]
struct HydrationSnapshot {
    /// Pinned `RunStarted` for the current run. Survives ring eviction
    /// so a hydrate after a long noisy run still reconstructs.
    run_started: Option<StampedEvent>,
    /// Subsequent events since `run_started`. VecDeque so eviction is
    /// O(1) amortised, not O(n) like Vec::remove(0).
    events: VecDeque<StampedEvent>,
    /// seq of the last event in `events`, or `run_started`'s seq if
    /// the ring is empty post-clear. `0` if no events have shipped
    /// yet. Used so the live pump knows where the snapshot ends.
    last_seq: u64,
    /// True after the pump task hit a `Lagged` recv error and cleared
    /// the ring. Tells the SPA "we lost events; treat this hydrate as
    /// partial — don't wipe live state you already have." Cleared when
    /// the next `RunStarted` lands and rebuilds a fresh ring. Without
    /// this, a hydration arriving after lag returned `{snapshot:null}`
    /// and the SPA fell to idle even though a run was still alive on
    /// the CLI.
    lagged: bool,
}

/// Hello payload sent as the first WS frame on connect, before any
/// stamped events. Folding bootstrap data into the socket gives the
/// SPA a single bootstrap path and guarantees the payload is
/// self-consistent with the connection that just opened.
#[derive(Clone, serde::Serialize)]
struct HelloPayload {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "stationId")]
    station_id: String,
    #[serde(rename = "stationName")]
    station_name: String,
    procedures: Vec<ProcedureRef>,
    /// `"local"` for `tofupilot run --kiosk` (single-procedure
    /// session), `"station"` for the long-lived station daemon
    /// (procedure list comes from the deployments dir). Used by the
    /// SPA to gate UI affordances that depend on the host model.
    mode: &'static str,
    /// Identity envelope for analytics (PostHog identify in the SPA).
    /// Sourced from the cached `WhoamiCache`. Optional everywhere so
    /// the kiosk still works pre-login or when whoami refresh has
    /// failed; the SPA's identify dispatcher no-ops on missing fields.
    /// `auth_type === "station"` today; user-mode kiosk lands later
    /// and populates the user_* fields without changing this shape.
    #[serde(rename = "authType", skip_serializing_if = "Option::is_none")]
    auth_type: Option<String>,
    #[serde(rename = "organizationSlug", skip_serializing_if = "Option::is_none")]
    organization_slug: Option<String>,
    #[serde(rename = "organizationName", skip_serializing_if = "Option::is_none")]
    organization_name: Option<String>,
    /// Canonical station id from `WhoamiCache.station_id`. Distinct
    /// from the top-level `stationId` field, which carries the
    /// `installation_id` (used for tab routing / bootstrap). Studio
    /// identifies on the canonical station id, so the operator-UI
    /// uses this when present to keep PostHog distinct_ids aligned
    /// across hosts. Falls back to `stationId` (installation id) when
    /// whoami is unavailable.
    #[serde(rename = "analyticsStationId", skip_serializing_if = "Option::is_none")]
    analytics_station_id: Option<String>,
    #[serde(rename = "userId", skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    #[serde(rename = "userEmail", skip_serializing_if = "Option::is_none")]
    user_email: Option<String>,
    #[serde(rename = "userName", skip_serializing_if = "Option::is_none")]
    user_name: Option<String>,
}

/// Identity bundle threaded into `Server::start`. Mirrors the subset
/// of `WhoamiCache` the operator-UI cares about. Defaults to all-None
/// so callers without a whoami cache (e.g. unauthenticated `run --kiosk`)
/// can pass `HelloIdentity::default()`.
#[derive(Clone, Default)]
pub struct HelloIdentity {
    pub auth_type: Option<String>,
    pub organization_slug: Option<String>,
    pub organization_name: Option<String>,
    /// Canonical station id (separate from `installation_id`). Used
    /// by the operator-UI for PostHog identify so distinct_ids match
    /// what studio sends.
    pub station_id: Option<String>,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub user_name: Option<String>,
}

#[derive(Clone)]
struct AppState {
    /// Inbound `UiResponse` sink for the active run. Swapped per-run
    /// via `attach_run`. Other run-scoped intents (Stop, Kill) go
    /// through `cancel_token`, NOT through this channel — kept narrow
    /// so its only contract is "deliver an answer to a prompt."
    ui_response_tx: Arc<Mutex<mpsc::Sender<StationCommand>>>,
    /// Run cancellation. Stop / Kill / Exit on the WS write here
    /// directly. `None` between runs (placeholder cancel token from
    /// `Server::start`); swapped per-run via `attach_run`.
    cancel_token: Arc<Mutex<crate::commands::run::cancel::CancelToken>>,
    /// Station-level command sink (Exit, Reboot, Shutdown, Run,
    /// etc.). Installed by station mode at startup and kept for the
    /// lifetime of the daemon. `None` for `run --kiosk` standalone —
    /// those commands are no-ops there.
    station_cmd_tx: Arc<Mutex<Option<mpsc::Sender<StationCommand>>>>,
    /// Materialized hydration state plus the seq of its tail.
    hydration: Arc<Mutex<HydrationSnapshot>>,
    /// Monotonic event seq, lives for the server's lifetime so seqs
    /// stay monotonic across `attach_run` swaps. Per-connection pump
    /// cursors compare against `last_seq` from hydration replies, so a
    /// per-run reset would let a new run's seq=1 fall behind a
    /// cursor advanced by an earlier run and the pump would silently
    /// drop the new `RunStarted` (browser tab stuck on prior PASS).
    seq_counter: Arc<AtomicU64>,
    /// Per-connection pumps read it via the StampedEvent broadcast they
    /// consume.
    seq_broadcast: broadcast::Sender<StampedEvent>,
    /// Current run's pump task. `attach_run` aborts the prior pump
    /// before installing a new one, so a Run-again click on the
    /// outcome screen can't race the prior run's late
    /// `RunComplete(ABORTED)` against the new run's `RunStarted` on
    /// the shared `seq_broadcast`. Without this, a prior pump kept
    /// pumping its broadcast for the duration of the parked
    /// teardown task — the operator-UI's pending state could be
    /// promoted to the prior run's id+outcome before the new
    /// `RunStarted` rebuilt state.
    current_pump: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Allowed Origin header values for WS upgrades.
    allowed_origins: Arc<Vec<String>>,
    /// Hello payload sent as the first WS frame on every connect.
    /// Mutated in place by `attach_run` so the procedure list reflects
    /// what the current run carries (single-procedure standalone vs.
    /// the full station list).
    hello: Arc<Mutex<HelloPayload>>,
    /// Optional override path to serve the SPA from disk instead of
    /// the binary-embedded `include_dir!` tree. Set via the env var
    /// `TOFUPILOT_LOCAL_UI_DEV_DIR` so SPA iteration doesn't require
    /// a `cargo build` per change.
    dev_dir: Option<PathBuf>,
    /// Root the `/files/*` route serves from: the attached run's
    /// procedure directory. UI components reference images relative
    /// to it (radio/checklist option `image`, image component
    /// `value`) — same base the TUI's `ImageCache` resolves against.
    /// Swapped per-run via `attach_run`; `None` between runs, so
    /// `/files/*` 404s when no run is attached.
    procedure_dir: Arc<Mutex<Option<PathBuf>>>,
}

/// Long-lived local WS server. One per CLI process. Bind once at
/// startup, then `attach_run` per test run. The listener task is
/// detached and dies when the process exits.
pub struct Server {
    state: AppState,
    boot_url: String,
    /// Bound loopback port. Stored so `attach_kiosk`'s readiness
    /// probe can connect directly instead of re-parsing `boot_url`
    /// (which is fragile if the URL shape ever changes).
    port: u16,
    /// Liveness flag flipped to `false` when the `axum::serve` task
    /// exits (clean shutdown or panic). `attach_kiosk` checks this
    /// before launching a browser at a dead port.
    alive: Arc<std::sync::atomic::AtomicBool>,
    /// Set true when we deliberately tear down the kiosk (Server
    /// drop, exec swap). The kiosk-exit watcher reads this before
    /// logging "kiosk browser exited" so a clean shutdown doesn't
    /// false-alarm.
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
    /// Watcher task handle, aborted on `Server` drop so we don't
    /// leak a tokio task polling a dead PID forever.
    kiosk_watcher: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Browser process spawned by `attach_kiosk`. Dropping the
    /// `Server` (CLI shutdown / kill) closes the kiosk window.
    /// Held under `Mutex` so `attach_kiosk` (called after `start`)
    /// can install it post-construction without `&mut Self`.
    kiosk: tokio::sync::Mutex<Option<crate::browser_open::KioskHandle>>,
}

impl Server {
    /// Bind the listener and spawn the axum task. Returns once the
    /// listener is live; the SPA is reachable at `boot_url()`.
    pub async fn start(
        station_id: String,
        station_name: String,
        identity: HelloIdentity,
    ) -> std::io::Result<Self> {
        let hydration = Arc::new(Mutex::new(HydrationSnapshot {
            run_started: None,
            events: VecDeque::new(),
            last_seq: 0,
            lagged: false,
        }));
        let (seq_broadcast, _) = broadcast::channel::<StampedEvent>(FORWARD_CHAN_CAP);

        // Stable port so a previously-opened tab survives across
        // runs: the browser keeps the tab pointed at
        // `http://127.0.0.1:7321/`, the SPA's plain-ws transport
        // reconnects automatically, and the hydration ring catches
        // the new run.
        //
        // The bind is also our single-instance gate. A second daemon
        // (e.g. the supervisor respawning while the previous instance
        // is still tearing down, or an operator running `tofupilot
        // service start` in a terminal alongside a systemd unit) hits
        // EADDRINUSE here and bubbles the error out so the caller
        // exits cleanly. No ephemeral fallback — that would silently
        // start a second daemon on a different port and leave two UIs
        // racing for the same DB / lock.
        //
        // `TOFUPILOT_LOCAL_UI_PORT=<u16>` overrides the default port
        // (e.g. for dev side-by-side instances). The override is
        // also enforced — no fallback, same single-instance guarantee.
        let preferred_port: u16 = env::var("TOFUPILOT_LOCAL_UI_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(crate::commands::service::DEFAULT_LOCAL_PORT);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", preferred_port))
            .await
            .map_err(|e| {
                match e.kind() {
                    std::io::ErrorKind::AddrInUse => {
                        crate::log::error(&format!(
                            "Port {preferred_port} on 127.0.0.1 is already in use. \
                             Another tofupilot daemon is likely running on this host. \
                             Stop it with `tofupilot service stop` (or \
                             `systemctl --user stop tofupilot` on Linux), \
                             or run `tofupilot service status` to see what's holding the port."
                        ));
                    }
                    std::io::ErrorKind::PermissionDenied => {
                        crate::log::error(&format!(
                            "Permission denied binding 127.0.0.1:{preferred_port}. \
                             Ports < 1024 require elevated privileges; pick a higher \
                             port via TOFUPILOT_LOCAL_UI_PORT."
                        ));
                    }
                    std::io::ErrorKind::AddrNotAvailable => {
                        crate::log::error(&format!(
                            "Cannot bind 127.0.0.1:{preferred_port}: loopback address \
                             unavailable. Check that the loopback interface (lo / lo0) \
                             is up: `ifconfig lo0` (macOS) or `ip addr show lo` (Linux). \
                             Hardened images may disable loopback for non-root users; \
                             containers without `--network host` and net-namespaced \
                             environments can also surface this."
                        ));
                    }
                    std::io::ErrorKind::WouldBlock => {
                        crate::log::error(&format!(
                            "Cannot bind 127.0.0.1:{preferred_port}: kernel returned \
                             EWOULDBLOCK. Likely SO_REUSEADDR contention with a \
                             socket in TIME_WAIT — wait ~60s and retry, or pick a \
                             different port via TOFUPILOT_LOCAL_UI_PORT."
                        ));
                    }
                    _ => {
                        // `Uncategorized` (Linux EPERM via seccomp / LSM) and
                        // `Other` end up here. Surface raw OS error code so a
                        // sysadmin reading the log has something to grep for.
                        crate::log::error(&format!(
                            "local-ui: bind 127.0.0.1:{preferred_port} failed: {e} \
                             (kind={:?}, raw_os_error={:?}). \
                             Run `tofupilot service status` for diagnostics.",
                            e.kind(),
                            e.raw_os_error()
                        ));
                    }
                }
                e
            })?;
        let port = listener.local_addr()?.port();
        let allowed_origins = Arc::new(vec![
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
        ]);
        crate::log::info(&format!(
            "local-ui: bound 127.0.0.1:{port}; allowed origins: {}",
            allowed_origins.join(", ")
        ));

        let dev_dir = env::var("TOFUPILOT_LOCAL_UI_DEV_DIR")
            .ok()
            .map(PathBuf::from);
        if let Some(ref p) = dev_dir {
            crate::log::info(&format!(
                "local-ui: serving SPA from disk override: {}",
                p.display()
            ));
        }

        // Bundled SPA inventory. A blank kiosk page almost always means
        // the embedded bundle is empty (build.rs placeholder ran but
        // the operator-ui Vite build didn't, so the placeholder HTML is
        // all we serve). Surface this loudly at boot so the operator
        // sees the cause without having to inspect Network tab.
        let has_index = SPA_DIST.get_file("index.html").is_some();
        let asset_count = count_spa_files(&SPA_DIST);
        if !has_index {
            crate::log::warn(
                "local-ui: embedded SPA has no index.html; only the placeholder page \
                 will render. Build the operator-ui SPA into operator-ui/dist and \
                 rebuild the CLI.",
            );
        } else if asset_count <= 1 {
            crate::log::warn(&format!(
                "local-ui: embedded SPA has only {asset_count} file(s); JS chunks may \
                 be missing and the kiosk will render blank. Rebuild operator-ui."
            ));
        } else {
            crate::log::info(&format!(
                "local-ui: embedded SPA ready ({asset_count} files, index.html present)"
            ));
        }

        // Placeholder ui_response_tx that drops messages until the
        // first `attach_run` swaps in a real one. The window is small
        // (a browser tab opened pre-attach has no run to answer), but
        // a closed channel here would surface as a noisy warning each
        // frame. Same idea for the placeholder cancel token.
        let (placeholder_tx, _placeholder_rx) = mpsc::channel::<StationCommand>(1);
        let (placeholder_cancel, _placeholder_cancel_rx) =
            crate::commands::run::cancel::CancelToken::new();

        let hello = Arc::new(Mutex::new(HelloPayload {
            kind: "hello",
            station_id,
            station_name,
            procedures: Vec::new(),
            mode: "station",
            auth_type: identity.auth_type,
            organization_slug: identity.organization_slug,
            organization_name: identity.organization_name,
            analytics_station_id: identity.station_id,
            user_id: identity.user_id,
            user_email: identity.user_email,
            user_name: identity.user_name,
        }));

        let state = AppState {
            ui_response_tx: Arc::new(Mutex::new(placeholder_tx)),
            cancel_token: Arc::new(Mutex::new(placeholder_cancel)),
            station_cmd_tx: Arc::new(Mutex::new(None)),
            hydration,
            seq_counter: Arc::new(AtomicU64::new(0)),
            seq_broadcast,
            current_pump: Arc::new(Mutex::new(None)),
            allowed_origins,
            hello,
            dev_dir,
            procedure_dir: Arc::new(Mutex::new(None)),
        };

        let app = Router::new()
            .route("/ws", get(ws_handler))
            .route("/files/*path", get(files_handler))
            .fallback(static_handler)
            .with_state(state.clone());

        let url = format!("http://127.0.0.1:{port}/");
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let alive_for_task = alive.clone();
        tokio::spawn(async move {
            let result = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
            // Flip first so any concurrent `attach_kiosk` sees the
            // dead state immediately; logging is non-atomic and could
            // be re-entered by the kiosk warning otherwise.
            alive_for_task.store(false, std::sync::atomic::Ordering::Release);
            match result {
                Err(e) => crate::log::error(&format!(
                    "local-ui server crashed: {e}. \
                     Kiosk will lose the operator UI. \
                     Restart the CLI to recover."
                )),
                Ok(()) => {
                    crate::log::warn("local-ui server stopped. The kiosk has lost its connection.")
                }
            }
        });

        Ok(Server {
            state,
            boot_url: url,
            port,
            alive,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kiosk_watcher: tokio::sync::Mutex::new(None),
            kiosk: tokio::sync::Mutex::new(None),
        })
    }

    /// Browser URL for the SPA. Shape: `http://127.0.0.1:<port>/`.
    pub fn boot_url(&self) -> &str {
        &self.boot_url
    }

    /// Liveness: `true` while the `axum::serve` task is still running.
    /// Flipped to `false` when the task returns (clean shutdown, panic,
    /// or the listener was closed). Callers can poll this before
    /// pointing a browser at `boot_url`.
    pub fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Set the idle deployment list AND broadcast the diff as
    /// synthetic `DeploymentAdded` / `DeploymentRemoved` events so
    /// an already-connected kiosk SPA refreshes `liveProcedures`
    /// without a reload. Called from `refresh_idle_procedures` (boot
    /// seed, post-run restore, post-pull). Pull loop also publishes
    /// its own `DeploymentAdded` per new deployment via
    /// `publish_event`; the SPA reducer folds idempotently on
    /// `procedure_id`, so a duplicate is a no-op.
    pub async fn set_procedures(&self, procedures: Vec<ProcedureRef>) {
        let (prior, station_id) = {
            let mut h = self.state.hello.lock().await;
            let station_id = h.station_id.clone();
            let prior = std::mem::replace(&mut h.procedures, procedures.clone());
            (prior, station_id)
        };
        let prior_ids: std::collections::HashSet<&str> =
            prior.iter().map(|p| p.id.as_str()).collect();
        let next_ids: std::collections::HashSet<&str> =
            procedures.iter().map(|p| p.id.as_str()).collect();
        for added in procedures
            .iter()
            .filter(|p| !prior_ids.contains(p.id.as_str()))
        {
            self.publish_event(station_protocol::StationEvent::DeploymentAdded {
                installation_id: station_id.clone(),
                procedure_id: added.id.clone(),
                procedure_name: added.name.clone(),
                deployment_id: String::new(),
            })
            .await;
        }
        for removed in prior.iter().filter(|p| !next_ids.contains(p.id.as_str())) {
            self.publish_event(station_protocol::StationEvent::DeploymentRemoved {
                installation_id: station_id.clone(),
                procedure_id: removed.id.clone(),
                deployment_id: String::new(),
            })
            .await;
        }
    }

    /// Inject a free-standing `StationEvent` into the local-WS
    /// broadcast and hydration ring, bypassing the per-run pump.
    /// Used by station-level emitters (pull loop, upload-queue drain)
    /// so a Vite kiosk SPA sees `DeploymentAdded` / `DeploymentRemoved`
    /// / `RunUpload*` while no run is in flight. Without this, those
    /// events only reached the web (Centrifugo) operator UI; the
    /// loopback transport stayed silent until the next run attached
    /// its own pump.
    pub async fn publish_event(&self, event: station_protocol::StationEvent) {
        let seq = self.state.seq_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let stamped = StampedEvent { seq, event };
        update_ring(&self.state.hydration, &stamped).await;
        let _ = self.state.seq_broadcast.send(stamped);
    }

    /// Plug the station-mode command channel into the local-WS
    /// server. Station-level commands arriving on a kiosk tab (Exit,
    /// Reboot, Shutdown, Run, ...) are forwarded here so the station
    /// loop's `handle_command` runs the same path the Centrifugo
    /// socket does. Run-scoped commands route elsewhere: `UiResponse`
    /// to the active run's `ui_response_tx`, `Stop` / `Kill` straight
    /// to the run's cancel token.
    pub async fn set_station_cmd_sink(&self, tx: mpsc::Sender<StationCommand>) {
        *self.state.station_cmd_tx.lock().await = Some(tx);
    }

    /// Synchronously drop the attached kiosk window if any, killing
    /// the browser child via `KioskHandle::drop`. Used before a
    /// process-image swap (auto-update reexec): `execvp` wipes the
    /// heap so Drop never runs, and the new tofupilot would spawn a
    /// second kiosk window on top of the orphaned first one.
    /// Best-effort and safe to call when no kiosk is attached.
    /// Implicit-drop path. The async lock acquisitions in
    /// `detach_kiosk` aren't reachable from `Drop`, so we publish the
    /// shutdown flag synchronously here. The kiosk watcher reads with
    /// `Acquire` and will short-circuit instead of false-alarming.
    /// `KioskHandle::Drop` (called by `Mutex<Option<_>>::drop`) still
    /// kills the browser child.
    fn flag_shutting_down(&self) {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub async fn detach_kiosk(&self) {
        // Suppress watcher false-alarm BEFORE the kill. If we set the
        // flag after dropping `KioskHandle`, the watcher's next tick
        // can race the Drop and log "kiosk browser exited" on a
        // perfectly clean teardown.
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        // Lock order: kiosk THEN kiosk_watcher. Mirrors `attach_kiosk`
        // to avoid AB/BA deadlock if a future caller runs the two
        // concurrently. Today both are serialized at the call sites
        // (station mode, run mode) but the inversion is a footgun.
        let mut slot = self.kiosk.lock().await;
        if let Some(h) = self.kiosk_watcher.lock().await.take() {
            h.abort();
        }
        // Taking out of the Option drops the KioskHandle right here,
        // which fires killpg(SIGTERM) on the browser process group.
        let _ = slot.take();
    }

    /// Open the SPA in a kiosk-mode browser window and tie its
    /// lifetime to this `Server`. The browser process is killed
    /// when the `Server` is dropped (CLI shutdown / kill / crash).
    /// Subsequent calls are no-ops while a kiosk is already
    /// attached — the existing window stays.
    pub async fn attach_kiosk(&self) -> Option<crate::browser_open::KioskBrowser> {
        if !self.is_alive() {
            crate::log::error(
                "local-ui: server task is not running; skipping kiosk launch. \
                 Pointing a browser at the URL would yield a connection-refused \
                 retry loop with no UI feedback. Restart the CLI.",
            );
            return None;
        }
        let mut slot = self.kiosk.lock().await;
        if let Some(existing) = slot.as_ref() {
            // Already attached. The browser is hopefully still
            // alive; we don't probe (no portable way) and we
            // don't relaunch (would create a duplicate window).
            crate::log::info(&format!(
                "local-ui: kiosk already attached ({:?}); skipping relaunch",
                existing.brand
            ));
            return Some(existing.brand);
        }

        // Pre-launch readiness probe: confirm the listener is actually
        // accepting on loopback before we point a kiosk at it. A race
        // here (browser launches before axum::serve() is ready) shows
        // up to the operator as a blank page that "fixes itself" on
        // refresh — pin it down at the source. Use the stored port
        // directly rather than re-parsing `boot_url`.
        let probe_port = self.port;
        match tokio::task::spawn_blocking(move || {
            let addr: std::net::SocketAddr = ([127, 0, 0, 1], probe_port).into();
            std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500))
                .map(|_| ())
                .map_err(|e| format!("{e} (kind={:?})", e.kind()))
        })
        .await
        {
            Ok(Ok(())) => {
                // Healthy case is implicit in the "kiosk launched" line
                // below — no separate "probe OK" log to keep boot
                // chatter down.
            }
            Ok(Err(msg)) => {
                crate::log::warn(&format!(
                    "local-ui: kiosk pre-launch probe FAILED ({msg}); launching anyway. \
                     If the page is blank, the listener wasn't ready when the browser \
                     loaded — refresh the kiosk window (Ctrl+R / Cmd+R)."
                ));
            }
            Err(e) => {
                crate::log::warn(&format!("local-ui: kiosk probe task join failed: {e}"));
            }
        }

        match crate::browser_open::open_kiosk(&self.boot_url) {
            Ok(handle) => {
                let brand = handle.brand;
                let pid = handle.pid();
                crate::log::info(&format!(
                    "local-ui: kiosk launched ({:?}) → {}",
                    brand, self.boot_url
                ));
                if matches!(brand, crate::browser_open::KioskBrowser::Fallback) {
                    crate::log::warn(
                        "local-ui: no kiosk-capable browser found; opened default browser \
                         instead. Window will have chrome / tabs and won't close on CLI exit. \
                         Install Chromium / Chrome / Edge / Firefox for true kiosk mode.",
                    );
                } else if let Some(pid) = pid {
                    // Watcher: log when the kiosk window exits unexpectedly.
                    // The single most common silent blank-page mode is
                    // Chrome immediately crashing on a profile lock or
                    // missing libs, leaving the operator staring at a
                    // closed window with no log entry. Polls every 5s
                    // with `kill(pid, 0)` (unix) or OpenProcess/Wait
                    // (windows). Aborts on Server drop; suppresses
                    // false-alarm via `shutting_down` flag.
                    let shutting_down = self.shutting_down.clone();
                    let handle = tokio::spawn(async move {
                        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                        interval.tick().await; // skip the immediate first tick
                        loop {
                            interval.tick().await;
                            // `pid_alive` is sync-only and can be expensive
                            // on Windows; spawn_blocking keeps the worker
                            // thread responsive.
                            let alive =
                                match tokio::task::spawn_blocking(move || pid_alive(pid)).await {
                                    Ok(b) => b,
                                    Err(_) => continue,
                                };
                            if !alive {
                                if shutting_down.load(std::sync::atomic::Ordering::Acquire) {
                                    // Clean teardown: KioskHandle::Drop killed
                                    // the browser. Don't false-alarm.
                                    return;
                                }
                                crate::log::warn(&format!(
                                    "local-ui: kiosk browser process (pid {pid}) has exited. \
                                     Operator UI window is gone. \
                                     Common causes: Chrome profile lock, missing libs, \
                                     OOM, or operator closed it manually."
                                ));
                                return;
                            }
                        }
                    });
                    *self.kiosk_watcher.lock().await = Some(handle);
                }
                *slot = Some(handle);
                Some(brand)
            }
            Err(e) => {
                crate::log::warn(&format!(
                    "couldn't auto-open browser ({e}); open the URL manually: {}",
                    self.boot_url
                ));
                None
            }
        }
    }

    /// Plug a run's broadcast into this server. The returned
    /// `RunAttachment` guard owns the pump task that ferries events
    /// into the seq broadcast and ring; dropping it stops the pump
    /// (the broadcast itself stays live for any other subscriber).
    /// The hydration ring is NOT cleared on drop — events from the
    /// previous run stay visible to a tab opening just after run end
    /// so the operator sees the final state. The next `attach_run`'s
    /// first `RunStarted` event clears the ring via `update_ring`.
    pub async fn attach_run(
        &self,
        event_tx: broadcast::Sender<StationEvent>,
        ui_response_tx: mpsc::Sender<StationCommand>,
        cancel_token: crate::commands::run::cancel::CancelToken,
        procedures: Vec<ProcedureRef>,
        // Directory `/files/*` serves from for this run. `None` when
        // the caller has no on-disk procedure (synthetic-fail handles)
        // — the route then 404s and the SPA shows its image fallback.
        procedure_dir: Option<PathBuf>,
        mode: &'static str,
    ) -> RunAttachment {
        // Swap the inbound sinks so frames arriving on existing WS
        // connections route to this run.
        *self.state.ui_response_tx.lock().await = ui_response_tx;
        *self.state.cancel_token.lock().await = cancel_token;
        *self.state.procedure_dir.lock().await = procedure_dir;

        // Refresh the hello payload so a tab that connects mid-run
        // (or reconnects after a restart) sees the right procedure
        // list and mode marker on its first frame.
        {
            let mut h = self.state.hello.lock().await;
            h.procedures = procedures;
            h.mode = mode;
        }

        // Stop the prior run's pump BEFORE spawning the new one. The
        // prior run might still be in teardown (parked on the station
        // dispatcher's `prior_run_teardowns` JoinSet) and its
        // broadcast is still alive — without this abort, the prior
        // pump would keep stamping its events into the shared
        // `seq_broadcast`, racing the new run's `RunStarted` on the
        // operator-UI WS. Operator-UI's pending state could be
        // promoted to the prior run's id+outcome before the new
        // `RunStarted` arrived, briefly flipping the screen to a
        // stale outcome.
        if let Some(prior) = self.state.current_pump.lock().await.take() {
            prior.abort();
        }

        // Pump task: tap the run's broadcast, stamp each event with
        // a monotonic seq, refresh the ring, and re-broadcast as
        // `StampedEvent` for per-connection pumps to consume. The
        // counter lives on `AppState` so seqs stay monotonic across
        // runs — see the field doc for why a per-run reset breaks
        // the connection-side dedupe cursor.
        let hydration = self.state.hydration.clone();
        let stamped_tx = self.state.seq_broadcast.clone();
        let counter = self.state.seq_counter.clone();
        let mut rx = event_tx.subscribe();
        let pump_handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let seq = counter.fetch_add(1, Ordering::Relaxed) + 1;
                        let stamped = StampedEvent { seq, event };
                        update_ring(&hydration, &stamped).await;
                        let _ = stamped_tx.send(stamped);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Upstream broadcast dropped events — we
                        // can't reconstruct them. Invalidate the
                        // ring so hydration isn't a partial lie, and
                        // surface the lag so dev iteration can
                        // diagnose dropped frames.
                        crate::log::warn(&format!(
                            "local-ui: lagged {n} broadcast event(s); hydration ring invalidated"
                        ));
                        let mut h = hydration.lock().await;
                        h.run_started = None;
                        h.events.clear();
                        h.lagged = true;
                        // last_seq stays so live consumers don't
                        // re-emit an event that already shipped.
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Hand the JoinHandle to AppState so the next `attach_run` can
        // abort it if the old broadcast is still alive when a new run
        // starts (parked teardown, Run-again race). On natural run
        // completion the pump exits via `RecvError::Closed` — no
        // explicit abort needed and indeed unsafe (would race the
        // drain of the terminal `RunComplete`).
        *self.state.current_pump.lock().await = Some(pump_handle);
        RunAttachment { _private: () }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Publish shutdown flag so the watcher short-circuits instead
        // of logging "kiosk browser exited" when `KioskHandle::Drop`
        // kills the child during implicit teardown. Async-lock acquire
        // isn't reachable from sync Drop, so we touch only the atomic.
        self.flag_shutting_down();
        // Best-effort watcher abort. `try_lock` because Drop runs sync
        // and the lock should be uncontested at process shutdown; if
        // it isn't, the watcher will just observe the shutdown flag
        // on its next tick and exit cleanly.
        if let Ok(mut guard) = self.kiosk_watcher.try_lock() {
            if let Some(h) = guard.take() {
                h.abort();
            }
        }
    }
}

/// Marker tying a run to its pump. Drop is a no-op: the pump exits
/// naturally when the run's broadcast closes (every sender dropped),
/// at which point its buffered events are still drained to the
/// `seq_broadcast` so the operator-UI sees the terminal `RunComplete`.
/// Aborting the pump from Drop would race that drain and could lose
/// the terminal — leaving the kiosk stuck on `'starting'`.
///
/// The pump's `JoinHandle` lives on `AppState::current_pump`. A
/// successor `attach_run` aborts it explicitly (the only case where
/// abort is correct: a new run is starting and the old broadcast
/// might still be alive in a parked teardown task).
pub struct RunAttachment {
    _private: (),
}

/// Apply ring lifecycle:
///   * `RunStarted` clears the ring and pins the new event.
///   * `RunComplete` / `RunCrashed` keep the events visible (so a
///     tab opening just after run end still hydrates the reports
///     screen) but stop pinning the started event for the *next*
///     run — that next `RunStarted` will pin itself.
///   * Other events push into the deque, evicting from the front
///     when the cap is reached.
///
/// `last_seq` always advances, even on the clear path, so the live
/// pump's dedupe cursor works across ring resets.
async fn update_ring(hydration: &Arc<Mutex<HydrationSnapshot>>, stamped: &StampedEvent) {
    let mut h = hydration.lock().await;
    h.last_seq = stamped.seq;
    if let StationEvent::RunStarted { .. } = &stamped.event {
        h.events.clear();
        h.run_started = Some(stamped.clone());
        // Fresh run = fresh ring, lag is no longer relevant.
        h.lagged = false;
        return;
    }
    if h.events.len() >= HYDRATION_RING_CAP {
        h.events.pop_front();
    }
    h.events.push_back(stamped.clone());
}

// ---------------------------------------------------------------------------
// WS handler
// ---------------------------------------------------------------------------

async fn ws_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let origin_raw = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let origin_ok = origin_raw
        .as_deref()
        .map(|origin| state.allowed_origins.iter().any(|a| a == origin))
        .unwrap_or(false);
    if !origin_ok {
        // Blank kiosk page often = SPA loaded but its WS connect was
        // 403'd here, leaving the operator stuck on a static shell with
        // no live state. Surface the offending Origin so the operator
        // can compare against `allowed_origins`. Dedupe by Origin so
        // a bad kiosk URL with browser auto-reconnect doesn't spam
        // journalctl at ~1Hz forever.
        let origin_str = origin_raw.as_deref().unwrap_or("<missing>");
        log_origin_reject_once(origin_str, &state.allowed_origins);
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    ws.on_upgrade(move |socket| connection(socket, state))
}

async fn connection(socket: WebSocket, state: AppState) {
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Per-connection outbound mailbox. The pump and the hydration
    // handler both write here; a single task owns the actual
    // `WebSocket` send so frame ordering stays sane.
    let (out_tx, mut out_rx) = mpsc::channel::<String>(OUTBOUND_CHAN_CAP);

    // Heartbeat cadence: 20s ping. Browsers usually keep idle WS
    // connections open for minutes, but a flaky NAT or load balancer
    // can silently drop a connection that hasn't sent a frame in a
    // while. The ping fires only when no other frame went out
    // recently; the SPA's WebSocket auto-pongs at the protocol layer
    // so we don't need a tracker on the receive side.
    const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);
    let writer = tokio::spawn(async move {
        let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so we don't ping
        // before the hello frame has gone out.
        tick.tick().await;
        loop {
            tokio::select! {
                biased;
                payload = out_rx.recv() => {
                    let Some(payload) = payload else { break };
                    if ws_sender.send(Message::Text(payload)).await.is_err() {
                        break;
                    }
                }
                _ = tick.tick() => {
                    if ws_sender.send(Message::Ping(Vec::new())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = ws_sender.close().await;
    });

    // Hello frame: send the bootstrap config first so the SPA
    // doesn't need a separate fetch. Snapshot under the lock so a
    // concurrent `attach_run` can't race a partial mutation.
    {
        let hello = state.hello.lock().await.clone();
        if let Ok(payload) = serde_json::to_string(&hello) {
            let _ = out_tx.send(payload).await;
        }
    }

    // Live pump: subscribe to the stamped broadcast NOW so we don't
    // miss events between hello and the first hydrate. Each frame
    // carries its seq; `cursor` (advanced by hydration replies) tells
    // us which events to drop as already-seen.
    //
    // The cursor lives on a tokio watch so the inbound branch can
    // bump it from the hydrate handler without contending with the
    // pump's read path.
    let (cursor_tx, cursor_rx) = tokio::sync::watch::channel::<u64>(0);
    let mut stamped_rx = state.seq_broadcast.subscribe();
    let out_tx_for_pump = out_tx.clone();
    let pump = tokio::spawn(async move {
        loop {
            match stamped_rx.recv().await {
                Ok(stamped) => {
                    if stamped.seq <= *cursor_rx.borrow() {
                        // Already covered by a hydration reply; skip.
                        continue;
                    }
                    let payload = match serde_json::to_string(&EventEnvelope {
                        r#type: "event",
                        seq: stamped.seq,
                        event: &stamped.event,
                    }) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if out_tx_for_pump.send(payload).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    crate::log::warn(&format!("local-ui: pump lagged {n} stamped event(s)"));
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Inbound: parse frames and dispatch.
    while let Some(msg) = ws_receiver.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break,
        };
        match msg {
            Message::Text(text) => {
                handle_text(&text, &state, &out_tx, &cursor_tx).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    pump.abort();
    drop(out_tx);
    let _ = writer.await;
}

async fn handle_text(
    text: &str,
    state: &AppState,
    out_tx: &mpsc::Sender<String>,
    cursor_tx: &tokio::sync::watch::Sender<u64>,
) {
    if let Ok(ctrl) = serde_json::from_str::<ControlFrame>(text) {
        match ctrl {
            ControlFrame::Hydrate { id } => {
                let snapshot = {
                    let h = state.hydration.lock().await;
                    let mut events: Vec<StampedEvent> = Vec::new();
                    if let Some(rs) = &h.run_started {
                        events.push(rs.clone());
                    }
                    events.extend(h.events.iter().cloned());
                    HydrationReply {
                        last_seq: h.last_seq,
                        events,
                        lagged: h.lagged,
                    }
                };

                // Upload-queue snapshot. Replays the current DB state
                // as synthetic events stamped at seq 0 so a tab that
                // refreshes mid-session sees the parked / pending
                // uploads immediately. seq 0 is below `since_seq`,
                // so the live pump can't collide. The client reducer
                // is idempotent on `run_upload_*` events keyed by
                // `queue_id`, so re-applying these on a refresh is
                // safe.
                let upload_events = crate::commands::run::queue::snapshot_events();

                // Advance this connection's pump cursor BEFORE sending
                // the hydration reply, so any frame in-flight on the
                // stamped broadcast that already lives in the
                // snapshot is silently dropped by the pump instead of
                // landing as a duplicate after the reply.
                let _ = cursor_tx.send(snapshot.last_seq);

                let mut envelopes: Vec<EventEnvelope> = upload_events
                    .iter()
                    .map(|e| EventEnvelope {
                        r#type: "event",
                        seq: 0,
                        event: e,
                    })
                    .collect();
                envelopes.extend(snapshot.events.iter().map(|e| EventEnvelope {
                    r#type: "event",
                    seq: e.seq,
                    event: &e.event,
                }));

                let response = HydrationResponse {
                    r#type: "hydration",
                    id,
                    since_seq: snapshot.last_seq,
                    events: envelopes,
                    partial: snapshot.lagged,
                };
                if let Ok(payload) = serde_json::to_string(&response) {
                    let _ = out_tx.send(payload).await;
                }
            }
        }
        return;
    }
    if let Ok(cmd) = serde_json::from_str::<StationCommand>(text) {
        // Routing matrix:
        //   * `UiResponse`            → active run's `ui_response_tx`
        //   * `Stop` / `Kill`         → active run's `cancel_token`
        //   * everything else         → station mode's command sink
        //                               (or dropped in standalone mode,
        //                               where station-level commands
        //                               like `Run` / `Exit` have nothing
        //                               to attach to).
        match &cmd {
            StationCommand::UiResponse { .. } => {
                let tx = state.ui_response_tx.lock().await.clone();
                let _ = tx.send(cmd).await;
            }
            StationCommand::Stop { .. } => {
                state.cancel_token.lock().await.cancel();
            }
            StationCommand::Kill { .. } => {
                state.cancel_token.lock().await.kill();
            }
            _ => {
                let station_sink = state.station_cmd_tx.lock().await.clone();
                if let Some(tx) = station_sink {
                    let _ = tx.send(cmd).await;
                }
                // Standalone `run --kiosk`: silently drop. There's no
                // station_cmd consumer; the previous fallback to
                // `ui_cmd_tx` only worked because the bridge translated
                // Stop/Kill there, which we've now removed.
            }
        }
        return;
    }
    // `{text:?}` (Debug) so embedded newlines / ANSI escape sequences
    // can't forge log lines or scribble on the operator's terminal.
    // The frame body is attacker-controlled (any local process with
    // `Origin: http://127.0.0.1:<port>` can connect).
    crate::log::warn(&format!("local-ui: dropped unparseable WS frame: {text:?}"));
}

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlFrame {
    Hydrate {
        /// Optional client-assigned correlation id. Echoed back on
        /// the hydration reply so concurrent in-flight requests
        /// pair to their resolvers without ambiguity.
        #[serde(default)]
        id: Option<String>,
    },
}

#[derive(serde::Serialize)]
struct EventEnvelope<'a> {
    r#type: &'a str,
    seq: u64,
    event: &'a StationEvent,
}

#[derive(serde::Serialize)]
struct HydrationResponse<'a> {
    r#type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    since_seq: u64,
    events: Vec<EventEnvelope<'a>>,
    /// `true` when the pump task hit a broadcast lag and cleared its
    /// ring. SPA should treat this hydrate as a partial replay — keep
    /// existing live state, do NOT fall back to idle if `events` is
    /// empty. Cleared automatically by the next `RunStarted`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    partial: bool,
}

struct HydrationReply {
    last_seq: u64,
    events: Vec<StampedEvent>,
    lagged: bool,
}

// ---------------------------------------------------------------------------
// Static handler (embedded SPA + dev-dir override)
// ---------------------------------------------------------------------------

async fn static_handler(State(state): State<AppState>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(dev_dir) = state.dev_dir.as_ref() {
        // Dev override: serve from disk so SPA iteration is just
        // `pnpm dev` / `pnpm build` away — no `cargo build`. Falls
        // through to the embedded SPA on miss so partial Vite
        // outputs still work.
        if let Some(resp) = read_dev_file(dev_dir, path).await {
            return resp;
        }
        if path != "index.html" {
            if let Some(resp) = read_dev_file(dev_dir, "index.html").await {
                crate::log::warn(&format!(
                    "local-ui: dev-dir miss for {path:?}; falling back to index.html. \
                     Did the Vite build emit this asset? ({})",
                    dev_dir.display()
                ));
                return resp;
            }
        }
    }

    if let Some(file) = SPA_DIST.get_file(path) {
        return file_response(path, file.contents());
    }
    if let Some(file) = SPA_DIST.get_file("index.html") {
        // SPA-route fallback (history-mode deep links). Only warn for
        // requests whose *trailing segment* has a known asset
        // extension. Bare deep links like `/runs/run.123abc` or
        // `/units/SN-1.2.3` contain dots in the slug but aren't
        // asset misses. Dedupe by path so a misconfigured SPA fetching
        // five missing chunks per page load doesn't spam the log.
        if looks_like_asset(path) && path != "index.html" {
            log_asset_miss_once(path);
        }
        return file_response("index.html", file.contents());
    }
    // Downgraded from `error` to `warn`: the boot-time SPA inventory
    // log already announced the empty bundle as a warning, and this
    // fires on every page load. `error` would double-log the same
    // fault and bury the more useful boot warning.
    crate::log::warn(&format!(
        "local-ui: 503 — no embedded SPA and no dev-dir match for {path:?}. \
         Operator UI is rendering the placeholder page; rebuild operator-ui."
    ));
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        PLACEHOLDER_HTML,
    )
        .into_response()
}

async fn read_dev_file(dev_dir: &std::path::Path, path: &str) -> Option<Response> {
    // Defensive path resolution: clamp to the dev dir so a request
    // for `..%2Fetc%2Fpasswd` can't escape. Component-walk then join
    // back, dropping anything that climbs above the root.
    let safe: PathBuf = std::path::Path::new(path)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    let full = dev_dir.join(safe);
    let bytes = tokio::fs::read(&full).await.ok()?;
    let mime = mime_guess::from_path(&full).first_or_octet_stream();
    let mut resp = (StatusCode::OK, bytes).into_response();
    if let Ok(value) = HeaderValue::from_str(mime.as_ref()) {
        resp.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    Some(resp)
}

fn file_response(path: &str, bytes: &'static [u8]) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut resp = (StatusCode::OK, bytes).into_response();
    if let Ok(value) = HeaderValue::from_str(mime.as_ref()) {
        resp.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    resp
}

/// Extension whitelist for `/files/*`. The route exists solely to
/// resolve UI component image references; clamping to image types
/// keeps the rest of the procedure dir (source, venv, dotfiles) off
/// the HTTP surface. SVG is deliberately excluded: it is served
/// same-origin as the SPA and can carry inline script, so opening a
/// bundle-authored `.svg` directly would run it in the SPA origin.
/// Operator reference images are raster in practice.
fn is_image_path(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif" | "ico")
    )
}

/// Serve an image from the attached run's procedure directory. The
/// kiosk SPA resolves relative component image paths (radio/checklist
/// option `image`, image component value) to `/files/<rel>` URLs — the
/// same strings the TUI's `ImageCache` resolves against the same root.
/// 404 on everything else: no run attached, non-image extension, or a
/// path that escapes the root.
async fn files_handler(
    State(state): State<AppState>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Response {
    let Some(root) = state.procedure_dir.lock().await.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Same clamp as `read_dev_file`: keep only Normal components so a
    // `..%2F` escape collapses back inside the root.
    let safe: PathBuf = std::path::Path::new(&path)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    if !is_image_path(&safe) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let full = root.join(safe);
    // The component clamp stops lexical traversal, but `tokio::fs::read`
    // follows symlinks — a `foo.png` symlink inside the procedure dir
    // pointing at an out-of-tree file would otherwise be served.
    // Canonicalize both sides and require the resolved target to stay
    // under the resolved root. (canonicalize also fails for a missing
    // file, collapsing the not-found case into the same 404.)
    let (Ok(canon_root), Ok(canon_full)) = (
        tokio::fs::canonicalize(&root).await,
        tokio::fs::canonicalize(&full).await,
    ) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !canon_full.starts_with(&canon_root) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(bytes) = tokio::fs::read(&canon_full).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = mime_guess::from_path(&full).first_or_octet_stream();
    let mut resp = (StatusCode::OK, bytes).into_response();
    if let Ok(value) = HeaderValue::from_str(mime.as_ref()) {
        resp.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    resp
}

const PLACEHOLDER_HTML: &str = include_str!("placeholder.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_extension_whitelist() {
        assert!(is_image_path(std::path::Path::new("a/b.PNG")));
        assert!(is_image_path(std::path::Path::new("b.webp")));
        // SVG excluded: same-origin + scriptable.
        assert!(!is_image_path(std::path::Path::new("b.svg")));
        assert!(!is_image_path(std::path::Path::new(".env")));
        assert!(!is_image_path(std::path::Path::new("main.py")));
        assert!(!is_image_path(std::path::Path::new("noext")));
    }

    #[test]
    fn traversal_components_collapse_inside_root() {
        // Mirrors the clamp in `files_handler`: only Normal components
        // survive, so `..`-escapes resolve inside the root.
        let clamp = |p: &str| -> PathBuf {
            std::path::Path::new(p)
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(s) => Some(s),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(clamp("../../etc/passwd"), PathBuf::from("etc/passwd"));
        assert_eq!(clamp("images/../a.png"), PathBuf::from("images/a.png"));
        assert_eq!(clamp("/abs/a.png"), PathBuf::from("abs/a.png"));
    }
}
