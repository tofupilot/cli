//! The `run` command: execute a procedure locally or from a pulled
//! deployment.
//!
//! Resolves the run source, drives the [`execution_engine`] run loop, and fans
//! events out to every active surface (Centrifugo, the kiosk WebSocket, the
//! TUI, and the agent protocol) via [`event_router`]. Failed uploads fall back
//! to the offline [`queue`].

pub(crate) mod agent_proto;
pub(crate) mod bootstrap;
pub(crate) mod cancel;
pub(crate) mod connector;
pub(crate) mod deployment_id;
pub(crate) mod emit;
mod engine;
pub(crate) mod event_router;
pub(crate) mod identify_host;
pub(crate) mod log_source;
pub(crate) mod outcomes;
pub(crate) mod procedure_version;
pub(crate) mod python;
pub(crate) mod queue;
pub(crate) mod time_fmt;
mod tui;
pub(crate) mod ui_response;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use station_protocol::StationEvent;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::commands::auth::credentials::Credentials;
use crate::commands::db;
use crate::commands::station;

use agent_proto::{AgentProtoCtx, CliEvent, RunLifecycle};

// ---------------------------------------------------------------------------
// Event publisher: how test events reach Centrifugo
// ---------------------------------------------------------------------------

/// How test events are published to the stream.
pub enum EventPublisher {
    /// Create a new StreamBridge (standalone `tofupilot run`).
    Standalone { creds: Credentials },
    /// Use an existing publish handle from station mode's client.
    Managed {
        publish: station::client::PublishHandle,
    },
}

// ---------------------------------------------------------------------------
// RunHandle: non-blocking test execution
// ---------------------------------------------------------------------------

