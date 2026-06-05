use crate::events::{PlugLogEvent, PlugStatusUpdateEvent, UiUpdateEvent};
use crate::job::{JobStatus, Outcome};
use crate::log::LogEntry;
use crate::measurements::Measurement;
use crate::procedure::schema::StageScope;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

/// Unified execution event emitted by the engine.
/// Every consumer (Studio, CLI TUI, WebSocket, web UI) must handle all variants.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionEvent {
    /// Execution plan computed, about to start
    Plan {
        phases: Vec<PlannedPhase>,
        plugs_all: Vec<PlannedPlug>,
        plugs_each: Vec<PlannedPlug>,
        slots: Vec<String>,
        total_expected_jobs: u32,
    },

    /// Job lifecycle update (queued, running, stopping)
    JobProgress {
        job_id: String,
        slot_id: Option<String>,
        phase_key: String,
        phase_name: String,
        stage_scope: StageScope,
        status: JobStatus,
        worker_id: Option<usize>,
        started_at: Option<chrono::DateTime<chrono::Utc>>,
        timeout_ms: Option<u64>,
        outcome: Option<Outcome>,
        retry_count: usize,
        error: Option<String>,
    },

    /// Job completed with full results
    JobComplete {
        job_id: String,
        slot_id: Option<String>,
        phase_key: String,
        phase_name: String,
        stage_scope: StageScope,
        outcome: Outcome,
        action: String,
        next_action: Option<String>,
        measurements: Vec<Measurement>,
        attachments: Vec<String>,
        logs: Vec<LogEntry>,
        resource_metrics: Option<crate::job::ResourceMetrics>,
        retry_count: usize,
        retry_limit: usize,
        started_at: String,
        completed_at: String,
        duration_ms: u64,
        worker_id: usize,
        error: Option<String>,
    },

    /// Execution statistics snapshot
    Stats {
        total_jobs: usize,
        completed_jobs: usize,
        failed_jobs: usize,
        running_jobs: usize,
        queued_jobs: usize,
        workers_busy: usize,
        workers_total: usize,
        run_outcome: Option<Outcome>,
        run_dir: Option<String>,
        run_id: Option<String>,
        slot_outcomes: HashMap<String, Outcome>,
        slot_run_ids: HashMap<String, String>,
        start_time: Option<chrono::DateTime<chrono::Utc>>,
        end_time: Option<chrono::DateTime<chrono::Utc>>,
    },

    /// Execution complete (final stats)
    Complete {
        total_jobs: usize,
        completed_jobs: usize,
        failed_jobs: usize,
        running_jobs: usize,
        queued_jobs: usize,
        workers_busy: usize,
        workers_total: usize,
        run_outcome: Option<Outcome>,
        run_dir: Option<String>,
        run_id: Option<String>,
        slot_outcomes: HashMap<String, Outcome>,
        slot_run_ids: HashMap<String, String>,
        start_time: Option<chrono::DateTime<chrono::Utc>>,
        end_time: Option<chrono::DateTime<chrono::Utc>>,
    },

    /// Plug lifecycle update
    PlugStatus(PlugStatusUpdateEvent),

    /// Plug log output
    PlugLog(PlugLogEvent),

    /// UI request for operator input
    UiRequest(crate::ui::UiRequestData),

    /// UI update (progress bar, display refresh)
    UiUpdate(UiUpdateEvent),

    /// Live log line from a running phase. Emitted as each line is
    /// produced so observers (TUI, agent protocol, dashboard) see logs
    /// in real time — and surviving a force-kill.
    PhaseLogLine {
        job_id: String,
        phase_key: String,
        slot_id: Option<String>,
        level: String,
        message: String,
        timestamp: String,
        file: Option<String>,
        line: Option<u32>,
    },

    /// Live measurement write. Emitted when Python does
    /// `measurements.voltage = 5.0`, not only at phase end.
    MeasurementRecorded {
        job_id: String,
        phase_key: String,
        slot_id: Option<String>,
        name: String,
        /// Raw JSON value shape produced by the Python side
        /// (e.g. `{"Numeric": 5.0}`, `{"String": "ok"}`).
        value: serde_json::Value,
        unit: Option<String>,
        timestamp: String,
    },

    /// Live attachment added to the current phase. Agents can see which
    /// files / data blobs the phase produced before the run ends.
    AttachmentAdded {
        phase_key: String,
        slot_id: Option<String>,
        name: String,
        path: Option<String>,
        mimetype: Option<String>,
    },

    /// Unit identity resolved (operator prompt, `auto_identify` defaults,
    /// or an external connector). Emitted by the runner before the
    /// orchestrator starts processing phases so consumers (TUI, agent,
    /// upload layer) have the resolved serial / part / revision /
    /// batch / sub-units in a structured event rather than having to
    /// reconstruct it from a `UiResponse`.
    UnitIdentified {
        slot_id: Option<String>,
        unit_info: crate::unit::UnitInfo,
    },
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct PlannedPhase {
    pub phase_key: String,
    pub phase_name: String,
    pub stage_scope: StageScope,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct PlannedPlug {
    pub plug_key: String,
    pub plug_name: String,
    pub scope: String,
}

/// Trait for receiving execution events.
/// Implemented by each consumer: Studio (Tauri), CLI (TUI + WebSocket), etc.
pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, event: &ExecutionEvent);
}

/// No-op sink for tests and headless execution.
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: &ExecutionEvent) {}
}

/// Sink that fans out to multiple sinks.
pub struct MultiSink(pub Vec<Arc<dyn EventSink>>);

impl EventSink for MultiSink {
    fn emit(&self, event: &ExecutionEvent) {
        for sink in &self.0 {
            sink.emit(event);
        }
    }
}
