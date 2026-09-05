//! YAML procedure execution via the shared execution engine.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};

use execution_engine::event_sink::ExecutionEvent;
use execution_engine::events::PlugStatusValue;
use execution_engine::job::Outcome;
use execution_engine::orchestrator::Orchestrator;
use execution_engine::procedure::loader::load_procedure_definition;
use execution_engine::procedure::schema::StageScope;
use execution_engine::ui::UiRequestData;
use execution_engine::EventSink;
use station_protocol::{
    AggregationResult, PhaseLogLine, PhasePlan, RunMeasurement, StationEvent, ValidatorResult,
};
use tofupilot_sdk::types::*;
// SDK enum names track the alphabetically-first endpoint; alias back to the
// names this crate uses (see connector/mod.rs).
use tofupilot_sdk::types::{
    LogGetOutcome as RunGetOutcome, PhaseGetOutcome as RunGetPhasesOutcome,
};

use super::agent_proto::events::to_agent_ui_component;
use super::agent_proto::{
    AgentProtoCtx, AgentUiComponent, CliEvent, PhasePlanPayload, UiAutoContinueSource,
};
use super::event_router::{EventRouter, PhaseFinished};
use super::identify_host;
use super::queue::QueuedRun;

/// The canonical procedure file names, in precedence order: `.yaml`
/// wins over `.yml`. One directory holds one procedure, and this is
/// what makes it one — the CLI, the git integration and Studio's
/// project discovery all decide "is this a procedure?" by this list, so
/// it is shared rather than spelled out per caller.
pub const PROCEDURE_FILENAMES: [&str; 2] = ["procedure.yaml", "procedure.yml"];

