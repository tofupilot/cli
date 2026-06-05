//! Shared context for the agent protocol: the run lifecycle handle, the event
//! emitter, and the pending-UI-request map.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::{oneshot, RwLock};

use super::emitter::Emitter;
use super::events::{PhaseSnapshot, RunStatus};
use super::prebaked::PreBakedValues;
use super::reader::PendingRequests;

/// Internal three-state lifecycle. Mirrors `RunStatus` but kept private so
/// we can evolve it (add transient states like `Aborting`) without breaking
/// the wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunLifecycle {
    NotStarted,
    Running,
    Finished,
}

impl From<RunLifecycle> for RunStatus {
    fn from(lc: RunLifecycle) -> Self {
        match lc {
            RunLifecycle::NotStarted => RunStatus::NotStarted,
            RunLifecycle::Running => RunStatus::Running,
            RunLifecycle::Finished => RunStatus::Finished,
        }
    }
}

/// Running snapshot of phase lifecycle. Updated by the engine sink on
/// every `phase_started` / `phase_finished` / `phase_skipped`, queried
/// by the stdin reader when an agent sends `get_state`.
#[derive(Default)]
pub struct PhaseHistory {
    pub phases: Vec<PhaseSnapshot>,
}

impl PhaseHistory {
    pub fn started(&mut self, phase_key: &str, attempt: u32, slot_id: Option<String>) {
        self.phases.push(PhaseSnapshot {
            phase_key: phase_key.to_string(),
            status: "started".to_string(),
            attempt,
            slot_id,
            outcome: None,
        });
    }

    pub fn finished(
        &mut self,
        phase_key: &str,
        attempt: u32,
        slot_id: &Option<String>,
        outcome: &str,
    ) {
        if let Some(p) = self
            .phases
            .iter_mut()
            .rev()
            .find(|p| p.phase_key == phase_key && p.attempt == attempt && p.slot_id == *slot_id)
        {
            p.status = "finished".to_string();
            p.outcome = Some(outcome.to_string());
        }
    }

    pub fn skipped(&mut self, phase_key: &str, slot_id: Option<String>) {
        self.phases.push(PhaseSnapshot {
            phase_key: phase_key.to_string(),
            status: "skipped".to_string(),
            attempt: 1,
            slot_id,
            outcome: Some(super::super::outcomes::SKIP.to_string()),
        });
    }
}

/// Shared context used by the engine's event sink and the stdin reader.
#[derive(Clone)]
pub struct AgentProtoCtx {
    pub emitter: Emitter,
    pub pending: Arc<RwLock<PendingRequests>>,
    pub prebaked: PreBakedValues,
    pub ui_timeout: Option<Duration>,
    /// Phase lifecycle history. Sync Mutex because the engine sink's
    /// `EventSink::emit` is sync — no `.await` allowed inside.
    pub history: Arc<StdMutex<PhaseHistory>>,
    /// Sent by the stdin reader when an `abort_run` command arrives; the
    /// run's `start()` loop selects on it alongside its other cancel path.
    pub abort_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
    /// Run-level lifecycle. Transitions: NotStarted → Running on the first
    /// `run_started` emit, Running → Finished on `run_finished`. The
    /// stdin reader consults this so `get_state` / `abort_run` can
    /// respond meaningfully before the run has booted or after it ends.
    lifecycle: Arc<RwLock<RunLifecycle>>,
}

impl AgentProtoCtx {
    pub fn new(
        emitter: Emitter,
        pending: Arc<RwLock<PendingRequests>>,
        prebaked: PreBakedValues,
        ui_timeout: Option<Duration>,
        abort_tx: oneshot::Sender<()>,
    ) -> Self {
        Self {
            emitter,
            pending,
            prebaked,
            ui_timeout,
            history: Arc::new(StdMutex::new(PhaseHistory::default())),
            abort_tx: Arc::new(RwLock::new(Some(abort_tx))),
            lifecycle: Arc::new(RwLock::new(RunLifecycle::NotStarted)),
        }
    }

    pub async fn mark_lifecycle(&self, state: RunLifecycle) {
        *self.lifecycle.write().await = state;
    }

    pub async fn lifecycle_status(&self) -> RunStatus {
        (*self.lifecycle.read().await).into()
    }
}
