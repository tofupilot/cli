//! Cross-framework event fan-out.
//!
//! Both execution paths — YAML (via the execution engine's `EventSink`)
//! and OpenHTF (via the Python connector's NDJSON stream) — emit the
//! same conceptual lifecycle signals into two sinks: `StationEvent`
//! broadcast + `CliEvent` enqueue into the agent protocol. `EventRouter`
//! owns the fan-out so both frameworks call the same small set of
//! methods — drift becomes a type error rather than a silent semantic
//! divergence.
//!
//! # Scope
//!
//! The router handles events that either have **multi-sink shape**
//! (same concept, different payload per sink — phase_started,
//! phase_finished, phase_skipped) OR are emitted from more than one
//! framework with the same wire shape (ui_request, which both the YAML
//! engine and the OpenHTF connector publish identically). Single-sink
//! single-emitter signals (plan, measurement_recorded, plug_status, …)
//! stay where they are — a wrapper method would be bureaucracy with no
//! safety win.

use tokio::sync::broadcast;

use station_protocol::{PhaseLogLine, RunMeasurement, StationEvent, UiComponent, UnitInfo};

use super::agent_proto::{events::to_agent_ui_component, AgentProtoCtx, CliEvent};
use super::time_fmt;

/// Shared fan-out sink. Clone-cheap: both inner handles are `Arc` / `Clone`
/// underneath, so multiple consumers can keep a handle without ref cells.
#[derive(Clone)]
pub struct EventRouter {
    station_tx: broadcast::Sender<StationEvent>,
    agent: Option<AgentProtoCtx>,
    /// Per-run identity, minted at `run::start()` and threaded into the
    /// router so every emit it owns carries the same id. Reducer drops
    /// cross-run leaks (e.g. a delayed `PhaseComplete{outcome:FAIL}`
    /// from a cancelled prior run that shares phase_key with the
    /// current run).
    execution_id: String,
}

impl EventRouter {
    pub fn new(
        station_tx: broadcast::Sender<StationEvent>,
        agent: Option<AgentProtoCtx>,
        execution_id: String,
    ) -> Self {
        Self {
            station_tx,
            agent,
            execution_id,
        }
    }

    fn eid(&self) -> Option<String> {
        Some(self.execution_id.clone())
    }

    /// Phase has entered execution. Emits the TUI's `PhaseStarted` and
    /// the agent's typed `PhaseStarted`, and records the `(key, attempt,
    /// slot_id)` tuple in lifecycle history so `get_state` can answer
    /// accurately later. Both frameworks delegate here so the history
    /// write happens exactly once per phase-attempt pair regardless of
    /// which path surfaced the event.
    /// `started_at` is the engine's authoritative start timestamp when
    /// available (yaml/engine path); connector frameworks pass None and
    /// the router stamps emission time instead. Using the engine's clock
    /// keeps phase_started.started_at consistent with the started_at the
    /// completion path later reports on phase_finished.
    pub fn phase_started(
        &self,
        phase_key: &str,
        phase_name: &str,
        attempt: u32,
        slot_id: Option<String>,
        started_at: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let started_at = started_at.unwrap_or_else(chrono::Utc::now).to_rfc3339();
        let _ = self.station_tx.send(StationEvent::PhaseStarted {
            phase_key: phase_key.to_string(),
            name: phase_name.to_string(),
            slot_id: slot_id.clone(),
            attempt,
            // The router doesn't carry stage context (it's looked up
            // from the run plan client-side via `RunStarted.phases`),
            // so we leave it unset here. UI reducers fall back to
            // the plan when this field is None.
            stage: None,
            timestamp: Some(started_at.clone()),
            execution_id: self.eid(),
        });
        if let Some(ref agent) = self.agent {
            if let Ok(mut h) = agent.history.lock() {
                h.started(phase_key, attempt, slot_id.clone());
            }
            agent.emitter.enqueue(CliEvent::PhaseStarted {
                phase_key: phase_key.to_string(),
                attempt,
                slot_id,
                started_at,
            });
        }
    }