/// Locate the YAML procedure file inside a root directory. Returns
/// `Some(path)` if `procedure.yaml` (or `.yml`) is present, `None`
/// otherwise. Caller has already resolved `package_dir` — this is just an
/// on-disk file lookup.
pub fn find_procedure_yaml(package_dir: &Path) -> Option<std::path::PathBuf> {
    for name in PROCEDURE_FILENAMES {
        let path = package_dir.join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Everything a run setup needs from a deployment dir. `manifest_present`
/// distinguishes a pulled deployment (manifest written by the deployer,
/// venv at `<package_dir>/venv` per sync.rs) from a local-path run
/// (`tofupilot run ./my-procedure` — no manifest, venv lives wherever
/// the dev's tooling put it). One filesystem read covers both
/// `package_dir` + `entry_point` lookups.
#[cfg_attr(test, derive(Debug))]
pub struct DeploymentLayout {
    pub package_dir: std::path::PathBuf,
    pub entry_point: Option<String>,
    pub manifest_present: bool,
}

/// Read the manifest once and surface everything `prepare_run` needs.
/// Errors when the manifest is unparseable or carries an unsafe
/// `root_directory` (server-validated, but the artifact path could be
/// tampered between build and station).
///
/// Workspace members installed as wheels (e.g. `shared`) resolve via the
/// venv's `site-packages/`, not via `package_dir` — see `tp_worker.py`'s
/// importlib fallback.
pub fn deployment_layout(deployment_dir: &Path) -> crate::error::CliResult<DeploymentLayout> {
    let manifest_path = deployment_dir.join("manifest.json");
    if !manifest_path.exists() {
        // Local-path runs (`tofupilot run ./my-procedure`) skip the
        // pull/install pipeline so they never produce a manifest. The
        // package dir = the deployment dir; framework defaults pick the
        // entry point.
        return Ok(DeploymentLayout {
            package_dir: deployment_dir.to_path_buf(),
            entry_point: None,
            manifest_present: false,
        });
    }
    let manifest =
        execution_engine::manifest::Manifest::parse(&manifest_path).map_err(|e| e.to_string())?;
    let package_dir = match manifest.root_directory() {
        Some(rel) => deployment_dir.join(rel),
        None => deployment_dir.to_path_buf(),
    };
    Ok(DeploymentLayout {
        package_dir,
        entry_point: manifest.entry_point().map(str::to_string),
        manifest_present: true,
    })
}

/// Collected phase data from JobComplete events.
#[derive(Clone)]
struct CompletedPhase {
    name: String,
    outcome: Outcome,
    started_at: String,
    completed_at: String,
    retry_count: usize,
    measurements: Vec<execution_engine::measurements::Measurement>,
    logs: Vec<execution_engine::log::LogEntry>,
    error: Option<String>,
    /// Slot this phase ran for. None for shared stages (setup / teardown
    /// at execution scope), which belong to every slot's run.
    slot_id: Option<String>,
}

/// Resolved unit identity of one slot, keyed by slot in `RunData.units`.
#[derive(Default, Clone, Debug)]
struct UnitSnapshot {
    serial: Option<String>,
    part: Option<String>,
    revision: Option<String>,
    batch: Option<String>,
    /// Operated-by resolved through the unit pipeline (identify prompt,
    /// `unit.operated_by` binding, Python write). Takes precedence over
    /// the session email forwarded on the WS run command when building
    /// the upload request.
    operated_by: Option<String>,
    sub_units: Option<Vec<String>>,
}

/// One metadata contribution: the slot it belongs to (None = shared,
/// applies to every slot's run), the source (identify step or phase
/// key), and the map. A retry REPLACES its (slot, source) entry.
type MetadataSource = (
    Option<String>,
    String,
    std::collections::HashMap<String, serde_json::Value>,
);

/// Collected run-level data from execution events.
///
/// One accumulator for the whole execution: phases, metadata and
/// attachments are tagged with the slot that produced them (None =
/// shared stage) and `build_run_request` projects one upload per slot
/// out of it. A single-slot procedure has exactly one slot, so the
/// projection is the identity.
struct RunData {
    phases: Vec<CompletedPhase>,
    run_outcome: Option<Outcome>,
    /// Per-slot outcomes from `ExecutionEvent::Complete`, each slot
    /// aggregated by the engine from its own phases plus the shared
    /// stages. Empty on crash paths; `build_run_request` falls back to
    /// `run_outcome`.
    slot_outcomes: std::collections::HashMap<String, Outcome>,
    run_id: Option<String>,
    start_time: Option<chrono::DateTime<chrono::Utc>>,
    end_time: Option<chrono::DateTime<chrono::Utc>>,
    /// Resolved unit per slot key.
    units: std::collections::HashMap<String, UnitSnapshot>,
    /// Run-level metadata contributions in completion order. A retry
    /// REPLACES its phase's whole entry — so keys a failed attempt wrote
    /// but the passing retry didn't rewrite don't leak into the upload.
    /// A slot's upload map is folded from the entries whose slot matches
    /// or is None, in order (later sources win per key).
    run_metadata_sources: Vec<MetadataSource>,
    /// Unit-level metadata contributions, same replace-per-source
    /// semantics. The operator identify-form entry lands first (pre-run)
    /// so phase writes override it per key.
    unit_metadata_sources: Vec<MetadataSource>,
    /// Attachments written to the report dir during the run, accumulated
    /// for the upload queue and tagged with the producing slot (None =
    /// shared stage). The native engine path emits AttachmentAdded live
    /// but, unlike the framework connectors, didn't collect them for
    /// upload — so a station `attach.data` image never reached the cloud
    /// and never showed on the remote dashboard. Collected here (only when
    /// the event carries an on-disk path) so the upload queue ships them
    /// and emits AttachmentUploaded.
    attachments: Vec<(
        Option<String>,
        crate::commands::run::queue::QueuedAttachment,
    )>,
}

/// EventSink that projects to StationEvents for TUI/WebSocket and accumulates data for upload.
struct CliEventSink {
    tx: broadcast::Sender<StationEvent>,
    ui_tx: Option<mpsc::Sender<UiRequestData>>,
    agent: Option<AgentProtoCtx>,
    router: EventRouter,
    data: Arc<Mutex<RunData>>,
    /// Resolved by `run_yaml_procedure` from the dashboard-pulled
    /// `PullState.name`. Stamped on every `RunStarted` emit so
    /// downstream consumers don't need a station-procedures
    /// reverse lookup to render the run header.
    procedure_name: String,
    /// Procedure id this run executes. Stamped on `RunStarted` so the
    /// operator-UI can echo it back on subsequent `Run` commands ("Run
    /// again" / "New run") and the station loop's `last_procedure_id`
    /// memo lines up with what the wire just sent.
    procedure_id: String,
    /// Per-run identity minted by the caller (`run::start()`). Stamped on
    /// every `RunStarted` / `RunComplete` so operator-UI can drop terminal
    /// events from a cancelled prior run that race a fresh `RunStarted`.
    execution_id: String,
    /// Snapshot of the resolved unit, written by the
    /// `ExecutionEvent::UnitIdentified` arm and read synchronously
    /// when emitting `StationEvent::RunStarted` so operator-UI sees
    /// the unit on `auto_identify: true` runs (no `UiRequest`/
    /// `UiResponse` cycle to capture it from). One cell for the whole
    /// execution: on a multi-slot run the last identified slot wins,
    /// and consumers read each slot's unit off `identify_resolved`.
    resolved_unit: Arc<std::sync::Mutex<Option<station_protocol::UnitInfo>>>,
    /// Per-slot twin of `resolved_unit`, stamped on `RunStarted.slot_units`
    /// so a consumer joining mid-run sees every slot's serial.
    resolved_units:
        Arc<std::sync::Mutex<std::collections::HashMap<String, station_protocol::UnitInfo>>>,
    /// Tickets of the deferred `RunData` writes spawned by `emit()`
    /// (phase accumulation, stats, outcome, attachments, mid-run unit
    /// updates). `emit` is sync, so each write runs as a spawned task;
    /// collecting the JoinHandles lets `run_yaml_procedure` await them
    /// all after engine shutdown, BEFORE `build_run_request` snapshots
    /// RunData — closing the spawn-and-pray race where a fast-ending
    /// run could upload without the last event's write (e.g. a
    /// final-phase `run.operated_by` from a badge scan).
    pending_writes: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    /// Deployment this run came from (local `PullState` lookup), None
    /// for ad-hoc local-path runs. Stamped on `RunStarted` so remote
    /// UIs can resolve relative component image paths against the
    /// deployment's stored files.
    deployment_id: Option<String>,
    /// Partial run: the main phase this run was narrowed to. Stamped
    /// on `RunStarted` so "Run again" can repeat the partial run
    /// instead of silently escalating to the whole procedure.
    only_phase: Option<String>,
    /// Slot keys the procedure was submitted with, in declaration order.
    /// A mid-run unit update from a shared stage (no slot) applies to
    /// every one of them.
    slots: Vec<String>,
    /// Display names of the declared slots, stamped on `RunStarted`.
    slot_names: std::collections::HashMap<String, String>,
    /// Flipped on the first post-submit sign of life from the engine
    /// (job dispatched, plug status, UI request, log line…). The
    /// dispatch-stall watchdog in `run_yaml_procedure` races this flag:
    /// if the engine accepted the procedure but nothing at all happens,
    /// the run is killed with a diagnostic instead of spinning forever.
    /// `Plan` / `UnitIdentified` don't count — both fire at or before
    /// submit, ahead of the window the watchdog guards.
    progressed: Arc<std::sync::atomic::AtomicBool>,
}

impl CliEventSink {
    #[allow(clippy::too_many_arguments)]
    fn new(
        tx: broadcast::Sender<StationEvent>,
        ui_tx: Option<mpsc::Sender<UiRequestData>>,
        agent: Option<AgentProtoCtx>,
        procedure_name: String,
        procedure_id: String,
        execution_id: String,
        deployment_id: Option<String>,
        only_phase: Option<String>,
        slots: Vec<String>,
        slot_names: std::collections::HashMap<String, String>,
    ) -> Self {
        let router = EventRouter::new(tx.clone(), agent.clone(), execution_id.clone());
        Self {
            tx,
            ui_tx,
            agent,
            router,
            procedure_name,
            procedure_id,
            execution_id,
            deployment_id,
            only_phase,
            slots,
            slot_names,
            data: Arc::new(Mutex::new(RunData {
                phases: Vec::new(),
                run_outcome: None,
                slot_outcomes: std::collections::HashMap::new(),
                run_id: None,
                start_time: None,
                end_time: None,
                units: std::collections::HashMap::new(),
                run_metadata_sources: Vec::new(),
                unit_metadata_sources: Vec::new(),
                attachments: Vec::new(),
            })),
            resolved_unit: Arc::new(std::sync::Mutex::new(None)),
            resolved_units: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            pending_writes: Arc::new(std::sync::Mutex::new(Vec::new())),
            progressed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Spawn a deferred `RunData` write and keep its ticket for the
    /// pre-upload barrier (see `pending_writes`).
    fn spawn_run_data_write<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let handle = tokio::spawn(fut);
        if let Ok(mut writes) = self.pending_writes.lock() {
            writes.push(handle);
        }
    }
}

impl EventSink for CliEventSink {
    fn emit(&self, event: &ExecutionEvent) {
        // Feed the dispatch-stall watchdog. Everything except the
        // submit-time events counts as progress — the watchdog only
        // exists to catch "engine accepted the procedure, then silence".
        match event {
            ExecutionEvent::Plan { .. } | ExecutionEvent::UnitIdentified { .. } => {}
            _ => self
                .progressed
                .store(true, std::sync::atomic::Ordering::Relaxed),
        }
        match event {
            ExecutionEvent::Plan {
                phases,
                plugs_all,
                plugs_each,
                slots,
                ..
            } => {
                let plan: Vec<PhasePlan> = phases
                    .iter()
                    .map(|p| PhasePlan {
                        key: p.phase_key.clone(),
                        name: p.phase_name.clone(),
                        stage: stage_scope_str(&p.stage_scope).to_string(),
                    })
                    .collect();
                // Flatten the engine's split plug plan into a single
                // wire vec keyed by `scope`. Consumers seed their plug
                // state from this so they don't have to materialize
                // entries reactively on the first plug_status event.
                let mut plug_defs: Vec<station_protocol::PlugDefinition> =
                    Vec::with_capacity(plugs_all.len() + plugs_each.len());
                // Carry the plan's scope string through verbatim
                // ("slot" / "execution" / "station") instead of re-deriving
                // it from which bucket the plug rode in — the buckets
                // group shared vs per-slot, which collapses station
                // into run and loses it on the wire.
                for p in plugs_all.iter().chain(plugs_each.iter()) {
                    plug_defs.push(station_protocol::PlugDefinition {
                        key: p.plug_key.clone(),
                        name: p.plug_name.clone(),
                        scope: p.scope.clone(),
                    });
                }
                // `unit` is populated from the resolved-unit cell when
                // the runner already ran identify-unit before
                // submit_procedure. For procedures without a `unit:`
                // block, the cell is None and `RunStarted.unit`
                // remains null — there's nothing to identify. Wire
                // consumers that care about late unit updates fold
                // the per-phase `IdentifyResolved` events that mid-
                // run identify emits.
                // run_id is still resolved later — `RunComplete`
                // carries it for now.
                let unit = self
                    .resolved_unit
                    .lock()
                    .ok()
                    .and_then(|guard| guard.clone());
                let _ = self.tx.send(StationEvent::RunStarted {
                    procedure_id: self.procedure_id.clone(),
                    procedure_name: self.procedure_name.clone(),
                    execution_id: self.execution_id.clone(),
                    phases: plan,
                    slots: slots.clone(),
                    slot_names: self.slot_names.clone(),
                    slot_units: self
                        .resolved_units
                        .lock()
                        .map(|m| m.clone())
                        .unwrap_or_default(),
                    plugs: plug_defs,
                    timestamp: Some(chrono::Utc::now().to_rfc3339()),
                    run_id: None,
                    deployment_id: self.deployment_id.clone(),
                    unit,
                    only_phase: self.only_phase.clone(),
                });
                if let Some(ref agent) = self.agent {
                    let payload_phases: Vec<PhasePlanPayload> = phases
                        .iter()
                        .map(|p| PhasePlanPayload {
                            key: p.phase_key.clone(),
                            name: p.phase_name.clone(),
                        })
                        .collect();
                    agent.emitter.enqueue(CliEvent::Plan {
                        phases: payload_phases,
                    });
                }
            }

            ExecutionEvent::JobProgress {
                phase_key,
                phase_name,
                status,
                retry_count,
                outcome,
                error,
                slot_id,
                started_at,
                ..
            } => {
                use execution_engine::job::JobStatus;
                match status {
                    JobStatus::Running => {
                        let attempt = (*retry_count as u32).saturating_add(1);
                        self.router.phase_started(
                            phase_key,
                            phase_name,
                            attempt,
                            slot_id.clone(),
                            *started_at,
                        );
                    }
                    JobStatus::Skipped => {
                        let outcome_str = outcome
                            .as_ref()
                            .map(super::outcomes::from_execution_outcome)
                            .unwrap_or(super::outcomes::SKIP);
                        self.router.phase_skipped(
                            phase_key,
                            phase_name,
                            slot_id.clone(),
                            error.clone(),
                            outcome_str,
                        );
                    }
                    _ => {}
                }
            }

            ExecutionEvent::JobComplete {
                phase_key,
                phase_name,
                outcome,
                measurements,
                logs,
                started_at,
                completed_at,
                retry_count,
                error,
                slot_id,
                run_metadata,
                unit_metadata,
                ..
            } => {
                // Send to TUI
                let run_measurements: Vec<RunMeasurement> = measurements
                    .iter()
                    .map(|m| {
                        let validator_results = build_validator_results(m);
                        // Station wire contract uses UPPERCASE outcome
                        // strings (`PASS`/`FAIL`/`UNSET`). Engine rolls up
                        // the measurement outcome from its validators —
                        // OpenHTF parity, so a measurement with a value
                        // and no validators is PASS (vacuously true).
                        let meas_outcome = validator_outcome_to_wire(&m.outcome).to_string();
                        RunMeasurement {
                            name: m.name.clone(),
                            outcome: meas_outcome,
                            measured_value: Some(m.value.to_raw_json()),
                            units: m.unit.clone(),
                            validators: validator_results,
                            aggregations: build_aggregation_results(m),
                        }
                    })
                    .collect();

                // If no explicit error but the phase failed because a
                // measurement validator flagged a value, synthesize a
                // diagnostic so agents don't have to parse measurements.
                let err = error
                    .clone()
                    .or_else(|| synthesize_measurement_error(*outcome, measurements));
                let duration_ms = duration_from_iso_pair(started_at, completed_at);
                let outcome_str = super::outcomes::from_execution_outcome(outcome).to_string();
                let attempt = (*retry_count as u32).saturating_add(1);
                let station_logs: Vec<PhaseLogLine> = logs
                    .iter()
                    .map(|l| PhaseLogLine {
                        level: l.level.clone(),
                        message: l.message.clone(),
                        timestamp: Some(l.timestamp.clone()),
                        file: l.file.clone(),
                        line: l.line,
                    })
                    .collect();
                self.router.phase_finished(PhaseFinished {
                    phase_key: phase_key.clone(),
                    phase_name: phase_name.clone(),
                    outcome: outcome_str,
                    attempt,
                    slot_id: slot_id.clone(),
                    error: err,
                    started_at: Some(started_at.clone()),
                    ended_at: Some(completed_at.clone()),
                    duration_ms,
                    station_measurements: run_measurements,
                    station_logs,
                });

                // Accumulate for upload
                let phase = CompletedPhase {
                    name: phase_name.clone(),
                    outcome: *outcome,
                    started_at: started_at.clone(),
                    completed_at: completed_at.clone(),
                    retry_count: *retry_count,
                    measurements: measurements.clone(),
                    logs: logs.clone(),
                    error: error.clone(),
                    slot_id: slot_id.clone(),
                };
                let data = self.data.clone();
                let run_md = run_metadata.clone();
                let unit_md = unit_metadata.clone();
                let source_key = phase_key.clone();
                let source_slot = slot_id.clone();
                self.spawn_run_data_write(async move {
                    // Same lock acquisition keeps phase data and metadata
                    // atomic per event. Each (slot, phase key) is one
                    // metadata source; a retry replaces the whole entry,
                    // so keys a failed attempt wrote don't leak into a
                    // passing run.
                    let mut d = data.lock().await;
                    d.phases.push(phase);
                    upsert_metadata_source(
                        &mut d.run_metadata_sources,
                        &source_slot,
                        &source_key,
                        run_md,
                    );
                    upsert_metadata_source(
                        &mut d.unit_metadata_sources,
                        &source_slot,
                        &source_key,
                        unit_md,
                    );
                });
            }

            ExecutionEvent::Stats { start_time, .. } => {
                if let Some(t) = start_time {
                    let data = self.data.clone();
                    let t = *t;
                    self.spawn_run_data_write(async move {
                        let mut d = data.lock().await;
                        if d.start_time.is_none() {
                            d.start_time = Some(t);
                        }
                    });
                }
            }

            ExecutionEvent::Complete {
                run_outcome,
                run_id,
                end_time,
                slot_outcomes,
                ..
            } => {
                let outcome_str = run_outcome
                    .as_ref()
                    .map(super::outcomes::from_execution_outcome)
                    .unwrap_or("UNKNOWN");
                // Multi-slot: each slot's own outcome rides the wire and the
                // agent protocol. Single slot keeps both as they were.
                let wire_slot_outcomes: std::collections::HashMap<String, String> =
                    if slot_outcomes.len() > 1 {
                        slot_outcomes
                            .iter()
                            .map(|(slot, o)| {
                                (
                                    slot.clone(),
                                    super::outcomes::from_execution_outcome(o).to_string(),
                                )
                            })
                            .collect()
                    } else {
                        Default::default()
                    };
                super::emit::run_complete(
                    &self.tx,
                    outcome_str,
                    &self.execution_id,
                    run_id.clone(),
                    wire_slot_outcomes.clone(),
                );
                if !wire_slot_outcomes.is_empty() {
                    if let Some(ref agent) = self.agent {
                        if let Ok(mut map) = agent.slot_outcomes.lock() {
                            *map = wire_slot_outcomes;
                        }
                    }
                }

                let data = self.data.clone();
                let ro = *run_outcome;
                let so = slot_outcomes.clone();
                let ri = run_id.clone();
                let et = *end_time;
                self.spawn_run_data_write(async move {
                    let mut d = data.lock().await;
                    d.run_outcome = ro;
                    d.slot_outcomes = so;
                    d.run_id = ri;
                    d.end_time = et;
                });
            }

            ExecutionEvent::PlugStatus(status) => {
                if matches!(status.status, PlugStatusValue::Error) {
                    crate::log::error(&format!("Plug '{}' error", status.plug_name));
                }
                let _ = self.tx.send(StationEvent::PlugStatus {
                    plug_key: status.plug_key.clone(),
                    plug_name: status.plug_name.clone(),
                    stage: plug_stage_str(&status.stage).to_string(),
                    status: plug_status_str(&status.status).to_string(),
                    scope: plug_scope_str(&status.scope).to_string(),
                    slot_id: status.slot_id.clone(),
                    execution_id: Some(self.execution_id.clone()),
                });
                if let Some(ref agent) = self.agent {
                    agent.emitter.enqueue(CliEvent::PlugStatus {
                        plug_key: status.plug_key.clone(),
                        plug_name: status.plug_name.clone(),
                        status: plug_status_str(&status.status).to_string(),
                        stage: plug_stage_str(&status.stage).to_string(),
                        scope: plug_scope_str(&status.scope).to_string(),
                        slot_id: status.slot_id.clone(),
                    });
                }
            }

            ExecutionEvent::PlugLog(log_event) => {
                // Plug logs flow exclusively to the broadcast and the
                // agent protocol. Writing to stderr would corrupt the
                // TUI frame (ratatui owns the terminal during a run);
                // operator surfaces consume the broadcast event below
                // for their own log views.
                let stage_str = log_event
                    .stage
                    .as_ref()
                    .map(plug_stage_str)
                    .map(String::from);
                let _ = self.tx.send(StationEvent::PlugLog {
                    plug_key: log_event.plug_key.clone(),
                    plug_name: log_event.plug_name.clone(),
                    level: log_event.level.clone(),
                    message: log_event.message.clone(),
                    slot_id: log_event.slot_id.clone(),
                    stage: stage_str.clone(),
                    timestamp: log_event.timestamp.clone(),
                    line: log_event.line,
                    execution_id: Some(self.execution_id.clone()),
                });
                if let Some(ref agent) = self.agent {
                    agent.emitter.enqueue(CliEvent::PlugLog {
                        plug_key: log_event.plug_key.clone(),
                        plug_name: log_event.plug_name.clone(),
                        level: log_event.level.clone(),
                        message: log_event.message.clone(),
                        slot_id: log_event.slot_id.clone(),
                        stage: stage_str,
                        timestamp: log_event.timestamp.clone(),
                        line: log_event.line,
                    });
                }
            }

            ExecutionEvent::UiRequest(request) => {
                // Forward to TUI for visual display
                if let Some(ref ui_tx) = self.ui_tx {
                    let _ = ui_tx.try_send(request.clone());
                }

                // Broadcast to Centrifugo for web dashboard / local UI.
                self.router.ui_request(
                    &request.request_id,
                    &request.phase_key,
                    request.slot_id.clone(),
                    &request.config.components,
                    request.config.requires_user_input(),
                );

                // Agent protocol path (--json, no TUI)
                if self.ui_tx.is_none() {
                    if let Some(ref agent) = self.agent {
                        handle_agent_ui_request(agent.clone(), request.clone());
                        return;
                    }

                    // Fallback: auto-continue display-only UIs when no TUI and no agent ctx
                    if !request.config.requires_user_input() {
                        let request_id = request.request_id.clone();
                        tokio::spawn(async move {
                            super::ui_response::send_empty(&request_id).await;
                        });
                    }
                }
            }

            ExecutionEvent::UiUpdate(ui_event) => {
                // Mid-run mutation of a live prompt's components from
                // Python (`ui.<key> = value`). Forward to the broadcast
                // so the TUI, local websocket, and Centrifugo subscribers
                // can reflect it. `data` is opaque JSON — the reducer
                // dispatches per `action`.
                //
                // The worker stamps `slot_id = "<shared>"` for jobs that
                // didn't bind to a slot (`worker.rs:540`). Strip the
                // sentinel back to `None` on the wire so reducers don't
                // try to slot-match against a literal that doesn't
                // appear in any `UiRequest`.
                let data = serde_json::to_string(&ui_event.data).ok();
                let slot_id = match ui_event.slot_id.as_str() {
                    "" | "<shared>" => None,
                    _ => Some(ui_event.slot_id.clone()),
                };
                self.router.ui_update(
                    &ui_event.phase_key,
                    slot_id,
                    Some(ui_event.job_id.clone()),
                    &ui_event.action,
                    data,
                );
            }

            ExecutionEvent::PhaseLogLine {
                phase_key,
                slot_id,
                level,
                message,
                timestamp,
                file,
                line,
                ..
            } => {
                // Live log line on the broadcast for UI consumers.
                // KNOWN LIMITATION: the execution-engine wire event
                // (`PhaseLogLineEvent`) carries `job_id` but no
                // attempt index — the engine doesn't expose retry
                // count at log-emit time. Defaulting to attempt 1
                // means a retried phase's live logs render under the
                // first attempt's slot until `PhaseComplete` lands
                // the canonical batched logs against the right
                // attempt. The reducer's terminal-slot guard at
                // `run-state.ts::phase_log` prevents the orphan
                // attempt-1 stub from materialising once a later
                // attempt completes; until then the operator sees
                // logs on attempt 1 even on a retry. Threading
                // retry_count through `ExecutionEvent::PhaseLog`
                // requires a coordinated change in the
                // execution-engine crate plus its consumers.
                let _ = self.tx.send(StationEvent::PhaseLog {
                    phase_key: phase_key.clone(),
                    attempt: 1,
                    slot_id: slot_id.clone(),
                    level: level.clone(),
                    message: message.clone(),
                    timestamp: Some(timestamp.clone()),
                    file: file.clone(),
                    line: *line,
                    execution_id: Some(self.execution_id.clone()),
                });
                if let Some(ref agent) = self.agent {
                    agent.emitter.enqueue(CliEvent::PhaseLog {
                        phase_key: phase_key.clone(),
                        level: level.clone(),
                        message: message.clone(),
                        timestamp: timestamp.clone(),
                        slot_id: slot_id.clone(),
                        file: file.clone(),
                        line: *line,
                    });
                }
            }

            ExecutionEvent::MeasurementRecorded {
                phase_key,
                slot_id,
                name,
                value,
                unit,
                ..
            } => {
                if let Some(ref agent) = self.agent {
                    // `outcome` is intentionally "unset": measurement validators
                    // don't fire at record time (only on phase close). Agents
                    // that want pass/fail read it from `phase_finished`; this
                    // live event exists for streaming raw values only.
                    //
                    // Every string field is bounded before going on the wire.
                    // A malicious/buggy phase can't wedge the stream with a
                    // 100MB measurement name.
                    let (capped_name, name_truncated) = cap_string(name, MAX_LABEL_BYTES);
                    let (capped_unit, unit_truncated) =
                        cap_optional(unit.as_deref(), MAX_LABEL_BYTES);
                    let (capped_value, value_truncated) = cap_measurement_value(value);
                    if name_truncated || unit_truncated || value_truncated {
                        agent.emitter.enqueue(CliEvent::InternalWarning {
                            kind: "measurement_truncated".into(),
                            message: format!(
                                "measurement '{}' exceeded payload caps; full record in phase_finished",
                                truncate_for_log(name)
                            ),
                            detail: Some(cap_warning_detail(serde_json::json!({
                                "phase_key": cap_string(phase_key, MAX_LABEL_BYTES).0,
                                "slot_id": slot_id.as_deref().map(|s| cap_string(s, MAX_LABEL_BYTES).0),
                                "name_truncated": name_truncated,
                                "unit_truncated": unit_truncated,
                                "value_truncated": value_truncated,
                            }))),
                        });
                    }
                    agent.emitter.enqueue(CliEvent::MeasurementRecorded {
                        phase_key: phase_key.clone(),
                        name: capped_name.clone(),
                        value: capped_value.clone(),
                        outcome: "unset".into(),
                        unit: capped_unit.clone(),
                        slot_id: slot_id.clone(),
                    });
                }
                // Live measurement on the broadcast: outcome
                // "UNSET" until phase_complete validates. Validators
                // arrive populated on `PhaseComplete.measurements`,
                // so the live update is safe to render as a row.
                let _ = self.tx.send(StationEvent::MeasurementUpdate {
                    phase_key: phase_key.clone(),
                    attempt: 1,
                    slot_id: slot_id.clone(),
                    measurement: RunMeasurement {
                        name: name.clone(),
                        outcome: "UNSET".into(),
                        measured_value: Some(value.clone()),
                        units: unit.clone(),
                        validators: Vec::new(),
                        // Aggregations are evaluated at phase end; the
                        // live update has nothing to carry yet.
                        aggregations: Vec::new(),
                    },
                    execution_id: Some(self.execution_id.clone()),
                });
            }

            ExecutionEvent::AttachmentAdded {
                phase_key,
                slot_id,
                name,
                path,
                mimetype,
            } => {
                if let Some(ref agent) = self.agent {
                    let (capped_name, name_truncated) = cap_string(name, MAX_LABEL_BYTES);
                    let (capped_path, path_truncated) =
                        cap_optional(path.as_deref(), MAX_ATTACHMENT_PATH_BYTES);
                    let (capped_mimetype, mime_truncated) =
                        cap_optional(mimetype.as_deref(), MAX_LABEL_BYTES);
                    if name_truncated || path_truncated || mime_truncated {
                        agent.emitter.enqueue(CliEvent::InternalWarning {
                            kind: "attachment_truncated".into(),
                            message: format!(
                                "attachment '{}' exceeded payload caps",
                                truncate_for_log(name)
                            ),
                            detail: Some(cap_warning_detail(serde_json::json!({
                                "phase_key": cap_string(phase_key, MAX_LABEL_BYTES).0,
                                "name_truncated": name_truncated,
                                "path_truncated": path_truncated,
                                "mimetype_truncated": mime_truncated,
                            }))),
                        });
                    }
                    agent.emitter.enqueue(CliEvent::AttachmentAdded {
                        phase_key: phase_key.clone(),
                        slot_id: slot_id.clone(),
                        name: capped_name.clone(),
                        path: capped_path.clone(),
                        mimetype: capped_mimetype.clone(),
                    });
                }
                let _ = self.tx.send(StationEvent::AttachmentAdded {
                    phase_key: phase_key.clone(),
                    slot_id: slot_id.clone(),
                    name: name.clone(),
                    path: path.clone(),
                    mimetype: mimetype.clone(),
                    size_bytes: None,
                    execution_id: Some(self.execution_id.clone()),
                });
                // Collect for the upload queue so the attachment reaches the
                // cloud (and the remote dashboard). Only when the event
                // carries the report-dir path — `attach_data` writes the
                // file and emits it; an emit without a path has nothing to
                // upload. Same accumulate-via-spawn pattern as phases above;
                // the push completes well before the post-run queue build.
                if let Some(stored) = path.clone() {
                    let data = self.data.clone();
                    let slot = slot_id.clone();
                    let queued_attachment = crate::commands::run::queue::QueuedAttachment {
                        name: name.clone(),
                        path: stored,
                        mimetype: mimetype.clone().unwrap_or_default(),
                        phase_key: phase_key.clone(),
                    };
                    self.spawn_run_data_write(async move {
                        data.lock()
                            .await
                            .attachments
                            .push((slot, queued_attachment));
                    });
                }
            }

            ExecutionEvent::UnitIdentified { slot_id, unit_info } => {
                // Cache the resolved unit so the `Plan` arm can stamp
                // it on `StationEvent::RunStarted.unit`. Without this
                // operator-UI never sees the unit on `auto_identify`
                // runs (no `UiRequest`/`UiResponse` to capture from).
                //
                // Identify-time resolutions are also written into
                // RunData synchronously upstream by `run_yaml_procedure`
                // before `submit_procedure`; the async apply below is
                // what carries MID-RUN updates (a `unit.<field>` UI
                // binding, a Python `unit.<field> = ...` write) into
                // the upload payload — without it those land on the
                // wire for live UIs but the created run keeps the
                // identify-time values.
                {
                    let data = self.data.clone();
                    let info = unit_info.clone();
                    let slot = slot_id.clone();
                    let all_slots = self.slots.clone();
                    self.spawn_run_data_write(async move {
                        // A slot's own update lands on that slot; a
                        // shared stage writing unit fields (no slot)
                        // updates every slot.
                        match slot {
                            Some(slot) => apply_unit_info_to_run_data(&data, &slot, &info).await,
                            None => {
                                for slot in &all_slots {
                                    apply_unit_info_to_run_data(&data, slot, &info).await;
                                }
                            }
                        }
                    });
                }
                let wire_unit = unit_info_to_wire(unit_info);
                if let Ok(mut guard) = self.resolved_unit.lock() {
                    *guard = Some(wire_unit.clone());
                }
                if let Some(slot) = slot_id {
                    if let Ok(mut map) = self.resolved_units.lock() {
                        map.insert(slot.clone(), wire_unit.clone());
                    }
                }
                // Fan out the dedicated `identify_resolved` event so
                // operator-UI / dashboard / agent stream learn about
                // every unit-resolution source uniformly: pre-run
                // operator prompt, pre-run `auto_identify` defaults,
                // mid-run prompt response, mid-run Python bound
                // measurement updates. The router emits the wire
                // event AND the agent-side typed event; consumers
                // merge field-level into their `RunState.unit`.
                self.router.identify_resolved(slot_id.clone(), &wire_unit);
            }
        }
    }
}

/// Max serialized size of a live `measurement_recorded.value`. Phases that
/// record huge blobs (100MB JSON, massive arrays) would otherwise bloat the
/// NDJSON stream and OOM agents that buffer line-by-line. The full record
/// still lands in `phase_finished.measurements` via the normal upload path;
/// the live event is for streaming preview only.
const MAX_MEASUREMENT_VALUE_BYTES: usize = 1_000_000;

/// Max byte length for attachment paths in the live event. Paths longer
/// than this are almost always pathological (a bug, not a real filesystem
/// path) and don't belong on the wire.
const MAX_ATTACHMENT_PATH_BYTES: usize = 4_096;

/// Max byte length for short-text fields: measurement name / unit,
/// attachment name / mimetype. These are supposed to be identifiers and
/// labels, not payloads. 1KB is generous.
const MAX_LABEL_BYTES: usize = 1_024;

/// Max serialized size of an `internal_warning.detail` payload.
///
/// Sized larger than the sum of per-field caps (1KB × 5 = 5KB) so a
/// well-formed warning always preserves its structured context
/// (phase_key, slot_id, which fields were truncated) without the outer
/// cap forcing a marker-only collapse that would discard exactly the
/// fields an agent needs to debug.
const MAX_WARNING_DETAIL_BYTES: usize = 10_240;

/// Enforce the total-size cap on `internal_warning.detail`. If the
/// construction-site field caps were correctly applied, this is a no-op;
/// if a new warning site grows a field without capping, we catch it here
/// and collapse the whole payload to a marker the agent can recognize.
fn cap_warning_detail(detail: serde_json::Value) -> serde_json::Value {
    let size = serde_json::to_vec(&detail).map(|v| v.len()).unwrap_or(0);
    if size <= MAX_WARNING_DETAIL_BYTES {
        return detail;
    }
    serde_json::json!({
        "truncated": true,
        "original_size_bytes": size,
        "reason": "detail exceeded MAX_WARNING_DETAIL_BYTES",
    })
}

/// Returns `(value, truncated)`. If the serialized size of `value` exceeds
/// `MAX_MEASUREMENT_VALUE_BYTES`, swap it for a placeholder shape the agent
/// can recognize. Falling back to a placeholder (rather than truncating the
/// JSON string, which would produce invalid JSON) keeps the stream valid.
fn cap_measurement_value(value: &serde_json::Value) -> (serde_json::Value, bool) {
    let size = serde_json::to_vec(value).map(|v| v.len()).unwrap_or(0);
    if size <= MAX_MEASUREMENT_VALUE_BYTES {
        return (value.clone(), false);
    }
    (
        serde_json::json!({
            "truncated": true,
            "original_size_bytes": size,
        }),
        true,
    )
}

/// Truncates `s` to `max_bytes` on a UTF-8 char boundary. Returns
/// `(capped, truncated)`. Truncation drops the tail rather than failing
/// loudly; an InternalWarning records the event separately.
fn cap_string(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

fn cap_optional(s: Option<&str>, max_bytes: usize) -> (Option<String>, bool) {
    match s {
        None => (None, false),
        Some(s) => {
            let (c, t) = cap_string(s, max_bytes);
            (Some(c), t)
        }
    }
}

/// Short form for names in log / warning messages so a 100MB name doesn't
/// bloat the warning itself. 128 chars is enough to identify the phase.
fn truncate_for_log(s: &str) -> String {
    const LOG_MAX: usize = 128;
    if s.len() <= LOG_MAX {
        return s.to_string();
    }
    let mut end = LOG_MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Build a human-readable error string from failing measurement validators.
/// Returns None when the phase didn't fail-kind, no validators fired, or
/// none of them reported Fail — in which case the caller leaves the agent's
/// `error` field absent.
fn synthesize_measurement_error(
    outcome: Outcome,
    measurements: &[execution_engine::measurements::Measurement],
) -> Option<String> {
    use execution_engine::procedure::schema::ValidatorOutcome;

    if !matches!(outcome, Outcome::Fail | Outcome::Error | Outcome::Timeout) {
        return None;
    }

    let failures: Vec<String> = measurements
        .iter()
        .filter_map(|m| {
            let vs = m.validators.as_ref()?;
            let failed: Vec<String> = vs
                .iter()
                .filter(|v| v.outcome == Some(ValidatorOutcome::Fail))
                .map(|v| {
                    v.expression.clone().unwrap_or_else(|| {
                        format!(
                            "{} {}",
                            v.operator.as_deref().unwrap_or("?"),
                            v.expected_value
                                .as_ref()
                                .and_then(|ev| serde_json::to_string(ev).ok())
                                .unwrap_or_default(),
                        )
                    })
                })
                .collect();
            if failed.is_empty() {
                None
            } else {
                Some(format!(
                    "measurement `{}` failed: {}",
                    m.name,
                    failed.join(", ")
                ))
            }
        })
        .collect();

    (!failures.is_empty()).then(|| failures.join("; "))
}

fn duration_from_iso_pair(start: &str, end: &str) -> Option<u64> {
    let s = super::time_fmt::parse_rfc3339(start)?;
    let e = super::time_fmt::parse_rfc3339(end)?;
    (e - s).num_milliseconds().try_into().ok()
}

fn plug_status_str(s: &PlugStatusValue) -> &'static str {
    match s {
        PlugStatusValue::Idle => "idle",
        PlugStatusValue::Initializing => "initializing",
        PlugStatusValue::Active => "active",
        PlugStatusValue::Destructing => "destructing",
        PlugStatusValue::Error => "error",
        PlugStatusValue::Skipped => "skipped",
    }
}

fn plug_stage_str(s: &execution_engine::events::PlugStage) -> &'static str {
    use execution_engine::events::PlugStage;
    match s {
        PlugStage::Setup => "setup",
        PlugStage::Teardown => "teardown",
        PlugStage::Manual => "manual",
    }
}

fn plug_scope_str(s: &execution_engine::events::PlugScope) -> &'static str {
    use execution_engine::events::PlugScope;
    match s {
        PlugScope::Execution => "execution",
        PlugScope::Slot => "slot",
        PlugScope::Station => "station",
    }
}

fn stage_scope_str(s: &StageScope) -> &'static str {
    match s {
        StageScope::SetupAll => "setup_all",
        StageScope::SetupEach => "setup_each",
        StageScope::Main => "main",
        StageScope::TeardownEach => "teardown_each",
        StageScope::TeardownAll => "teardown_all",
    }
}

fn engine_outcome_to_sdk(outcome: &Outcome) -> RunGetOutcome {
    match outcome {
        Outcome::Pass => RunGetOutcome::Pass,
        Outcome::Fail => RunGetOutcome::Fail,
        Outcome::Error => RunGetOutcome::Error,
        Outcome::Timeout => RunGetOutcome::Timeout,
        Outcome::Stop => RunGetOutcome::Aborted,
        Outcome::Skip => RunGetOutcome::Pass,
        Outcome::Retry => RunGetOutcome::Fail,
    }
}

fn engine_outcome_to_phase(outcome: &Outcome) -> RunGetPhasesOutcome {
    match outcome {
        Outcome::Pass => RunGetPhasesOutcome::Pass,
        Outcome::Skip => RunGetPhasesOutcome::Skip,
        Outcome::Error => RunGetPhasesOutcome::Error,
        _ => RunGetPhasesOutcome::Fail,
    }
}

/// Replace-or-append a metadata source entry in place. In-place
/// replacement keeps the source's original position, so the identify
/// entry stays first (phases override it) and a retried phase keeps its
/// completion-order slot while dropping its failed attempt's keys.
fn upsert_metadata_source(
    sources: &mut Vec<MetadataSource>,
    slot: &Option<String>,
    source: &str,
    map: std::collections::HashMap<String, serde_json::Value>,
) {
    if let Some(entry) = sources
        .iter_mut()
        .find(|(s, k, _)| s == slot && k == source)
    {
        entry.2 = map;
    } else if !map.is_empty() {
        sources.push((slot.clone(), source.to_string(), map));
    }
}

/// Fold the metadata sources relevant to `slot` into its upload map, in
/// order (later per-key writes win). Shared sources (slot None) apply to
/// every slot's run.
fn fold_metadata_sources(
    sources: &[MetadataSource],
    slot: &str,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut merged = std::collections::HashMap::new();
    for (source_slot, _, map) in sources {
        if source_slot.is_none() || source_slot.as_deref() == Some(slot) {
            merged.extend(map.clone());
        }
    }
    merged
}

/// Whether a slot-tagged item belongs in `slot`'s run: its own, or shared.
fn belongs_to_slot(item_slot: &Option<String>, slot: &str) -> bool {
    item_slot.is_none() || item_slot.as_deref() == Some(slot)
}

/// Cap a merged metadata map at the server's 50-keys-per-entity limit.
/// Per-phase validation can't see the merged total (each phase gets a
/// fresh metadata dict), so without this cap an over-limit map would
/// reject the entire run upload. Keys are kept in sorted order so the
/// drop is deterministic; dropped keys are logged.
fn cap_metadata_keys(
    label: &str,
    map: &std::collections::HashMap<String, serde_json::Value>,
) -> std::collections::HashMap<String, serde_json::Value> {
    const MAX_KEYS: usize = 50;
    if map.len() <= MAX_KEYS {
        return map.clone();
    }
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let dropped: Vec<&str> = keys[MAX_KEYS..].iter().map(|k| k.as_str()).collect();
    crate::log::warn(&format!(
        "{} exceeds {} keys ({}); dropping: {}",
        label,
        MAX_KEYS,
        map.len(),
        dropped.join(", ")
    ));
    keys[..MAX_KEYS]
        .iter()
        .map(|k| ((*k).clone(), map[*k].clone()))
        .collect()
}

/// Render a validator's `expected_value` onto the wire as plain JSON.
fn expected_value_to_json(
    exp: &execution_engine::procedure::schema::ValidatorExpectedValue,
) -> serde_json::Value {
    use execution_engine::procedure::schema::ValidatorExpectedValue as E;
    match exp {
        E::Number(n) => serde_json::json!(n),
        E::String(s) => serde_json::json!(s),
        E::Boolean(b) => serde_json::json!(b),
        E::Null => serde_json::Value::Null,
        E::NumberArray(a) => serde_json::json!(a),
        E::StringArray(a) => serde_json::json!(a),
        E::MixedArray(a) => serde_json::json!(a),
        E::Object(o) => serde_json::Value::Object(o.clone()),
    }
}

/// Render an aggregation's computed value onto the wire as plain JSON.
fn aggregation_value_to_json(
    value: &execution_engine::procedure::schema::AggregationValue,
) -> serde_json::Value {
    use execution_engine::procedure::schema::AggregationValue as A;
    match value {
        A::Number(n) => serde_json::json!(n),
        A::String(s) => serde_json::json!(s),
        A::Boolean(b) => serde_json::json!(b),
        A::Object(o) => serde_json::Value::Object(o.clone()),
    }
}

/// The generated SDK declares one validator/aggregation struct per nesting
/// site (`RunCreateValidators`, `RunCreateYAxisValidators`, …) even though
/// they are field-for-field identical. These macros map the engine's
/// `ValidatorSpec` / `AggregationSpec` onto whichever pair the call site
/// needs, so the same conversion isn't written out six times.
macro_rules! map_validators {
    ($ty:ident, $vs:expr) => {
        $vs.iter()
            .filter_map(|v| {
                let mut vb = $ty::builder();
                if let Some(ref op) = v.operator {
                    vb = vb.operator(op);
                }
                if let Some(ref exp) = v.expected_value {
                    vb = vb.expected_value(expected_value_to_json(exp));
                }
                if let Some(ref expr) = v.expression {
                    vb = vb.expression(expr);
                }
                if let Some(ref o) = v.outcome {
                    vb = vb.outcome(validator_outcome_to_wire(o));
                }
                vb.build().ok()
            })
            .collect::<Vec<_>>()
    };
}

macro_rules! map_aggregations {
    ($agg_ty:ident, $val_ty:ident, $aggs:expr) => {
        $aggs
            .iter()
            .filter_map(|a| {
                let mut ab = $agg_ty::builder().r#type(&a.aggregation_type);
                if let Some(ref v) = a.value {
                    ab = ab.value(aggregation_value_to_json(v));
                }
                if let Some(ref u) = a.unit {
                    ab = ab.unit(u);
                }
                if let Some(ref o) = a.outcome {
                    ab = ab.outcome(validator_outcome_to_wire(o));
                }
                if let Some(ref vs) = a.validators {
                    let validators = map_validators!($val_ty, vs);
                    if !validators.is_empty() {
                        ab = ab.validators(validators);
                    }
                }
                ab.build().ok()
            })
            .collect::<Vec<_>>()
    };
}

fn build_measurement(
    m: &execution_engine::measurements::Measurement,
) -> crate::error::CliResult<RunCreateMeasurements> {
    use execution_engine::measurements::MeasurementValue;
    use execution_engine::procedure::schema::ValidatorOutcome;

    use tofupilot_sdk::types::Outcome as SdkOutcome;
    let outcome = m
        .validators
        .as_ref()
        .map(|vs| {
            if vs.iter().any(|v| v.outcome == Some(ValidatorOutcome::Fail)) {
                SdkOutcome::Fail
            } else if vs.iter().all(|v| v.outcome == Some(ValidatorOutcome::Pass)) {
                SdkOutcome::Pass
            } else {
                SdkOutcome::Unset
            }
        })
        .unwrap_or(SdkOutcome::Unset);

    let mut b = RunCreateMeasurements::builder()
        .name(&m.name)
        .outcome(outcome);

    // Handle multi-dimensional vs scalar
    if let MeasurementValue::MultiDimensional(ref multidim) = m.value {
        // X axis
        let x_data: Vec<f64> = match &multidim.x_axis.data {
            Some(execution_engine::procedure::schema::AxisData::Numeric(nums)) => nums.clone(),
            _ => Vec::new(),
        };
        let mut xb = RunCreateXAxis::builder().data(x_data);
        if let Some(ref u) = multidim.x_axis.unit {
            xb = xb.units(u);
        }
        // The wire calls the series label `name`; the engine calls it
        // `legend` (falling back to the axis key when unset).
        if let Some(legend) = multidim.x_axis.get_legend() {
            xb = xb.name(legend);
        }
        if let Some(ref vs) = multidim.x_axis.validators {
            let validators = map_validators!(RunCreateValidators, vs);
            if !validators.is_empty() {
                xb = xb.validators(validators);
            }
        }
        if let Some(ref aggs) = multidim.x_axis.aggregations {
            let aggregations =
                map_aggregations!(RunCreateAggregations, RunCreateAggregationsValidators, aggs);
            if !aggregations.is_empty() {
                xb = xb.aggregations(aggregations);
            }
        }
        if let Ok(xa) = xb.build() {
            b = b.x_axis(xa);
        }

        // Y axes
        let y_axes: Vec<RunCreateYAxis> = multidim
            .y_axis
            .iter()
            .filter_map(|y| {
                let y_data: Vec<f64> = match &y.data {
                    Some(execution_engine::procedure::schema::AxisData::Numeric(nums)) => {
                        nums.clone()
                    }
                    _ => return None,
                };
                let mut yb = RunCreateYAxis::builder().data(y_data);
                if let Some(ref u) = y.unit {
                    yb = yb.units(u);
                }
                if let Some(legend) = y.get_legend() {
                    yb = yb.name(legend);
                }
                if let Some(ref vs) = y.validators {
                    let validators = map_validators!(RunCreateYAxisValidators, vs);
                    if !validators.is_empty() {
                        yb = yb.validators(validators);
                    }
                }
                if let Some(ref aggs) = y.aggregations {
                    let aggregations = map_aggregations!(
                        RunCreateYAxisAggregations,
                        RunCreateYAxisAggregationsValidators,
                        aggs
                    );
                    if !aggregations.is_empty() {
                        yb = yb.aggregations(aggregations);
                    }
                }
                yb.build().ok()
            })
            .collect();
        if !y_axes.is_empty() {
            b = b.y_axis(y_axes);
        }
    } else {
        b = b.measured_value(m.value.to_raw_json());
        if let Some(ref u) = m.unit {
            b = b.units(serde_json::json!(u));
        }
    }

    // Validators. Wire contract for `outcome` is uppercase
    // (`PASS`/`FAIL`/`UNSET`) — the V2 Zod schema rejects anything else.
    if let Some(ref vs) = m.validators {
        let validators = map_validators!(RunCreateMeasurementsValidators, vs);
        if !validators.is_empty() {
            b = b.validators(validators);
        }
    }

    // Aggregations
    if let Some(ref aggs) = m.aggregations {
        let aggregations = map_aggregations!(
            RunCreateMeasurementsAggregations,
            RunCreateMeasurementsAggregationsValidators,
            aggs
        );
        if !aggregations.is_empty() {
            b = b.aggregations(aggregations);
        }
    }

    if let Some(ref d) = m.description {
        b = b.docstring(d);
    }

    b.build().map_err(|e| e.to_string().into())
}

/// The grouping fields stamped on a multi-slot upload:
/// `(execution_id, slot_key, slot_name)`. None on a single-slot run,
/// which keeps its wire shape exactly as before multi-slot existed.
type SlotStamp<'a> = Option<(&'a str, &'a str, Option<&'a str>)>;