/// Handle to a running test. Returned by `start()`.
///
/// Cancellation flows through a single watch channel (see [`cancel`]).
/// Callers route stop/kill via `RunHandle::cancel`/`RunHandle::kill`;
/// these write the new state and return immediately. The run task
/// observes the change via its own watch receiver and unwinds.
pub struct RunHandle {
    /// Receives exit code when the test completes.
    pub done_rx: oneshot::Receiver<i32>,
    /// Cancellation signal source. Cloned by callers that need to keep
    /// firing escalations (Exit's Stop → timeout → Kill ladder) past
    /// the consumption of `Self`.
    cancel: cancel::CancelToken,
    /// Operator-UI response sink. Receives only `UiResponse` frames —
    /// cancellation goes through `cancel`. Kept as `mpsc<StationCommand>`
    /// for wire-shape parity with the operator-UI WS handler.
    pub ui_response_tx: mpsc::Sender<station_protocol::StationCommand>,
    /// Inject externally-sourced `StationEvent`s for the TUI to render —
    /// presence events received on the station's status channel being
    /// the current use case. Delivered on a dedicated mpsc (not the
    /// run's internal broadcast) so the managed publisher doesn't
    /// re-publish them to Centrifugo and cause a fanout loop.
    pub tui_presence_tx: mpsc::Sender<StationEvent>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl RunHandle {
    /// Cancel the run gracefully and wait for cleanup. Idempotent on
    /// a handle whose task already completed (await resolves promptly).
    pub async fn abort(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    /// Signal a graceful stop. Idempotent. Returns immediately —
    /// caller awaits `done_rx` or `take_task` for completion.
    pub fn request_cancel(&mut self) {
        self.cancel.cancel();
    }

    /// Signal a force kill. Idempotent. Always escalates if a graceful
    /// cancel was already requested.
    pub fn request_kill(&mut self) {
        self.cancel.kill();
    }

    /// Detach the background task so the caller can await it under a
    /// timeout without consuming the handle. Used by the Exit
    /// escalation path (Stop → timeout → Kill → timeout → drop). The
    /// `Drop` backstop still fires `task.abort()` if the handle is
    /// dropped before the returned task completes.
    pub fn take_task(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        self.task.take()
    }

    /// Wait for the run to complete naturally and return its exit code.
    /// Consumes the handle; the [`Drop`] backstop won't fire afterwards
    /// because the owning task will have finished.
    pub async fn join(mut self) -> i32 {
        let Some(task) = self.task.take() else {
            return 1;
        };
        // Await the test to signal done, then reap the background task so we
        // don't leave it dangling. If either side errored, fall back to 1.
        let code = ((&mut self.done_rx).await).unwrap_or(1);
        let _ = task.await;
        code
    }
}

impl Drop for RunHandle {
    /// Last-ditch cleanup: if the handle is dropped without calling
    /// `abort().await` (e.g. a panic between construct and first await),
    /// abort the background task so the Python child doesn't leak.
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Start a test without blocking. Returns a RunHandle.
/// The caller's event loop stays running for telemetry, config, etc.
///
/// For `tofupilot run` (standalone), use `run()` which calls start() + awaits.
pub type AgentProtoOptions = agent_proto::Options;

/// Source of the procedure to run.
#[derive(Clone, Debug)]
pub enum RunSource {
    /// A pulled deployment referenced by ID (looked up in the deployments dir).
    /// `None` triggers interactive selection.
    Deployment(Option<String>),
    /// A local path (file or directory). Runs the local source directly.
    /// `upload` requests syncing the run to the dashboard via the dir's
    /// `tofupilot.json` link (see `tofupilot link`); without it the run
    /// stays local.
    LocalPath { path: PathBuf, upload: bool },
}

struct ResolvedSource {
    id: String,
    dir: PathBuf,
    upload: bool,
    /// Set when a local dir is linked (`tofupilot.json` present) but the
    /// run is staying local because `--upload` wasn't passed. Carries the
    /// linked procedure's display label so the post-run hint can nudge the
    /// user toward `--upload`.
    link_hint: Option<String>,
    /// Explicit file passed on the command line (`tofupilot run ./my-test.yml`).
    /// When it has a YAML extension, framework detection treats it as the
    /// procedure file regardless of its name — the local-path equivalent of
    /// a manifest `entry_point`.
    yaml_hint: Option<PathBuf>,
}

/// True when `rel` ends in a YAML extension (`.yaml`/`.yml`,
/// case-insensitive). Used to classify a manifest-declared entry point
/// as a YAML procedure regardless of its filename. The execution-engine
/// loader applies the same extension rule when it opens the file.
fn has_yaml_extension(rel: &str) -> bool {
    Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
}

/// Which framework drives this procedure. Resolved once per run by
/// scanning the package dir; replaces the prior implicit
/// "yaml_lookup → has_openhtf → fallthrough" chain that was duplicated
/// across `start()` and `run_test()`.
///
/// Detection order:
///   1. manifest `entry_point` naming a `.yaml`/`.yml` file → `Yaml`
///      (lets a procedure file use any name, not just `procedure.yaml`).
///   2. `procedure.yaml` (or `.yml`) in the package dir → `Yaml`.
///   3. otherwise, `openhtf` named in pyproject / uv.lock / pylock.toml /
///      requirements.txt → `Openhtf`.
///   4. otherwise → `Plain` python.
///
/// Adding a new framework (pytest, robot, …) is one new arm here plus
/// one new arm in `run_test`. No other call site needs touching.
#[cfg_attr(test, derive(Debug))]
enum Framework {
    /// YAML-driven procedure. The engine reads the file at this path and
    /// drives subprocess spawning itself; no Python entry-point needed.
    Yaml(PathBuf),
    /// OpenHTF-based procedure. The connector launches the user's
    /// entry-point (`main.py`) under a Python interpreter and proxies
    /// events.
    Openhtf,
    /// pytest-driven procedure. The connector launches pytest in-process
    /// (via the embedded plugin script) and bridges its lifecycle hooks
    /// onto the same event router.
    Pytest,
    /// Robot Framework procedure. The connector launches Robot under
    /// the embedded listener script and bridges suite/test events
    /// onto the same event router.
    Robot,
    /// Plain Python script. Exec the entry-point with no orchestration.
    Plain,
}

impl Framework {
    /// Detect the framework from disk inside `package_dir`. Cheap —
    /// one or two file existence checks plus a small text scan when
    /// neither yaml nor openhtf is obvious.
    ///
    /// `manifest_entry` is the user-declared entry point from the
    /// deployment manifest (`null` for local-path runs and pre-entry
    /// bundles). When it names a `.yaml`/`.yml` file the procedure is
    /// unambiguously YAML-driven regardless of filename — this is the
    /// only way to run a procedure file not named `procedure.yaml`. The
    /// path is server-validated and re-checked by `Manifest::parse`, so
    /// it's safe to join onto `package_dir`.
    ///
    /// Order: explicit-yaml-entry > yaml > openhtf > pytest > robot >
    /// plain. yaml wins (after the explicit entry) because it's the
    /// canonical TofuPilot procedure format. openhtf wins over pytest
    /// because a project that has both (e.g. an OpenHTF test suite that
    /// also runs unit tests via pytest) is driven by openhtf — pytest is
    /// incidental tooling, not the run target. Robot sits below pytest
    /// for the same reason: a repo shipping both `test_*.py` and
    /// `*.robot` is almost always pytest-driven with Robot used as
    /// auxiliary suites.
    fn detect(package_dir: &Path, manifest_entry: Option<&str>) -> Self {
        if let Some(rel) = manifest_entry {
            if has_yaml_extension(rel) {
                return Framework::Yaml(package_dir.join(rel));
            }
        }
        if let Some(yaml) = engine::find_procedure_yaml(package_dir) {
            return Framework::Yaml(yaml);
        }
        if connector::has_openhtf(package_dir) {
            return Framework::Openhtf;
        }
        if connector::has_pytest(package_dir) {
            return Framework::Pytest;
        }
        if connector::has_robot(package_dir) {
            return Framework::Robot;
        }
        Framework::Plain
    }

    /// What this framework treats as its "entry surface". OpenHTF and
    /// Plain need a single `main.py` file; pytest scans a directory and
    /// auto-discovers `test_*.py`. Returning the discriminant up-front
    /// avoids a downstream `find_entry_point` call accidentally picking
    /// `main.py` for a pytest deployment that also ships a sentinel
    /// `main.py` (templates do this so the deployer's "has Python entry"
    /// check passes).
    fn entry_surface(&self) -> EntrySurface {
        match self {
            Framework::Openhtf | Framework::Plain => EntrySurface::MainPy,
            Framework::Pytest | Framework::Robot => EntrySurface::PackageDir,
            Framework::Yaml(_) => EntrySurface::PackageDir,
        }
    }
}

/// Where the framework points its connector at when invoked.
enum EntrySurface {
    /// Resolve to a `main.py` (or other recognized entry file) and pass
    /// that file path to the connector. Missing entry → run rejected.
    MainPy,
    /// Pass the package dir to the connector and let it discover what
    /// to run inside (pytest scans for `test_*.py`, YAML never reaches
    /// this path because it skips Python entirely).
    PackageDir,
}

/// Everything `start()` resolves before kicking off a run. Bundling
/// these into one struct lets `prepare_run` return a single Result and
/// keeps the failure-handling code in `start()` to a single match arm.
#[cfg_attr(test, derive(Debug))]
struct Prepared {
    package_dir: PathBuf,
    framework: Framework,
    entry_file: PathBuf,
    python_path: PathBuf,
}

/// What went wrong before the run could start. `kind` is the
/// load-time error category surfaced to the operator UI; `message` is
/// the human-readable detail.
#[cfg_attr(test, derive(Debug))]
struct PrepareFail {
    kind: &'static str,
    message: String,
}

/// Resolve package dir + framework + entry file + venv interpreter for
/// a run. Each step that can fail short-circuits with a [`PrepareFail`]
/// describing the category and detail; `start()` turns those into
/// `synthetic_fail_handle` calls without copy-pasting the same six
/// arguments at every error site.
///
/// `deployment_dir` is the deployment root (= `<deployments>/<id>`).
/// We resolve `package_dir` from the manifest and then never need
/// `deployment_dir` again — venv, source tree, and cwd all live inside
/// `package_dir`.
/// Manifest-declared entry overrides the framework default. Server
/// validates the value before it gets baked into the manifest, and
/// `Manifest::parse` re-validates on read, so by the time we land here
/// it's safe to `Path::join` onto `package_dir`. Bundles built before
/// the entry-point field existed (and local-path runs without a
/// manifest at all) hit the `None` arm and fall back to the framework's
/// declared `EntrySurface`. The pytest sentinel `main.py` no longer
/// wins because pytest's surface is `PackageDir`, not `MainPy`.
fn resolve_entry_file(
    manifest_entry: Option<&str>,
    framework: &Framework,
    package_dir: &Path,
) -> Result<PathBuf, PrepareFail> {
    if let Some(rel) = manifest_entry {
        return Ok(if rel == "." {
            package_dir.to_path_buf()
        } else {
            package_dir.join(rel)
        });
    }
    match framework.entry_surface() {
        EntrySurface::MainPy => python::find_entry_point(package_dir).ok_or_else(|| PrepareFail {
            kind: "load_error",
            message: format!(
                "No procedure found in {}. Expected a procedure.yaml or a Python entry point \
                 (main.py).",
                package_dir.display(),
            ),
        }),
        EntrySurface::PackageDir => Ok(package_dir.to_path_buf()),
    }
}

async fn prepare_run(
    deployment_dir: &Path,
    bootstrap_enabled: bool,
    // Explicit YAML file from the command line (`tofupilot run ./my-test.yml`).
    // Wins over on-disk detection the same way a manifest `entry_point` does.
    yaml_hint: Option<&Path>,
) -> Result<Prepared, PrepareFail> {
    let layout = engine::deployment_layout(deployment_dir).map_err(|e| PrepareFail {
        kind: "load_error",
        message: format!("Bad deployment manifest: {e}"),
    })?;
    let engine::DeploymentLayout {
        package_dir,
        entry_point,
        manifest_present,
    } = layout;

    let framework = match yaml_hint {
        Some(hint) if hint.to_str().is_some_and(has_yaml_extension) => {
            Framework::Yaml(hint.to_path_buf())
        }
        _ => Framework::detect(&package_dir, entry_point.as_deref()),
    };

    let entry_file = resolve_entry_file(entry_point.as_deref(), &framework, &package_dir)?;

    // Station-mode (`manifest_present`) keeps the installer-managed
    // venv at `<package_dir>/venv`: `deployment_python` is the single
    // source of truth, no bootstrap, no stamp check. Local-path runs
    // hand everything to `bootstrap::ensure_venv` — it owns missing-
    // venv prompt, stamp-drift rebuild, and `--no-bootstrap` opt-out.
    // All frameworks (including YAML) flow through the same path
    // because every phase ultimately spawns under a Python interpreter.
    let python_path = if manifest_present {
        python::deployment_python(&package_dir)
    } else {
        bootstrap::ensure_venv(&package_dir, bootstrap_enabled)
            .await
            .map(|e| e.python)
    }
    .map_err(|e| PrepareFail {
        kind: "env_error",
        message: format!("Python environment error: {e}"),
    })?;

    Ok(Prepared {
        package_dir,
        framework,
        entry_file,
        python_path,
    })
}

/// Identity for TUI-originated presence publishes. Callers that have
/// an identity to stamp (station mode, where the installation id is a
/// stable per-station identifier) pass this; standalone runs pass
/// `None` and the TUI only receives presence, never publishes its own.
pub struct TuiPresenceIdentity {
    pub user_id: String,
    pub display_name: String,
    pub color: String,
}

/// Synthesize a `RunHandle` whose background task immediately publishes
/// a terminal `RunCrashed` + `RunComplete(ERROR)` for a run that failed
/// before the engine could spawn (missing entry point, Python venv
/// resolution error, …). The handle behaves exactly like a normal one
/// from the caller's perspective: `done_rx` resolves with the exit
/// code, the task drains the publisher, and `request_cancel/kill` are
/// idempotent no-ops on a task that's already finished.
///
/// Without this, construction failures returned `Err(i32)` from
/// `start()` and the operator-UI never received a terminal event for
/// the run it had just commanded — the spinner sat forever.
async fn synthetic_fail_handle(
    procedure_id: String,
    execution_id: String,
    error_kind: &'static str,
    message: String,
    publisher: Option<EventPublisher>,
    local_ws_server: Option<std::sync::Arc<crate::local_ws::Server>>,
    exit_code: i32,
    // Optional agent context. When set, the synthetic crash also
    // enqueues a `CliEvent::RunCrashed` so a headless agent caller
    // sees the same signal a real run would emit. Without this,
    // agent-driven flows that hit a synthetic-fail (no entry shim,
    // broken venv, agent init error) saw the run vanish silently —
    // Centrifugo got the wire event, agent stdout did not.
    agent_ctx: Option<AgentProtoCtx>,
) -> RunHandle {
    let (event_tx, _) = broadcast::channel::<StationEvent>(8);
    let (ui_response_tx, _ui_response_rx) = mpsc::channel::<station_protocol::StationCommand>(1);
    let (tui_presence_tx, _tui_presence_rx) = mpsc::channel::<StationEvent>(1);
    let (done_tx, done_rx) = oneshot::channel::<i32>();
    let (cancel_token, _cancel_rx) = cancel::CancelToken::new();

    // Wire the publisher so terminal events reach Centrifugo / the
    // dashboard. For station mode (`Managed`) this is a cheap clone
    // of an existing HTTP client. For standalone (`Standalone`),
    // opening a fresh `StreamBridge` would require a network handshake
    // for a run that already failed — skip it; standalone runs report
    // failure through stderr + exit code and don't need the wire
    // round-trip.
    let publisher_handle: Option<tokio::task::JoinHandle<()>> = match publisher {
        Some(EventPublisher::Standalone { .. }) | None => None,
        Some(EventPublisher::Managed { publish }) => {
            let mut rx = event_tx.subscribe();
            Some(tokio::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    let _ = tokio::time::timeout(
                        crate::config::timeouts::PUBLISH_PER_EVENT,
                        publish.publish(&event),
                    )
                    .await;
                }
            }))
        }
    };

    // Attach to the local-WS server so the operator-UI's WebSocket
    // clients see the terminal events. Without this, a station-mode
    // synthetic-fail (no entry shim, broken venv, agent init error) only
    // reached Centrifugo — the kiosk's pending state seeded by
    // `handleRun` sat at `'starting'` forever because no terminal
    // ever crossed the local-WS pump.
    let local_ws_attachment = if let Some(server) = local_ws_server {
        let procedures = vec![crate::local_ws::ProcedureRef {
            id: procedure_id.clone(),
            name: procedure_id.clone(),
        }];
        Some(
            server
                .attach_run(
                    event_tx.clone(),
                    ui_response_tx.clone(),
                    cancel_token.clone(),
                    procedures,
                    // Synthetic-fail: the run never started, so there is
                    // no procedure dir to serve images from.
                    None,
                    "station",
                )
                .await,
        )
    } else {
        None
    };

