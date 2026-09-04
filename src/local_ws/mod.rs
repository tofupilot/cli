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
// Loopback bind. Two ways onto the WS:
//   * Origin header allow-listed to the server's own host:port
//     (kiosk tabs), or
//   * a valid `?token=` carrying the per-process session token
//     (dashboard Studio pages on a foreign Origin), honored only
//     while the studio surface is enabled.
//
// Threat model: localhost-only bind + Origin allow-list defends
// against (a) cross-Origin browser CSRF from a hostile page on a
// different local port, (b) curl/python clients without an Origin
// header. The token path is the stronger credential: possession is
// full kiosk-equivalent access (StationCommand frames — run control,
// UI responses) AND, via `/studio/rpc`, scoped file read/write under
// the studio root. The token only leaves the process in the URL
// `tofupilot studio` prints. Neither path defends against (a) other
// local processes that can craft Origin headers (any process with
// `tofupilot` already gets full access on this machine — same posture
// as the rest of the CLI), or (b) malicious browser extensions, which
// can rewrite headers via webRequest. LAN-mode (binding to 0.0.0.0)
// stays unexposed; if it ever lands it must require the token path
// exclusively.

pub mod studio;

mod plug_debug;

#[cfg(test)]
mod e2e_tests;

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

/// Grace period between the graceful cancel and the forced kill when a
/// foreground run is closed from the operator UI. Mirrors the station
/// daemon's own `Exit` ladder so the two hosts behave the same.
const EXIT_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(3);

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

/// Which host this server belongs to. `Local` is a foreground
/// `tofupilot run --kiosk` — one process, one run, no supervisor.
/// `Station` is the long-lived daemon that outlives its runs.
///
/// Threaded from `Server::start` rather than defaulted, because every
/// consumer of it (root-bind policy, the SPA's exit confirmation copy)
/// is wrong in a way nobody notices if the value doesn't match the
/// actual host. Serializes to the `"local"` / `"station"` strings the
/// SPA's `HelloPayload.mode` union expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostMode {
    Local,
    Station,
}

impl HostMode {
    /// Whether this host may bind the local-UI listener as root. The
    /// danger is not root itself — it's binding the *station command*
    /// channel (Run, Exit, Reboot, ...) unauthenticated on loopback,
    /// where any local user reaching 127.0.0.1 could drive root. Only
    /// the station daemon installs that sink (`set_station_cmd_sink`);
    /// a foreground `run --kiosk` leaves it `None`, so its station
    /// commands are dropped and the residual surface is just the
    /// current run's UiResponse/Stop/Kill/Exit — the same posture as
    /// the rest of the CLI (any local `tofupilot` process already has
    /// full access). This unblocks the legitimate headless-root +
    /// SSH-forward operator workflow.
    ///
    /// Derived from the mode rather than passed alongside it: the two
    /// can then never disagree.
    fn allows_root_bind(self) -> bool {
        matches!(self, HostMode::Local)
    }
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
    /// Host model, set at `Server::start` and re-asserted by each
    /// `attach_run`. Used by the SPA to gate UI affordances that
    /// depend on it — notably the Close-CLI confirmation copy, which
    /// promises different things for a supervised daemon and for a
    /// foreground run.
    mode: HostMode,
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
    /// Feature capabilities of this server, advertised so remote hosts
    /// (dashboard Studio) can gate UI on what the daemon supports
    /// instead of hanging on unanswered requests. Empty on kiosk-only
    /// servers; `enable_studio` appends `"studio-rpc-v1"`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    capabilities: Vec<String>,
}