/// One slot's `runs.create` request, projected out of the shared
/// accumulator: the slot's own phases plus the shared stages, its unit,
/// its metadata, and the outcome the engine computed for it.
fn build_run_request(
    data: &RunData,
    slot: &str,
    stamp: SlotStamp<'_>,
    procedure_id: &str,
    procedure_dir: &Path,
    // The procedure file actually being run. Passed explicitly rather
    // than re-derived from `procedure_dir`: a manifest `entry_point` can
    // name any `.yaml`, so the file is not always `procedure.yaml`.
    procedure_yaml: &Path,
    operated_by: Option<&str>,
) -> crate::error::CliResult<RunCreateRequest> {
    // Multi-slot: the engine's per-slot outcome (own phases plus shared
    // stages, under the slot's own stop state), never the run rollup that
    // would mark every slot FAIL for one failing unit. Single slot keeps
    // the run outcome it always uploaded. Crash paths have neither → Error.
    let outcome = stamp
        .and_then(|_| data.slot_outcomes.get(slot))
        .or(data.run_outcome.as_ref())
        .map(engine_outcome_to_sdk)
        .unwrap_or(RunGetOutcome::Error);

    // A slot's run spans the slot's OWN phases, not the whole execution:
    // a slot done at t+10s of a 5-minute execution must not upload as a
    // 5-minute run, and the shared teardown that waits for the last slot
    // must not stretch every slot's duration to it. Shared stages are
    // still listed in `phases`. Falls back to the shared stages' window
    // when the slot ran nothing of its own, then to the execution window.
    let exec_started = data.start_time.unwrap_or_else(chrono::Utc::now);
    let exec_ended = data.end_time.unwrap_or_else(chrono::Utc::now);
    let slot_phases: Vec<&CompletedPhase> = data
        .phases
        .iter()
        .filter(|p| belongs_to_slot(&p.slot_id, slot))
        .collect();
    let window = |own_only: bool| {
        let times = slot_phases
            .iter()
            .filter(|p| !own_only || p.slot_id.is_some())
            .filter_map(|p| {
                Some((
                    super::time_fmt::parse_rfc3339(&p.started_at)?,
                    super::time_fmt::parse_rfc3339(&p.completed_at)?,
                ))
            });
        let (mut start, mut end) = (None, None);
        for (s, e) in times {
            start = Some(start.map_or(s, |v: chrono::DateTime<chrono::Utc>| v.min(s)));
            end = Some(end.map_or(e, |v: chrono::DateTime<chrono::Utc>| v.max(e)));
        }
        start.zip(end)
    };
    let (started_at, ended_at) = window(true)
        .or_else(|| window(false))
        .unwrap_or((exec_started, exec_ended));

    let phases: Vec<RunCreatePhases> = slot_phases
        .iter()
        .filter_map(|p| {
            let measurements: Vec<RunCreateMeasurements> = p
                .measurements
                .iter()
                .filter_map(|m| build_measurement(m).ok())
                .collect();

            let phase_started = super::time_fmt::parse_rfc3339(&p.started_at).unwrap_or(started_at);
            let phase_ended = super::time_fmt::parse_rfc3339(&p.completed_at).unwrap_or(ended_at);

            let mut b = RunCreatePhases::builder()
                .name(&p.name)
                .outcome(engine_outcome_to_phase(&p.outcome))
                .started_at(phase_started)
                .ended_at(phase_ended)
                .measurements(measurements);

            if p.retry_count > 0 {
                b = b.retry_count(p.retry_count as i64);
            }
            if let Some(ref e) = p.error {
                b = b.docstring(e);
            }

            b.build().ok()
        })
        .collect();

    // Collect logs from the slot's phases (shared stages included) into
    // run-level logs
    let logs: Vec<RunCreateLogs> = slot_phases
        .iter()
        .flat_map(|p| {
            p.logs.iter().map(|l| {
                let level = super::outcomes::parse_log_level(l.level.as_str());
                let ts =
                    super::time_fmt::parse_rfc3339(&l.timestamp).unwrap_or_else(chrono::Utc::now);
                RunCreateLogs {
                    level,
                    timestamp: ts,
                    message: l.message.clone(),
                    source_file: super::log_source::sanitize_source_file(
                        l.file.as_deref().unwrap_or(""),
                        procedure_dir,
                    ),
                    line_number: l.line.unwrap_or(0) as i64,
                }
            })
        })
        .collect();

    let unit = data.units.get(slot).cloned().unwrap_or_default();
    let serial = unit.serial.clone().unwrap_or_else(|| "UNKNOWN".to_string());

    let mut b = RunCreateRequest::builder()
        .outcome(outcome)
        .procedure_id(procedure_id)
        .serial_number(&serial)
        .started_at(started_at)
        .ended_at(ended_at)
        .phases(phases);

    if !logs.is_empty() {
        b = b.logs(logs);
    }

    if let Some(ref pn) = unit.part {
        b = b.part_number(pn);
    }
    if let Some(ref rn) = unit.revision {
        b = b.revision_number(rn);
    }
    if let Some(ref bn) = unit.batch {
        b = b.batch_number(bn);
    }
    if let Some(ref su) = unit.sub_units {
        b = b.sub_units(su.clone());
    }

    if let Some((execution_id, slot_key, slot_name)) = stamp {
        b = b.execution_id(execution_id).slot_key(slot_key);
        if let Some(name) = slot_name {
            b = b.slot_name(name);
        }
    }

    // Empty maps must not call the builders — the SDK would serialize
    // `"metadata": {}` instead of omitting the field. Per-phase writes
    // are capped at 50 keys each, but multiple phases can accumulate
    // past the server's 50-keys-per-entity limit; cap here so one
    // oversized map doesn't reject the whole run upload.
    let run_md = cap_metadata_keys(
        "run metadata",
        &fold_metadata_sources(&data.run_metadata_sources, slot),
    );
    if !run_md.is_empty() {
        b = b.metadata(run_md);
    }
    let unit_md = cap_metadata_keys(
        "unit metadata",
        &fold_metadata_sources(&data.unit_metadata_sources, slot),
    );
    if !unit_md.is_empty() {
        b = b.unit_metadata(unit_md);
    }

    if let Some(version) = super::procedure_version::read_yaml_version(procedure_yaml) {
        b = b.procedure_version(version);
    }

    if let Some(deployment_id) = super::deployment_id::lookup_deployment_id(procedure_id) {
        b = b.deployment_id(deployment_id);
    }

    // Operated-by resolved through the identify pipeline (prompt,
    // `run.operated_by` binding, Python write) wins over the session
    // email forwarded on the WS run command. The value may be a member
    // email (server links the account) or a free-text operator name
    // (server records it verbatim). Blank values fall through: a
    // cleared bound text input ships "" on the wire, and attributing a
    // run to "" is never right (the server also rejects it).
    // The blank filter drives the FALLBACK (an emptied prompt must fall
    // through to the session email); clamp_operated_by is the boundary guard
    // on whatever wins. See its doc comment for why over-length is clamped.
    let candidate = unit
        .operated_by
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or(operated_by);
    if let Some(value) = super::clamp_operated_by(candidate) {
        b = b.operated_by(value);
    }

    b.build().map_err(|e| e.to_string().into())
}