    let task = tokio::spawn(async move {
        // Hold the attachment for the duration of the synthetic emit
        // + drain. Drop fires after the publisher task exits, which
        // happens after `event_tx` is dropped below — bounded by
        // PUBLISH_DRAIN so a stuck HTTP call can't pin teardown.
        let _local_ws_attachment = local_ws_attachment;
        // RunCrashed carries the diagnostic; the helper also fires a
        // synthetic RunComplete(ERROR) so consumers that key on
        // completeness still terminate cleanly.
        emit::run_crashed(
            &event_tx,
            agent_ctx.as_ref(),
            &procedure_id,
            &execution_id,
            error_kind,
            &message,
            exit_code,
        );
        // Drop the broadcast sender so the publisher task exits its
        // recv loop. Then wait for it to drain (bounded — one stuck
        // HTTP call shouldn't hold synthetic-fail teardown hostage).
        drop(event_tx);
        if let Some(h) = publisher_handle {
            let _ = tokio::time::timeout(crate::config::timeouts::PUBLISH_DRAIN, h).await;
        }
        let _ = done_tx.send(exit_code);
    });

    RunHandle {
        done_rx,
        cancel: cancel_token,
        ui_response_tx,
        tui_presence_tx,
        task: Some(task),
    }
}

// Args genuinely orthogonal: procedure source vs. publisher transport vs.
// optional TUI/agent/local-UI knobs. A param bag would just rename the
// fields without reducing callsite noise — this fn has one caller.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    procedure_id: &str,
    procedure_dir: PathBuf,
    upload: bool,
    json_mode: bool,
    creds: Option<&Credentials>,
    publisher: Option<EventPublisher>,
    agent_opts: AgentProtoOptions,
    tui_presence: Option<TuiPresenceIdentity>,
    tui_override: Option<bool>,
    kiosk_override: Option<bool>,
    // Long-lived local-WS server to attach this run's broadcast to.
    // `Some(_)` in station mode (server bound at station startup,
    // reused across runs); `None` in standalone `tofupilot run`,
    // where the server (if kiosk_enabled) is created inline below.
    local_ws_server: Option<std::sync::Arc<crate::local_ws::Server>>,
    // Pre-resolved unit data when the operator clicked "Run again" on
    // the outcome screen. The engine skips identify-unit and emits an
    // `identify_resolved` event with these values directly, so the
    // run begins with the same unit as the previous cycle.
    reuse_unit: Option<station_protocol::UnitInfo>,
    // Email forwarded to `runs.create` as `operated_by`. Set when the
    // run was triggered from the web operator UI; None for kiosk and
    // CLI-driven runs.
    operated_by: Option<String>,
    // Whether local-path runs may auto-provision a missing venv via
    // `bootstrap::ensure_venv`. Station-mode runs (manifest-present
    // deployments) ignore this flag; their venvs are owned by the
    // deployer's installer. False corresponds to `--no-bootstrap`.
    bootstrap_enabled: bool,
    // Explicit YAML procedure file from the command line; None for
    // directory runs and deployments (the manifest covers those).
    yaml_hint: Option<PathBuf>,
) -> RunHandle {
    // Per-run identity, minted up-front so even synthetic-fail handles
    // (no entry point, no venv) carry a stable id consumers can correlate
    // with the operator-UI's `'pending'` state seed.
    let execution_id = uuid::Uuid::new_v4().to_string();

    if let Some(c) = creds {
        let drain_creds = c.clone();
        // Fire-and-forget: drain can take tens of seconds if the queue is
        // backed up, but the user's run shouldn't wait on it. Station mode
        // also runs a continuous drain loop (set up in `commands/station`)
        // so this kick is just an immediate first pass; the loop picks up
        // anything that lands afterwards.
        tokio::spawn(async move {
            let handle = tokio::spawn(async move { queue::drain(&drain_creds, None, true).await });
            if let Err(e) = handle.await {
                if e.is_panic() {
                    crate::log::error(&format!("background queue drain panicked: {e}"));
                }
            }
        });
    }

    // Resolve everything that can go wrong before the run starts in
    // one place. Each variant carries the kind label + message that
    // would otherwise be repeated at three return sites.
    //
    // Workspace-mode bundles surface the procedure source under
    // `<deployment>/<root_directory>/`; the source tree, venv,
    // entry file, and Python subprocess cwd all live there. For
    // single-package bundles `root_directory` is null and the
    // package dir collapses to the deployment root.
    let prepared = match prepare_run(&procedure_dir, bootstrap_enabled, yaml_hint.as_deref()).await
    {
        Ok(p) => p,
        Err(fail) => {
            crate::log::error(&fail.message);
            return synthetic_fail_handle(
                procedure_id.to_string(),
                execution_id,
                fail.kind,
                fail.message,
                publisher,
                local_ws_server.clone(),
                1,
                None,
            )
            .await;
        }
    };
    let Prepared {
        package_dir,
        framework,
        entry_file,
        python_path,
    } = prepared;

    // TUI needs a real tty to drive crossterm's EventStream — without one,
    // `EventStream::new()` panics with "reader source not set" on the first
    // poll. A station running detached/daemonized (launch-on-boot) inherits a
    // pipe for stdin, not a tty, so we skip the TUI there regardless of
    // what the user's config says. Foreground runs (local or triggered
    // from the web while the station has a tty open) still get the TUI.
    //
    // Precedence: explicit CLI flag (`--tui`/`--no-tui`,
    // `--kiosk`/`--no-kiosk`) > station config > default. The TUI
    // defaults ON for an interactive run so a bare `tofupilot run` in a
    // terminal shows live phases, measurements, and logs instead of a
    // silent screen. The two gates ahead of the default keep it off
    // where a takeover would be wrong: `--json` / agent mode owns stdout
    // with NDJSON, and a non-tty (pipes, CI, the station daemon under
    // launchd/systemd) has no terminal to draw into. Kiosk stays opt-in.
    let tui_enabled = !json_mode
        && std::io::stdin().is_terminal()
        && resolve_ui_pref("terminal_ui", tui_override, true);
    // Kiosk is a separate browser process, not a terminal UI — the
    // station-mode daemon (launchd / systemd) runs without a tty but
    // still needs the local-ws server up so the kiosk browser can
    // attach. Don't gate it on `is_terminal()`.
    let kiosk_enabled = resolve_ui_pref("kiosk_ui", kiosk_override, false);

    // Both modes own stdout: TUI draws ratatui frames, agent-protocol
    // writes NDJSON. Enabling both would interleave garbage. Derivation
    // above already excludes the `json_mode && tui_enabled` combination,
    // but a future refactor could easily break that — this assertion
    // pins the invariant at run startup so the bug surfaces immediately
    // instead of as corrupted output.
    //
    // `assert!` not `debug_assert!`: stdout corruption is a release-mode
    // problem, not a debug-mode one. A debug_assert elides in release
    // builds — the only build that actually ships — leaving the guard
    // cosmetic.
    assert!(
        !(json_mode && tui_enabled),
        "tui and agent protocol are mutually exclusive stdout owners"
    );

    if !json_mode {
        crate::log::info(&format!(
            "Running {} ({})",
            entry_file.display(),
            procedure_id
        ));
        eprintln!();
    }

    // 128 slots. Sized for typical procedures (phase+UI events, a handful
    // per second). A phase streaming high-frequency measurements (>500/s)
    // could lag a slow consumer — the TUI surfaces that via a one-frame
    // "lagged N events" warning; the station publisher drops and moves on.
    // Raise here if empirically needed; no hard ceiling.
    let (event_tx, _) = broadcast::channel::<StationEvent>(128);
    // Dedicated channel for presence events arriving on the status
    // channel from other participants. Kept separate from `event_tx`
    // so the managed publisher doesn't re-publish incoming presence
    // back to Centrifugo and cause a fanout loop.
    let (tui_presence_tx, tui_presence_rx) = mpsc::channel::<StationEvent>(128);

    // Per-run cancellation, single source of truth. Replaces the
    // earlier 3-oneshot + bridge-task design (cancel_tx, engine_stop_tx,
    // engine_force_tx) with one watch channel observed by every cancel
    // consumer (engine, connector, outer cancel arm, agent abort).
    let (cancel_token, cancel_rx) = cancel::CancelToken::new();

    // Operator-UI response sink. ONLY UiResponse frames flow here —
    // cancellation goes through `cancel_token` directly. Kept as a
    // typed mpsc so the local_ws server can still send the same
    // `StationCommand::UiResponse` shape it always has.
    let (ui_response_tx, mut ui_response_rx) =
        mpsc::channel::<station_protocol::StationCommand>(32);

    // Kiosk UI: attach this run's broadcast + ui-cmd sink to the
    // long-lived local-WS server. Two cases:
    //   * Station mode: the server was bound at startup; we reuse
    //     the same listener and same browser tab across every run.
    //   * Standalone `tofupilot run --kiosk`: bind a fresh server
    //     inline; it dies with the process when the run ends.
    // The `RunAttachment` guard returned by `attach_run` lives on
    // the run task (moved into the spawn below) so its Drop fires
    // exactly when the run terminates.
    let local_ws_attachment = if kiosk_enabled {
        // Prefer the procedure's declared name from procedure.yaml
        // over the directory basename — same source the dashboard
        // shows. Fall back to the dir name when the file isn't a
        // YAML procedure (OpenHTF or plain Python). Reuses the
        // already-detected `Framework::Yaml(path)` so this avoids a
        // second on-disk lookup.
        let yaml_path = match &framework {
            Framework::Yaml(p) => Some(p),
            Framework::Openhtf | Framework::Pytest | Framework::Robot | Framework::Plain => None,
        };
        let proc_name = yaml_path
            .and_then(|p| {
                execution_engine::procedure::loader::load_procedure_definition(p)
                    .ok()
                    .map(|def| def.name)
            })
            .unwrap_or_else(|| {
                package_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Procedure")
                    .to_string()
            });
        let procedures = vec![crate::local_ws::ProcedureRef {
            id: procedure_id.to_string(),
            name: proc_name,
        }];

        // Station-mode runs ship the caller's pre-bound server.
        // Standalone runs bind one inline and keep it alive via
        // `Arc` for the run's lifetime. Both branches converge on
        // `attach_run`.
        let server = match local_ws_server.clone() {
            Some(s) => Some(s),
            None => {
                let identity = crate::commands::db::open()
                    .ok()
                    .and_then(|db| db.get_whoami().ok().flatten())
                    .map(|w| crate::local_ws::HelloIdentity {
                        auth_type: Some(w.auth_type),
                        organization_slug: Some(w.organization_slug),
                        organization_name: Some(w.organization_name),
                        station_id: w.station_id,
                        user_id: w.user_id,
                        user_email: w.user_email,
                        user_name: w.user_name,
                    })
                    .unwrap_or_default();
                match crate::local_ws::Server::start(
                    procedure_id.to_string(),
                    "CLI".to_string(),
                    identity,
                )
                .await
                {
                    Ok(s) => {
                        crate::log::info(&format!(
                            "Opening operator UI in browser ({})",
                            s.boot_url()
                        ));
                        eprintln!();
                        // Attach the kiosk window to the server so it
                        // closes automatically when the server (and the
                        // CLI process) shuts down.
                        if let Some(brand) = s.attach_kiosk().await {
                            if matches!(brand, crate::browser_open::KioskBrowser::Fallback) {
                                crate::log::warn(
                                "no kiosk-capable browser found; opened default browser instead. \
                                Use the Maximize button in the operator topbar for fullscreen.",
                            );
                            }
                        }
                        Some(std::sync::Arc::new(s))
                    }
                    Err(e) => {
                        crate::log::warn(&format!("local-ui server failed to start: {e}"));
                        None
                    }
                }
            }
        };

        if let Some(server) = server {
            // Standalone mode: server is the local Arc above and dies
            // with the run task. Station mode: server outlives every
            // run; we hold an Arc clone for the run task's lifetime.
            let attach = server
                .attach_run(
                    event_tx.clone(),
                    ui_response_tx.clone(),
                    cancel_token.clone(),
                    procedures,
                    Some(package_dir.clone()),
                    if local_ws_server.is_some() {
                        "station"
                    } else {
                        "local"
                    },
                )
                .await;
            // Move both the Arc and the attachment into the run
            // task so they live exactly as long as the run. Pair so
            // they drop together: dropping the attachment stops the
            // pump; dropping the standalone Arc (refcount → 0) is
            // a no-op for station mode (caller still holds the
            // canonical Arc) and tears down the listener task in
            // standalone mode.
            Some((server, attach))
        } else {
            None
        }
    } else {
        None
    };

    // Event publishing: forward broadcast events to the stream. Both branches
    // produce a value that exposes a bounded-timeout drain via ActivePublisher::flush.
    enum ActivePublisher {
        Standalone(Option<station::bridge::StreamBridge>),
        Managed(tokio::task::JoinHandle<()>),
    }
    impl ActivePublisher {
        async fn flush(self, timeout: std::time::Duration) {
            match self {
                ActivePublisher::Standalone(Some(bridge)) => bridge.flush(timeout).await,
                ActivePublisher::Standalone(None) => {}
                ActivePublisher::Managed(handle) => {
                    crate::tasks::drain_or_abort(
                        handle,
                        timeout,
                        "Timed out draining publish queue; some events may have been dropped.",
                    )
                    .await;
                }
            }
        }
    }
    // Agent protocol (json mode only, no TUI). Initialized BEFORE the
    // publisher is consumed so a `--ui-values` parse error (the only
    // currently-defined fatal) can still be surfaced to the operator
    // via `synthetic_fail_handle` using the same publisher transport
    // a normal run would have used.
    let agent_init = agent_proto::initialize(
        json_mode,
        tui_enabled,
        procedure_id,
        &package_dir,
        &agent_opts,
    )
    .await;
    let (agent_ctx, agent_stdin_handle, agent_abort_rx) = match agent_init {
        Ok(Some(init)) => (Some(init.ctx), Some(init.stdin_handle), Some(init.abort_rx)),
        Ok(None) => (None, None, None),
        Err(code) => {
            let msg = "agent protocol initialization failed".to_string();
            return synthetic_fail_handle(
                procedure_id.to_string(),
                execution_id,
                "agent_init_error",
                msg,
                publisher,
                local_ws_server.clone(),
                code,
                // Init failed — no AgentProtoCtx exists. The error is
                // surfaced via stderr + exit code instead.
                None,
            )
            .await;
        }
    };

    // A wired publisher is a bidirectional Centrifugo bridge: it
    // republishes `IdentifyRequest` to the dashboard AND routes the
    // operator's `UiResponse` back to the prompt's oneshot. So a run with
    // a publisher CAN have its unit-identify answered by a remote operator,
    // even with no local UI (TUI/kiosk/agent). Both standalone (`--upload`
    // / deployment runs) and managed (station daemon) publishers carry this
    // inbound path, so either counts as an operator surface for `has_ui`.
    let has_publisher = publisher.is_some();
    let active_publisher: ActivePublisher = match publisher {
        Some(EventPublisher::Standalone { ref creds }) => {
            let bridge = station::bridge::StreamBridge::new(creds, event_tx.subscribe()).await;
            ActivePublisher::Standalone(bridge)
        }
        Some(EventPublisher::Managed { publish }) => {
            let mut rx = event_tx.subscribe();
            let task = tokio::spawn(async move {
                // Bound each publish so a single stuck Centrifugo call can't
                // monopolize the whole drain window the caller passes to flush().
                // `Lagged` is surfaced as a warn so dropped frames are
                // diagnosable instead of silently lost (matches the
                // local-ui server's posture).
                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            let _ = tokio::time::timeout(
                                crate::config::timeouts::PUBLISH_PER_EVENT,
                                publish.publish(&event),
                            )
                            .await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            crate::log::warn(&format!("centrifugo publisher lagged {n} event(s)"));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            ActivePublisher::Managed(task)
        }
        None => ActivePublisher::Standalone(None),
    };

    // Agent-mode bridge: relay upload-queue events from the
    // station-event broadcast onto the agent-protocol stream so
    // `--json` consumers see the queue activity. The run-state
    // events (PhaseStarted, PhaseFinished, …) are enqueued from
    // their own emit sites — only upload events lack a direct
    // call site because the queue runs detached from the run task.
    if let Some(ref agent) = agent_ctx {
        let mut rx = event_tx.subscribe();
        let agent_for_bridge = agent.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => match ev {
                        StationEvent::RunUploadStarted { queue_id, attempt } => {
                            agent_for_bridge
                                .emitter
                                .enqueue(CliEvent::RunUploadStarted { queue_id, attempt });
                        }
                        StationEvent::RunUploadSucceeded {
                            queue_id,
                            run_id,
                            dashboard_url,
                        } => {
                            agent_for_bridge
                                .emitter
                                .enqueue(CliEvent::RunUploadSucceeded {
                                    queue_id,
                                    run_id,
                                    dashboard_url,
                                });
                        }
                        StationEvent::RunUploadFailed {
                            queue_id,
                            attempt,
                            kind,
                            status,
                            error,
                            next_retry_at,
                        } => {
                            agent_for_bridge.emitter.enqueue(CliEvent::RunUploadFailed {
                                queue_id,
                                attempt,
                                kind,
                                status,
                                error,
                                next_retry_at,
                            });
                        }
                        StationEvent::RunUploadDropped { queue_id, reason } => {
                            agent_for_bridge
                                .emitter
                                .enqueue(CliEvent::RunUploadDropped { queue_id, reason });
                        }
                        _ => {}
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // TUI
    let (tui_ui_tx, tui_handle) = if tui_enabled {
        let rx = event_tx.subscribe();
        let (ui_tx, ui_rx) = mpsc::channel(16);
        let proc_dir = package_dir.clone();
        // Outbound presence rides the internal broadcast like any
        // other run event; the managed publisher re-publishes it to
        // Centrifugo. Inbound presence (from other participants) is
        // delivered on the dedicated `tui_presence_rx` mpsc.
        let presence_ctx = tui_presence.map(|ident| tui::PresenceContext {
            self_user_id: ident.user_id,
            display_name: ident.display_name,
            color: ident.color,
            tx: event_tx.clone(),
        });
        // TUI keystrokes (Ctrl-C, q, kill) call `cancel_token.cancel()`
        // / `kill()` directly. No mpsc adapter; one cancel surface for
        // every cancellation source.
        let tui_cancel = cancel_token.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = tui::run_tui(
                rx,
                ui_rx,
                proc_dir,
                presence_ctx,
                tui_presence_rx,
                tui_cancel,
            )
            .await
            {
                crate::log::error(&format!("TUI error: {e}"));
            }
        });
        (Some(ui_tx), Some(handle))
    } else {
        (None, None)
    };

    // Console log stream. Surfaces phases, measurements, logs, and the
    // final outcome (including phase error tracebacks) as plain console
    // lines so a run is never silent — for both a bare `tofupilot run`
    // and a `--no-tui` / piped / CI run. When the TUI owns the alternate
    // screen, writing live would corrupt its frames, so we buffer and
    // flush once after the TUI tears down (the alt-screen restore wipes
    // the TUI, leaving the buffered log in the operator's scrollback).
    // Agent/JSON mode already streams structured NDJSON, so skip it.
    let console_buffer: Option<ConsoleBuffer> = if json_mode {
        None
    } else if tui_enabled {
        Some(Arc::new(std::sync::Mutex::new(Vec::new())))
    } else {
        None // print live (no buffer)
    };
    let console_handle = if json_mode {
        None
    } else {
        let mut rx = event_tx.subscribe();
        let buf = console_buffer.clone();
        Some(tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => console_log_event(ev, buf.as_ref()),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }))
    };

    // UiResponse pump: drain the run-scoped ui_response_rx and
    // dispatch each response to the matching prompt. The pump exits
    // when the sender is dropped (run task tears down).
    let ui_response_handle = tokio::spawn(async move {
        while let Some(cmd) = ui_response_rx.recv().await {
            if let station_protocol::StationCommand::UiResponse { request_id, values } = cmd {
                ui_response::send(&request_id, values).await;
            }
            // Anything other than UiResponse on this channel is a
            // protocol bug at the sender — log and drop in debug,
            // silently ignore in release. The sender (operator-UI WS
            // routing in `local_ws::handle_text`) only ever sends
            // UiResponse here.
        }
    });

    // Done channel
    let (done_tx, done_rx) = oneshot::channel();

    // Test execution
    let test_creds = creds.cloned();
    let proc_id = procedure_id.to_string();
    // Resolve human-readable procedure name once, before the run task
    // spawns. Single source of truth: `procedure.name` from the
    // dashboard, persisted to `PullState.name` at pull time. Pulled
    // here so YAML / OpenHTF / plain-Python paths all inject the
    // same value into their `RunStarted` emit — no framework-specific
    // resolution downstream.
    let proc_name = db::open()
        .map(|d| d.resolve_procedure_name(procedure_id))
        .unwrap_or_else(|_| procedure_id.to_string());
    // Package dir is the single source path: source tree, venv, cwd,
    // and entry file all live underneath it. For single-package
    // bundles it equals the deployment root; for workspace-mode
    // bundles it's `<deployment>/<root_directory>`.
    let proc_dir = package_dir.clone();
    // Hand the resolved framework off to the run task. Detection
    // already happened above; `run_test` just dispatches on it.
    let proc_framework = framework;
    // Last broadcast sender we hold past `run_test`. `run_test` consumes
    // its own clone of `event_tx`; this one stays alive so the
    // publisher subscriber doesn't see `Closed` until we explicitly
    // drop it post-`run_test` — at which point the publisher loop
    // exits cleanly and `flush()` waits for in-flight publishes.
    let teardown_event_tx = event_tx.clone();
    let agent_for_test = agent_ctx.clone();
    let agent_for_finish = agent_ctx.clone();
    // Forward an agent `abort_run` command into the cancel token so
    // every cancellation source funnels through the same surface. The
    // side task races the abort signal against the run finishing —
    // when the run task drops its receivers, our `wait_any` resolves
    // (via the dropped-sender → Force translation in `cancel.rs`) and
    // the side task exits cleanly. Without that race the task would
    // hold a `CancelToken` clone forever in the (common) case where
    // the agent never sends `abort_run`.
    if let Some(rx) = agent_abort_rx {
        let agent_cancel = cancel_token.clone();
        let mut shutdown_watch = cancel_rx.clone();
        tokio::spawn(async move {
            tokio::select! {
                res = rx => {
                    if res.is_ok() {
                        // `abort_run` is the agent-protocol's "kill it
                        // now" command — escalate straight to Force so
                        // headless callers don't sit through a graceful
                        // teardown while waiting for `run_finished`.
                        agent_cancel.kill();
                    }
                }
                _ = shutdown_watch.wait_any() => {
                    // Run task is winding down — nothing to cancel.
                }
            }
        });
    }

    let test_handle = tokio::spawn(async move {
        // Hold the local-ws attachment in scope: it must outlive the
        // pinned run future below, since events flowing during the
        // test depend on the attachment's pump task. Drop fires when
        // the run finishes — pump task stops, broadcast goes quiet,
        // standalone-mode Arc releases the listener. Station-mode
        // keeps its own canonical Arc so the listener stays up across
        // runs.
        let _local_ws_attachment = local_ws_attachment;
        // Any surface that could answer a unit-identify prompt. Computed
        // here while every signal is still in scope (tui_ui_tx is moved
        // into the call below). Covers every responder:
        //   - tui_ui_tx: in-terminal form
        //   - agent: agent-protocol stdin (json mode)
        //   - kiosk / local_ws_server: local browser operator UI
        //   - has_publisher: remote dashboard operator over Centrifugo
        //     (standalone `--upload`/deployment runs AND station daemon)
        //   - reuse_unit: engine skips identify entirely, so no prompt
        // Missing any of these would turn the anti-hang guard into a
        // wrongful abort of a run a real operator could have answered.
        let has_ui = tui_ui_tx.is_some()
            || agent_for_test.is_some()
            || kiosk_enabled
            || local_ws_server.is_some()
            || has_publisher
            || reuse_unit.is_some();
        let run_fut = run_test(
            &proc_id,
            &proc_name,
            &proc_dir,
            &proc_framework,
            &entry_file,
            &python_path,
            &execution_id,
            test_creds.as_ref(),
            upload,
            json_mode,
            event_tx,
            tui_ui_tx,
            agent_for_test,
            has_ui,
            reuse_unit,
            operated_by,
            cancel_rx,
        );
        tokio::pin!(run_fut);

        // The engine and OpenHTF connector each emit their own terminal
        // `RunComplete` when cancellation reaches them (engine teardown
        // → `ExecutionEvent::Complete` → `RunComplete`; connector's
        // shutdown path → `RunComplete(ABORTED)`). The outer task just
        // awaits the run future. No synthetic terminal here — emitting
        // one before the engine's caused a Run-again flicker where the
        // operator-UI's pending state got promoted to the dying run's
        // id and stamped ABORTED for one render before the new run's
        // `RunStarted` rebuilt state.
        let exit_code = (&mut run_fut).await;

        if let Some(h) = tui_handle {
            let _ = h.await;
        }

        // Drop the last broadcast sender we hold so publishers can exit
        // their recv loops naturally after draining buffered events. Any
        // downstream task that still holds a sender clone (e.g. a pipe
        // reader in connector::run_openhtf draining Python stdout) will
        // release it shortly after the Python child dies via kill_on_drop.
        drop(teardown_event_tx);

        // Bounded drain so dashboards see the inner terminal RunComplete
        // even when the run was cancelled — same budget either way.
        // Inner 500ms per-publish bound in the Managed publisher loop
        // caps how long a single stuck HTTP call can hold teardown
        // hostage. Upload-result events (`RunUploaded` / `RunUploadFailed`)
        // are emitted during this drain, so the console stream must still
        // be alive here to pick them up.
        let drain = crate::config::timeouts::PUBLISH_DRAIN;
        active_publisher.flush(drain).await;

        // The console stream has now seen every event including the
        // terminal RunComplete and the upload result. Give it a final
        // beat to drain the broadcast, then stop it and flush any
        // buffered lines (TUI mode) to the restored main screen so the
        // operator keeps the phase/log/measurement/error output in their
        // scrollback after the alternate screen is torn down.
        if let Some(h) = console_handle {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            h.abort();
        }
        if let Some(buf) = console_buffer {
            // Recover a poisoned lock — never silently drop the whole log.
            let lines = buf.lock().unwrap_or_else(|e| e.into_inner());
            for (level, msg) in lines.iter() {
                level.print(msg);
            }
        }
        ui_response_handle.abort();
        if let Some(h) = agent_stdin_handle {
            h.abort();
        }
        if let Some(agent) = agent_for_finish {
            // Drain every pending event (phase_started / phase_finished /
            // plan / ui_*) before announcing the run is done, so agents
            // always see the full lifecycle before run_finished.
            agent.emitter.flush().await;
            agent.emitter.enqueue(CliEvent::RunFinished {
                outcome: outcomes::from_exit_code(exit_code).to_string(),
                exit_code,
            });
            agent.emitter.flush().await;
            // No events may legitimately follow run_finished. Finalize so
            // any late producer (late crash handler, stray logger) has its
            // events dropped rather than breaking the "run_finished is
            // last" invariant agents rely on. Also flip lifecycle so a
            // post-hoc `get_state` / `abort_run` sees Finished.
            agent.mark_lifecycle(RunLifecycle::Finished).await;
            agent.emitter.finalize();
        }

        let _ = done_tx.send(exit_code);
    });

    RunHandle {
        done_rx,
        cancel: cancel_token,
        ui_response_tx,
        tui_presence_tx,
        task: Some(test_handle),
    }
}