    /// Phase finished (pass/fail/error/timeout/stop/retry). Emits TUI's
    /// `PhaseComplete` and the agent's typed `PhaseFinished`. Updates
    /// lifecycle history.
    ///
    /// `measurements` is the TUI-shaped `RunMeasurement` vec; the richer
    /// agent-side measurement payload is emitted separately via
    /// `MeasurementRecorded` at record time, so we don't carry it here.
    pub fn phase_finished(&self, finished: PhaseFinished) {
        let _ = self.station_tx.send(StationEvent::PhaseComplete {
            phase_key: finished.phase_key.clone(),
            name: finished.phase_name.clone(),
            outcome: finished.outcome.clone(),
            measurements: finished.station_measurements,
            slot_id: finished.slot_id.clone(),
            attempt: finished.attempt,
            started_at: finished.started_at.clone(),
            ended_at: finished.ended_at.clone(),
            // `u32` caps at ~49 days — well past any sane phase duration. Clamp on the
            // off-chance the engine returned something pathological.
            duration_ms: finished.duration_ms.map(|d| d.min(u32::MAX as u64) as u32),
            error: finished.error.clone(),
            logs: finished.station_logs,
            execution_id: self.eid(),
        });
        if let Some(ref agent) = self.agent {
            if let Ok(mut h) = agent.history.lock() {
                h.finished(
                    &finished.phase_key,
                    finished.attempt,
                    &finished.slot_id,
                    &finished.outcome,
                );
            }
            agent.emitter.enqueue(CliEvent::PhaseFinished {
                phase_key: finished.phase_key,
                outcome: finished.outcome,
                attempt: finished.attempt,
                slot_id: finished.slot_id,
                error: finished.error,
                started_at: finished.started_at,
                ended_at: finished.ended_at,
                duration_ms: finished.duration_ms,
            });
        }
    }

    /// Operator UI request. Both the YAML engine and the OpenHTF
    /// connector emit this with the same wire shape; centralizing here
    /// avoids the byte-for-byte component-mapping duplication that used
    /// to live at both sites and stamps `timestamp` from a single
    /// helper so reconnect / hydration replays anchor the auto-submit
    /// countdown on the engine's clock.
    ///
    /// Note that `requires_input` is intentionally caller-supplied: the
    /// engine reads it from `UiConfig::requires_user_input()`, while the
    /// connector hard-codes `true` for OpenHTF prompts (which are always
    /// interactive). Don't unify — the upstream semantics differ.
    pub fn ui_request(
        &self,
        request_id: &str,
        phase_key: &str,
        slot_id: Option<String>,
        components: &[UiComponent],
        requires_input: bool,
    ) {
        let _ = self.station_tx.send(StationEvent::UiRequest {
            request_id: request_id.to_string(),
            phase_key: phase_key.to_string(),
            slot_id,
            components: Some(components.to_vec()),
            requires_input,
            timestamp: Some(time_fmt::now_rfc3339()),
            execution_id: self.eid(),
        });
    }

    /// Operator UI runtime update. Emitted when a phase mutates a live
    /// prompt's component values from Python (`ui.<key> = value`).
    /// `data` is the engine's JSON payload — passed through opaquely so
    /// future actions don't churn this method.
    pub fn ui_update(
        &self,
        phase_key: &str,
        slot_id: Option<String>,
        job_id: Option<String>,
        action: &str,
        data: Option<String>,
    ) {
        let _ = self.station_tx.send(StationEvent::UiUpdate {
            phase_key: phase_key.to_string(),
            action: action.to_string(),
            data,
            job_id,
            slot_id,
            execution_id: self.eid(),
        });
    }

    /// Pre-run identify-unit operator prompt. Receipt is the
    /// unambiguous "operator must scan the next unit" signal — no
    /// `ui_request` heuristic, no synthetic phase wrapping. The
    /// auto-resolve path (`auto_identify: true`) skips this entirely
    /// and emits only `identify_resolved`.
    pub fn identify_request(
        &self,
        request_id: &str,
        procedure_id: &str,
        slot_id: Option<String>,
        components: &[UiComponent],
    ) {
        let _ = self.station_tx.send(StationEvent::IdentifyRequest {
            request_id: request_id.to_string(),
            procedure_id: Some(procedure_id.to_string()),
            slot_id: slot_id.clone(),
            components: Some(components.to_vec()),
            timestamp: Some(time_fmt::now_rfc3339()),
            execution_id: self.eid(),
        });
        if let Some(ref agent) = self.agent {
            agent.emitter.enqueue(CliEvent::IdentifyRequest {
                request_id: request_id.to_string(),
                slot_id,
                components: components.iter().map(to_agent_ui_component).collect(),
            });
        }
    }