/// Convert the engine's `UnitInfo` (used internally by the framework)
/// to the station-protocol wire shape (`Option<String>` for each
/// scalar, `HashMap<String, String>` for sub-units). This is the form
/// operator-UI / dashboard / station bridge consume on
/// `StationEvent::RunStarted.unit`.
fn unit_info_to_wire(info: &execution_engine::unit::UnitInfo) -> station_protocol::UnitInfo {
    station_protocol::UnitInfo {
        serial_number: info.serial_number.clone(),
        part_number: info.part_number.clone(),
        revision_number: info.revision_number.clone(),
        batch_number: info.batch_number.clone(),
        sub_units: info.sub_units.clone().unwrap_or_default(),
    }
}

/// Reverse of `unit_info_to_wire`: a wire `UnitInfo` (sent by the
/// operator UI's "Run again" button or other reuse path) becomes the
/// engine-side `UnitInfo` the orchestrator expects. Empty `sub_units`
/// becomes `None` so downstream emptiness checks behave the same as a
/// fresh identify with no sub-unit fields.
fn wire_unit_to_engine(info: station_protocol::UnitInfo) -> execution_engine::unit::UnitInfo {
    let sub_units = if info.sub_units.is_empty() {
        None
    } else {
        Some(info.sub_units)
    };
    execution_engine::unit::UnitInfo {
        serial_number: info.serial_number,
        part_number: info.part_number,
        revision_number: info.revision_number,
        batch_number: info.batch_number,
        // Run attribution is not on the wire UnitInfo — a "Run again"
        // reuse re-attributes from the session email instead.
        operated_by: None,
        sub_units,
        status: String::new(),
        // Wire UnitInfo carries no metadata — a "Run again" reuse
        // uploads without operator-entered metadata (v1 limitation;
        // the prior run already upserted it server-side).
        metadata: None,
    }
}

/// Write a resolved `UnitInfo` into the shared `RunData` mutex so
/// `build_run_request` reads the operator-supplied serial / part
/// instead of the "UNKNOWN" fallback used when identify is skipped.
///
/// The synchronous CLI path calls this once per slot from
/// `run_yaml_procedure`. `EventSink::emit` (sync) also calls it for
/// mid-run updates, via `spawn_run_data_write`: those deferred writes
/// are ticketed in `CliEventSink::pending_writes` and awaited after
/// engine shutdown, before `build_run_request` snapshots RunData — so
/// a fast-ending run can no longer race the upload ahead of them.
///
/// Sub-units are flattened to a `Vec<String>` of serials sorted by
/// key — the SDK's `RunCreateRequest.sub_units` is `Option<Vec<String>>`,
/// so keys aren't transmitted; sorting keeps the on-wire order stable
/// across runs / hosts / hashmap implementations.
async fn apply_unit_info_to_run_data(
    run_data: &Arc<Mutex<RunData>>,
    slot_id: &str,
    info: &execution_engine::unit::UnitInfo,
) {
    let mut d = run_data.lock().await;
    let unit = d.units.entry(slot_id.to_string()).or_default();
    if let Some(ref sn) = info.serial_number {
        unit.serial = Some(sn.clone());
    }
    if let Some(ref pn) = info.part_number {
        unit.part = Some(pn.clone());
    }
    if let Some(ref rn) = info.revision_number {
        unit.revision = Some(rn.clone());
    }
    if let Some(ref bn) = info.batch_number {
        unit.batch = Some(bn.clone());
    }
    if let Some(ref ob) = info.operated_by {
        unit.operated_by = Some(ob.clone());
    }
    if let Some(ref sub) = info.sub_units {
        let mut entries: Vec<(&String, &String)> = sub.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        unit.sub_units = Some(entries.into_iter().map(|(_, v)| v.clone()).collect());
    }
    // Operator-entered identify-form metadata lands pre-run as the
    // first source; Python `unit.metadata[...]` writes arrive later via
    // JobComplete and override per key (operator form < Python API).
    if let Some(ref md) = info.metadata {
        let map: std::collections::HashMap<String, serde_json::Value> = md
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        upsert_metadata_source(
            &mut d.unit_metadata_sources,
            &Some(slot_id.to_string()),
            "identify_unit",
            map,
        );
    }
}