/// Blocking run: resolve source + start + await completion.
#[allow(clippy::too_many_arguments)]
pub async fn run_cmd(
    source: RunSource,
    json_mode: bool,
    creds: Option<&Credentials>,
    agent_opts: AgentProtoOptions,
    tui_override: Option<bool>,
    kiosk_override: Option<bool>,
    bootstrap_enabled: bool,
) -> i32 {
    let resolved = match resolve_source(source, json_mode) {
        Ok(r) => r,
        Err(code) => return code,
    };

    // Captured before `resolved` is consumed below; emitted after the run
    // completes to nudge linked-but-local runs toward `--upload`.
    let link_hint = resolved.link_hint.clone();

    let publisher = if resolved.upload {
        creds.map(|c| EventPublisher::Standalone { creds: c.clone() })
    } else {
        None
    };

    let handle = start(
        &resolved.id,
        resolved.dir,
        resolved.upload,
        json_mode,
        creds,
        publisher,
        agent_opts,
        // Standalone `tofupilot run` has no stable identity to stamp;
        // let the TUI only receive presence, never publish its own.
        None,
        tui_override,
        kiosk_override,
        // Standalone mode binds its own local-WS server inline if
        // `kiosk_enabled` resolves to true; nothing to inject here.
        None,
        // Standalone runs always identify normally — the
        // "Run again" flow only exists in station mode.
        None,
        // Standalone CLI runs are unattributed; only web-mode operator
        // UI runs forward an `operated_by` email.
        None,
        bootstrap_enabled,
        resolved.yaml_hint,
    )
    .await;
    let code = handle.join().await;

    // Linked dir, but the run stayed local. Surface the link once so the
    // user knows uploading is one flag away — without ever uploading
    // implicitly.
    if let Some(label) = link_hint {
        if !json_mode {
            crate::log::info(&format!(
                "Linked to {label}. Run with --upload to sync this run to the dashboard."
            ));
        }
    }
    code
}