impl HelloPayload {
    /// The one identity→payload fan-out, shared by `Server::start` and
    /// `set_identity` — same promise as the `From<&WhoamiCache>` impl
    /// below: a single mapping, so a new identity field can't be wired
    /// into one site and missed in the other. Note the
    /// `station_id → analytics_station_id` rename: the payload's
    /// top-level `station_id` carries the installation id and is NOT
    /// touched here.
    fn apply_identity(&mut self, identity: HelloIdentity) {
        self.auth_type = identity.auth_type;
        self.organization_slug = identity.organization_slug;
        self.organization_name = identity.organization_name;
        self.analytics_station_id = identity.station_id;
        self.user_id = identity.user_id;
        self.user_email = identity.user_email;
        self.user_name = identity.user_name;
    }
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

/// Single mapping from the whoami cache — station mode, `run --kiosk`,
/// and `tofupilot studio` all build their hello identity through this,
/// so a new identity field can't be wired into some hosts and missed
/// in others.
impl From<&crate::commands::db::WhoamiCache> for HelloIdentity {
    fn from(w: &crate::commands::db::WhoamiCache) -> Self {
        HelloIdentity {
            auth_type: Some(w.auth_type.clone()),
            organization_slug: Some(w.organization_slug.clone()),
            organization_name: Some(w.organization_name.clone()),
            station_id: w.station_id.clone(),
            user_id: w.user_id.clone(),
            user_email: w.user_email.clone(),
            user_name: w.user_name.clone(),
        }
    }
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
    /// Swapped per-run via `attach_run`; `None` between runs, where
    /// `/files/*` falls back to the studio root (studio sessions) or
    /// 404s (every other daemon).
    procedure_dir: Arc<Mutex<Option<PathBuf>>>,
    /// Root the `/attachments/*` route serves from: the directory the
    /// engine writes run attachments to (the report dir). Unlike
    /// `procedure_dir`, this isn't known at `attach_run` time — the
    /// report dir is created inside the engine during the run — so it's
    /// learned lazily from the first `AttachmentAdded` event's absolute
    /// `path` (its parent dir). Lets the kiosk render `attach.data`
    /// images locally; the remote dashboard uses `/api/attachments/:id`
    /// instead. Cleared on `attach_run` so a stale run's dir can't leak.
    attachment_dir: Arc<Mutex<Option<PathBuf>>>,
    /// Per-process session token. Generated once at `Server::start`;
    /// never logged. Two consumers: the `/studio/rpc` bearer check and
    /// the `/ws?token=` upgrade path that lets a token-bearing page on
    /// a non-allow-listed Origin (the dashboard Studio route) connect.
    /// Possession is NOT read-only: a token-authenticated WS client is
    /// kiosk-equivalent (can send StationCommand frames — run control,
    /// UI responses), and the same token unlocks scoped file writes on
    /// `/studio/rpc`. It only ever leaves the process via the URL
    /// printed to the operator's terminal by `tofupilot studio`.
    session_token: Arc<String>,
    /// Studio surface configuration. `None` (surface off, requests
    /// 403) unless `tofupilot studio` enabled it with a project root.
    studio: Arc<Mutex<Option<studio::StudioConfig>>>,
    /// Set while the studio dispatcher has a run in flight. Only the
    /// dispatcher knows: `procedure_dir` stays populated after a run
    /// ends, so it cannot answer "is one running now".
    studio_run_active: Arc<std::sync::atomic::AtomicBool>,
    /// Generation stamp for `studio_run_active`. Bumped every time the
    /// dispatcher pins the flag for a NEW run; each event pump captures
    /// the generation at attach time and only CLEARS the flag if it
    /// still owns the current one. Without it, "Run again" defeated the
    /// eager pin: the cancelled prior run's terminal event (or its
    /// channel closing) cleared the flag while the replacement run was
    /// still provisioning its venv or parked at the identify prompt.
    studio_run_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Where `pick_project` sends its folder-dialog jobs. The dialog
    /// must run on the process's MAIN thread (AppKit requirement — rfd
    /// panics anywhere else in a non-windowed process), and axum
    /// handlers run on tokio workers, so the handler posts a job here
    /// and `tofupilot studio`'s foreground loop — the code `block_on`
    /// polls on the main thread — shows the dialog and answers on the
    /// job's oneshot. `None` on every other daemon: the op reports
    /// itself unavailable instead of hanging.
    studio_dialog_tx: Arc<Mutex<Option<mpsc::Sender<StudioDialogJob>>>>,
    /// A dialog choice that holds no procedure, parked instead of
    /// granted: almost always a mis-click, and a grant is permanent
    /// full read/write. `ConfirmPick` grants it (single-use);
    /// `DiscardPick` drops it (the human said no); and EVERY new
    /// `PickProject` clears it on entry, so a stale offer never
    /// survives the dialog interaction that created it. The path only
    /// ever comes from the native dialog — never from the browser —
    /// which is what keeps the dialog the one door to a new root.
    studio_pending_pick: Arc<Mutex<Option<PathBuf>>>,
    /// True while a native folder dialog is on screen. Set by
    /// `pick_project` (compare-exchange, so only one wins) and cleared
    /// by the JOB's Drop on the host side — see `StudioDialogJob`.
    studio_dialog_open: Arc<std::sync::atomic::AtomicBool>,
    /// Out-of-run plug debug sessions (Studio's plug debugger). Torn
    /// down by run start, project/procedure switch, and daemon exit.
    plug_debug: Arc<plug_debug::PlugDebugState>,
}

/// One folder-dialog request: the reply channel `pick_project` waits
/// on (`None` = the human dismissed the dialog), plus the one-dialog
/// gate. The gate travels WITH the job and releases in `Drop`, so it
/// opens exactly when the host loop is done with the dialog — never
/// earlier. The failure this shape exists for: the page's 120s give-up
/// aborts the request future, and a gate held by that future would
/// release while the native panel is still on screen, letting a second
/// request queue behind a window the human still has to answer (and
/// the first human choice would land on a dropped receiver).
pub struct StudioDialogJob {
    reply: Option<tokio::sync::oneshot::Sender<Option<PathBuf>>>,
    open_flag: Arc<std::sync::atomic::AtomicBool>,
}

impl StudioDialogJob {
    pub(crate) fn new(
        reply: tokio::sync::oneshot::Sender<Option<PathBuf>>,
        open_flag: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            reply: Some(reply),
            open_flag,
        }
    }

    /// Deliver the human's choice. Consumes the job; the gate releases
    /// in the ensuing Drop — i.e. after the dialog closed.
    pub fn send(mut self, choice: Option<PathBuf>) -> Result<(), Option<PathBuf>> {
        match self.reply.take() {
            Some(tx) => tx.send(choice),
            None => Err(choice),
        }
    }
}