/// The attachments of one slot's run: its own, plus the shared stages'.
/// The upload queue deletes a file once uploaded, so a shared attachment
/// referenced by several runs would vanish under the second one; every
/// slot after the first gets its own copy (a hard link where the
/// filesystem allows it) next to the original.
fn attachments_for_slot(
    all: &[(
        Option<String>,
        crate::commands::run::queue::QueuedAttachment,
    )],
    slot: &str,
    slot_index: usize,
) -> Vec<crate::commands::run::queue::QueuedAttachment> {
    all.iter()
        .filter(|(s, _)| belongs_to_slot(s, slot))
        .filter_map(|(s, att)| {
            if s.is_some() || slot_index == 0 {
                return Some(att.clone());
            }
            let source = Path::new(&att.path);
            let file_name = source.file_name()?.to_string_lossy();
            let safe_slot: String = slot
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            let copy = source.with_file_name(format!("{safe_slot}.{file_name}"));
            let linked = std::fs::hard_link(source, &copy)
                .or_else(|_| std::fs::copy(source, &copy).map(|_| ()));
            match linked {
                Ok(()) => Some(crate::commands::run::queue::QueuedAttachment {
                    path: copy.to_string_lossy().to_string(),
                    ..att.clone()
                }),
                Err(e) => {
                    crate::log::warn(&format!(
                        "attachment '{}' not duplicated for slot {slot}: {e}",
                        att.name
                    ));
                    None
                }
            }
        })
        .collect()
}

/// Run a YAML procedure and return the QueuedRuns for upload, one per
/// slot (a single-slot procedure yields exactly one).
#[allow(clippy::too_many_arguments)]
// `python_path`: pre-resolved venv interpreter for this deployment.
// Threaded into the orchestrator so workers + plug services skip the
// engine's `resolve_python` walk-up. CLI's `prepare_run` computes it
// deterministically from `<package_dir>/venv/`.
pub async fn run_yaml_procedure(
    procedure_yaml: &Path,
    procedure_dir: &Path,
    python_path: &Path,
    procedure_id: &str,
    procedure_name: &str,
    execution_id: &str,
    event_tx: broadcast::Sender<StationEvent>,
    ui_tx: Option<mpsc::Sender<UiRequestData>>,
    agent: Option<AgentProtoCtx>,
    // Whether any operator surface can answer a unit-identify prompt.
    // False on a fully headless run, which makes `identify` fail fast
    // instead of hanging on a prompt nobody can answer.
    has_ui: bool,
    // Pre-resolved unit data from the operator UI's "Run again"
    // flow. When set, the identify step is bypassed entirely: the
    // CLI feeds the supplied unit straight into the run plan and
    // emits an `identify_resolved` event so downstream consumers
    // (UI, dashboard upload) see the same wire signal as a normal
    // identify path.
    reuse_unit: Option<station_protocol::UnitInfo>,
    // Multi-slot "Run again": unit to reuse per slot key. A slot absent
    // from the map falls back to `reuse_unit`, then to its identify
    // prompt.
    reuse_units: Option<std::collections::HashMap<String, station_protocol::UnitInfo>>,
    // Email forwarded to `runs.create` as `operated_by`. Set when the
    // run was triggered from the web operator UI; None for kiosk and
    // CLI-driven runs.
    operated_by: Option<String>,
    // Single cancel surface. `cancel_rx.wait_any()` resolves on the
    // first Stop request (flip `shutdown_requested`); `wait_force()`
    // resolves on a Kill request (parallel SIGKILL via
    // `force_kill_immediate`). The watch lets the same receiver
    // observe escalation from Stop → Kill without a second oneshot.
    cancel_rx: super::cancel::Receiver,
    // Per-run options umbrella (debug, station plug host, partial-run
    // phase selection) — passed wholesale so new run-scoped flags don't
    // keep growing this signature.
    run_opts: super::RunOptions,
) -> (i32, Vec<QueuedRun>) {
    let super::RunOptions {
        debug,
        station_plug_host,
        only_phase,
        ..
    } = run_opts;
    let procedure_def = match load_procedure_definition(procedure_yaml) {
        Ok(def) => def,
        Err(e) => {
            emit_crash(
                &event_tx,
                &agent,
                procedure_id,
                execution_id,
                "load_error",
                1,
                format!("Failed to load procedure: {e}"),
            );
            return (1, Vec::new());
        }
    };

    // Loading is structural: a dangling `python:` reference passes it and
    // then fails mid-run — an unreadable tp_worker traceback for a phase,
    // a silently omitted plug argument for a plug. Refuse to start instead,
    // with every unresolvable reference named. Refs resolve against
    // `procedure_dir` (the package dir the orchestrator hands the worker),
    // NOT the YAML's parent — they differ for a nested `entry_point`. On a
    // partial run only the target's dependency closure gates — plugs not at
    // all, since the runtime narrows the plug set by introspection; an
    // invalid target skips the gate entirely so initialization reports the
    // target problem instead of an unrelated dangling ref.
    let main_filter = only_phase.as_deref().map(|target| {
        execution_engine::orchestrator::partial_main_phase_set(&procedure_def, target)
    });
    // Errors only: a `Warning` is a likely mistake the lint cannot be
    // certain of, reported by `validate` and never a reason to refuse.
    let ref_problems: Vec<String> = match &main_filter {
        Some(Err(_)) => Vec::new(),
        Some(Ok(set)) => procedure_def.resolve_runtime_refs(procedure_dir, Some(set)),
        None => procedure_def.resolve_runtime_refs(procedure_dir, None),
    }
    .into_iter()
    .filter(|p| p.is_error())
    .map(|p| p.message)
    .collect();
    if !ref_problems.is_empty() {
        emit_crash(
            &event_tx,
            &agent,
            procedure_id,
            execution_id,
            "load_error",
            1,
            format!("Procedure cannot start:\n{}", ref_problems.join("\n")),
        );
        return (1, Vec::new());
    }

    // Debug mode forces a single worker so the fixed debug port doesn't
    // collide across the pool.
    let worker_count = if debug.enabled {
        1
    } else {
        procedure_def
            .execution
            .as_ref()
            .map(|e| e.workers)
            .unwrap_or(4)
    };

    let strategy = procedure_def
        .execution
        .as_ref()
        .map(|e| e.strategy)
        .unwrap_or(execution_engine::procedure::schema::ExecutionStrategy::PhaseFirst);

    let slots: Vec<String> = procedure_def
        .execution
        .as_ref()
        .map(|e| {
            if e.slots.is_empty() {
                vec!["default".to_string()]
            } else {
                e.slots.iter().map(|s| s.key.clone()).collect()
            }
        })
        .unwrap_or_else(|| vec!["default".to_string()]);

    // Orchestrator wants its own owned String + a separate run_id (engine-
    // internal ids it stamps on its workers / reports). The wire-side
    // `execution_id` we receive is the same identity, just owned-by-caller.
    let orchestrator_execution_id = execution_id.to_string();
    let run_id = uuid::Uuid::new_v4().to_string();

    // Display names of the declared slots, keyed by slot key. Stamped on
    // each slot's upload as `slot_name` and expanded into `{slot_name}`
    // defaults. Empty on the synthetic single slot.
    let slot_names: std::collections::HashMap<String, String> = procedure_def
        .execution
        .as_ref()
        .map(|e| {
            e.slots
                .iter()
                .map(|s| (s.key.clone(), s.name.clone()))
                .collect()
        })
        .unwrap_or_default();

    // Snapshot the unit config before `procedure_def` moves into the
    // orchestrator. `identify(...)` only needs the unit block.
    //
    // YAML procedures without a `unit:` block historically skipped the
    // identify step entirely, ran phases, and queued an upload with
    // empty serial/part — which the API rejects (both fields min(1)).
    // Fall back to a default config that prompts for the two API-
    // required fields so simple "hello world" templates that omit
    // `unit:` still upload correctly.
    // Same rationale applied per field: a `unit:` block that declares
    // only one of the two (e.g. `serial_number:` alone) used to make
    // the identify host hard-error the whole run with "invalid unit
    // config: unit config missing part_number" — the loader does NOT
    // enforce presence. Defaulting the missing field prompts for it
    // instead, exactly like the whole-block fallback.
    let unit_cfg = procedure_def
        .unit
        .clone()
        .or_else(|| Some(execution_engine::procedure::UnitConfig::default()))
        .map(|mut cfg| {
            cfg.serial_number
                .get_or_insert_with(execution_engine::procedure::UnitFieldConfig::default);
            cfg.part_number
                .get_or_insert_with(execution_engine::procedure::UnitFieldConfig::default);
            cfg
        });
    // Same snapshot rationale: the procedure-root `operated_by:` (run
    // attribution) is read by `identify(...)` after `procedure_def`
    // moves into the orchestrator.
    let operated_by_cfg = procedure_def.operated_by.clone();

    // Scratch dir the engine writes attachment bytes into so every
    // `AttachmentAdded` carries an on-disk path the CLI can upload then
    // delete — the same `~/.tofupilot/attachments/<id>/` convention the
    // openhtf/robot connectors use, so the kiosk `/attachments/*` route
    // and the upload queue's per-run cleanup work uniformly. Keyed by
    // execution_id (the queue_id isn't minted until post-run upload).
    // None on failure to resolve home: the run still executes, only the
    // attachments are skipped (mirrors the prior report-dir-absent path).
    let attachment_dir = super::super::db::home_dir().ok().map(|home| {
        home.join(".tofupilot")
            .join("attachments")
            .join(execution_id)
    });

    let mut orchestrator = Orchestrator::new_with_python(
        worker_count,
        procedure_dir.to_path_buf(),
        Some(python_path.to_path_buf()),
        attachment_dir.clone(),
        orchestrator_execution_id,
        run_id,
        procedure_def,
        if debug.enabled {
            Some(debug.port)
        } else {
            None
        },
    )
    .with_station_plug_host(station_plug_host);

    // Worker stderr is not surfaced on the CLI, so the "waiting for
    // debugger" notice must come from here. The worker blocks in
    // wait_for_client() until an IDE attaches on this port.
    if debug.enabled {
        crate::log::info(&format!(
            "Debug mode: waiting for a debugger to attach on localhost:{} (VS Code: \"Python: Attach\"). Phase timeouts are disabled.",
            debug.port
        ));
    }

    let sink = CliEventSink::new(
        event_tx.clone(),
        ui_tx.clone(),
        agent.clone(),
        procedure_name.to_string(),
        procedure_id.to_string(),
        execution_id.to_string(),
        super::deployment_id::lookup_deployment_id(procedure_id),
        only_phase.clone(),
        slots.clone(),
        slot_names.clone(),
    );
    let run_data = sink.data.clone();
    let pending_run_data_writes = sink.pending_writes.clone();
    let engine_progressed = sink.progressed.clone();
    let event_sink: Arc<dyn EventSink> = Arc::new(sink);
    orchestrator.set_event_sink(event_sink.clone());

    // Stamp partial runs so an upload stays identifiable in the
    // dashboard — a partial PASS covers only the phases that ran, not
    // the whole procedure. `build_run_request` folds this into
    // `runs.create`'s `metadata`; no API or SDK change.
    if let Some(phase_key) = &only_phase {
        run_data.lock().await.run_metadata_sources.push((
            None,
            "studio_partial_run".to_string(),
            std::iter::once((
                "studio_partial_run".to_string(),
                serde_json::Value::String(phase_key.clone()),
            ))
            .collect(),
        ));
    }

    if let Err(e) = orchestrator.initialize().await {
        emit_crash(
            &event_tx,
            &agent,
            procedure_id,
            execution_id,
            "init_error",
            1,
            format!("Failed to initialize execution engine: {e}"),
        );
        // initialize() may have spawned a partial worker pool
        // before failing — tear it down so we don't leak.
        let _ = orchestrator.shutdown().await;
        return (1, Vec::new());
    }

    // Identify-unit step: canonical framework entry point. It always
    // runs: `unit_cfg` above defaults the block when the YAML omits it,
    // so the `(None, _)` arms below are unreachable in practice and
    // only satisfy the match on the `Option`.
    // `auto_identify: true` resolves from `default_value`s without an
    // operator prompt; otherwise the host emits a `UiRequest` and
    // awaits the response. Resolved info is written directly into
    // RunData (synchronous, before `submit_procedure`) so the upload
    // path always sees the real serial/part instead of "UNKNOWN" — even
    // for runs that abort before any phase runs. The `UnitIdentified`
    // event is also emitted on the sink for downstream observers
    // (TUI / agent / dashboard) that prefer a structured signal over
    // peeking at RunData.
    // Per-slot unit resolution. A slot whose unit the caller supplied
    // ("Run again": `reuse_units[slot]`, else the whole-fixture
    // `reuse_unit`) skips its identify prompt; every other slot is
    // identified as usual, one prompt at a time. Reuse is gated on
    // `unit_cfg` (always Some thanks to the default block above): a
    // procedure with no unit schema has no shape for a reused unit.
    let identify_host = identify_host::CliIdentifyHost {
        router: EventRouter::new(event_tx.clone(), agent.clone(), execution_id.to_string()),
        ui_tx: ui_tx.clone(),
        agent: agent.clone(),
        procedure_id: procedure_id.to_string(),
        has_ui,
    };
    let mut identify_cancel = cancel_rx.clone();
    let mut unit_infos: std::collections::HashMap<String, execution_engine::unit::UnitInfo> =
        std::collections::HashMap::new();
    for slot_id in &slots {
        let Some(cfg) = unit_cfg.as_ref() else { break };
        let reused = reuse_units
            .as_ref()
            .and_then(|m| m.get(slot_id).cloned())
            .or_else(|| reuse_unit.clone());
        let info = match reused {
            Some(reused) => {
                // No validation here, deliberately. The reused values
                // were already accepted by the identify form that
                // produced them; re-judging them against the
                // procedure's CURRENT `unit:` config turned a config
                // edit between two runs into an instant crash the
                // operator could neither understand nor correct — the
                // reuse path has no prompt to fall back to (TP-1092).
                // "Run again" means exactly that: the same unit, the
                // same values, no second gate.
                wire_unit_to_engine(reused)
            }
            None => {
                // Race the operator prompt against cancellation: a Stop
                // while parked on identify-unit must not hang the run
                // task. `execution_engine::identify` parks on a oneshot
                // inside the IdentifyHost; without this select neither it
                // nor the orchestrator cancel loop (which only runs after
                // identify resolves) ever sees the signal.
                let identify_fut = execution_engine::identify(
                    cfg,
                    operated_by_cfg.as_ref(),
                    Some(execution_engine::SlotRef::new(
                        slot_id,
                        slot_names.get(slot_id).map(String::as_str),
                    )),
                    &identify_host,
                );
                tokio::pin!(identify_fut);
                let result = tokio::select! {
                    r = &mut identify_fut => Some(r),
                    _ = identify_cancel.wait_any() => None,
                };
                match result {
                    Some(Ok(info)) => info,
                    Some(Err(err)) => {
                        emit_crash(
                            &event_tx,
                            &agent,
                            procedure_id,
                            execution_id,
                            "identify_unit_failed",
                            1,
                            format!("{err}"),
                        );
                        let _ = orchestrator.shutdown().await;
                        return (1, Vec::new());
                    }
                    None => {
                        // Cancel during identify: drop any parked UI
                        // prompt sender so consumers stop waiting, then
                        // crash with ABORTED so the operator-UI flips off
                        // the prompt screen.
                        crate::commands::run::ui_response::cancel_all().await;
                        super::emit::run_complete(
                            &event_tx,
                            super::outcomes::ABORTED,
                            execution_id,
                            None,
                            Default::default(),
                        );
                        let _ = orchestrator.shutdown().await;
                        return (1, Vec::new());
                    }
                }
            }
        };
        // Written into RunData synchronously (before `submit_procedure`)
        // so the upload always sees the real serial, even for runs that
        // abort before any phase; `UnitIdentified` fans out to
        // `identify_resolved` on the wire for every observer.
        apply_unit_info_to_run_data(&run_data, slot_id, &info).await;
        event_sink.emit(&ExecutionEvent::UnitIdentified {
            slot_id: Some(slot_id.clone()),
            unit_info: info.clone(),
        });
        unit_infos.insert(slot_id.clone(), info);
    }

    if let Err(e) = orchestrator
        .submit_procedure(slots.clone(), strategy, unit_infos, only_phase.as_deref())
        .await
    {
        emit_crash(
            &event_tx,
            &agent,
            procedure_id,
            execution_id,
            "submit_error",
            1,
            format!("Failed to submit procedure: {e}"),
        );
        let _ = orchestrator.shutdown().await;
        return (1, Vec::new());
    }

    // Clone Arcs out of the orchestrator so the Stop/Kill paths can
    // mutate state and tear down workers concurrently with `execute_all`.
    // `force_kill_immediate` is intentionally a static fn taking these
    // Arcs (mirroring studio): reading the same shared state the running
    // orchestrator reads, so flag flips and parallel-SIGKILL race correctly
    // against the in-flight scheduling loop.
    let state_arc = orchestrator.state.clone();
    let workers_arc = orchestrator.workers.clone();
    let resource_arc = orchestrator.resource_manager.clone();
    let event_sink_for_kill = event_sink.clone();
    // Outlives the select below: the engine's shutdown reads the stop
    // deadline from it and keeps watching for a force kill.
    let mut shutdown_rx = cancel_rx.clone();
    let mut force_fired = false;

    // Run `execute_all` inside a scope so its `&mut orchestrator` borrow
    // is released before we call `orchestrator.shutdown()` below. The
    // select loop holds the borrow via the pinned future; once the block
    // returns, the future is dropped and the borrow ends.
    let exec_result = {
        let exec_fut = orchestrator.execute_all();
        tokio::pin!(exec_fut);

        // Two clones of the watch receiver — one for the graceful arm,
        // one for the force arm. select! takes mutable refs to both
        // arm futures, so the borrow checker rejects re-borrowing the
        // same receiver in two arms. Watch::Receiver clones are cheap
        // (one Arc).
        let mut graceful_rx = cancel_rx.clone();
        let mut interrupt_rx = cancel_rx.clone();
        let mut force_rx = cancel_rx;

        let mut graceful_fired = false;
        let mut interrupt_fired = false;
        loop {
            tokio::select! {
                // Resolves when execution completes naturally (or after a
                // graceful shutdown_requested flip lets the loop drain).
                res = &mut exec_fut => break res,

                // Dispatch-stall watchdog: the engine accepted the
                // procedure but emitted nothing at all. The arm resolves
                // only if `engine_progressed` is still false when the
                // window elapses (the helper polls the flag and pends
                // forever once it flips — same mechanism as the OpenHTF
                // connector's startup watchdog). Debug runs are exempt:
                // a debugger paused before the first event is
                // indistinguishable from a stall.
                _ = super::connector::startup_stall_elapsed(
                    &engine_progressed,
                    crate::config::timeouts::ENGINE_DISPATCH_STALL,
                ), if !debug.enabled => {
                    let secs = crate::config::timeouts::ENGINE_DISPATCH_STALL.as_secs();
                    if let Err(e) = Orchestrator::force_kill_immediate(
                        state_arc.clone(),
                        workers_arc.clone(),
                        resource_arc.clone(),
                        None,
                        event_sink_for_kill.clone(),
                    ).await {
                        crate::log::warn(&format!("force_kill_immediate failed: {e}"));
                    }
                    // Await the unblocked execute_all so worker teardown
                    // completes, then override its result with the stall
                    // diagnosis — whatever it returns after a force-kill
                    // is a symptom, not the cause.
                    let _ = (&mut exec_fut).await;
                    let log_hint = super::run_log::log_path(execution_id)
                        .map(|p| format!(" Full event log: {}", p.display()))
                        .unwrap_or_default();
                    break Err(format!(
                        "The engine did not start executing within {secs}s of \
                         submitting the procedure: no phase was dispatched and no \
                         event was emitted. The run was terminated. This usually \
                         means the machine's endpoint-protection (EDR/antivirus) \
                         software is holding the Python worker processes — check \
                         its logs and allowlist the deployment's venv Python.{log_hint}"
                    ));
                }

                // Stop: flip the shared flag, keep awaiting `execute_all`
                // so teardown phases run and plugs close cleanly. Don't
                // break — loop picks up the natural-completion arm next.
                _ = graceful_rx.wait_any(), if !graceful_fired => {
                    graceful_fired = true;
                    // Through `request_shutdown`, never the bare flag: the
                    // aggregation needs the cause to tell this operator stop
                    // (→ ABORTED) from the engine stopping itself after a
                    // failed phase (→ FAIL). Writing only the flag once made
                    // a cancelled run upload as PASS.
                    state_arc
                        .write()
                        .await
                        .request_shutdown(execution_engine::state::ShutdownCause::Operator);
                }

                // Interrupt: a stop with a deadline behind it (SIGTERM,
                // console close). The running phases are killed so
                // `execute_all` drains now instead of after an
                // hours-long phase; the `shutdown()` below still runs
                // the teardown phases and powers the bench down, which
                // a force kill would skip.
                _ = interrupt_rx.wait_interrupt(), if !interrupt_fired => {
                    interrupt_fired = true;
                    graceful_fired = true;
                    Orchestrator::interrupt_running_jobs(
                        state_arc.clone(),
                        workers_arc.clone(),
                    ).await;
                }

                // Kill: force_kill_immediate runs in parallel with
                // execute_all (touching the same Arcs). After it returns,
                // `execute_all` unblocks because workers are gone — await
                // it once more to collect the result.
                _ = force_rx.wait_force() => {
                    force_fired = true;
                    if let Err(e) = Orchestrator::force_kill_immediate(
                        state_arc.clone(),
                        workers_arc.clone(),
                        resource_arc.clone(),
                        None,
                        event_sink_for_kill.clone(),
                    ).await {
                        crate::log::warn(&format!("force_kill_immediate failed: {e}"));
                    }
                    break (&mut exec_fut).await;
                }
            }
        }
    };

    let exit_code = match exec_result {
        Ok(stats) => match stats.run_outcome {
            Some(Outcome::Pass) => 0,
            _ => 1,
        },
        Err(e) => {
            emit_crash(
                &event_tx,
                &agent,
                procedure_id,
                execution_id,
                "execution_error",
                1,
                format!("Execution failed: {e}"),
            );
            // Even on execution error: tear down the worker pool
            // before returning so the python `tp_worker.py` /
            // `tp_plug.py` subprocesses don't outlive the run.
            // Studio does this at every run-completion site; the
            // CLI station-mode loop spawns a fresh orchestrator per
            // run, and the previous one's workers leaked otherwise.
            let _ = shutdown_engine(
                &mut orchestrator,
                &mut shutdown_rx,
                force_fired,
                &state_arc,
                &workers_arc,
                &resource_arc,
                &event_sink_for_kill,
            )
            .await;
            // No upload queue runs on this path, so sweep any attachments
            // already written this run instead of leaking the scratch dir.
            if let Some(dir) = &attachment_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            return (1, Vec::new());
        }
    };

    // Tear down worker pool + plug processes. Without this,
    // `tp_worker.py` and `tp_plug.py` subprocesses spawned by
    // `Orchestrator::initialize` outlive the run — in station
    // mode this leaks ~7 processes per run (4 workers + 3 plugs
    // for the demo procedure), saturating the host within a few
    // dozen runs.
    if let Err(e) = shutdown_engine(
        &mut orchestrator,
        &mut shutdown_rx,
        force_fired,
        &state_arc,
        &workers_arc,
        &resource_arc,
        &event_sink_for_kill,
    )
    .await
    {
        crate::log::warn(&format!("Orchestrator shutdown error: {e}"));
    }

    // Rendezvous barrier: emit() defers its RunData writes to spawned
    // tasks (see CliEventSink::pending_writes). The engine is shut down,
    // so nothing new can be queued — await every outstanding ticket
    // before snapshotting, so the last event's write (e.g. a final-phase
    // `run.operated_by`) can't lose the race against the upload payload.
    let pending: Vec<tokio::task::JoinHandle<()>> = pending_run_data_writes
        .lock()
        .map(|mut writes| writes.drain(..).collect())
        .unwrap_or_default();
    for handle in pending {
        let _ = handle.await;
    }

    // One RunCreateRequest per slot out of the accumulated data. A
    // single-slot procedure (the synthetic "default" slot included)
    // keeps the exact pre-multi-slot wire shape: no execution_id /
    // slot_key stamped, one QueuedRun.
    // Last, and only while the OS still leaves time: under a console
    // close the bench had priority, and a queue write the OS cuts short
    // is a wasted transaction, not a queued run.
    let stop_deadline_passed = shutdown_rx
        .deadline()
        .is_some_and(|deadline| std::time::Instant::now() >= deadline);
    if stop_deadline_passed {
        crate::log::warn("Stop deadline passed before the partial runs could be queued");
    }
    let data = run_data.lock().await;
    let is_multi = slots.len() > 1;
    let mut queued_runs: Vec<QueuedRun> = Vec::with_capacity(slots.len());
    for (i, slot) in slots.iter().enumerate() {
        if stop_deadline_passed {
            break;
        }
        let stamp: SlotStamp<'_> = is_multi.then_some((
            execution_id,
            slot.as_str(),
            slot_names.get(slot).map(String::as_str),
        ));
        match build_run_request(
            &data,
            slot,
            stamp,
            procedure_id,
            procedure_dir,
            procedure_yaml,
            operated_by.as_deref(),
        ) {
            Ok(request) => queued_runs.push(QueuedRun {
                request,
                attachments: attachments_for_slot(&data.attachments, slot, i),
                run_id: None,
                attempt_count: 0,
                last_attempt_at: None,
                next_retry_at: None,
                parked: false,
                last_error: None,
                queued_at: None,
            }),
            Err(e) => {
                crate::log::error(&format!("Failed to build run request for slot {slot}: {e}"));
            }
        }
    }
    if queued_runs.is_empty() {
        // No QueuedRun means the upload queue never runs, so its per-run
        // attachment cleanup never fires. Drop the scratch dir here or
        // every failed-to-build run leaks its attachment files under
        // ~/.tofupilot/attachments/ forever.
        if let Some(dir) = &attachment_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    (exit_code, queued_runs)
}