// Resolve a UI preference: explicit CLI flag wins, otherwise read the
// station-config key, otherwise the built-in default. Used for both
// `terminal_ui` and `kiosk_ui` so the precedence stays consistent.
fn resolve_ui_pref(config_key: &str, cli_override: Option<bool>, default_on: bool) -> bool {
    if let Some(v) = cli_override {
        return v;
    }
    db::open()
        .ok()
        .and_then(|db| db.get_config(config_key).ok().flatten())
        .map(|v| v == "on")
        .unwrap_or(default_on)
}

/// Shared sink for the console log stream. When the TUI owns the
/// alternate screen, lines accumulate here and flush after teardown;
/// otherwise the formatter prints them live and this is `None`.
type ConsoleBuffer = Arc<std::sync::Mutex<Vec<(ConsoleLevel, String)>>>;

/// Render a single run event as a console line. Drives the human-
/// readable stream for a `tofupilot run` that isn't in `--json`/agent
/// mode, so the run is never silent: it surfaces phase boundaries,
/// live phase/plug logs, per-measurement results, the crash reason for
/// a run that dies before any phase, the upload result, and the final
/// outcome. The structured NDJSON stream (`agent_proto`) is the machine
/// counterpart — this is the same data, formatted for a person.
fn console_log_event(ev: StationEvent, buffer: Option<&ConsoleBuffer>) {
    use station_protocol::StationEvent as E;

    // A line tagged with the level it renders at. With the TUI active we
    // buffer `(level, text)` and flush through the colored writers after
    // teardown; otherwise we print live the same way.
    let emit = |level: ConsoleLevel, text: String| {
        if let Some(buf) = buffer {
            // Recover a poisoned lock (a panic in a prior format call
            // must not silently swallow the rest of the run's output).
            let mut v = buf.lock().unwrap_or_else(|e| e.into_inner());
            v.push((level, text));
        } else {
            level.print(&text);
        }
    };

    match ev {
        E::PhaseStarted { name, slot_id, .. } => {
            emit(
                ConsoleLevel::Info,
                format!("Phase: {name}{}", slot(&slot_id)),
            );
        }
        E::PhaseLog {
            level,
            message,
            slot_id,
            ..
        } => emit(
            ConsoleLevel::from_log_level(&level),
            format!("{}{message}", slot_prefix(&slot_id)),
        ),
        // Plug logs: only surface warnings and errors. INFO/DEBUG from a
        // plug subprocess is framework transport chatter (e.g. the
        // service's "listening on port" line) — noise for an operator,
        // still available in the NDJSON stream and the TUI.
        E::PlugLog {
            plug_name,
            level,
            message,
            slot_id,
            ..
        } => {
            let level = ConsoleLevel::from_log_level(&level);
            if matches!(level, ConsoleLevel::Warn | ConsoleLevel::Error) {
                emit(
                    level,
                    format!("{}[{plug_name}] {message}", slot_prefix(&slot_id)),
                );
            }
        }
        // Measurements are rendered from `PhaseComplete`, not the live
        // `MeasurementUpdate`: the live event hard-codes `UNSET` (it
        // fires before validation) and frameworks like OpenHTF/pytest
        // only attach measurements at phase completion. `PhaseComplete`
        // carries the validated outcome and the already-raw value.
        E::PhaseComplete {
            name,
            outcome,
            measurements,
            error,
            slot_id,
            ..
        } => {
            let level = ConsoleLevel::from_outcome(&outcome);
            emit(level, format!("Phase {name}: {outcome}{}", slot(&slot_id)));
            for m in &measurements {
                emit(ConsoleLevel::Info, format!("  {}", measurement_line(m)));
            }
            if let Some(err) = error {
                let err = err.trim_end();
                if !err.is_empty() {
                    emit(ConsoleLevel::Error, err.to_string());
                }
            }
        }
        // A run that fails to start (bad procedure, Python bootstrap
        // crash, load error) emits `RunCrashed` before the synthetic
        // `RunComplete(ERROR)`. Without this arm the operator would see
        // only "Run complete: ERROR" with no reason.
        E::RunCrashed { error, .. } => {
            let error = error.trim_end();
            if !error.is_empty() {
                emit(ConsoleLevel::Error, error.to_string());
            }
        }
        E::RunUploaded { dashboard_url, .. } => {
            let url = dashboard_url.map(|u| format!(" — {u}")).unwrap_or_default();
            emit(ConsoleLevel::Success, format!("Uploaded to dashboard{url}"));
        }
        E::RunUploadFailed { error, .. } => {
            emit(ConsoleLevel::Error, format!("Upload failed: {error}"));
        }
        E::RunComplete { outcome, .. } => {
            emit(
                ConsoleLevel::from_outcome(&outcome),
                format!("Run complete: {outcome}"),
            );
        }
        _ => {}
    }
}