impl Drop for StudioDialogJob {
    fn drop(&mut self) {
        self.open_flag
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

/// How the server picks its port.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PortChoice {
    /// Stable well-known port (kiosk / station daemon). The bind
    /// doubles as the single-instance gate: EADDRINUSE is fatal and
    /// the error guidance tells the operator to stop the daemon.
    Stable,
    /// OS-assigned ephemeral port (`tofupilot studio` sessions). Never
    /// collides with a running daemon or a second studio session; the
    /// pairing URL carries the actual port explicitly.
    Ephemeral,
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

/// Operator-facing explanation for a failed loopback bind.
///
/// Pure, and `windows` is a parameter rather than a `cfg!` read inside,
/// so the Windows wording is exercised by tests from any host.
fn bind_error_hint(e: &std::io::Error, port: u16, stable_port: bool, windows: bool) -> String {
    let raw = e.raw_os_error();
    // Arms reachable on BOTH paths must not print a port number: the
    // ephemeral path binds 0, so `127.0.0.1:0` names something that does not
    // exist. The arms below that are guarded on `stable_port` keep using
    // `{port}` directly, since they can only be reached with a real one.
    let addr = if stable_port {
        format!("127.0.0.1:{port}")
    } else {
        "an ephemeral port on 127.0.0.1".to_string()
    };
    match e.kind() {
        std::io::ErrorKind::AddrInUse if stable_port => format!(
            "Port {port} on 127.0.0.1 is already in use. \
             Another tofupilot daemon is likely running on this host. \
             Stop it with `tofupilot service stop` (or \
             `systemctl --user stop tofupilot` on Linux), \
             or run `tofupilot service status` to see what's holding the port."
        ),
        // `PortChoice` has two variants and the guarded arm above absorbs
        // every stable-port collision, so this is the ephemeral path: `port`
        // is always 0 and TOFUPILOT_LOCAL_UI_PORT is ignored. The inherited
        // wording named a port that does not exist and prescribed unsetting a
        // variable nothing reads. Rare but reachable: the kernel returns
        // EADDRINUSE on a port-0 bind when the local ephemeral range is
        // exhausted.
        std::io::ErrorKind::AddrInUse => format!(
            "Cannot allocate an ephemeral port on 127.0.0.1 \
             (raw_os_error={raw:?}). The kernel reports the port as already in \
             use, which on this path means the local ephemeral range is \
             exhausted rather than a port of ours being taken. There is no \
             port to change here, TOFUPILOT_LOCAL_UI_PORT is ignored. Close \
             some sockets or wait for TIME_WAIT entries to drain, then retry."
        ),
        // The ephemeral path binds port 0 and deliberately ignores
        // TOFUPILOT_LOCAL_UI_PORT (see the PortChoice::Ephemeral comment at
        // the bind site), so nothing here may tell the caller to pick a port:
        // the kernel picked it, and the override would not be read. Matched
        // before the rest because both the privileged-port rule (0 < 1024)
        // and the Windows reservation story would otherwise hand out advice
        // that cannot be followed.
        std::io::ErrorKind::PermissionDenied if !stable_port => format!(
            "Permission denied binding an ephemeral port on 127.0.0.1 \
             (raw_os_error={raw:?}). The port was kernel-assigned, so this is \
             not about the port number, and TOFUPILOT_LOCAL_UI_PORT is ignored \
             on this path. Something is refusing loopback binds outright: a \
             sandbox, a seccomp / LSM policy or a container on Unix, a host \
             firewall or endpoint-protection product on Windows."
        ),
        // Windows is matched next, for any port. It has no privileged-port
        // range at all, so the Unix "below 1024 needs elevation" advice is
        // wrong there whatever the port — and putting the port test first
        // made it win on Windows port 80, where the cause is http.sys / IIS
        // or a reservation. Stating that rule for every refused bind is what
        // sent a customer hunting for elevation on 7321, and the raw OS error
        // that would have identified the real cause was not printed either.
        std::io::ErrorKind::PermissionDenied if windows => format!(
            "Permission denied binding 127.0.0.1:{port} (raw_os_error={raw:?}). \
             Windows has no privileged-port range, so this is not about \
             running as administrator. A refused bind there is usually a \
             reserved port range — Hyper-V, WinNAT, WSL2 and Docker reserve \
             blocks of TCP ports and any bind inside one fails this way, \
             elevated or not — or, on a well-known port, an http.sys or IIS \
             registration. List the reservations with \
             `netsh interface ipv4 show excludedportrange protocol=tcp`, then \
             pick a port outside every listed range via TOFUPILOT_LOCAL_UI_PORT. \
             Reservations taken at runtime are often gone after a reboot."
        ),
        std::io::ErrorKind::PermissionDenied if port < 1024 => format!(
            "Permission denied binding 127.0.0.1:{port} (raw_os_error={raw:?}). \
             Ports below 1024 require elevated privileges; pick a higher port \
             via TOFUPILOT_LOCAL_UI_PORT."
        ),
        std::io::ErrorKind::PermissionDenied => format!(
            "Permission denied binding 127.0.0.1:{port} (raw_os_error={raw:?}). \
             Port {port} is above the privileged range, so this is not about \
             elevation: a sandbox, a seccomp / LSM policy or a container \
             restriction is refusing the bind. Pick a different port via \
             TOFUPILOT_LOCAL_UI_PORT if that is an option."
        ),
        std::io::ErrorKind::AddrNotAvailable => format!(
            "Cannot bind {addr}: loopback address \
             unavailable. Check that the loopback interface (lo / lo0) \
             is up: `ifconfig lo0` (macOS) or `ip addr show lo` (Linux). \
             Hardened images may disable loopback for non-root users; \
             containers without `--network host` and net-namespaced \
             environments can also surface this."
        ),
        // Last arm that still offered a port to change on the ephemeral path.
        // The invariant is path-wide, not per failure mode.
        std::io::ErrorKind::WouldBlock if !stable_port => format!(
            "Cannot bind an ephemeral port on 127.0.0.1: kernel returned \
             EWOULDBLOCK (raw_os_error={raw:?}). Likely SO_REUSEADDR \
             contention with a socket in TIME_WAIT — wait ~60s and retry. \
             There is no port to change here, TOFUPILOT_LOCAL_UI_PORT is \
             ignored on this path."
        ),
        std::io::ErrorKind::WouldBlock => format!(
            "Cannot bind 127.0.0.1:{port}: kernel returned \
             EWOULDBLOCK. Likely SO_REUSEADDR contention with a \
             socket in TIME_WAIT — wait ~60s and retry, or pick a \
             different port via TOFUPILOT_LOCAL_UI_PORT."
        ),
        // `Uncategorized` (Linux EPERM via seccomp / LSM) and `Other` end up
        // here. Surface the raw OS error code so a sysadmin reading the log
        // has something to grep for.
        kind => format!(
            "local-ui: bind {addr} failed: {e} \
             (kind={kind:?}, raw_os_error={raw:?}). \
             Run `tofupilot service status` for diagnostics."
        ),
    }
}

impl Server {
    /// Bind the listener and spawn the axum task. Returns once the
    /// listener is live; the SPA is reachable at `boot_url()`.
    pub async fn start(
        station_id: String,
        station_name: String,
        identity: HelloIdentity,
        // Which host is starting this server. Drives both the root-bind
        // policy (see `HostMode::allows_root_bind`) and the `mode` the
        // hello frame advertises to the SPA. Previously the root policy
        // was a separate `allow_root: bool` and the hello frame was
        // hardcoded to `"station"` here, corrected later by
        // `attach_run` — so a foreground run served `"station"` to any
        // tab that connected before its run attached.
        mode: HostMode,
        port_choice: PortChoice,
    ) -> std::io::Result<Self> {
        if !mode.allows_root_bind() && crate::commands::config::is_root_system() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "local operator UI is disabled for the root station service \
                 (unauthenticated loopback command channel); control the \
                 station from the dashboard instead",
            ));
        }

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
        let preferred_port: u16 = match port_choice {
            PortChoice::Stable => env::var("TOFUPILOT_LOCAL_UI_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(crate::commands::service::DEFAULT_LOCAL_PORT),
            // Port 0 = kernel-assigned ephemeral. Coexists with a
            // running daemon on the stable port and allows several
            // studio sessions side by side. The env override is
            // deliberately ignored here: honoring it would let a
            // studio session squat the daemon's fixed port and take
            // the station down on its next restart.
            PortChoice::Ephemeral => 0,
        };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", preferred_port))
            .await
            .inspect_err(|e| {
                crate::log::error(&bind_error_hint(
                    e,
                    preferred_port,
                    port_choice == PortChoice::Stable,
                    cfg!(target_os = "windows"),
                ));
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

        let mut hello_payload = HelloPayload {
            kind: "hello",
            station_id,
            station_name,
            procedures: Vec::new(),
            mode,
            auth_type: None,
            organization_slug: None,
            organization_name: None,
            analytics_station_id: None,
            user_id: None,
            user_email: None,
            user_name: None,
            capabilities: Vec::new(),
        };
        hello_payload.apply_identity(identity);
        let hello = Arc::new(Mutex::new(hello_payload));

        // Random session token (UUIDv4 → 122 random bits), hex-encoded.
        // Generated even when studio never gets enabled — a token that
        // gates nothing is harmless, and generating unconditionally
        // keeps `enable_studio` free of state ordering.
        let session_token = Arc::new(uuid::Uuid::new_v4().simple().to_string());

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
            attachment_dir: Arc::new(Mutex::new(None)),
            session_token,
            studio: Arc::new(Mutex::new(None)),
            studio_run_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            studio_run_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            studio_dialog_tx: Arc::new(Mutex::new(None)),
            studio_pending_pick: Arc::new(Mutex::new(None)),
            studio_dialog_open: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            plug_debug: Arc::new(plug_debug::PlugDebugState::new()),
        };

        /// Origins allowed to call `/studio/rpc`: the dashboard this
        /// CLI is pointed at, plus the dev dashboard on :3000.
        fn studio_allowed_origins() -> Vec<axum::http::HeaderValue> {
            /// Reduce a base URL to its `scheme://host[:port]` origin.
            /// The Origin header never carries a path, so a configured
            /// base that has one would otherwise never match.
            fn origin_of(base: &str) -> Option<String> {
                let (scheme, rest) = base.split_once("://")?;
                let host = rest.split('/').next()?;
                (!host.is_empty()).then(|| format!("{scheme}://{host}"))
            }
            fn push(origins: &mut Vec<axum::http::HeaderValue>, value: &str) {
                if let Ok(header) = axum::http::HeaderValue::from_str(value) {
                    if !origins.contains(&header) {
                        origins.push(header);
                    }
                }
            }

            let mut origins: Vec<axum::http::HeaderValue> = Vec::new();
            // The dashboard the operator is logged into, resolved the
            // same way `tofupilot studio` builds the pairing URL
            // (see commands/studio.rs) so the printed link and the
            // allow-list cannot disagree. `base()` already strips
            // trailing slashes.
            if let Some(credentials) = crate::commands::auth::credentials::load() {
                if let Some(origin) = origin_of(credentials.base()) {
                    push(&mut origins, &origin);
                }
            }
            // The default dashboard, so a session started before login
            // still pairs against the production host.
            if let Some(origin) = origin_of(crate::commands::auth::config::DEFAULT_BASE_URL) {
                push(&mut origins, &origin);
            }
            // Dev dashboards: `pnpm dev` serves the web app on :3000.
            // Unconditional so a release binary pointed at a local
            // dashboard pairs without extra configuration.
            push(&mut origins, "http://localhost:3000");
            push(&mut origins, "http://127.0.0.1:3000");
            // Escape hatch for preview deployments (comma-separated).
            if let Ok(extra) = std::env::var("TOFUPILOT_STUDIO_ALLOWED_ORIGINS") {
                for candidate in extra.split(',') {
                    if let Some(origin) = origin_of(candidate.trim()) {
                        push(&mut origins, &origin);
                    }
                }
            }
            origins
        }

        // CORS for the studio RPC route only. The dashboard page runs
        // on a different origin (the configured dashboard base, or
        // localhost:3000 in dev) and fetches this loopback endpoint
        // directly, so the browser needs CORS + (Chrome) Private
        // Network Access approval on the preflight.
        //
        // The origin is ALLOW-LISTED, not `Any`. The bearer session
        // token is still the real boundary for a local caller, but
        // 127.0.0.1 is only reachable through the operator's own
        // browser: with `Any`, a token that leaks (URL pasted into a
        // chat, a screenshot, shell history) becomes remotely
        // exploitable file read/write from any page the operator
        // visits. Pinning the origin removes that class outright — a
        // hostile page cannot even send the request.
        let studio_cors = tower_http::cors::CorsLayer::new()
            .allow_origin(studio_allowed_origins())
            .allow_methods([axum::http::Method::POST, axum::http::Method::OPTIONS])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
            .allow_private_network(true);

        let app = Router::new()
            .route("/ws", get(ws_handler))
            .route("/files/*path", get(files_handler))
            .route("/attachments/*path", get(attachments_handler))
            .route(
                "/studio/rpc",
                axum::routing::post(studio::rpc_handler).layer(studio_cors),
            )
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

    /// Per-process session token. Print-once secret for `tofupilot
    /// studio`; never log it.
    pub fn session_token(&self) -> &str {
        &self.state.session_token
    }

    /// Bound loopback port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Turn on the Studio RPC surface for `root` and advertise the
    /// capability in the hello frame. Only `tofupilot studio` calls
    /// this; every other process keeps the surface off (all
    /// `/studio/rpc` requests 403). The hello capability is advisory
    /// today (the Studio page detects support via the RPC probe and
    /// the typed `Unsupported` error); it exists so remote hosts can
    /// gate UI without a probe once capability sets grow.
    pub async fn enable_studio(&self, root: PathBuf) -> std::io::Result<()> {
        self.enable_studio_with_recents(root, crate::commands::studio_recents::recents_path())
            .await
    }

    /// `enable_studio`, with the session's recents file named
    /// explicitly. Exists for the tests: the default location is under
    /// the real `~/.tofupilot`, and a test that switches projects would
    /// otherwise reshuffle the developer's own recents list.
    pub async fn enable_studio_with_recents(
        &self,
        root: PathBuf,
        recents_file: PathBuf,
    ) -> std::io::Result<()> {
        // Canonicalize once here; every RPC path check builds on the
        // granted roots being canonical (see `GrantedRoot::path`).
        let root = tokio::fs::canonicalize(root).await?;
        *self.state.studio.lock().await =
            Some(studio::StudioConfig::with_recents_file(root, recents_file));
        let mut hello = self.state.hello.lock().await;
        // `partial_run`: the studio dispatcher honors `Run.only_phase`.
        // Not advisory — an older daemon ignores the field and runs the
        // WHOLE procedure against real hardware, so the dashboard must
        // hide the per-phase play button unless this is present.
        // `upload_run`: the dispatcher handles `StationCommand::UploadRun`
        // (explicit post-run upload); without it the command is dropped.
        // `idle_files`: `/files/*` serves the project root between runs.
        // Gated so the dashboard previews keep their neutral image
        // placeholder against an older daemon instead of a 404 error.
        // `plug_debug`: the dispatcher handles the PlugMethods /
        // PlugDebug* requests; without it the page hides the debugger.
        // `phase_executable`: `get_sequence` carries a phase's
        // `executable:` block (command, shell, working directory), not
        // just the flag saying it has one. Without it the dashboard
        // cannot read a command back, so the Inspector shows "update
        // the CLI" in place of the block's fields. Informational only:
        // switching a phase's runtime is NOT gated on it, so a daemon
        // this old can still be handed `command: ""`, which its engine
        // (`min = 1` back then) refuses at load. Accepted — daemons
        // that predate this capability sit below Studio's recommended
        // version and get the update warning, not support.
        for cap in [
            "studio-rpc-v1",
            "partial_run",
            "upload_run",
            "idle_files",
            "plug_debug",
            "phase_executable",
        ] {
            if !hello.capabilities.iter().any(|c| c == cap) {
                hello.capabilities.push(cap.to_string());
            }
        }
        Ok(())
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

    /// Update the hello frame's identity envelope (and, when known, the
    /// displayed station name) on a running server. Tabs already connected
    /// keep the hello they received — there is no re-broadcast — so this
    /// only helps connections opened AFTERWARDS. On the boot that heals an
    /// empty station slot, the auto-launched kiosk tab usually connects
    /// before the auth probe returns and therefore keeps the anonymous
    /// hello (its PostHog identify no-ops) until a reload or the next
    /// boot; the tab that does benefit is the dashboard Web-UI tab the
    /// operator opens from the URL the heal prints. Re-identifying live
    /// tabs would need an identity frame in the protocol plus an SPA
    /// handler — deliberately out of scope for the analytics-only stake
    /// (TP-1040).
    pub async fn set_identity(&self, identity: HelloIdentity, station_name: Option<String>) {
        let mut h = self.state.hello.lock().await;
        h.apply_identity(identity);
        if let Some(name) = station_name {
            h.station_name = name;
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

    /// Directory the studio run dispatcher should execute: the active
    /// procedure's own directory in a multi-procedure project, else the
    /// project root. Read at dispatch time rather than captured at
    /// launch, so a procedure switched in the UI is the one that runs.
    /// `None` when the studio surface is off (no studio session).
    pub async fn studio_run_dir(&self) -> Option<PathBuf> {
        self.state
            .studio
            .lock()
            .await
            .as_ref()
            .map(|config| config.active_procedure_dir())
    }

    /// Install the folder-dialog host for `pick_project`. Only
    /// `tofupilot studio` calls this, from its foreground loop — the
    /// receiving end MUST be polled on the process's main thread,
    /// because that is where the native dialog is legal to open (see
    /// `AppState::studio_dialog_tx`).
    pub async fn set_studio_dialog_host(&self, tx: mpsc::Sender<StudioDialogJob>) {
        *self.state.studio_dialog_tx.lock().await = Some(tx);
    }

    /// Install the sender debug-session plug events flow into: the
    /// studio command loop pumps its receiving end to `publish_event`,
    /// which is how out-of-run `plug_status`/`plug_log` (no
    /// `execution_id`) reach the page.
    pub async fn set_plug_debug_event_sender(
        &self,
        tx: mpsc::UnboundedSender<station_protocol::StationEvent>,
    ) {
        self.state.plug_debug.set_event_sender(tx).await;
    }

    /// Stop every plug debug session. Idempotent; fired by run start,
    /// project/procedure switch, and daemon shutdown.
    pub async fn teardown_plug_debug(&self) {
        self.state.plug_debug.teardown_all().await;
    }

    /// Pin (or release) the run-in-flight flag directly. The run
    /// dispatcher calls this the moment it ACCEPTS a Run command:
    /// `RunStarted` only fires after venv bootstrap, python resolution
    /// and the identify prompt, so the event edge alone leaves that
    /// whole pre-run window unguarded — a project switch there reloads
    /// the page onto project B while the engine boots project A. The
    /// event-pump edges stay as the backstop that eventually clears it.
    pub fn set_studio_run_active(&self, active: bool) {
        if active {
            // A new pin invalidates every older pump's right to clear:
            // see `studio_run_generation`.
            self.state
                .studio_run_generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        self.state
            .studio_run_active
            .store(active, std::sync::atomic::Ordering::Release);
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
        mode: HostMode,
    ) -> RunAttachment {
        // Swap the inbound sinks so frames arriving on existing WS
        // connections route to this run.
        *self.state.ui_response_tx.lock().await = ui_response_tx;
        *self.state.cancel_token.lock().await = cancel_token;
        *self.state.procedure_dir.lock().await = procedure_dir;
        // New run: forget the prior run's attachment dir so a stale path
        // can't serve a finished run's files. Re-latched lazily from the
        // next run's first AttachmentAdded.
        *self.state.attachment_dir.lock().await = None;

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
        let attachment_dir = self.state.attachment_dir.clone();
        let run_active = self.state.studio_run_active.clone();
        // This pump may only CLEAR the flag while it owns the current
        // generation: a "Run again" pins a new generation before
        // cancelling this run, and this run's terminal event must not
        // unpin the replacement's bootstrap window.
        let run_generation = self.state.studio_run_generation.clone();
        let pump_generation = run_generation.load(Ordering::Acquire);
        let owns_pin = move |generation: &std::sync::atomic::AtomicU64| {
            generation.load(Ordering::Acquire) == pump_generation
        };
        let mut rx = event_tx.subscribe();
        let pump_handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        // The run's own lifecycle is the only honest
                        // source for "is one in flight": the studio
                        // dispatcher keeps its `RunHandle` after the run
                        // ends, and `procedure_dir` is never cleared, so
                        // neither can answer it. Read by `open_project`.
                        match &event {
                            StationEvent::RunStarted { .. } => {
                                run_active.store(true, Ordering::Release)
                            }
                            StationEvent::RunComplete { .. } | StationEvent::RunCrashed { .. }
                                if owns_pin(&run_generation) =>
                            {
                                run_active.store(false, Ordering::Release)
                            }
                            _ => {}
                        }
                        // Learn the run's attachment dir from the first
                        // `AttachmentAdded` that carries an absolute path —
                        // its parent is the engine's report dir, the root
                        // `/attachments/*` serves from. The path isn't known
                        // at attach_run time (the engine creates the dir
                        // mid-run), so we latch it here instead.
                        if let StationEvent::AttachmentAdded { path: Some(p), .. } = &event {
                            if let Some(parent) = std::path::Path::new(p).parent() {
                                let mut slot = attachment_dir.lock().await;
                                if slot.is_none() {
                                    *slot = Some(parent.to_path_buf());
                                }
                            }
                        }
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
                        // The dropped frames may include the terminal
                        // `RunComplete`: a lagged pump can no longer
                        // prove a run is live, and a stale `true` turns
                        // every project/procedure switch into a
                        // permanent `Busy` that only a daemon restart
                        // clears. Err toward re-allowing the switch:
                        // the worst case is one under-guarded switch
                        // during a run that survived the lag, against
                        // a stranded session on the other side. Still
                        // generation-gated: an OLD lagged pump has no
                        // say over the replacement run's pin.
                        if owns_pin(&run_generation) {
                            run_active.store(false, Ordering::Release);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Sender gone = the attachment is over; if the
                        // terminal event never arrived (aborted run,
                        // dropped engine), the flag must not outlive
                        // the channel that fed it. Generation-gated:
                        // the cancelled prior run's channel closing is
                        // exactly how "Run again" used to unpin the
                        // replacement's bootstrap window.
                        if owns_pin(&run_generation) {
                            run_active.store(false, Ordering::Release);
                        }
                        break;
                    }
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
    axum::extract::RawQuery(query): axum::extract::RawQuery,
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
    // Token path: a page holding the per-process session token (the
    // dashboard Studio route, which the operator opened via the URL
    // `tofupilot studio` printed) may attach from an Origin outside
    // the loopback allow-list. Possession of the 128-bit token is a
    // stronger credential than the forgeable Origin header; only
    // honored while the studio surface is enabled so kiosk-only
    // processes keep the historic Origin-only posture.
    let token_ok = match extract_ws_token(query.as_deref()) {
        Some(presented) => {
            state.studio.lock().await.is_some()
                && studio::token_matches(&presented, &state.session_token)
        }
        None => false,
    };
    if !origin_ok && !token_ok {
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

/// Where an inbound `StationCommand` is dispatched. Extracted from
/// `handle_text` so the matrix is assertable without standing up an
/// `AppState` — the routing rules are the only real logic in this
/// module's inbound path, and they were previously untested.
#[derive(Debug, PartialEq, Eq)]
enum CommandRoute {
    /// Active run's operator-UI response sink.
    RunUi,
    /// Active run's cancel token, graceful.
    Cancel,
    /// Active run's cancel token, forced.
    Kill,
    /// Station daemon's command loop.
    Station,
    /// Foreground `run --kiosk`: no daemon to exit, so closing the CLI
    /// means ending the one run this process owns. Notably the only way
    /// out before `RunStarted`, where the UI renders no Stop button.
    LocalExit,
    /// Nothing to attach to. Logged, never silent.
    Drop,
}

fn route_command(cmd: &StationCommand, has_station_sink: bool) -> CommandRoute {
    match cmd {
        StationCommand::UiResponse { .. } => CommandRoute::RunUi,
        StationCommand::Stop { .. } => CommandRoute::Cancel,
        StationCommand::Kill { .. } => CommandRoute::Kill,
        StationCommand::Exit {} if !has_station_sink => CommandRoute::LocalExit,
        _ if has_station_sink => CommandRoute::Station,
        _ => CommandRoute::Drop,
    }
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
        let station_sink = state.station_cmd_tx.lock().await.clone();
        match route_command(&cmd, station_sink.is_some()) {
            CommandRoute::RunUi => {
                let tx = state.ui_response_tx.lock().await.clone();
                let _ = tx.send(cmd).await;
            }
            CommandRoute::Cancel => state.cancel_token.lock().await.cancel(),
            CommandRoute::Kill => state.cancel_token.lock().await.kill(),
            CommandRoute::Station => {
                // `is_some()` above proves the sink is there.
                if let Some(tx) = station_sink {
                    let _ = tx.send(cmd).await;
                }
            }
            CommandRoute::LocalExit => {
                // Logged on both rungs, mirroring the station daemon's
                // Exit handler. Without these, an operator reporting
                // "I clicked and nothing happened" is indistinguishable
                // between three very different causes: the frame never
                // arrived, it arrived and the run is ignoring the
                // cancel, or it arrived before `attach_run` and hit the
                // placeholder token. The whole point of this branch is
                // that a control which can't be falsified from the
                // outside stays broken for months.
                crate::log::info(
                    "Operator requested exit; aborting active run and stopping the CLI...",
                );
                state.cancel_token.lock().await.cancel();
                let token = state.cancel_token.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(EXIT_GRACE_PERIOD).await;
                    // Fires unconditionally: the usual case is that the
                    // run already wound down and took the process with
                    // it, so this line is never reached. If it is, the
                    // kill is either a no-op (run already finished, CLI
                    // still flushing uploads) or a real escalation.
                    crate::log::warn(&format!(
                        "Exit grace period ({}s) elapsed; escalating to force-kill. \
                         Any teardown still in progress is abandoned.",
                        EXIT_GRACE_PERIOD.as_secs()
                    ));
                    token.lock().await.kill();
                });
            }
            // Warn instead of dropping in silence: an inert control
            // with no log line is unfalsifiable from the outside,
            // which is how the dead Exit button survived for months.
            CommandRoute::Drop => {
                crate::log::warn(&format!(
                    "local-ui: dropped station command {cmd:?} — no station sink \
                     (foreground `tofupilot run`, not the station daemon)"
                ));
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

/// Pull `token=<value>` out of a raw query string without a full
/// query-parser dependency. Values are the hex token — no percent
/// escapes to worry about; anything else simply fails the match.
fn extract_ws_token(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
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
/// 404 on everything else: no root to serve from, non-image extension,
/// or a path that escapes the root.
async fn files_handler(
    State(state): State<AppState>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Response {
    // Between runs a studio session still resolves against its project
    // root — same directory a studio run serves, since the procedure
    // YAML sits at the root — so the Builder and Sequence previews can
    // show component images without starting a run. The root is already
    // exposed read/write through `/studio/rpc`, so serving images from
    // it widens nothing. Kiosk/station daemons keep the 404.
    let run_dir = state.procedure_dir.lock().await.clone();
    let root = match run_dir {
        Some(dir) => dir,
        None => match state
            .studio
            .lock()
            .await
            .as_ref()
            .map(|s| s.active().to_path_buf())
        {
            Some(root) => root,
            None => return StatusCode::NOT_FOUND.into_response(),
        },
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

/// Serve a run attachment image from the engine's report dir (the root
/// latched from `AttachmentAdded` paths). The kiosk SPA resolves an
/// attachment to `/attachments/<stored_name>` — the basename the engine
/// wrote (`<id8>_<name>`). The remote dashboard can't reach the station
/// disk and uses `/api/attachments/:id` instead; this route is the
/// kiosk's local counterpart, mirroring `/files/*`'s clamps:
/// single-path-component basename only, image-extension whitelist (SVG
/// excluded), and canonicalization confining the target under the root.
/// 404 on everything else (no run, not an image, escape attempt, or the
/// file already removed after upload).
async fn attachments_handler(
    State(state): State<AppState>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Response {
    let Some(root) = state.attachment_dir.lock().await.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Collapse to Normal components, same as files_handler. Attachments
    // are flat (`<id8>_<name>`), so this also strips any path separators
    // an attacker might inject to reach a sibling of the report dir.
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

    fn ui_response() -> StationCommand {
        StationCommand::UiResponse {
            request_id: "r1".into(),
            values: std::collections::HashMap::new(),
        }
    }

    /// Run-scoped commands never reach the station sink, in either
    /// host mode: they belong to the run, not to the process.
    #[test]
    fn run_scoped_commands_ignore_host_mode() {
        for has_sink in [true, false] {
            assert_eq!(route_command(&ui_response(), has_sink), CommandRoute::RunUi);
            assert_eq!(
                route_command(&StationCommand::Stop { reason: None }, has_sink),
                CommandRoute::Cancel
            );
            assert_eq!(
                route_command(&StationCommand::Kill { reason: None }, has_sink),
                CommandRoute::Kill
            );
        }
    }

    /// The regression this module's `Exit` handling exists for: with no
    /// station sink (foreground `tofupilot run`), Exit must end the run
    /// rather than fall through to the drop arm. It was silently dropped
    /// here, leaving "Close CLI" inert for every local run.
    #[test]
    fn exit_ends_the_run_when_there_is_no_station() {
        assert_eq!(
            route_command(&StationCommand::Exit {}, false),
            CommandRoute::LocalExit
        );
    }

    /// With a daemon present, Exit is the daemon's business — it owns
    /// the process lifetime and applies its own teardown ladder.
    #[test]
    fn exit_goes_to_the_daemon_when_one_is_attached() {
        assert_eq!(
            route_command(&StationCommand::Exit {}, true),
            CommandRoute::Station
        );
    }

    /// Station-level commands other than Exit have nothing to attach to
    /// in a foreground run. They must land on the logged Drop arm, not
    /// vanish.
    #[test]
    fn other_station_commands_drop_loudly_without_a_sink() {
        let station_only = [
            StationCommand::Pull {},
            StationCommand::Run {
                procedure_id: None,
                reuse_unit: None,
                reuse_units: None,
                operated_by: None,
                only_phase: None,
            },
            StationCommand::ConfigUpdate {
                key: "kiosk_ui".into(),
                value: "true".into(),
            },
        ];
        for cmd in &station_only {
            assert_eq!(route_command(cmd, false), CommandRoute::Drop);
            assert_eq!(route_command(cmd, true), CommandRoute::Station);
        }
    }

    /// The SPA's exit frame is `{"type":"exit"}`. Pin the wire shape:
    /// a rename on either side would otherwise fail the parse and land
    /// in the "unparseable frame" warning instead of the routing matrix.
    #[test]
    fn exit_frame_deserializes_from_the_spa_payload() {
        let cmd: StationCommand = serde_json::from_str(r#"{"type":"exit"}"#)
            .expect("SPA exit frame must parse as StationCommand");
        assert_eq!(route_command(&cmd, false), CommandRoute::LocalExit);
    }

    /// The SPA types `HelloPayload.mode` as the union
    /// `'local' | 'station'` and branches the Close-CLI confirmation
    /// copy on it. A rename on this side would match neither arm of
    /// that union and silently fall through to the station wording.
    #[test]
    fn host_mode_serializes_to_the_spa_union() {
        assert_eq!(
            serde_json::to_string(&HostMode::Local).unwrap(),
            r#""local""#
        );
        assert_eq!(
            serde_json::to_string(&HostMode::Station).unwrap(),
            r#""station""#
        );
    }

    /// Root-bind policy is derived from the host mode, so the two can
    /// never disagree. Only the daemon exposes the unauthenticated
    /// station-command channel on loopback, so only the daemon is
    /// refused under root.
    #[test]
    fn only_the_foreground_run_may_bind_as_root() {
        assert!(HostMode::Local.allows_root_bind());
        assert!(!HostMode::Station.allows_root_bind());
    }

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

    #[test]
    fn attachment_basename_clamp_and_whitelist() {
        // `/attachments/*` serves the engine's flat `<id8>_<name>` stored
        // files. The kiosk resolver sends a basename; assert the same
        // clamp + image whitelist the handler applies. A normal stored
        // image name survives; an escape attempt collapses inside root and
        // a non-image is rejected.
        let clamp = |p: &str| -> PathBuf {
            std::path::Path::new(p)
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(s) => Some(s),
                    _ => None,
                })
                .collect()
        };
        let stored = clamp("a1b2c3d4_board.png");
        assert_eq!(stored, PathBuf::from("a1b2c3d4_board.png"));
        assert!(is_image_path(&stored));
        // Separators an attacker injects are stripped to a flat path that
        // canonicalization then confines under the report dir.
        assert_eq!(
            clamp("../../secrets/key.png"),
            PathBuf::from("secrets/key.png")
        );
        // Non-image stored file (e.g. a CSV attachment) is not served.
        assert!(!is_image_path(&clamp("a1b2c3d4_data.csv")));
    }

    fn denied() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied")
    }

    /// The privileged-port rule is the *only* case that may mention
    /// elevation. Claiming it on 7321 sent a customer looking for admin
    /// rights while a Windows port reservation was the real cause.
    #[test]
    fn privileged_port_rule_is_scoped_to_ports_below_1024() {
        let low = bind_error_hint(&denied(), 80, true, false);
        assert!(low.contains("below 1024"));
        assert!(low.contains("elevated privileges"));

        // The combination that used to misbehave: low port on Windows, where
        // the rule does not exist at all.
        let low_windows = bind_error_hint(&denied(), 80, true, true);
        assert!(!low_windows.contains("elevated privileges"));

        for windows in [true, false] {
            let high = bind_error_hint(&denied(), 7321, true, windows);
            assert!(
                !high.contains("1024"),
                "high port must not cite the privileged-port rule: {high}"
            );
        }
    }

    /// The ephemeral path (`tofupilot studio`) binds port 0 and ignores the
    /// env override on purpose, so no message on that path may tell anyone to
    /// pick a port. Port 0 also trips the `< 1024` rule if it is matched
    /// first, which is how it used to get the elevation advice.
    #[test]
    fn ephemeral_bind_never_suggests_picking_a_port() {
        // Both kinds, because the rule is about the path and not about one
        // failure mode: `AddrInUse` used to walk straight past it.
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::AddrInUse,
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::AddrNotAvailable,
            // Stands in for anything that lands in the catch-all arm, which
            // is where a kind nobody has thought about yet will go.
            std::io::ErrorKind::Other,
        ] {
            for windows in [true, false] {
                let e = std::io::Error::new(kind, "boom");
                let hint = bind_error_hint(&e, 0, false, windows);
                assert!(hint.contains("ephemeral"), "{hint}");
                assert!(!hint.contains("elevated privileges"), "{hint}");
                assert!(!hint.contains("Unset TOFUPILOT_LOCAL_UI_PORT"), "{hint}");
                for advice in [
                    "pick a higher port",
                    "pick a different port",
                    "pick a port outside",
                    "pick a free port",
                ] {
                    assert!(
                        !hint.contains(advice),
                        "unfollowable advice on the ephemeral path: {hint}"
                    );
                }
            }
        }
    }

    /// Windows has no privileged-port range, so a refused bind on port 80
    /// there is http.sys / IIS or a reservation, never elevation. Ordering
    /// the port test before the platform test made the Unix advice win here.
    #[test]
    fn windows_never_gets_the_elevation_advice_even_below_1024() {
        let hint = bind_error_hint(&denied(), 80, true, true);
        assert!(
            !hint.contains("elevated privileges"),
            "Windows must not be told to elevate: {hint}"
        );
        assert!(hint.contains("excludedportrange"));
        assert!(hint.contains("http.sys"));
    }

    #[test]
    fn windows_high_port_points_at_reserved_ranges() {
        let hint = bind_error_hint(&denied(), 7321, true, true);
        assert!(hint.contains("excludedportrange"));
        assert!(hint.contains("not about"));
    }

    /// Same refusal on a Unix host is a sandbox / LSM story, and must not
    /// send anyone to netsh.
    #[test]
    fn unix_high_port_points_at_sandbox_not_netsh() {
        let hint = bind_error_hint(&denied(), 7321, true, false);
        assert!(hint.contains("seccomp"));
        assert!(!hint.contains("netsh"));
    }

    /// The raw OS error is what identifies the real failure (WSAEACCES
    /// 10013 vs EPERM). It was missing from every PermissionDenied message.
    #[test]
    fn permission_denied_always_carries_the_raw_os_error() {
        for (port, windows) in [(80u16, false), (7321, true), (7321, false)] {
            let hint = bind_error_hint(&denied(), port, true, windows);
            assert!(hint.contains("raw_os_error="), "missing raw error: {hint}");
        }
    }

    /// A real port collision keeps its own actionable message, and the
    /// stable-port variant is the one naming `tofupilot service stop`.
    #[test]
    fn addr_in_use_distinguishes_stable_from_ephemeral() {
        let in_use = || std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use");
        let stable = bind_error_hint(&in_use(), 7321, true, true);
        assert!(stable.contains("tofupilot service stop"));
        assert!(stable.contains("7321"));

        // Ephemeral always binds 0, so `stable_port == false` on any other
        // port describes a state `Server::start` cannot reach. Assert the
        // ignored-variable phrasing rather than the bare variable name: the
        // old wording ("Unset TOFUPILOT_LOCAL_UI_PORT") also contained the
        // name, so a bare `contains` would let it come back unnoticed.
        let ephemeral = bind_error_hint(&in_use(), 0, false, true);
        assert!(ephemeral.contains("TOFUPILOT_LOCAL_UI_PORT is ignored"));
        assert!(!ephemeral.contains("Unset TOFUPILOT_LOCAL_UI_PORT"));
        assert!(!ephemeral.contains("service stop"));
    }
}