/// The engine's shutdown once `execute_all` has returned: the teardown
/// phases, then the plug release. Reports the teardown started, so the
/// signal ladder arms its cap now and not at the signal; takes the
/// hurried path when an OS deadline is behind the stop; and keeps a
/// force kill observable meanwhile. `shutdown()` reads the force flag
/// on entry only, so without the watcher a second signal during a
/// wedged power-off changed nothing until the engine's own cap.
async fn shutdown_engine(
    orchestrator: &mut Orchestrator,
    cancel_rx: &mut super::cancel::Receiver,
    force_fired: bool,
    state: &Arc<tokio::sync::RwLock<execution_engine::state::OrchestratorState>>,
    workers: &Arc<tokio::sync::RwLock<Vec<execution_engine::worker::Worker>>>,
    resource_manager: &Arc<tokio::sync::RwLock<execution_engine::plugs::manager::ResourceManager>>,
    event_sink: &Arc<dyn EventSink>,
) -> Result<(), String> {
    use execution_engine::orchestrator::ShutdownMode;

    cancel_rx.mark_teardown_started();
    let mode = if cancel_rx.deadline().is_some() {
        crate::log::warn(
            "Stop under an OS deadline: hurried teardown, plugs released without cleanup",
        );
        ShutdownMode::Hurried
    } else {
        ShutdownMode::Normal
    };
    let shutdown = orchestrator.shutdown_with(mode);
    tokio::pin!(shutdown);
    if force_fired {
        return shutdown.await;
    }
    tokio::select! {
        result = &mut shutdown => result,
        _ = cancel_rx.wait_force() => {
            crate::log::warn("Force kill during teardown; abandoning the teardown phases");
            let kill = Orchestrator::force_kill_immediate(
                state.clone(),
                workers.clone(),
                resource_manager.clone(),
                None,
                event_sink.clone(),
            );
            let (result, killed) = tokio::join!(&mut shutdown, kill);
            if let Err(e) = killed {
                crate::log::warn(&format!("force_kill_immediate failed: {e}"));
            }
            result
        }
    }
}

/// Emit a crash diagnostic on every channel that needs it:
///   * `event_tx` — the broadcast UIs subscribe to. Sends a
///     `RunCrashed` carrying `procedure_id` + `error_kind` + `error`,
///     followed immediately by a synthetic `RunComplete` with outcome
///     `"ERROR"` so reducers that key off completeness still terminate.
///   * `agent` — the headless JSON protocol's own `RunCrashed` (carries
///     the stderr tail; the `error_kind` taxonomy is UI-only for now).
///   * stderr — human-readable for operators watching the terminal.
///
/// `run_finished` (agent protocol terminator) is emitted by the caller
/// in `run::start()` once the test future resolves; we don't fire it
/// here.
fn emit_crash(
    event_tx: &broadcast::Sender<StationEvent>,
    agent: &Option<AgentProtoCtx>,
    procedure_id: &str,
    execution_id: &str,
    error_kind: &str,
    exit_code: i32,
    message: String,
) {
    crate::log::error(&message);
    super::emit::run_crashed(
        event_tx,
        agent.as_ref(),
        procedure_id,
        execution_id,
        error_kind,
        &message,
        exit_code,
    );
}

fn handle_agent_ui_request(agent: AgentProtoCtx, request: UiRequestData) {
    tokio::spawn(async move {
        let request_id = request.request_id.clone();
        let phase_key = request.phase_key.clone();
        let components = request.config.components.clone();

        // 1. Check pre-baked values. If every required input is provided, auto-respond.
        if let Some(map) = agent.prebaked.for_phase(&phase_key) {
            let all_required_ready = components.iter().all(|c| {
                if !c.is_input || !c.required {
                    return true;
                }
                map.contains_key(&c.key)
            });
            if all_required_ready {
                let values: std::collections::HashMap<String, serde_json::Value> = components
                    .iter()
                    .filter(|c| c.is_input)
                    .filter_map(|c| map.get(&c.key).map(|v| (c.key.clone(), v.clone())))
                    .collect();

                match super::agent_proto::validate::validate_and_coerce(&components, values.clone())
                {
                    Ok(coerced) => {
                        super::ui_response::send(&request_id, coerced).await;
                        agent.emitter.enqueue(CliEvent::UiAutoContinue {
                            request_id: request_id.clone(),
                            phase_key: phase_key.clone(),
                            source: UiAutoContinueSource::PreBaked,
                            values,
                        });
                        return;
                    }
                    Err(err) => {
                        agent.emitter.enqueue(err.into_event(&request_id));
                        // Fall through and treat as a regular request
                    }
                }
            }
        }

        // 2. Display-only UI: auto-continue without waiting.
        if !request.config.requires_user_input() {
            super::ui_response::send_empty(&request_id).await;
            agent.emitter.enqueue(CliEvent::UiAutoContinue {
                request_id,
                phase_key,
                source: UiAutoContinueSource::DisplayOnly,
                values: HashMap::new(),
            });
            return;
        }

        // 3. Register the pending request so the stdin reader can validate responses.
        agent.pending.write().await.insert(
            request_id.clone(),
            phase_key.clone(),
            components.clone(),
        );

        // 4. Emit ui_request so the agent can answer.
        let payload_components: Vec<AgentUiComponent> =
            components.iter().map(to_agent_ui_component).collect();
        agent.emitter.enqueue(CliEvent::UiRequest {
            request_id: request_id.clone(),
            phase_key: phase_key.clone(),
            phase_description: None,
            requires_input: request.config.requires_user_input(),
            components: payload_components,
        });

        // 5. Optional timeout: if the agent doesn't respond in time, drop the
        //    oneshot sender so the engine surfaces a missing-required error,
        //    and emit ui_timeout so the agent can observe the failure.
        if let Some(timeout) = agent.ui_timeout {
            let emitter = agent.emitter.clone();
            let pending = agent.pending.clone();
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                if pending.write().await.remove(&request_id).is_none() {
                    return;
                }
                super::ui_response::cancel(&request_id).await;
                emitter.enqueue(CliEvent::UiTimeout {
                    request_id,
                    phase_key,
                });
            });
        }
    });
}

