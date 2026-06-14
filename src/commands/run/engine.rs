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
use station_protocol::{PhaseLogLine, PhasePlan, RunMeasurement, StationEvent, ValidatorResult};
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

/// Locate the YAML procedure file inside a root directory. Returns
/// `Some(path)` if `procedure.yaml` (or `.yml`) is present, `None`
/// otherwise. Caller has already resolved `package_dir` — this is just an
/// on-disk file lookup.
pub fn find_procedure_yaml(package_dir: &Path) -> Option<std::path::PathBuf> {
    for name in ["procedure.yaml", "procedure.yml"] {
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
}

/// Collected run-level data from execution events.
struct RunData {
    phases: Vec<CompletedPhase>,
    run_outcome: Option<Outcome>,
    run_id: Option<String>,
    start_time: Option<chrono::DateTime<chrono::Utc>>,
    end_time: Option<chrono::DateTime<chrono::Utc>>,
    unit_serial: Option<String>,
    unit_part: Option<String>,
    unit_revision: Option<String>,
    unit_batch: Option<String>,
    unit_sub_units: Option<Vec<String>>,
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
    /// `UiResponse` cycle to capture it from). Single-slot today;
    /// the last write wins (same as `RunData.unit_serial`).
    resolved_unit: Arc<std::sync::Mutex<Option<station_protocol::UnitInfo>>>,
    /// Deployment this run came from (local `PullState` lookup), None
    /// for ad-hoc local-path runs. Stamped on `RunStarted` so remote
    /// UIs can resolve relative component image paths against the
    /// deployment's stored files.
    deployment_id: Option<String>,
}

impl CliEventSink {
    fn new(
        tx: broadcast::Sender<StationEvent>,
        ui_tx: Option<mpsc::Sender<UiRequestData>>,
        agent: Option<AgentProtoCtx>,
        procedure_name: String,
        procedure_id: String,
        execution_id: String,
        deployment_id: Option<String>,
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
            data: Arc::new(Mutex::new(RunData {
                phases: Vec::new(),
                run_outcome: None,
                run_id: None,
                start_time: None,
                end_time: None,
                unit_serial: None,
                unit_part: None,
                unit_revision: None,
                unit_batch: None,
                unit_sub_units: None,
            })),
            resolved_unit: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl EventSink for CliEventSink {
    fn emit(&self, event: &ExecutionEvent) {
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
                for p in plugs_all {
                    plug_defs.push(station_protocol::PlugDefinition {
                        key: p.plug_key.clone(),
                        name: p.plug_name.clone(),
                        scope: "all".to_string(),
                    });
                }
                for p in plugs_each {
                    plug_defs.push(station_protocol::PlugDefinition {
                        key: p.plug_key.clone(),
                        name: p.plug_name.clone(),
                        scope: "each".to_string(),
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
                    plugs: plug_defs,
                    timestamp: Some(chrono::Utc::now().to_rfc3339()),
                    run_id: None,
                    deployment_id: self.deployment_id.clone(),
                    unit,
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
                };
                let data = self.data.clone();
                tokio::spawn(async move {
                    data.lock().await.phases.push(phase);
                });
            }

            ExecutionEvent::Stats { start_time, .. } => {
                if let Some(t) = start_time {
                    let data = self.data.clone();
                    let t = *t;
                    tokio::spawn(async move {
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
                ..
            } => {
                let outcome_str = run_outcome
                    .as_ref()
                    .map(super::outcomes::from_execution_outcome)
                    .unwrap_or("UNKNOWN");
                super::emit::run_complete(
                    &self.tx,
                    outcome_str,
                    &self.execution_id,
                    run_id.clone(),
                );

                let data = self.data.clone();
                let ro = *run_outcome;
                let ri = run_id.clone();
                let et = *end_time;
                tokio::spawn(async move {
                    let mut d = data.lock().await;
                    d.run_outcome = ro;
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
            }

            ExecutionEvent::UnitIdentified { slot_id, unit_info } => {
                // Cache the resolved unit so the `Plan` arm can stamp
                // it on `StationEvent::RunStarted.unit`. Without this
                // operator-UI never sees the unit on `auto_identify`
                // runs (no `UiRequest`/`UiResponse` to capture from).
                //
                // RunData itself is populated synchronously upstream
                // by `run_yaml_procedure` before `submit_procedure`
                // runs — this arm is purely the wire-event surface
                // for downstream UIs.
                let wire_unit = unit_info_to_wire(unit_info);
                if let Ok(mut guard) = self.resolved_unit.lock() {
                    *guard = Some(wire_unit.clone());
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
        PlugScope::All => "all",
        PlugScope::Each => "each",
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

    // Validators
    if let Some(ref vs) = m.validators {
        let validators: Vec<RunCreateMeasurementsValidators> = vs
            .iter()
            .filter_map(|v| {
                let mut vb = RunCreateMeasurementsValidators::builder();
                if let Some(ref op) = v.operator {
                    vb = vb.operator(op);
                }
                if let Some(ref exp) = v.expected_value {
                    let json_val = match exp {
                        execution_engine::procedure::schema::ValidatorExpectedValue::Number(n) => serde_json::json!(n),
                        execution_engine::procedure::schema::ValidatorExpectedValue::String(s) => serde_json::json!(s),
                        execution_engine::procedure::schema::ValidatorExpectedValue::Boolean(b) => serde_json::json!(b),
                        execution_engine::procedure::schema::ValidatorExpectedValue::Null => serde_json::Value::Null,
                        execution_engine::procedure::schema::ValidatorExpectedValue::NumberArray(a) => serde_json::json!(a),
                        execution_engine::procedure::schema::ValidatorExpectedValue::StringArray(a) => serde_json::json!(a),
                        execution_engine::procedure::schema::ValidatorExpectedValue::MixedArray(a) => serde_json::json!(a),
                        execution_engine::procedure::schema::ValidatorExpectedValue::Object(o) => serde_json::Value::Object(o.clone()),
                    };
                    vb = vb.expected_value(json_val);
                }
                if let Some(ref expr) = v.expression {
                    vb = vb.expression(expr);
                }
                if let Some(ref o) = v.outcome {
                    // Wire contract is uppercase (`PASS`/`FAIL`/`UNSET`); the
                    // previous `format!("{:?}", o).to_lowercase()` produced
                    // `"pass"` etc. which the V2 Zod schema rejects.
                    vb = vb.outcome(validator_outcome_to_wire(o));
                }
                vb.build().ok()
            })
            .collect();
        if !validators.is_empty() {
            b = b.validators(validators);
        }
    }

    // Aggregations
    if let Some(ref aggs) = m.aggregations {
        let aggregations: Vec<RunCreateMeasurementsAggregations> = aggs
            .iter()
            .filter_map(|a| {
                let mut ab =
                    RunCreateMeasurementsAggregations::builder().r#type(&a.aggregation_type);
                if let Some(ref v) = a.value {
                    let json_val = match v {
                        execution_engine::procedure::schema::AggregationValue::Number(n) => {
                            serde_json::json!(n)
                        }
                        execution_engine::procedure::schema::AggregationValue::String(s) => {
                            serde_json::json!(s)
                        }
                        execution_engine::procedure::schema::AggregationValue::Boolean(b) => {
                            serde_json::json!(b)
                        }
                        execution_engine::procedure::schema::AggregationValue::Object(o) => {
                            serde_json::Value::Object(o.clone())
                        }
                    };
                    ab = ab.value(json_val);
                }
                if let Some(ref u) = a.unit {
                    ab = ab.unit(u);
                }
                if let Some(ref o) = a.outcome {
                    // Same uppercase wire contract as validator outcome.
                    ab = ab.outcome(validator_outcome_to_wire(o));
                }
                ab.build().ok()
            })
            .collect();
        if !aggregations.is_empty() {
            b = b.aggregations(aggregations);
        }
    }

    if let Some(ref d) = m.description {
        b = b.docstring(d);
    }

    b.build().map_err(|e| e.to_string().into())
}

fn build_run_request(
    data: &RunData,
    procedure_id: &str,
    procedure_dir: &Path,
    operated_by: Option<&str>,
) -> crate::error::CliResult<RunCreateRequest> {
    let outcome = data
        .run_outcome
        .as_ref()
        .map(engine_outcome_to_sdk)
        .unwrap_or(RunGetOutcome::Error);

    let started_at = data.start_time.unwrap_or_else(chrono::Utc::now);
    let ended_at = data.end_time.unwrap_or_else(chrono::Utc::now);

    let phases: Vec<RunCreatePhases> = data
        .phases
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

    // Collect logs from all phases into run-level logs
    let logs: Vec<RunCreateLogs> = data
        .phases
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

    let serial = data
        .unit_serial
        .clone()
        .unwrap_or_else(|| "UNKNOWN".to_string());

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

    if let Some(ref pn) = data.unit_part {
        b = b.part_number(pn);
    }
    if let Some(ref rn) = data.unit_revision {
        b = b.revision_number(rn);
    }
    if let Some(ref bn) = data.unit_batch {
        b = b.batch_number(bn);
    }
    if let Some(ref su) = data.unit_sub_units {
        b = b.sub_units(su.clone());
    }

    if let Some(version) = super::procedure_version::read_procedure_version(procedure_dir) {
        b = b.procedure_version(version);
    }

    if let Some(deployment_id) = super::deployment_id::lookup_deployment_id(procedure_id) {
        b = b.deployment_id(deployment_id);
    }

    if let Some(email) = operated_by {
        b = b.operated_by(email);
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
        sub_units,
        status: String::new(),
    }
}

/// Write a resolved `UnitInfo` into the shared `RunData` mutex so
/// `build_run_request` reads the operator-supplied serial / part
/// instead of the "UNKNOWN" fallback used when identify is skipped.
///
/// The synchronous CLI path calls this once per slot from
/// `run_yaml_procedure`; doing the write here (with
/// `lock().await`) instead of inside `EventSink::emit` (sync) avoids
/// the spawn-and-pray race where a fast-failing run could race
/// `build_run_request` ahead of a deferred write.
///
/// Sub-units are flattened to a `Vec<String>` of serials sorted by
/// key — the SDK's `RunCreateRequest.sub_units` is `Option<Vec<String>>`,
/// so keys aren't transmitted; sorting keeps the on-wire order stable
/// across runs / hosts / hashmap implementations.
async fn apply_unit_info_to_run_data(
    run_data: &Arc<Mutex<RunData>>,
    info: &execution_engine::unit::UnitInfo,
) {
    let mut d = run_data.lock().await;
    if let Some(ref sn) = info.serial_number {
        d.unit_serial = Some(sn.clone());
    }
    if let Some(ref pn) = info.part_number {
        d.unit_part = Some(pn.clone());
    }
    if let Some(ref rn) = info.revision_number {
        d.unit_revision = Some(rn.clone());
    }
    if let Some(ref bn) = info.batch_number {
        d.unit_batch = Some(bn.clone());
    }
    if let Some(ref sub) = info.sub_units {
        let mut entries: Vec<(&String, &String)> = sub.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        d.unit_sub_units = Some(entries.into_iter().map(|(_, v)| v.clone()).collect());
    }
}

/// Run a YAML procedure and return a QueuedRun for upload.
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
) -> (i32, Option<QueuedRun>) {
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
            return (1, None);
        }
    };

    let worker_count = procedure_def
        .execution
        .as_ref()
        .map(|e| e.workers)
        .unwrap_or(4);

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

    // Multi-slot YAML upload isn't wired yet — RunData is single-slot
    // and `build_run_create_request` emits one upload per run. Reject
    // upfront when the caller would lose per-slot identity rather than
    // collapsing into "UNKNOWN".
    if slots.len() > 1 && procedure_def.unit.is_some() {
        emit_crash(
            &event_tx,
            &agent,
            procedure_id,
            execution_id,
            "multi_slot_unsupported",
            1,
            "Multi-slot YAML procedures are not yet supported by the CLI \
             upload path. Run with a single slot."
                .to_string(),
        );
        return (1, None);
    }

    // Snapshot the unit config before `procedure_def` moves into the
    // orchestrator. `identify(...)` only needs the unit block.
    //
    // YAML procedures without a `unit:` block historically skipped the
    // identify step entirely, ran phases, and queued an upload with
    // empty serial/part — which the API rejects (both fields min(1)).
    // Fall back to a default config that prompts for the two API-
    // required fields so simple "hello world" templates that omit
    // `unit:` still upload correctly.
    let unit_cfg = procedure_def.unit.clone().or_else(|| {
        Some(execution_engine::procedure::UnitConfig {
            serial_number: Some(execution_engine::procedure::UnitFieldConfig::default()),
            part_number: Some(execution_engine::procedure::UnitFieldConfig::default()),
            ..Default::default()
        })
    });

    let mut orchestrator = Orchestrator::new_with_python(
        worker_count,
        procedure_dir.to_path_buf(),
        Some(python_path.to_path_buf()),
        orchestrator_execution_id,
        run_id,
        procedure_def,
    );

    let sink = CliEventSink::new(
        event_tx.clone(),
        ui_tx.clone(),
        agent.clone(),
        procedure_name.to_string(),
        procedure_id.to_string(),
        execution_id.to_string(),
        super::deployment_id::lookup_deployment_id(procedure_id),
    );
    let run_data = sink.data.clone();
    let event_sink: Arc<dyn EventSink> = Arc::new(sink);
    orchestrator.set_event_sink(event_sink.clone());

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
        return (1, None);
    }

    // Identify-unit step: canonical framework entry point. Procedures
    // without a `unit:` block skip identify entirely.
    // `auto_identify: true` resolves from `default_value`s without an
    // operator prompt; otherwise the host emits a `UiRequest` and
    // awaits the response. Resolved info is written directly into
    // RunData (synchronous, before `submit_procedure`) so the upload
    // path always sees the real serial/part instead of "UNKNOWN" — even
    // for runs that abort before any phase runs. The `UnitIdentified`
    // event is also emitted on the sink for downstream observers
    // (TUI / agent / dashboard) that prefer a structured signal over
    // peeking at RunData.
    let unit_infos: std::collections::HashMap<String, execution_engine::unit::UnitInfo> =
        match (unit_cfg.as_ref(), reuse_unit) {
            // "Run again" path: operator UI supplied the unit from the
            // previous run. Skip the identify-unit prompt entirely,
            // populate every slot with the same unit, and emit
            // `UnitIdentified` (which fans out to `identify_resolved`
            // on the wire) so consumers see the same signal as a
            // normal identify resolution.
            //
            // Gated on `unit_cfg.is_some()`: a procedure that didn't
            // declare a `unit:` block has no schema for what a unit
            // looks like, so honoring `reuse_unit` here would let the
            // wire shape leak in unchecked. The reuse is silently
            // ignored in that case (next cycle starts with no unit,
            // matching the procedure's declaration).
            (Some(cfg), Some(reused)) => {
                // Validate against the procedure's `unit_cfg` so a
                // stale or hand-crafted reuse can't bypass regex /
                // required-field constraints the procedure declared.
                // `validate_unit_info` is the same check the normal
                // identify path runs after parsing the operator
                // response.
                let info = wire_unit_to_engine(reused);
                if let Err(err) =
                    execution_engine::unit::validate_unit_info(&info, &Some(cfg.clone()))
                {
                    emit_crash(
                        &event_tx,
                        &agent,
                        procedure_id,
                        execution_id,
                        "identify_unit_failed",
                        1,
                        format!("reuse_unit failed validation: {err}"),
                    );
                    let _ = orchestrator.shutdown().await;
                    return (1, None);
                }
                let mut infos = std::collections::HashMap::new();
                for slot_id in &slots {
                    apply_unit_info_to_run_data(&run_data, &info).await;
                    event_sink.emit(&ExecutionEvent::UnitIdentified {
                        slot_id: Some(slot_id.clone()),
                        unit_info: info.clone(),
                    });
                    infos.insert(slot_id.clone(), info.clone());
                }
                infos
            }
            (None, Some(_)) => {
                // Procedure declared no `unit:` block; reuse_unit has
                // nowhere to land. Silently drop and run with empty
                // unit info — same as a normal first run on this
                // procedure.
                std::collections::HashMap::new()
            }
            (Some(cfg), None) => {
                let host = identify_host::CliIdentifyHost {
                    router: EventRouter::new(
                        event_tx.clone(),
                        agent.clone(),
                        execution_id.to_string(),
                    ),
                    ui_tx: ui_tx.clone(),
                    agent: agent.clone(),
                    procedure_id: procedure_id.to_string(),
                    has_ui,
                };
                let mut infos = std::collections::HashMap::new();
                let mut identify_cancel = cancel_rx.clone();
                for slot_id in &slots {
                    // Race the operator prompt against cancellation: a
                    // Stop while parked on identify-unit must not hang
                    // the run task. `execution_engine::identify` parks
                    // on a oneshot inside the IdentifyHost; without
                    // this select, neither it nor the orchestrator
                    // cancel loop (which only runs after identify
                    // resolves) ever sees the signal, and the operator
                    // is stuck on a prompt with no way out.
                    let identify_fut = execution_engine::identify(cfg, Some(slot_id), &host);
                    tokio::pin!(identify_fut);
                    let result = tokio::select! {
                        r = &mut identify_fut => Some(r),
                        _ = identify_cancel.wait_any() => None,
                    };
                    match result {
                        Some(Ok(info)) => {
                            apply_unit_info_to_run_data(&run_data, &info).await;
                            event_sink.emit(&ExecutionEvent::UnitIdentified {
                                slot_id: Some(slot_id.clone()),
                                unit_info: info.clone(),
                            });
                            infos.insert(slot_id.clone(), info);
                        }
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
                            return (1, None);
                        }
                        None => {
                            // Cancel during identify: drop any parked
                            // UI prompt sender so consumers stop
                            // waiting, then crash with ABORTED so the
                            // operator-UI flips off the prompt screen.
                            crate::commands::run::ui_response::cancel_all().await;
                            super::emit::run_complete(
                                &event_tx,
                                super::outcomes::ABORTED,
                                execution_id,
                                None,
                            );
                            let _ = orchestrator.shutdown().await;
                            return (1, None);
                        }
                    }
                }
                infos
            }
            // Procedure has no `unit:` block: nothing to identify,
            // run starts with empty unit info regardless of reuse.
            (None, None) => std::collections::HashMap::new(),
        };

    if let Err(e) = orchestrator
        .submit_procedure(slots, strategy, unit_infos)
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
        return (1, None);
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
        let mut force_rx = cancel_rx;

        let mut graceful_fired = false;
        loop {
            tokio::select! {
                // Resolves when execution completes naturally (or after a
                // graceful shutdown_requested flip lets the loop drain).
                res = &mut exec_fut => break res,

                // Stop: flip the shared flag, keep awaiting `execute_all`
                // so teardown phases run and plugs close cleanly. Don't
                // break — loop picks up the natural-completion arm next.
                _ = graceful_rx.wait_any(), if !graceful_fired => {
                    graceful_fired = true;
                    state_arc.write().await.shutdown_requested = true;
                }

                // Kill: force_kill_immediate runs in parallel with
                // execute_all (touching the same Arcs). After it returns,
                // `execute_all` unblocks because workers are gone — await
                // it once more to collect the result.
                _ = force_rx.wait_force() => {
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
            let _ = orchestrator.shutdown().await;
            return (1, None);
        }
    };

    // Tear down worker pool + plug processes. Without this,
    // `tp_worker.py` and `tp_plug.py` subprocesses spawned by
    // `Orchestrator::initialize` outlive the run — in station
    // mode this leaks ~7 processes per run (4 workers + 3 plugs
    // for the demo procedure), saturating the host within a few
    // dozen runs.
    if let Err(e) = orchestrator.shutdown().await {
        crate::log::warn(&format!("Orchestrator shutdown error: {e}"));
    }

    // Build RunCreateRequest from accumulated data
    let data = run_data.lock().await;
    match build_run_request(&data, procedure_id, procedure_dir, operated_by.as_deref()) {
        Ok(request) => {
            let queued = QueuedRun {
                request,
                attachments: Vec::new(),
                run_id: None,
                attempt_count: 0,
                last_attempt_at: None,
                next_retry_at: None,
                parked: false,
                last_error: None,
                queued_at: None,
            };
            (exit_code, Some(queued))
        }
        Err(e) => {
            crate::log::error(&format!("Failed to build run request: {e}"));
            (exit_code, None)
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
}