/// Format one measurement row: `name = value unit [OUTCOME]`. The value
/// is already raw JSON (`to_raw_json` ran in the engine), so strings
/// render unquoted and everything else uses its compact JSON form.
fn measurement_line(m: &station_protocol::RunMeasurement) -> String {
    let value = match &m.measured_value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Null) | None => "—".to_string(),
        Some(other) => other.to_string(),
    };
    let unit = m
        .units
        .as_deref()
        .map(|u| format!(" {u}"))
        .unwrap_or_default();
    format!("{} = {value}{unit} [{}]", m.name, m.outcome)
}

/// `" (slot X)"` suffix for a phase/run line, empty for single-slot.
fn slot(slot_id: &Option<String>) -> String {
    slot_id
        .as_deref()
        .filter(|s| !s.is_empty() && *s != "default")
        .map(|s| format!(" (slot {s})"))
        .unwrap_or_default()
}

/// `"[slot X] "` prefix for an indented log line in a multi-slot run.
fn slot_prefix(slot_id: &Option<String>) -> String {
    slot_id
        .as_deref()
        .filter(|s| !s.is_empty() && *s != "default")
        .map(|s| format!("[slot {s}] "))
        .unwrap_or_default()
}

/// Console severity for a streamed run line — picks the matching
/// `crate::log` writer (live) or buffers it (deferred flush).
#[derive(Clone, Copy)]
enum ConsoleLevel {
    Info,
    Success,
    Warn,
    Error,
}

impl ConsoleLevel {
    /// Map a phase/plug log level string to a console severity.
    fn from_log_level(level: &str) -> Self {
        match level.to_ascii_uppercase().as_str() {
            "ERROR" | "CRITICAL" => Self::Error,
            "WARNING" | "WARN" => Self::Warn,
            _ => Self::Info,
        }
    }

    /// Map a phase/run outcome to a console severity. Single source of
    /// truth shared by the phase, run, and crash lines so they never
    /// disagree on what counts as success vs failure.
    fn from_outcome(outcome: &str) -> Self {
        match outcome {
            outcomes::PASS | outcomes::XPASS => Self::Success,
            outcomes::SKIP => Self::Info,
            outcomes::RETRY => Self::Warn,
            _ => Self::Error,
        }
    }

    fn print(self, msg: &str) {
        match self {
            Self::Info => crate::log::info(msg),
            Self::Success => crate::log::success(msg),
            Self::Warn => crate::log::warn(msg),
            Self::Error => crate::log::error(msg),
        }
    }
}

fn resolve_source(source: RunSource, json_mode: bool) -> Result<ResolvedSource, i32> {
    match source {
        RunSource::LocalPath { path, upload } => {
            let (dir, yaml_hint) = classify_local_path(&path)?;

            // A linked local dir carries a `tofupilot.json` binding it to a
            // remote procedure. `--upload` activates that link, uploading
            // the run under the linked procedure id. The env override lets
            // CI point a run at a procedure without a checked-in link file.
            let env_id = std::env::var("TOFUPILOT_PROCEDURE_ID")
                .ok()
                .filter(|s| !s.is_empty());

            if upload {
                // Env wins over the file; only touch `tofupilot.json` when the
                // env override is absent so a pure-env CI run never trips the
                // corrupt-file warning over a file it doesn't use.
                let procedure_id = env_id
                    .or_else(|| crate::commands::link::read_link(&dir).map(|l| l.procedure_id));
                let Some(procedure_id) = procedure_id else {
                    crate::log::error(
                        "Not linked to a procedure. Run `tofupilot link` first, or set TOFUPILOT_PROCEDURE_ID.",
                    );
                    return Err(1);
                };
                // Downstream, the connector still keys `deployment_id` off
                // this procedure_id via PullState. A purely linked dir has no
                // PullState, so the upload carries no deployment_id (correct).
                // The one overlap: if the same procedure was *also* pulled,
                // the run inherits that deployment_id — acceptable, since it's
                // the same procedure and the association is informational.
                return Ok(ResolvedSource {
                    id: procedure_id,
                    dir,
                    upload: true,
                    link_hint: None,
                    yaml_hint,
                });
            }

            // Local-only run. If the dir is linked, stash a label so the
            // post-run hint can suggest `--upload`.
            let link_hint = crate::commands::link::read_link(&dir).map(|l| {
                l.procedure_name
                    .map(|n| format!("{n} ({})", l.procedure_id))
                    .unwrap_or(l.procedure_id)
            });
            let id = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("local")
                .to_string();
            Ok(ResolvedSource {
                id,
                dir,
                upload: false,
                link_hint,
                yaml_hint,
            })
        }
        RunSource::Deployment(id_arg) => {
            let id = resolve_procedure_id(id_arg.as_deref(), json_mode)?;
            let dir = resolve_procedure_dir(&id)?;
            Ok(ResolvedSource {
                id,
                dir,
                upload: true,
                link_hint: None,
                yaml_hint: None,
            })
        }
    }
}

