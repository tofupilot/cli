mod completion;
mod events;
mod execution;
mod initialization;
mod jobs;
mod plugs;
mod scheduling;
mod shutdown;
mod stats;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock, Semaphore};
use uuid::Uuid;

use crate::job::{JobResult, JobStatus, Outcome};
use crate::reports::ReportManager;
use crate::state::OrchestratorState;
use crate::worker::Worker;
use crate::plugs::manager::ResourceManager;
use crate::procedure::schema::ProcedureDefinition;

// Re-export ExecutionStrategy from schema instead of duplicating
pub use crate::procedure::schema::ExecutionStrategy;

#[derive(Debug)]
pub struct JobCompletionEvent {
    pub job_id: Uuid,
    pub result: Result<JobResult, String>,
    pub original_job: crate::job::Job,
    pub worker_id: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionStats {
    pub total_jobs: usize,
    pub completed_jobs: usize,
    pub failed_jobs: usize,
    pub running_jobs: usize,
    pub queued_jobs: usize,
    pub workers_busy: usize,
    pub workers_total: usize,
    pub run_outcome: Option<Outcome>,
    pub run_dir: Option<String>,
    pub run_id: Option<String>,
    pub slot_outcomes: HashMap<String, Outcome>,
    pub slot_run_ids: HashMap<String, String>,
    pub start_time: Option<chrono::DateTime<chrono::Utc>>,
    pub end_time: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobProgress {
    pub job_id: String,
    pub slot_id: Option<String>,
    pub phase_key: String,
    pub phase_name: String,
    pub stage_scope: crate::procedure::schema::StageScope,
    pub status: JobStatus,
    pub worker_id: Option<usize>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub timeout_ms: Option<u64>,
    pub outcome: Option<Outcome>,
    pub retry_count: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobCompleteEvent {
    pub job_id: String,
    pub slot_id: Option<String>,
    pub phase_key: String,
    pub phase_name: String,
    pub stage_scope: crate::procedure::schema::StageScope,
    pub outcome: Outcome,
    pub action: String,
    pub next_action: Option<String>,
    pub measurements: Vec<crate::measurements::Measurement>,
    pub attachments: Vec<String>,
    pub logs: Vec<crate::log::LogEntry>,
    pub resource_metrics: Option<crate::job::ResourceMetrics>,
    pub retry_count: usize,
    pub retry_limit: usize,
    pub started_at: String,
    pub completed_at: String,
    pub duration_ms: u64,
    pub worker_id: usize,
    pub error: Option<String>,
}

// PlannedPhase and PlannedPlug are defined in crate::event_sink and re-exported from crate root
pub use crate::{PlannedPhase, PlannedPlug};

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionPlan {
    pub phases: Vec<PlannedPhase>,
    pub plugs_all: Vec<PlannedPlug>,
    pub plugs_each: Vec<PlannedPlug>,
    pub slots: Vec<String>,
    pub total_expected_jobs: u32,
}

pub struct Orchestrator {
    pub state: Arc<RwLock<OrchestratorState>>,
    pub workers: Arc<RwLock<Vec<Worker>>>,
    pub resource_manager: Arc<RwLock<ResourceManager>>,
    pub(super) report_managers: Arc<RwLock<HashMap<String, ReportManager>>>,
    pub(super) job_semaphore: Arc<Semaphore>,
    pub(super) procedure_dir: std::path::PathBuf,
    /// Pre-resolved Python interpreter for workers + plug services. When
    /// set, downstream consumers skip the engine's `resolve_python`
    /// walk-up. CLI runs always set this; legacy callers leave it `None`.
    pub(super) python_path: Option<std::path::PathBuf>,
    pub(super) completion_tx: mpsc::Sender<JobCompletionEvent>,
    pub(super) completion_rx: Option<mpsc::Receiver<JobCompletionEvent>>,
    pub(super) run_id: String,
    pub execution_id: String,
    pub(super) procedure_definition: ProcedureDefinition,
    pub(super) procedure_plugs_created: Arc<RwLock<bool>>,
    pub(super) slot_plugs_created: Arc<RwLock<HashSet<String>>>,
    pub(super) event_sink: Arc<dyn crate::EventSink>,
    pub(super) start_time: Option<chrono::DateTime<chrono::Utc>>,
    pub(super) end_time: Option<chrono::DateTime<chrono::Utc>>,
    pub(super) initial_unit_infos: HashMap<String, crate::unit::UnitInfo>,
}

impl Orchestrator {
    #[must_use = "orchestrator must be initialized before use"]
    pub fn new(
        worker_count: usize,
        procedure_dir: std::path::PathBuf,
        execution_id: String,
        run_id: String,
        procedure_definition: ProcedureDefinition,
    ) -> Self {
        Self::new_with_python(
            worker_count,
            procedure_dir,
            None,
            execution_id,
            run_id,
            procedure_definition,
        )
    }

    /// Construct an orchestrator with a pre-resolved Python interpreter.
    /// CLI runs use this so workers + plug services skip the engine's
    /// `resolve_python` walk-up. The path comes from the deterministic
    /// `<package_dir>/venv/python` resolver in CLI's `prepare_run`.
    #[must_use = "orchestrator must be initialized before use"]
    pub fn new_with_python(
        worker_count: usize,
        procedure_dir: std::path::PathBuf,
        python_path: Option<std::path::PathBuf>,
        execution_id: String,
        run_id: String,
        procedure_definition: ProcedureDefinition,
    ) -> Self {
        let mut workers = Vec::new();
        for i in 0..worker_count {
            workers.push(Worker::new_with_python(
                i,
                procedure_dir.clone(),
                python_path.clone(),
            ));
        }

        let num_workers = workers.len();

        // Bounded so a lagging consumer (the schedule loop holding
        // locks while recursing) doesn't pile up `JobCompletionEvent`s
        // — each carries the full `Job` plus result (logs,
        // measurements). Capacity covers a couple of fully-saturated
        // worker pools plus headroom; senders `.send().await` so
        // backpressure flows back into the worker task naturally.
        let capacity = num_workers.max(1) * 8 + 16;
        let (tx, rx) = mpsc::channel(capacity);

        Self {
            state: Arc::new(RwLock::new(OrchestratorState::new(num_workers))),
            workers: Arc::new(RwLock::new(workers)),
            resource_manager: Arc::new(RwLock::new(ResourceManager::new_with_python(
                procedure_dir.clone(),
                python_path.clone(),
            ))),
            report_managers: Arc::new(RwLock::new(HashMap::new())),
            job_semaphore: Arc::new(Semaphore::new(
                crate::constants::limits::MAX_CONCURRENT_JOBS,
            )),
            procedure_dir,
            python_path,
            completion_tx: tx,
            completion_rx: Some(rx),
            run_id,
            execution_id,
            procedure_definition,
            procedure_plugs_created: Arc::new(RwLock::new(false)),
            slot_plugs_created: Arc::new(RwLock::new(HashSet::new())),
            event_sink: Arc::new(crate::NullSink),
            start_time: None,
            end_time: None,
            initial_unit_infos: HashMap::new(),
        }
    }

    pub fn set_initial_unit_infos(&mut self, unit_infos: HashMap<String, crate::unit::UnitInfo>) {
        self.initial_unit_infos = unit_infos;
    }

    pub fn set_event_sink(&mut self, sink: Arc<dyn crate::EventSink>) {
        self.event_sink = sink;
    }
}