    /// Unit identity (or a subset) became known. Single emission point
    /// for every resolution source — pre-run prompt response, pre-run
    /// auto-resolve, mid-run prompt response, mid-run Python
    /// bound-measurement updates. UIs do a field-level merge so a
    /// mid-run scan that only fills `sub_unit:wifi:serial_number`
    /// doesn't clobber the pre-run-set `serial_number`.
    pub fn identify_resolved(&self, slot_id: Option<String>, unit: &UnitInfo) {
        let _ = self.station_tx.send(StationEvent::IdentifyResolved {
            slot_id: slot_id.clone(),
            unit: unit.clone(),
            timestamp: Some(time_fmt::now_rfc3339()),
            execution_id: self.eid(),
        });
        if let Some(ref agent) = self.agent {
            agent.emitter.enqueue(CliEvent::IdentifyResolved {
                slot_id,
                unit: unit.into(),
            });
        }
    }

    /// Pre-run identify prompt timed out. Separate from `ui_timeout`
    /// because consumers want to distinguish "operator never
    /// identified the unit" (run cancelled before any phase) from
    /// "operator never answered a mid-phase prompt" (phase timed out
    /// inside the run).
    pub fn identify_timeout(&self, request_id: &str) {
        let _ = self.station_tx.send(StationEvent::IdentifyTimeout {
            request_id: request_id.to_string(),
            execution_id: self.eid(),
        });
        if let Some(ref agent) = self.agent {
            agent.emitter.enqueue(CliEvent::IdentifyTimeout {
                request_id: request_id.to_string(),
            });
        }
    }

    /// Phase never executed (orchestrator cancelled it). The TUI hasn't
    /// learned about a first-class "skipped" state yet, so we emit a
    /// synthetic `PhaseStarted` + `PhaseComplete` pair for it; the agent
    /// protocol gets a dedicated `PhaseSkipped` so timeline consumers
    /// don't mistake "started" for "executed".
    pub fn phase_skipped(
        &self,
        phase_key: &str,
        phase_name: &str,
        slot_id: Option<String>,
        reason: Option<String>,
        outcome: &str,
    ) {
        let _ = self.station_tx.send(StationEvent::PhaseStarted {
            phase_key: phase_key.to_string(),
            name: phase_name.to_string(),
            slot_id: slot_id.clone(),
            attempt: 1,
            stage: None,
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
            execution_id: self.eid(),
        });
        let _ = self.station_tx.send(StationEvent::PhaseComplete {
            phase_key: phase_key.to_string(),
            name: phase_name.to_string(),
            outcome: outcome.to_string(),
            measurements: Vec::new(),
            slot_id: slot_id.clone(),
            attempt: 1,
            started_at: None,
            ended_at: None,
            duration_ms: None,
            error: reason.clone(),
            logs: Vec::new(),
            execution_id: self.eid(),
        });
        if let Some(ref agent) = self.agent {
            if let Ok(mut h) = agent.history.lock() {
                h.skipped(phase_key, slot_id.clone());
            }
            agent.emitter.enqueue(CliEvent::PhaseSkipped {
                phase_key: phase_key.to_string(),
                slot_id,
                reason,
            });
        }
    }
}

/// Parameter bag for `EventRouter::phase_finished`. Many fields, low
/// cohesion beyond "the phase ended" — a struct keeps call sites
/// readable instead of a 9-arg method signature.
pub struct PhaseFinished {
    pub phase_key: String,
    pub phase_name: String,
    pub outcome: String,
    pub attempt: u32,
    pub slot_id: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub station_measurements: Vec<RunMeasurement>,
    pub station_logs: Vec<PhaseLogLine>,
}