/// Resolve a local path to a (procedure_dir, yaml_hint) pair.
/// - file: parent dir is the procedure dir.
/// - directory: path is the procedure dir.
fn classify_local_path(path: &Path) -> Result<(PathBuf, Option<PathBuf>), i32> {
    let canonical = execution_engine::path_utils::canonicalize_for_spawn(path).map_err(|e| {
        crate::log::error(&format!("Cannot resolve {}: {e}", path.display()));
        1
    })?;
    if canonical.is_dir() {
        Ok((canonical, None))
    } else if canonical.is_file() {
        let parent = canonical.parent().map(|p| p.to_path_buf()).ok_or_else(|| {
            crate::log::error(&format!(
                "Cannot determine parent dir of {}",
                canonical.display()
            ));
            1
        })?;
        Ok((parent, Some(canonical)))
    } else {
        crate::log::error(&format!("Path does not exist: {}", path.display()));
        Err(1)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

// Framework-agnostic test driver, one caller. Detection happened in
// `start()`; this is pure dispatch.
#[allow(clippy::too_many_arguments)]
async fn run_test(
    procedure_id: &str,
    procedure_name: &str,
    // Where the procedure source, venv, and cwd all live. Equal to the
    // deployment root for single-package bundles, equal to
    // `<deployment>/<root_directory>` for workspace-mode bundles.
    package_dir: &Path,
    framework: &Framework,
    entry_file: &Path,
    python_path: &Path,
    execution_id: &str,
    creds: Option<&Credentials>,
    upload: bool,
    json_mode: bool,
    event_tx: broadcast::Sender<StationEvent>,
    ui_tx: Option<mpsc::Sender<execution_engine::ui::UiRequestData>>,
    agent: Option<AgentProtoCtx>,
    // Whether any operator surface exists (TUI / kiosk / agent / station).
    // Threaded to the identify host so a headless run fails fast instead
    // of hanging on a unit prompt nobody can answer.
    has_ui: bool,
    reuse_unit: Option<station_protocol::UnitInfo>,
    // Email forwarded to `runs.create` as `operated_by`. Set when the
    // run was triggered from the web operator UI; None for kiosk and
    // CLI-driven runs.
    operated_by: Option<String>,
    // Single cancellation surface for every framework path. Each driver
    // clones the receiver and selects on `wait_any` (graceful) and/or
    // `wait_force` (escalation). One channel, one source of truth —
    // replaces the prior trio of oneshot pairs.
    cancel_rx: cancel::Receiver,
) -> i32 {
    match framework {
        Framework::Yaml(procedure_yaml) => {
            // The engine needs the package dir so file-path module references
            // like `python: phases.foo` resolve to `<package>/phases/foo.py`.
            // Workspace siblings shipped as wheels (e.g. `shared`) resolve
            // through the venv's site-packages via `tp_worker.py`'s importlib
            // fallback.
            let agent_for_upload = agent.clone();
            let bus_for_upload = event_tx.clone();
            let (exit_code, queued_run) = engine::run_yaml_procedure(
                procedure_yaml,
                package_dir,
                python_path,
                procedure_id,
                procedure_name,
                execution_id,
                event_tx,
                ui_tx,
                agent,
                has_ui,
                reuse_unit,
                operated_by,
                cancel_rx,
            )
            .await;

            if upload {
                if let (Some(creds), Some(queued)) = (creds, queued_run) {
                    spawn_upload(
                        creds,
                        procedure_id,
                        queued,
                        json_mode,
                        agent_for_upload.as_ref(),
                        Some(bus_for_upload),
                    );
                }
            }

            exit_code
        }
        Framework::Openhtf => {
            // OpenHTF has no force/graceful distinction — both Stop and
            // Kill collapse to `graceful_shutdown` (SIGTERM → wait →
            // SIGKILL via the process group). The cancel receiver is
            // enough; the connector subscribes once and maps the watch
            // state to its single shutdown path.
            //
            // cwd = package_dir so monorepo openhtf can import sibling
            // files and resolve relative paths the same way
            // single-package openhtf does. The connector script itself
            // is written into package_dir and cleaned up by RAII.
            connector::run_openhtf(
                python_path,
                entry_file,
                package_dir,
                procedure_id,
                procedure_name,
                execution_id,
                creds,
                upload,
                json_mode,
                event_tx,
                ui_tx,
                agent,
                has_ui,
                reuse_unit,
                operated_by,
                cancel_rx,
            )
            .await
        }
        Framework::Pytest => {
            // pytest connector mirrors the OpenHTF connector's spawn
            // shape — same identify-unit handshake, same event router
            // wiring. Differences (no operator prompts, simpler outcome
            // mapping) are encapsulated inside `run_pytest`.
            connector::run_pytest(
                python_path,
                entry_file,
                package_dir,
                procedure_id,
                procedure_name,
                execution_id,
                creds,
                upload,
                json_mode,
                event_tx,
                ui_tx,
                agent,
                has_ui,
                reuse_unit,
                operated_by,
                cancel_rx,
            )
            .await
        }
        Framework::Robot => {
            // Robot connector is a near-mirror of the pytest path:
            // same identify-unit handshake, same wire enum, no
            // operator prompts. Each Robot test case becomes one
            // phase; measurements arrive via the embedded
            // `tofupilot_robot` keyword library.
            connector::run_robot(
                python_path,
                entry_file,
                package_dir,
                procedure_id,
                procedure_name,
                execution_id,
                creds,
                upload,
                json_mode,
                event_tx,
                ui_tx,
                agent,
                has_ui,
                reuse_unit,
                operated_by,
                cancel_rx,
            )
            .await
        }
        Framework::Plain => {
            // No phases, no UI prompts — just exec a script. The
            // operator-UI still needs RunStarted + RunComplete to escape
            // the pending state seeded by handleRun, so pass a context.
            // `execute` consumes the broadcast sender via the context —
            // we move `event_tx` into it, the caller doesn't need it
            // anymore.
            let ctx = python::PlainRunContext {
                procedure_id: procedure_id.to_string(),
                procedure_name: procedure_name.to_string(),
                execution_id: execution_id.to_string(),
                event_tx,
            };
            python::execute(
                python_path,
                entry_file,
                package_dir,
                json_mode,
                cancel_rx,
                Some(ctx),
            )
            .await
        }
    }
}

pub fn resolve_procedure_dir(procedure_id: &str) -> Result<PathBuf, i32> {
    // Reject empty / "." / ".." / path separators before joining: an
    // empty string lets `dir.join("")` collapse to the deployments
    // root (which is_dir == true), and the YAML detector then sees no
    // procedure.yaml and falls into the "No Python entry point" arm.
    // A stale Run command carrying an empty procedure_id once triggered
    // this; validating here keeps any future regression contained
    // regardless of what the caller sends.
    if procedure_id.is_empty()
        || procedure_id == "."
        || procedure_id == ".."
        || procedure_id.contains('/')
        || procedure_id.contains('\\')
    {
        crate::log::error(&format!("Invalid procedure id: {procedure_id:?}",));
        return Err(1);
    }
    let dir = db::deployments_dir().map_err(|e| {
        crate::log::error(&format!("Failed to resolve tofupilot directory: {e}"));
        1
    })?;
    let path = dir.join(procedure_id);
    if !path.is_dir() {
        crate::log::error(&format!(
            "Procedure '{procedure_id}' not found in deployments."
        ));
        return Err(1);
    }
    Ok(path)
}

fn resolve_procedure_id(procedure_id_arg: Option<&str>, json_mode: bool) -> Result<String, i32> {
    if let Some(id) = procedure_id_arg {
        return Ok(id.to_string());
    }

    let dir = db::deployments_dir().map_err(|e| {
        crate::log::error(&format!("Failed to resolve tofupilot directory: {e}"));
        1
    })?;

    if !dir.exists() {
        crate::log::error("No deployments found. Run `tofupilot pull` first.");
        return Err(1);
    }

    let mut procedures: Vec<(String, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| {
        crate::log::error(&format!("Failed to read deployments directory: {e}"));
        1
    })? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(id) = path.file_name().and_then(|n| n.to_str()) {
            if id.ends_with(".tmp") || id.ends_with(".old") || id.ends_with(".staging") {
                continue;
            }
            procedures.push((id.to_string(), path));
        }
    }

    if procedures.is_empty() {
        crate::log::error("No deployments found. Run `tofupilot pull` first.");
        return Err(1);
    }

    procedures.sort_by(|a, b| a.0.cmp(&b.0));

    if procedures.len() == 1 {
        return Ok(procedures.into_iter().next().expect("len checked == 1").0);
    }

    if json_mode {
        crate::log::error("Multiple deployments found. Use --procedure to select one.");
        for (id, _) in &procedures {
            println!(
                "{}",
                serde_json::json!({ "type": "procedure", "procedure_id": id })
            );
        }
        return Err(1);
    }

    let db = db::open().map_err(|e| {
        crate::log::error(&format!("Failed to open database: {e}"));
        1
    })?;

    let labels: Vec<String> = procedures
        .iter()
        .map(|(id, _)| {
            let suffix = match db.get_pull_state(id) {
                Ok(Some(state)) => format!(" ({})", &state.sha[..7.min(state.sha.len())]),
                _ => String::new(),
            };
            format!("{id}{suffix}")
        })
        .collect();
    // Release the redb lock before blocking on operator input — the
    // picker can sit open indefinitely and other tofupilot processes
    // would hit the busy error.
    drop(db);

    let selection = dialoguer::FuzzySelect::new()
        .with_prompt("Select a procedure to run (type to filter)")
        .items(&labels)
        .default(0)
        .interact()
        .map_err(|_| {
            crate::log::info("Selection cancelled.");
            1
        })?;

    Ok(procedures
        .into_iter()
        .nth(selection)
        .expect("selection is a valid index into procedures")
        .0)
}

fn spawn_upload(
    creds: &Credentials,
    procedure_id: &str,
    mut queued: queue::QueuedRun,
    json_mode: bool,
    agent: Option<&AgentProtoCtx>,
    bus: Option<tokio::sync::broadcast::Sender<station_protocol::StationEvent>>,
) {
    let queue_id = queue::new_queue_id(procedure_id);

    let db = match db::open() {
        Ok(db) => db,
        Err(e) => {
            crate::log::error(&format!("Failed to open database: {e}"));
            return;
        }
    };

    if let Err(e) = queue::enqueue(&db, &queue_id, &mut queued, bus.as_ref()) {
        crate::log::error(&format!("Failed to queue run: {e}"));
        return;
    }

    if json_mode {
        println!(
            "{}",
            serde_json::json!({"type": "upload_queued", "queue_id": queue_id})
        );
    }
    if let Some(agent) = agent {
        agent.emitter.enqueue(CliEvent::RunUploadQueued {
            queue_id: queue_id.clone(),
            procedure_id: Some(queued.request.procedure_id.clone()),
            outcome: Some(queued.request.outcome.to_string()),
            serial_number: Some(queued.request.serial_number.clone()),
            attachment_count: Some(queued.attachments.len() as u32),
            queued_at: queued.queued_at.clone(),
        });
    }

    let upload_creds = creds.clone();
    let bus_for_task = bus.clone();
    tokio::spawn(async move {
        queue::upload_queued_run(
            crate::http::client(),
            &upload_creds,
            &queue_id,
            &queued,
            bus_for_task.as_ref(),
            true,
        )
        .await;
    });
}

#[cfg(test)]
mod prepare_run_tests {
    use super::*;
    use std::fs;

    fn write_manifest(dir: &Path, root_directory: Option<&str>) {
        let rd = match root_directory {
            Some(s) => format!("\"{s}\""),
            None => "null".into(),
        };
        let body = format!(
            r#"{{"version":1,"kind":"source","mode":"sync","root_directory":{rd},"runtime_version":"3.12.13","platform":null}}"#,
        );
        fs::write(dir.join("manifest.json"), body).unwrap();
    }

    fn touch_venv(package_dir: &Path) -> PathBuf {
        let python = if cfg!(target_os = "windows") {
            package_dir.join("venv").join("Scripts").join("python.exe")
        } else {
            package_dir.join("venv").join("bin").join("python")
        };
        fs::create_dir_all(python.parent().unwrap()).unwrap();
        fs::write(&python, b"").unwrap();
        python
    }

    fn touch_procedure_yaml(package_dir: &Path) {
        fs::write(package_dir.join("procedure.yaml"), b"name: test\n").unwrap();
    }

    /// A manifest `entry_point` ending in `.yaml`/`.yml` forces YAML
    /// detection even when the file isn't named `procedure.yaml` and
    /// even when nothing else on disk hints at a framework. This is the
    /// only way to run a procedure file under a custom name.
    #[test]
    fn detect_honors_yaml_entry_point_with_custom_name() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path();
        // No procedure.yaml on disk — detection must come purely from
        // the manifest entry. (The file need not exist for detection;
        // the loader opens it later.)
        let fw = Framework::detect(pkg, Some("my-test.yaml"));
        match fw {
            Framework::Yaml(p) => assert_eq!(p, pkg.join("my-test.yaml")),
            other => panic!("expected Yaml, got {other:?}"),
        }
    }

    /// A non-YAML entry point (e.g. a pytest sentinel `main.py`) must
    /// NOT be misclassified as YAML — the extension check is exact.
    #[test]
    fn detect_ignores_non_yaml_entry_point() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path();
        // main.py present → without a yaml entry this is Openhtf/Plain,
        // never Yaml.
        fs::write(pkg.join("main.py"), b"print('hi')\n").unwrap();
        let fw = Framework::detect(pkg, Some("main.py"));
        assert!(
            !matches!(fw, Framework::Yaml(_)),
            "main.py entry must not be YAML, got {fw:?}",
        );
    }

    /// `procedure.yaml` on disk still wins when no entry point is set —
    /// the legacy auto-discovery path is unchanged.
    #[test]
    fn detect_falls_back_to_procedure_yaml_when_no_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path();
        touch_procedure_yaml(pkg);
        let fw = Framework::detect(pkg, None);
        match fw {
            Framework::Yaml(p) => assert_eq!(p, pkg.join("procedure.yaml")),
            other => panic!("expected Yaml, got {other:?}"),
        }
    }

    /// Monorepo: manifest carries `root_directory`, venv lives at
    /// `<deployment>/<root_directory>/venv`. The bug the first reviewer
    /// caught was passing `deployment_dir` to `deployment_python` —
    /// this test pins the correct wiring.
    #[tokio::test]
    async fn prepare_run_resolves_venv_under_root_directory_for_monorepo() {
        let tmp = tempfile::tempdir().unwrap();
        let deployment = tmp.path();
        let package = deployment.join("procedures").join("ft_device_a");
        fs::create_dir_all(&package).unwrap();
        write_manifest(deployment, Some("procedures/ft_device_a"));
        touch_procedure_yaml(&package);
        let expected_python = touch_venv(&package);

        let prepared = prepare_run(deployment, false, None)
            .await
            .expect("prepare_run should succeed");
        assert_eq!(prepared.package_dir, package);
        assert_eq!(prepared.python_path, expected_python);
    }

    /// Single-package: no `root_directory`, venv at `<deployment>/venv`,
    /// `package_dir` collapses to deployment root.
    #[tokio::test]
    async fn prepare_run_resolves_venv_at_deployment_root_for_single_package() {
        let tmp = tempfile::tempdir().unwrap();
        let deployment = tmp.path();
        write_manifest(deployment, None);
        touch_procedure_yaml(deployment);
        let expected_python = touch_venv(deployment);

        let prepared = prepare_run(deployment, false, None)
            .await
            .expect("prepare_run should succeed");
        assert_eq!(prepared.package_dir, deployment);
        assert_eq!(prepared.python_path, expected_python);
    }

    /// Manifest present but no venv → `env_error` for frameworks that
    /// need Python (openhtf, pytest, plain). YAML procedures don't need
    /// the venv directly so missing-venv there is non-fatal — this test
    /// uses a `main.py`-only layout to exercise the failing branch.
    #[tokio::test]
    async fn prepare_run_errors_when_manifest_present_but_venv_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let deployment = tmp.path();
        write_manifest(deployment, None);
        // No procedure.yaml + a main.py → Framework::Plain (or Openhtf/
        // Pytest depending on imports). All three need a venv.
        fs::write(deployment.join("main.py"), b"print('hi')\n").unwrap();

        let err = prepare_run(deployment, false, None)
            .await
            .expect_err("should fail without venv");
        assert_eq!(err.kind, "env_error");
    }

    /// Local-path run with `bootstrap_enabled = false` and no venv:
    /// must surface the bootstrap module's error verbatim through the
    /// `env_error` channel rather than silently provisioning. Pins the
    /// `--no-bootstrap` contract end-to-end.
    #[tokio::test]
    async fn prepare_run_errors_local_path_when_bootstrap_disabled_and_no_venv() {
        let tmp = tempfile::tempdir().unwrap();
        // main.py forces Framework::Plain, which requires a Python env.
        fs::write(tmp.path().join("main.py"), b"print('hi')\n").unwrap();

        let err = prepare_run(tmp.path(), false, None)
            .await
            .expect_err("should fail without venv when bootstrap disabled");
        assert_eq!(err.kind, "env_error");
        assert!(
            err.message.contains("--no-bootstrap"),
            "got: {}",
            err.message,
        );
    }

    /// Local-path run with a pre-existing `venv/` (operator hand-built
    /// before invoking `tofupilot run`). Bootstrap leaves it alone,
    /// stamps it, and returns the interpreter. No `.venv` fallback —
    /// `<project>/venv` is the single canonical location.
    #[tokio::test]
    async fn prepare_run_uses_existing_venv_for_local_path() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        touch_procedure_yaml(project);
        let python = touch_venv(project);

        let prepared = prepare_run(project, true, None)
            .await
            .expect("prepare_run should succeed");
        assert_eq!(prepared.python_path, python);
    }
}