/// Map the engine's internal `ValidatorOutcome` enum to the station wire
/// vocabulary (`PASS`/`FAIL`/`UNSET`). Debug-formatting the enum gave
/// PascalCase strings that broke string-compare in both clients; keep this
/// helper next to the one call site so drift is obvious.
fn validator_outcome_to_wire(
    o: &execution_engine::procedure::schema::ValidatorOutcome,
) -> tofupilot_sdk::types::Outcome {
    use execution_engine::procedure::schema::ValidatorOutcome;
    use tofupilot_sdk::types::Outcome as SdkOutcome;
    match o {
        ValidatorOutcome::Pass => SdkOutcome::Pass,
        ValidatorOutcome::Fail => SdkOutcome::Fail,
        ValidatorOutcome::Unset => SdkOutcome::Unset,
    }
}

/// Uppercase wire string for the station-protocol `ValidatorResult.outcome`
/// (a plain String field), distinct from the SDK-enum variant above.
fn validator_outcome_wire_str(
    o: &execution_engine::procedure::schema::ValidatorOutcome,
) -> &'static str {
    use execution_engine::procedure::schema::ValidatorOutcome;
    match o {
        ValidatorOutcome::Pass => "PASS",
        ValidatorOutcome::Fail => "FAIL",
        ValidatorOutcome::Unset => "UNSET",
    }
}

/// Translate each validator on a measurement into the wire shape consumed
/// by TUI and web. Expression is either the validator's own `expression`
/// field or synthesized from `operator + expected_value`. `is_decisive` is
/// unknown at the engine layer today (the YAML schema has no corresponding
/// field) so we leave it as `None` — clients treat absent as "decisive".
fn build_validator_results(
    m: &execution_engine::measurements::Measurement,
) -> Vec<ValidatorResult> {
    let Some(validators) = m.validators.as_ref() else {
        return Vec::new();
    };
    validator_specs_to_results(validators)
}

fn validator_specs_to_results(
    validators: &[execution_engine::procedure::schema::ValidatorSpec],
) -> Vec<ValidatorResult> {
    validators
        .iter()
        .map(|v| {
            let expression = v
                .expression
                .clone()
                .unwrap_or_else(|| format_validator_expression(v));
            let outcome = v
                .outcome
                .as_ref()
                .map(validator_outcome_wire_str)
                .unwrap_or("UNSET")
                .to_string();
            ValidatorResult {
                expression,
                outcome,
                is_decisive: None,
            }
        })
        .collect()
}

/// Map the engine's evaluated measurement-level aggregations onto the wire.
/// The engine stamps `outcome` on every aggregation during evaluation;
/// axis-level aggregations of multi-dimensional values are NOT mapped here —
/// they travel inside `measured_value` with the rest of the spec.
fn build_aggregation_results(
    m: &execution_engine::measurements::Measurement,
) -> Vec<AggregationResult> {
    let Some(aggregations) = m.aggregations.as_ref() else {
        return Vec::new();
    };
    aggregations
        .iter()
        .map(|a| AggregationResult {
            aggregation_type: a.aggregation_type.clone(),
            value: a.value.as_ref().and_then(|v| serde_json::to_value(v).ok()),
            unit: a.unit.clone(),
            outcome: a
                .outcome
                .as_ref()
                .map(validator_outcome_wire_str)
                .unwrap_or("UNSET")
                .to_string(),
            validators: a
                .validators
                .as_deref()
                .map(validator_specs_to_results)
                .unwrap_or_default(),
        })
        .collect()
}

