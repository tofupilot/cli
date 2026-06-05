//! The JSON agent protocol: drive a run programmatically over stdin/stdout.
//!
//! An emitter queues `CliEvent`s to stdout, a reader task parses operator
//! responses from stdin, and pending-request bookkeeping lets a late-attaching
//! consumer reconstruct an in-flight prompt.

pub mod ctx;
pub mod emitter;
pub mod events;
pub mod prebaked;
pub mod reader;
pub mod validate;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, RwLock};

pub use ctx::{AgentProtoCtx, RunLifecycle};
pub use emitter::Emitter;
pub use events::{AgentUiComponent, CliEvent, PhasePlanPayload, UiAutoContinueSource};
pub use prebaked::PreBakedValues;
pub use reader::{spawn_stdin_reader, PendingRequests};

/// Options surfaced from CLI flags into the agent-protocol bootstrap.
#[derive(Default, Clone)]
pub struct Options {
    pub ui_timeout_secs: Option<u64>,
    pub ui_values: Option<std::path::PathBuf>,
}

/// Everything `start()` needs from the agent-protocol subsystem. Bundling
/// the context, the reader handle, and the abort receiver means the
/// caller can't drop one and keep the others — each piece is paired with
/// its lifecycle partner.
///
/// # Drop semantics
///
/// Dropping `Initialized` on a panic path is safe:
/// - `stdin_handle` is a `JoinHandle` whose underlying task is NOT
///   aborted on drop (tokio policy). In normal flow `start()` aborts it
///   explicitly before teardown; on panic the task eventually exits when
///   the stdin file descriptor closes. No leak, no runaway.
/// - `abort_rx` drops cleanly; the matching `abort_tx` lives inside
///   `ctx.abort_tx` so a producer still sees "receiver gone" when it
///   tries to signal abort, avoiding a stuck channel.
///
/// If you refactor this bundle (add a field, split it, etc.) preserve
/// these semantics explicitly — future callers must not assume
/// `stdin_handle` survives past the bundle's lifetime.
pub struct Initialized {
    pub ctx: AgentProtoCtx,
    pub stdin_handle: tokio::task::JoinHandle<()>,
    pub abort_rx: oneshot::Receiver<()>,
}

/// Spin up the full agent-protocol stack: emitter, pending map, prebaked
/// values, lifecycle context, stdin reader. Enqueues `run_started` with
/// the lifecycle already flipped to Running so a late `get_state` can't
/// see NotStarted + a non-empty plan.
///
/// Returns `Ok(None)` when agent protocol is disabled (not json_mode or
/// TUI active). Returns `Err(exit_code)` on fatal config errors (e.g.
/// `--ui-values` outside `procedure_dir`).
pub async fn initialize(
    json_mode: bool,
    tui_enabled: bool,
    procedure_id: &str,
    procedure_dir: &Path,
    options: &Options,
) -> Result<Option<Initialized>, i32> {
    if !json_mode || tui_enabled {
        return Ok(None);
    }

    let emitter = Emitter::new();
    let pending = Arc::new(RwLock::new(PendingRequests::default()));

    let prebaked = match &options.ui_values {
        Some(path) => PreBakedValues::load(path, procedure_dir).map_err(|e| {
            crate::log::error(&format!("Failed to load --ui-values: {e}"));
            1
        })?,
        None => PreBakedValues::default(),
    };

    let ui_timeout = options.ui_timeout_secs.map(Duration::from_secs);
    let (abort_tx, abort_rx) = oneshot::channel::<()>();
    let ctx = AgentProtoCtx::new(emitter.clone(), pending, prebaked, ui_timeout, abort_tx);

    // Flip lifecycle → Running *before* enqueueing run_started so an
    // agent's get_state never observes run_status=NotStarted alongside
    // a non-empty phases list (the engine sink can enqueue `plan` the
    // instant after run_started lands).
    ctx.mark_lifecycle(RunLifecycle::Running).await;
    emitter.enqueue(CliEvent::RunStarted {
        procedure_id: procedure_id.to_string(),
        protocol_version: events::PROTOCOL_VERSION,
    });

    let stdin_handle = spawn_stdin_reader(ctx.clone());
    Ok(Some(Initialized {
        ctx,
        stdin_handle,
        abort_rx,
    }))
}