/// Render a validator as a short display string. Mirrors web's
/// `formatValidatorSpecToString` just enough for live-view use —
/// full analytics-grade formatting stays on the server side.
fn format_validator_expression(v: &execution_engine::procedure::schema::ValidatorSpec) -> String {
    use execution_engine::procedure::schema::ValidatorExpectedValue;
    let op = v.operator.as_deref().unwrap_or("").trim();
    let rendered = match v.expected_value.as_ref() {
        Some(ValidatorExpectedValue::Number(n)) => format!("{n}"),
        Some(ValidatorExpectedValue::Boolean(b)) => b.to_string(),
        Some(ValidatorExpectedValue::String(s)) => s.clone(),
        Some(ValidatorExpectedValue::NumberArray(a)) => a
            .iter()
            .map(|n| format!("{n}"))
            .collect::<Vec<_>>()
            .join(","),
        Some(ValidatorExpectedValue::StringArray(a)) => a.join(","),
        Some(ValidatorExpectedValue::MixedArray(_))
        | Some(ValidatorExpectedValue::Object(_))
        | Some(ValidatorExpectedValue::Null)
        | None => String::new(),
    };
    // Match web's display format: `x <op> <value>` (e.g. `x >= 3.0`).
    // Operator-only renders as `x <op>` (rare, mostly for "in" / "not in"
    // style validators without a discrete value).
    if op.is_empty() && rendered.is_empty() {
        String::new()
    } else if rendered.is_empty() {
        format!("x {op}")
    } else {
        format!("x {op} {rendered}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_run_data() -> RunData {
        RunData {
            phases: Vec::new(),
            run_outcome: None,
            slot_outcomes: std::collections::HashMap::new(),
            run_id: None,
            start_time: None,
            end_time: None,
            units: [(
                "default".to_string(),
                UnitSnapshot {
                    serial: Some("SN-TEST".to_string()),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            run_metadata_sources: Vec::new(),
            unit_metadata_sources: Vec::new(),
            attachments: Vec::new(),
        }
    }

    fn default_unit(data: &mut RunData) -> &mut UnitSnapshot {
        data.units.entry("default".to_string()).or_default()
    }

    /// `build_run_request` for the synthetic single slot, no stamp.
    fn build_single(data: &RunData, dir: &std::path::Path) -> RunCreateRequest {
        build_run_request(
            data,
            "default",
            None,
            "proc-1",
            dir,
            &dir.join("procedure.yaml"),
            None,
        )
        .unwrap()
    }

    fn md(
        pairs: &[(&str, serde_json::Value)],
    ) -> std::collections::HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn build_run_request_populates_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let mut data = empty_run_data();
        data.run_metadata_sources.push((
            None,
            "phase_a".into(),
            md(&[("modification", serde_json::json!("MOD-42"))]),
        ));
        data.unit_metadata_sources.push((
            None,
            "phase_a".into(),
            md(&[
                ("asset_owner", serde_json::json!("lab-3")),
                ("cycles", serde_json::json!(3)),
                ("calibrated", serde_json::json!(true)),
            ]),
        ));

        let req = build_single(&data, tmp.path());

        let rmd = req.metadata.expect("run metadata set");
        assert_eq!(rmd.get("modification"), Some(&serde_json::json!("MOD-42")));

        let umd = req.unit_metadata.expect("unit metadata set");
        assert_eq!(umd.get("asset_owner"), Some(&serde_json::json!("lab-3")));
        assert_eq!(umd.get("cycles"), Some(&serde_json::json!(3)));
        assert_eq!(umd.get("calibrated"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn operated_by_over_the_ceiling_is_clamped_not_dropped() {
        // A 300-char badge scan reaching build_run_request means the test has
        // already run: rejecting here would lose the whole run, so the value
        // is clamped to the server ceiling instead.
        let tmp = tempfile::tempdir().unwrap();
        let mut data = empty_run_data();
        let max = execution_engine::unit::OPERATED_BY_MAX_CHARS;
        default_unit(&mut data).operated_by = Some("x".repeat(300));

        let req = build_single(&data, tmp.path());

        assert_eq!(req.operated_by.as_deref(), Some("x".repeat(max).as_str()));
    }

    #[test]
    fn operated_by_is_clamped_on_characters_not_bytes() {
        // 200 accented characters are 400 bytes: under the ceiling, so the
        // value must survive whole. A byte-based clamp would cut it in half,
        // and a byte slice would have panicked on the char boundary.
        let tmp = tempfile::tempdir().unwrap();
        let mut data = empty_run_data();
        let name = "é".repeat(200);
        default_unit(&mut data).operated_by = Some(name.clone());

        let req = build_single(&data, tmp.path());

        assert_eq!(req.operated_by.as_deref(), Some(name.as_str()));
    }

    #[test]
    fn retry_replaces_phase_metadata_source() {
        // Attempt 1 writes a diagnostic key and fails; the passing retry
        // writes nothing — the stale key must not leak into the upload.
        let mut sources = Vec::new();
        upsert_metadata_source(
            &mut sources,
            &None,
            "check_voltage",
            md(&[("error_code", serde_json::json!("E42"))]),
        );
        upsert_metadata_source(&mut sources, &None, "check_voltage", md(&[]));
        let merged = fold_metadata_sources(&sources, "default");
        assert!(!merged.contains_key("error_code"));
    }

    #[test]
    fn identify_source_stays_first_and_is_overridable() {
        let mut sources = Vec::new();
        upsert_metadata_source(
            &mut sources,
            &None,
            "identify_unit",
            md(&[("modification", serde_json::json!("MOD-1"))]),
        );
        upsert_metadata_source(
            &mut sources,
            &None,
            "phase_a",
            md(&[("modification", serde_json::json!("MOD-42"))]),
        );
        // Re-identify replaces in place, staying before phase_a
        upsert_metadata_source(
            &mut sources,
            &None,
            "identify_unit",
            md(&[("modification", serde_json::json!("MOD-2"))]),
        );
        let merged = fold_metadata_sources(&sources, "default");
        assert_eq!(
            merged.get("modification"),
            Some(&serde_json::json!("MOD-42"))
        );
    }

    #[test]
    fn build_run_request_omits_empty_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let data = empty_run_data();
        let req = build_single(&data, tmp.path());
        assert!(req.metadata.is_none());
        assert!(req.unit_metadata.is_none());
    }

    #[test]
    fn run_metadata_later_sources_win_per_key() {
        let mut sources = Vec::new();
        upsert_metadata_source(
            &mut sources,
            &None,
            "phase_a",
            md(&[
                ("line", serde_json::json!("L3")),
                ("modification", serde_json::json!("MOD-1")),
            ]),
        );
        upsert_metadata_source(
            &mut sources,
            &None,
            "phase_b",
            md(&[("modification", serde_json::json!("MOD-42"))]),
        );
        let merged = fold_metadata_sources(&sources, "default");
        assert_eq!(
            merged.get("modification"),
            Some(&serde_json::json!("MOD-42"))
        );
        assert_eq!(merged.get("line"), Some(&serde_json::json!("L3")));
    }

    #[test]
    fn cap_metadata_keys_under_limit_unchanged() {
        let map: std::collections::HashMap<String, serde_json::Value> = (0..50)
            .map(|i| (format!("k{i:02}"), serde_json::json!(i)))
            .collect();
        assert_eq!(cap_metadata_keys("run metadata", &map).len(), 50);
    }

    #[test]
    fn cap_metadata_keys_drops_sorted_tail() {
        let map: std::collections::HashMap<String, serde_json::Value> = (0..60)
            .map(|i| (format!("k{i:02}"), serde_json::json!(i)))
            .collect();
        let capped = cap_metadata_keys("run metadata", &map);
        assert_eq!(capped.len(), 50);
        // Sorted order: k00..k49 kept, k50..k59 dropped
        assert!(capped.contains_key("k00"));
        assert!(capped.contains_key("k49"));
        assert!(!capped.contains_key("k50"));
    }

    #[tokio::test]
    async fn apply_unit_info_metadata_lands_in_unit_metadata() {
        let data = Arc::new(Mutex::new(empty_run_data()));
        let info = execution_engine::unit::UnitInfo {
            serial_number: Some("SN-1".into()),
            part_number: None,
            revision_number: None,
            batch_number: None,
            operated_by: None,
            sub_units: None,
            status: "complete".into(),
            metadata: Some(
                [("modification".to_string(), "MOD-7".to_string())]
                    .into_iter()
                    .collect(),
            ),
        };
        apply_unit_info_to_run_data(&data, "default", &info).await;
        let d = data.lock().await;
        let merged = fold_metadata_sources(&d.unit_metadata_sources, "default");
        assert_eq!(
            merged.get("modification"),
            Some(&serde_json::json!("MOD-7"))
        );
    }

    fn write_v1_manifest(dir: &Path, root_directory: Option<&str>) {
        let pd = match root_directory {
            Some(s) => format!("\"{s}\""),
            None => "null".into(),
        };
        let body = format!(
            r#"{{"version":1,"kind":"source","mode":"sync","root_directory":{pd},"runtime_version":"3.12.13","platform":null}}"#,
        );
        std::fs::write(dir.join("manifest.json"), body).unwrap();
    }

    #[test]
    fn deployment_layout_falls_back_to_deployment_when_manifest_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = deployment_layout(tmp.path()).unwrap();
        assert_eq!(layout.package_dir, tmp.path());
        assert_eq!(layout.entry_point, None);
        assert!(!layout.manifest_present);
    }

    #[test]
    fn deployment_layout_errors_on_unparseable_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("manifest.json"), "{not json").unwrap();
        let err = deployment_layout(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("unparseable"), "got: {err}");
    }

    #[test]
    fn deployment_layout_errors_on_unsafe_manifest_value() {
        let tmp = tempfile::tempdir().unwrap();
        write_v1_manifest(tmp.path(), Some("../etc"));
        let err = deployment_layout(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("root_directory"), "got: {err}");
    }

    #[test]
    fn deployment_layout_returns_deployment_when_manifest_field_null() {
        let tmp = tempfile::tempdir().unwrap();
        write_v1_manifest(tmp.path(), None);
        let layout = deployment_layout(tmp.path()).unwrap();
        assert_eq!(layout.package_dir, tmp.path());
        assert!(layout.manifest_present);
    }

    #[test]
    fn deployment_layout_joins_safe_value() {
        let tmp = tempfile::tempdir().unwrap();
        write_v1_manifest(tmp.path(), Some("procedures/foo"));
        let layout = deployment_layout(tmp.path()).unwrap();
        assert_eq!(layout.package_dir, tmp.path().join("procedures/foo"));
        assert!(layout.manifest_present);
    }

    #[test]
    fn cap_string_preserves_utf8_boundary() {
        // "café" = 5 bytes (c,a,f,0xC3,0xA9). Cap at 4 would land mid-é.
        let (capped, truncated) = cap_string("café", 4);
        assert!(truncated);
        assert_eq!(capped, "caf");
        assert!(capped.is_char_boundary(capped.len()));
    }

    #[test]
    fn cap_string_handles_multi_byte_precisely_at_limit() {
        // "日本語" = 3 chars × 3 bytes = 9 bytes. Cap at 6 → "日本".
        let (capped, truncated) = cap_string("日本語", 6);
        assert!(truncated);
        assert_eq!(capped, "日本");
    }

    #[test]
    fn cap_string_no_op_under_limit() {
        let (capped, truncated) = cap_string("hello", 10);
        assert!(!truncated);
        assert_eq!(capped, "hello");
    }

    #[test]
    fn truncate_for_log_ascii() {
        let long = "x".repeat(200);
        let out = truncate_for_log(&long);
        assert!(out.ends_with('…'));
        // 128 'x' bytes + 3-byte ellipsis.
        assert_eq!(out.len(), 128 + 3);
    }

    #[test]
    fn truncate_for_log_under_cap_unchanged() {
        assert_eq!(truncate_for_log("short"), "short");
    }

    #[test]
    fn cap_measurement_value_below_limit_passes_through() {
        let v = serde_json::json!({"a": 1, "b": "hello"});
        let (out, truncated) = cap_measurement_value(&v);
        assert!(!truncated);
        assert_eq!(out, v);
    }

    #[test]
    fn cap_measurement_value_at_exact_limit_passes() {
        // Value whose serialized size is exactly MAX_MEASUREMENT_VALUE_BYTES
        // should pass through unchanged. Serialized form of a JSON string
        // is len + 2 (the quote chars), so aim for MAX - 2.
        let filler = "x".repeat(MAX_MEASUREMENT_VALUE_BYTES - 2);
        let v = serde_json::Value::String(filler);
        assert_eq!(
            serde_json::to_vec(&v).unwrap().len(),
            MAX_MEASUREMENT_VALUE_BYTES
        );
        let (_, truncated) = cap_measurement_value(&v);
        assert!(!truncated, "value at exact cap must not trigger truncation");
    }

    #[test]
    fn cap_measurement_value_one_over_limit_truncates() {
        let filler = "x".repeat(MAX_MEASUREMENT_VALUE_BYTES - 1);
        let v = serde_json::Value::String(filler);
        assert_eq!(
            serde_json::to_vec(&v).unwrap().len(),
            MAX_MEASUREMENT_VALUE_BYTES + 1
        );
        let (out, truncated) = cap_measurement_value(&v);
        assert!(truncated);
        assert_eq!(out["truncated"], true);
        assert_eq!(out["original_size_bytes"], MAX_MEASUREMENT_VALUE_BYTES + 1);
    }

    #[test]
    fn cap_warning_detail_small_payload_passes_through() {
        let d = serde_json::json!({"phase_key": "p", "truncated": true});
        let out = cap_warning_detail(d.clone());
        assert_eq!(out, d);
    }

    #[test]
    fn cap_warning_detail_over_limit_collapses_to_marker() {
        // Build a warning payload that exceeds MAX_WARNING_DETAIL_BYTES.
        let filler = "x".repeat(MAX_WARNING_DETAIL_BYTES);
        let d = serde_json::json!({"phase_key": filler, "truncated": true});
        let out = cap_warning_detail(d);
        assert_eq!(out["truncated"], true);
        assert!(out["original_size_bytes"].as_u64().unwrap() > MAX_WARNING_DETAIL_BYTES as u64);
        assert!(out.get("reason").is_some());
    }

    // -----------------------------------------------------------------
    // build_measurement — the run-upload wire mapping
    // -----------------------------------------------------------------

    use execution_engine::measurements::{Measurement, MeasurementValue};
    use execution_engine::procedure::schema::{
        AggregationSpec, AggregationValue, AxisData, AxisSpec, MultiDimensionalSpec,
        ValidatorExpectedValue, ValidatorOutcome, ValidatorSpec,
    };

    fn validator(op: &str, expected: f64, outcome: ValidatorOutcome) -> ValidatorSpec {
        ValidatorSpec {
            outcome: Some(outcome),
            operator: Some(op.to_string()),
            expected_value: Some(ValidatorExpectedValue::Number(expected)),
            expression: None,
        }
    }

    fn aggregation(
        kind: &str,
        value: f64,
        unit: &str,
        outcome: ValidatorOutcome,
        validators: Vec<ValidatorSpec>,
    ) -> AggregationSpec {
        AggregationSpec {
            aggregation_type: kind.to_string(),
            outcome: Some(outcome),
            value: Some(AggregationValue::Number(value)),
            unit: Some(unit.to_string()),
            validators: Some(validators),
        }
    }

    fn measurement(name: &str, value: MeasurementValue) -> Measurement {
        Measurement {
            name: name.to_string(),
            value,
            unit: None,
            timestamp: "2026-08-07T18:07:31Z".to_string(),
            validators: None,
            aggregations: None,
            description: None,
            outcome: ValidatorOutcome::Unset,
        }
    }

    /// Assert against the serialized form: the JSON body is the actual
    /// contract with the V2 API, and it sidesteps `NullableField` matching.
    fn wire(m: &Measurement) -> serde_json::Value {
        serde_json::to_value(build_measurement(m).expect("build_measurement")).unwrap()
    }

    #[test]
    fn build_measurement_carries_axis_legend_validators_and_aggregations() {
        let mut m = measurement(
            "power_sweep",
            MeasurementValue::MultiDimensional(MultiDimensionalSpec {
                title: Some("Voltage and current vs step".to_string()),
                x_axis: AxisSpec {
                    data: Some(AxisData::Numeric(vec![0.0, 1.0])),
                    unit: None,
                    legend: Some("Step".to_string()),
                    key: Some("step".to_string()),
                    aggregations: None,
                    validators: None,
                    description: None,
                },
                y_axis: vec![AxisSpec {
                    data: Some(AxisData::Numeric(vec![3.28, 3.32])),
                    unit: Some("V".to_string()),
                    legend: Some("Rail voltage".to_string()),
                    key: Some("voltage".to_string()),
                    aggregations: Some(vec![aggregation(
                        "max",
                        3.32,
                        "V",
                        ValidatorOutcome::Fail,
                        vec![validator("<=", 3.3, ValidatorOutcome::Fail)],
                    )]),
                    validators: Some(vec![validator("<=", 3.5, ValidatorOutcome::Pass)]),
                    description: None,
                }],
            }),
        );
        m.outcome = ValidatorOutcome::Fail;

        let w = wire(&m);

        assert_eq!(w["x_axis"]["name"], "Step");
        assert_eq!(w["x_axis"]["data"], serde_json::json!([0.0, 1.0]));

        let y = &w["y_axis"][0];
        assert_eq!(y["name"], "Rail voltage");
        assert_eq!(y["units"], "V");
        assert_eq!(y["validators"][0]["operator"], "<=");
        assert_eq!(y["validators"][0]["expected_value"], 3.5);
        assert_eq!(y["validators"][0]["outcome"], "PASS");

        let agg = &y["aggregations"][0];
        assert_eq!(agg["type"], "max");
        assert_eq!(agg["value"], 3.32);
        assert_eq!(agg["unit"], "V");
        assert_eq!(agg["outcome"], "FAIL");
        assert_eq!(agg["validators"][0]["expected_value"], 3.3);
        assert_eq!(agg["validators"][0]["outcome"], "FAIL");
    }

    #[test]
    fn build_measurement_carries_aggregation_validators_on_scalars() {
        let mut m = measurement("supply_ripple", MeasurementValue::Numeric(17.9));
        m.unit = Some("mV".to_string());
        m.aggregations = Some(vec![
            aggregation(
                "avg",
                20.28,
                "mV",
                ValidatorOutcome::Pass,
                vec![validator("<=", 25.0, ValidatorOutcome::Pass)],
            ),
            // No validators: must still upload, with an empty/absent list
            // rather than being dropped.
            AggregationSpec {
                aggregation_type: "count".to_string(),
                outcome: Some(ValidatorOutcome::Unset),
                value: Some(AggregationValue::Number(5.0)),
                unit: None,
                validators: None,
            },
        ]);

        let w = wire(&m);
        let aggs = w["aggregations"].as_array().expect("aggregations");
        assert_eq!(aggs.len(), 2);
        assert_eq!(aggs[0]["type"], "avg");
        assert_eq!(aggs[0]["validators"][0]["expected_value"], 25.0);
        assert_eq!(aggs[0]["validators"][0]["outcome"], "PASS");
        assert_eq!(aggs[1]["type"], "count");
        assert_eq!(aggs[1]["value"], 5.0);
        assert!(aggs[1].get("validators").is_none());
    }

    fn completed_phase(name: &str, slot_id: Option<&str>, outcome: Outcome) -> CompletedPhase {
        CompletedPhase {
            name: name.to_string(),
            outcome,
            started_at: "2026-01-01T00:00:00Z".into(),
            completed_at: "2026-01-01T00:00:01Z".into(),
            retry_count: 0,
            measurements: Vec::new(),
            logs: Vec::new(),
            error: None,
            slot_id: slot_id.map(String::from),
        }
    }

    fn two_slot_data() -> RunData {
        let mut data = empty_run_data();
        data.units = [
            (
                "s1".to_string(),
                UnitSnapshot {
                    serial: Some("SN-A".into()),
                    ..Default::default()
                },
            ),
            (
                "s2".to_string(),
                UnitSnapshot {
                    serial: Some("SN-B".into()),
                    ..Default::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        data.phases
            .push(completed_phase("setup", None, Outcome::Pass));
        let mut late = completed_phase("measure", Some("s1"), Outcome::Pass);
        late.started_at = "2026-01-01T00:00:05Z".into();
        late.completed_at = "2026-01-01T00:00:09Z".into();
        data.phases.push(late);
        data.phases
            .push(completed_phase("measure", Some("s2"), Outcome::Fail));
        data.slot_outcomes.insert("s1".into(), Outcome::Pass);
        data.slot_outcomes.insert("s2".into(), Outcome::Fail);
        data.run_outcome = Some(Outcome::Fail);
        data.start_time = Some("2026-01-01T00:00:00Z".parse().unwrap());
        data.end_time = Some("2026-01-01T00:05:00Z".parse().unwrap());
        data
    }

    /// One request per slot: its own unit, its own phases plus the shared
    /// stage, the engine's per-slot outcome (never the run rollup), a
    /// window spanning its own phases, and the grouping stamp.
    #[test]
    fn multi_slot_requests_split_phases_units_and_outcomes() {
        let tmp = tempfile::tempdir().unwrap();
        let data = two_slot_data();
        let req1 = build_run_request(
            &data,
            "s1",
            Some(("exec-1", "s1", Some("Left Nest"))),
            "proc-1",
            tmp.path(),
            &tmp.path().join("procedure.yaml"),
            None,
        )
        .unwrap();
        let req2 = build_run_request(
            &data,
            "s2",
            Some(("exec-1", "s2", None)),
            "proc-1",
            tmp.path(),
            &tmp.path().join("procedure.yaml"),
            None,
        )
        .unwrap();

        assert_eq!(req1.serial_number, "SN-A");
        assert_eq!(req2.serial_number, "SN-B");
        assert_eq!(req1.phases.as_ref().map(|p| p.len()), Some(2));
        assert_eq!(req2.phases.as_ref().map(|p| p.len()), Some(2));
        assert!(matches!(req1.outcome, RunGetOutcome::Pass));
        assert!(matches!(req2.outcome, RunGetOutcome::Fail));
        // s1's window: its own measure :05–:09, not the shared setup at :00
        // nor the five-minute execution; s2's own phase ran :00–:01.
        assert_eq!(req1.started_at.to_rfc3339(), "2026-01-01T00:00:05+00:00");
        assert_eq!(req1.ended_at.to_rfc3339(), "2026-01-01T00:00:09+00:00");
        assert_eq!(req2.started_at.to_rfc3339(), "2026-01-01T00:00:00+00:00");
        assert_eq!(req2.ended_at.to_rfc3339(), "2026-01-01T00:00:01+00:00");
        let j1 = serde_json::to_value(&req1).unwrap();
        assert_eq!(j1["execution_id"], "exec-1");
        assert_eq!(j1["slot_key"], "s1");
        assert_eq!(j1["slot_name"], "Left Nest");
        let j2 = serde_json::to_value(&req2).unwrap();
        assert_eq!(j2["slot_key"], "s2");
        assert!(j2.get("slot_name").is_none_or(|v| v.is_null()));
    }

    /// The shared teardown that waits for the last slot must not stretch
    /// an early slot's run to it; a slot that ran nothing of its own
    /// (cancelled before its first phase) falls back to the shared window.
    #[test]
    fn slot_window_ignores_shared_teardown_and_falls_back_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut data = two_slot_data();
        let mut teardown = completed_phase("power_off", None, Outcome::Pass);
        teardown.started_at = "2026-01-01T00:04:00Z".into();
        teardown.completed_at = "2026-01-01T00:04:30Z".into();
        data.phases.push(teardown);
        data.units.insert("s3".into(), UnitSnapshot::default());
        let build = |slot: &str| {
            build_run_request(
                &data,
                slot,
                Some(("exec-1", slot, None)),
                "proc-1",
                tmp.path(),
                &tmp.path().join("procedure.yaml"),
                None,
            )
            .unwrap()
        };
        assert_eq!(
            build("s1").ended_at.to_rfc3339(),
            "2026-01-01T00:00:09+00:00"
        );
        assert_eq!(
            build("s1").phases.as_ref().map(|p| p.len()),
            Some(3),
            "shared stages still listed"
        );
        let empty = build("s3");
        assert_eq!(empty.started_at.to_rfc3339(), "2026-01-01T00:00:00+00:00");
        assert_eq!(empty.ended_at.to_rfc3339(), "2026-01-01T00:04:30+00:00");
    }

    /// The single-slot wire shape is untouched: no grouping fields at all.
    #[test]
    fn single_slot_request_carries_no_grouping_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let req = build_single(&empty_run_data(), tmp.path());
        let j = serde_json::to_value(&req).unwrap();
        for k in ["execution_id", "slot_key", "slot_name"] {
            assert!(j.get(k).is_none_or(|v| v.is_null()), "{k} must be absent");
        }
    }

    /// A slot the engine reported no outcome for (crash mid-run) falls
    /// back to the run outcome, never to a sibling's.
    #[test]
    fn slot_without_engine_outcome_falls_back_to_run_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let mut data = two_slot_data();
        data.slot_outcomes.clear();
        data.run_outcome = Some(Outcome::Error);
        let req = build_run_request(
            &data,
            "s1",
            Some(("exec-1", "s1", None)),
            "proc-1",
            tmp.path(),
            &tmp.path().join("procedure.yaml"),
            None,
        )
        .unwrap();
        assert!(matches!(req.outcome, RunGetOutcome::Error));
    }

    #[test]
    fn slot_metadata_does_not_leak_across_slots() {
        let mut sources = Vec::new();
        upsert_metadata_source(
            &mut sources,
            &Some("s1".to_string()),
            "phase_a",
            md(&[("temp", serde_json::json!(21))]),
        );
        upsert_metadata_source(
            &mut sources,
            &None,
            "setup",
            md(&[("fixture", serde_json::json!("F-9"))]),
        );
        let s1 = fold_metadata_sources(&sources, "s1");
        let s2 = fold_metadata_sources(&sources, "s2");
        assert_eq!(s1.get("temp"), Some(&serde_json::json!(21)));
        assert_eq!(s1.get("fixture"), Some(&serde_json::json!("F-9")));
        assert!(!s2.contains_key("temp"));
        assert_eq!(s2.get("fixture"), Some(&serde_json::json!("F-9")));
    }

    /// A shared attachment reaches every slot's run on its own file; a
    /// slot's attachment stays with its slot.
    #[test]
    fn shared_attachments_fan_out_with_their_own_files() {
        let tmp = tempfile::tempdir().unwrap();
        let shared_path = tmp.path().join("rig.png");
        std::fs::write(&shared_path, b"png").unwrap();
        let own_path = tmp.path().join("s2-scope.png");
        std::fs::write(&own_path, b"png").unwrap();
        let att =
            |name: &str, path: &std::path::Path| crate::commands::run::queue::QueuedAttachment {
                name: name.into(),
                path: path.to_string_lossy().to_string(),
                mimetype: "image/png".into(),
                phase_key: "p".into(),
            };
        let all = vec![
            (None, att("rig", &shared_path)),
            (Some("s2".to_string()), att("scope", &own_path)),
        ];

        let s1 = attachments_for_slot(&all, "s1", 0);
        let s2 = attachments_for_slot(&all, "s2", 1);

        assert_eq!(s1.len(), 1);
        assert_eq!(s1[0].path, shared_path.to_string_lossy());
        assert_eq!(s2.len(), 2);
        let s2_shared = s2.iter().find(|a| a.name == "rig").unwrap();
        assert_ne!(
            s2_shared.path,
            shared_path.to_string_lossy(),
            "second slot gets its own copy"
        );
        assert!(std::path::Path::new(&s2_shared.path).exists());
        assert!(s2.iter().any(|a| a.name == "scope"));
    }

    /// A unit update from a shared stage (no slot) lands on every slot;
    /// a slot's own update on that slot only.
    #[tokio::test]
    async fn unit_updates_are_scoped_by_slot() {
        let data = Arc::new(Mutex::new(two_slot_data()));
        let info = execution_engine::unit::UnitInfo {
            serial_number: None,
            part_number: None,
            revision_number: Some("B".into()),
            batch_number: None,
            operated_by: None,
            sub_units: None,
            status: "complete".into(),
            metadata: None,
        };
        apply_unit_info_to_run_data(&data, "s2", &info).await;
        let d = data.lock().await;
        assert_eq!(d.units["s1"].revision, None);
        assert_eq!(d.units["s2"].revision.as_deref(), Some("B"));
        assert_eq!(
            d.units["s2"].serial.as_deref(),
            Some("SN-B"),
            "other fields untouched"
        );
    }
}
