//! TUI state machine: folds run/UI events into the renderable view model and
//! tracks the active operator prompt and its input.

use execution_engine::ui::{ComponentType, ComponentValue, UiComponent, UiRequestData};
use station_protocol::{is_stale_for_execution, PresencePayload, RunMeasurement, StationEvent};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, PartialEq)]
pub enum PhaseStatus {
    Pending,
    Running,
    Pass,
    Fail,
    Skip,
    Error,
    Timeout,
    Aborted,
}

#[derive(Debug, Clone)]
pub struct PhaseState {
    pub key: String,
    pub name: String,
    pub status: PhaseStatus,
    pub measurements: Vec<RunMeasurement>,
    /// Wall-clock moment the phase transitioned to Running. Populated in
    /// `TuiState::apply` on `PhaseStarted`. Used to render the Time column
    /// as an offset from the run's start.
    pub started_at: Option<std::time::Instant>,
}

/// Per-component typed state. Parallel to `ActiveUiRequest::components`.
/// Input types own their cursor (radios remember their highlighted option
/// independently of other radios on the form). Display types carry no state.
#[derive(Debug, Clone)]
pub enum ComponentState {
    Text(String),
    Number(String),
    Textarea(String),
    Switch(bool),
    Slider(f64),
    SingleChoice {
        value: Option<String>,
        cursor: usize,
    },
    MultiChoice {
        selected: Vec<String>,
        cursor: usize,
    },
    Display,
}

impl ComponentState {
    pub fn init(comp: &UiComponent) -> Self {
        match comp.component_type {
            ComponentType::TextInput => Self::Text(default_string(&comp.default_value)),
            ComponentType::NumberInput => Self::Number(default_string(&comp.default_value)),
            ComponentType::Textarea => Self::Textarea(default_string(&comp.default_value)),
            ComponentType::Switch => Self::Switch(default_bool(&comp.default_value)),
            ComponentType::Slider => {
                let min = comp.min.unwrap_or(0.0);
                Self::Slider(default_number(&comp.default_value).unwrap_or(min))
            }
            ComponentType::Radio | ComponentType::Select => {
                let value = default_single(&comp.default_value);
                let cursor = value
                    .as_ref()
                    .and_then(|v| comp.options.as_ref()?.iter().position(|o| &o.value == v))
                    .unwrap_or(0);
                Self::SingleChoice { value, cursor }
            }
            ComponentType::Multiselect | ComponentType::Checklist => Self::MultiChoice {
                selected: default_array(&comp.default_value),
                cursor: 0,
            },
            ComponentType::Text | ComponentType::Image | ComponentType::Progress => Self::Display,
        }
    }

    /// Serialize to the `HashMap<String,String>` expected by the engine.
    /// None means "omit from response" (display components).
    pub fn to_response(&self, comp: &UiComponent) -> Option<String> {
        let raw = match self {
            Self::Text(s) | Self::Number(s) | Self::Textarea(s) => s.clone(),
            Self::Switch(b) => b.to_string(),
            Self::Slider(n) => format!("{n}"),
            Self::SingleChoice { value, .. } => value.clone().unwrap_or_default(),
            Self::MultiChoice { selected, .. } => selected.join(","),
            Self::Display => return None,
        };
        Some(if comp.trim {
            raw.trim().to_string()
        } else {
            raw
        })
    }
}

fn default_string(v: &Option<ComponentValue>) -> String {
    match v {
        Some(ComponentValue::String(s)) => s.clone(),
        Some(ComponentValue::Number(n)) => n.to_string(),
        Some(ComponentValue::Boolean(b)) => b.to_string(),
        Some(ComponentValue::Array(a)) => a.join(","),
        None => String::new(),
    }
}

fn default_bool(v: &Option<ComponentValue>) -> bool {
    matches!(v, Some(ComponentValue::Boolean(true)))
}

fn default_number(v: &Option<ComponentValue>) -> Option<f64> {
    match v {
        Some(ComponentValue::Number(n)) => Some(*n),
        Some(ComponentValue::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn default_single(v: &Option<ComponentValue>) -> Option<String> {
    match v {
        Some(ComponentValue::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn default_array(v: &Option<ComponentValue>) -> Vec<String> {
    match v {
        Some(ComponentValue::Array(a)) => a.clone(),
        Some(ComponentValue::String(s)) if !s.is_empty() => s
            .split(',')
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Tracks the active operator UI request
pub struct ActiveUiRequest {
    pub request_id: String,
    pub phase_key: String,
    pub components: Vec<UiComponent>,
    pub states: Vec<ComponentState>,
    pub requires_input: bool,
    /// Index into `components` of the currently focused input component.
    pub focused_index: usize,
    /// Validation errors keyed by component key.
    pub errors: HashMap<String, String>,
    /// Submitted but not yet cleared (engine is processing).
    pub submitted: bool,
}

impl ActiveUiRequest {
    pub fn new(request: &UiRequestData) -> Self {
        let components = request.config.components.clone();
        let states: Vec<ComponentState> = components.iter().map(ComponentState::init).collect();
        let requires_input = request.config.requires_user_input();
        let focused_index = components.iter().position(|c| c.is_input).unwrap_or(0);

        Self {
            request_id: request.request_id.clone(),
            phase_key: request.phase_key.clone(),
            components,
            states,
            requires_input,
            focused_index,
            errors: HashMap::new(),
            submitted: false,
        }
    }

    pub fn input_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.components
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_input)
            .map(|(i, _)| i)
    }

    pub fn focused_mut(&mut self) -> Option<(&UiComponent, &mut ComponentState)> {
        let i = self.focused_index;
        let comp = self.components.get(i)?;
        let state = self.states.get_mut(i)?;
        Some((comp, state))
    }

    /// Immutable peek at the focused component + its typed state. Used
    /// by the presence publisher to read the current draft value /
    /// component key without needing mutable borrow.
    pub fn focused_pair(&self) -> Option<(&UiComponent, &ComponentState)> {
        let i = self.focused_index;
        let comp = self.components.get(i)?;
        let state = self.states.get(i)?;
        Some((comp, state))
    }

    pub fn focus_next(&mut self) {
        let inputs: Vec<usize> = self.input_indices().collect();
        if inputs.is_empty() {
            return;
        }
        let pos = inputs
            .iter()
            .position(|&i| i == self.focused_index)
            .unwrap_or(0);
        self.focused_index = inputs[(pos + 1) % inputs.len()];
    }

    pub fn focus_prev(&mut self) {
        let inputs: Vec<usize> = self.input_indices().collect();
        if inputs.is_empty() {
            return;
        }
        let pos = inputs
            .iter()
            .position(|&i| i == self.focused_index)
            .unwrap_or(0);
        let prev = if pos == 0 { inputs.len() - 1 } else { pos - 1 };
        self.focused_index = inputs[prev];
    }

    /// Validate all input components. Populates `errors`, returns true if clean.
    pub fn validate(&mut self) -> bool {
        self.errors.clear();
        for (comp, state) in self.components.iter().zip(self.states.iter()) {
            if !comp.is_input {
                continue;
            }
            if let Some(err) = validate_one(comp, state) {
                self.errors.insert(comp.key.clone(), err);
            }
        }
        self.errors.is_empty()
    }

    pub fn collect_response(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for (comp, state) in self.components.iter().zip(self.states.iter()) {
            if !comp.is_input {
                continue;
            }
            if let Some(v) = state.to_response(comp) {
                out.insert(comp.key.clone(), v);
            }
        }
        out
    }
}

fn validate_one(comp: &UiComponent, state: &ComponentState) -> Option<String> {
    let label = comp.label.as_deref().unwrap_or(&comp.key);

    let is_empty = match state {
        ComponentState::Text(s) | ComponentState::Number(s) | ComponentState::Textarea(s) => {
            s.is_empty()
        }
        ComponentState::SingleChoice { value, .. } => value.is_none(),
        ComponentState::MultiChoice { selected, .. } => selected.is_empty(),
        ComponentState::Switch(_) | ComponentState::Slider(_) | ComponentState::Display => false,
    };

    if comp.required && is_empty {
        return Some(format!("{label} is required"));
    }
    if is_empty {
        return None;
    }

    if let ComponentState::Text(s) | ComponentState::Textarea(s) = state {
        if let Some(min) = comp.min_length {
            if s.chars().count() < min as usize {
                return Some(format!("Minimum {min} characters"));
            }
        }
        if let Some(max) = comp.max_length {
            if s.chars().count() > max as usize {
                return Some(format!("Maximum {max} characters"));
            }
        }
        if let Some(ref pattern) = comp.pattern {
            if let Ok(re) = regex::Regex::new(pattern) {
                if !re.is_match(s) {
                    return Some(format!("Must match pattern: {pattern}"));
                }
            }
        }
    }

    if let ComponentState::Number(s) = state {
        let Ok(n) = s.parse::<f64>() else {
            return Some("Must be a number".to_string());
        };
        if let Some(min) = comp.min {
            if n < min {
                return Some(format!("Minimum {min}"));
            }
        }
        if let Some(max) = comp.max {
            if n > max {
                return Some(format!("Maximum {max}"));
            }
        }
    }

    None
}

pub struct TuiState {
    /// Per-run identity from `RunStarted.execution_id`. Stored so the TUI
    /// can correlate later lifecycle events with the run that started
    /// them; not user-visible (it's a UUID).
    pub execution_id: String,
    pub procedure_id: String,
    pub phases: Vec<PhaseState>,
    pub outcome: Option<String>,
    pub done: bool,
    pub active_ui: Option<ActiveUiRequest>,
    /// FIFO of UI requests that arrived while another was still active.
    pub pending_ui: VecDeque<UiRequestData>,
    /// Last observed broadcast lag — surfaced in the footer for one tick
    /// so the operator knows events were dropped.
    pub lag_warning: Option<u64>,
    /// Local monotonic clock at `RunStarted`. Used to render the Time
    /// column in the phase table as an offset from run start.
    pub run_started_at: Option<std::time::Instant>,
    /// Local monotonic clock at `RunComplete`. Freezes the elapsed
    /// timer in `draw_progress` so it stops counting once the run
    /// reaches a terminal state, matching the web ticker behaviour.
    pub run_ended_at: Option<std::time::Instant>,
    /// Who's currently focused / typing on the active UI request. Keyed
    /// by `user_id`. Stale entries get evicted on every tick via
    /// `evict_stale_presence` so a crashed dashboard tab stops showing
    /// up as "typing" after the budget elapses.
    pub presence: HashMap<String, PresenceState>,
    /// TUI's own user id when it publishes presence. Centrifugo echoes
    /// publishes back to subscribers including the publisher, so we
    /// drop incoming presence that matches our own id — without this
    /// filter the TUI would render its own badge.
    pub self_user_id: Option<String>,
    /// Diagnostic from a `RunCrashed` event — load failure, init
    /// error, subprocess crash. The terminal screen renders this
    /// front-and-centre instead of the (empty) phase table when set.
    /// Mutually informative with `outcome` = "ERROR" but kept on its
    /// own field because plain phase-level failures don't populate it.
    pub run_error: Option<RunErrorState>,
    /// Tracks whether the operator has already pressed the stop key
    /// once. Mirrors the web operator-UI button morph: first press
    /// publishes `Stop` (graceful), second press escalates to `Kill`
    /// (force). Reset on a fresh run by virtue of a new `TuiState`
    /// being constructed; not cleared mid-run because there's no
    /// "un-stop" — once the operator commits to abort, the only
    /// follow-up is escalation.
    pub stop_pressed: bool,
}

#[derive(Debug, Clone)]
pub struct RunErrorState {
    pub message: String,
    pub kind: String,
}

/// TUI-local view of a remote participant's presence. Unlike the wire
/// `PresencePayload`, this carries a local `received_at` we use to
/// enforce the stale budget against our own monotonic clock — the
/// publisher's `updated_at` only helps across re-broadcasts.
#[derive(Debug, Clone)]
pub struct PresenceState {
    // Deserialized from the presence wire payload and kept for parity with the
    // protocol; the TUI renders by display_name only, so it isn't read yet.
    #[allow(dead_code)]
    pub user_id: String,
    pub display_name: String,
    pub focus_request_id: Option<String>,
    pub seq: u32,
    pub received_at: std::time::Instant,
}

/// Match the web's PRESENCE_STALE_MS / 1000. A remote that hasn't
/// re-broadcast within this window is considered gone.
pub const PRESENCE_STALE: std::time::Duration = std::time::Duration::from_secs(5);

impl TuiState {
    pub fn new() -> Self {
        Self {
            execution_id: String::new(),
            procedure_id: String::new(),
            phases: Vec::new(),
            outcome: None,
            done: false,
            active_ui: None,
            pending_ui: VecDeque::new(),
            lag_warning: None,
            run_started_at: None,
            run_ended_at: None,
            presence: HashMap::new(),
            self_user_id: None,
            run_error: None,
            stop_pressed: false,
        }
    }

    /// Drop any presence entries that haven't refreshed within
    /// `PRESENCE_STALE`. Called from the TUI tick so a badge for a
    /// crashed dashboard tab doesn't stick forever.
    pub fn evict_stale_presence(&mut self) {
        let now = std::time::Instant::now();
        self.presence
            .retain(|_, p| now.duration_since(p.received_at) < PRESENCE_STALE);
    }

    /// Fold a `PresencePayload` into the presence map. Drops
    /// out-of-order deliveries via the monotonic `seq`. When both focus
    /// and draft are cleared the entry stays alive for heartbeat
    /// purposes, shown as "present but idle".
    pub fn apply_presence(&mut self, payload: PresencePayload) {
        // Centrifugo echoes publishes back to every subscriber, including
        // the publisher. Ignore our own echoes so the TUI doesn't render
        // itself as a remote presence badge.
        if let Some(self_id) = &self.self_user_id {
            if payload.user_id == *self_id {
                return;
            }
        }
        if let Some(existing) = self.presence.get(&payload.user_id) {
            if existing.seq >= payload.seq {
                return;
            }
        }
        // Remote counts as "typing" when they have focus OR a draft on any
        // request. Draft without focus is possible transiently; either
        // presence signal is enough to surface the indicator.
        let focus_req = payload
            .focus
            .as_ref()
            .map(|f| f.request_id.clone())
            .or_else(|| payload.draft.as_ref().map(|d| d.request_id.clone()));
        self.presence.insert(
            payload.user_id.clone(),
            PresenceState {
                user_id: payload.user_id,
                display_name: payload.display_name,
                focus_request_id: focus_req,
                seq: payload.seq,
                received_at: std::time::Instant::now(),
            },
        );
    }

    /// Apply a StationEvent. Returns true when the run is complete.
    pub fn apply(&mut self, event: StationEvent) -> bool {
        match event {
            StationEvent::RunStarted {
                procedure_id,
                execution_id,
                phases,
                ..
            } => {
                self.procedure_id = procedure_id;
                self.execution_id = execution_id;
                self.phases = phases
                    .into_iter()
                    .map(|p| PhaseState {
                        key: p.key,
                        name: p.name,
                        status: PhaseStatus::Pending,
                        measurements: Vec::new(),
                        started_at: None,
                    })
                    .collect();
                self.run_started_at = Some(std::time::Instant::now());
            }
            StationEvent::PhaseStarted { phase_key, .. } => {
                if let Some(p) = self.phases.iter_mut().find(|p| p.key == phase_key) {
                    p.status = PhaseStatus::Running;
                    p.started_at = Some(std::time::Instant::now());
                }
            }
            StationEvent::PhaseComplete {
                phase_key,
                outcome,
                measurements,
                ..
            } => {
                if let Some(p) = self.phases.iter_mut().find(|p| p.key == phase_key) {
                    p.status = match outcome.as_str() {
                        super::super::outcomes::PASS => PhaseStatus::Pass,
                        super::super::outcomes::SKIP => PhaseStatus::Skip,
                        super::super::outcomes::ERROR => PhaseStatus::Error,
                        super::super::outcomes::TIMEOUT => PhaseStatus::Timeout,
                        super::super::outcomes::ABORTED => PhaseStatus::Aborted,
                        _ => PhaseStatus::Fail,
                    };
                    p.measurements = measurements;
                }
                if let Some(ref ui) = self.active_ui {
                    if ui.phase_key == phase_key {
                        self.active_ui = None;
                    }
                }
                self.pending_ui.retain(|r| r.phase_key != phase_key);
                self.promote_pending_ui();
            }
            StationEvent::RunComplete { outcome, .. } => {
                self.outcome = Some(outcome);
                self.done = true;
                if self.run_ended_at.is_none() {
                    self.run_ended_at = Some(std::time::Instant::now());
                }
                self.active_ui = None;
                self.pending_ui.clear();
                return true;
            }
            StationEvent::RunCrashed {
                error,
                error_kind,
                procedure_id,
                ..
            } => {
                // The CLI emits a synthetic RunComplete(ERROR) right
                // after every RunCrashed; this arm just stamps the
                // error blurb so the terminal screen has something
                // operator-meaningful to render. Idempotent: a stray
                // second RunCrashed is a no-op.
                if self.procedure_id.is_empty() {
                    self.procedure_id = procedure_id;
                }
                if self.run_error.is_none() {
                    self.run_error = Some(RunErrorState {
                        message: error,
                        kind: error_kind,
                    });
                }
                self.active_ui = None;
                self.pending_ui.clear();
            }
            StationEvent::Presence(payload) => {
                self.apply_presence(payload);
            }
            StationEvent::UiUpdate {
                phase_key,
                action,
                data,
                slot_id,
                execution_id,
                ..
            } => {
                // Cross-run gate via shared protocol predicate. Drops
                // a `ui_update` whose `execution_id` belongs to a
                // cancelled prior run — same-procedure runs share
                // phase keys, so without the gate a stale event would
                // overwrite the active prompt's component value.
                let active = (!self.execution_id.is_empty()).then_some(self.execution_id.as_str());
                if is_stale_for_execution(active, execution_id.as_deref()) {
                    return false;
                }
                self.apply_ui_update(&phase_key, slot_id.as_deref(), &action, data.as_deref());
            }
            _ => {}
        }
        false
    }

    /// Apply a mid-prompt mutation from Python (`ui.<key> = value`) to
    /// the active and pending UI requests on the matching phase. No-ops
    /// when no request matches or the payload is malformed — older
    /// engines emit `UiUpdate` without a body and we shouldn't panic.
    fn apply_ui_update(
        &mut self,
        phase_key: &str,
        slot_id: Option<&str>,
        action: &str,
        data: Option<&str>,
    ) {
        if action != "set_value" {
            return;
        }
        let Some(payload) = data else { return };
        let parsed: serde_json::Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return,
        };
        let Some(id) = parsed.get("id").and_then(|v| v.as_str()) else {
            return;
        };
        let value = match parsed.get("value") {
            Some(serde_json::Value::String(s)) => ComponentValue::String(s.clone()),
            Some(serde_json::Value::Number(n)) => match n.as_f64() {
                Some(f) => ComponentValue::Number(f),
                None => return,
            },
            Some(serde_json::Value::Bool(b)) => ComponentValue::Boolean(*b),
            Some(serde_json::Value::Array(a)) => {
                let strs: Option<Vec<String>> =
                    a.iter().map(|v| v.as_str().map(String::from)).collect();
                match strs {
                    Some(s) => ComponentValue::Array(s),
                    None => return,
                }
            }
            _ => return,
        };

        if let Some(ui) = self.active_ui.as_mut() {
            if ui.phase_key == phase_key {
                if let Some(c) = ui.components.iter_mut().find(|c| c.key == id) {
                    c.value = Some(value.clone());
                }
            }
        }
        // KNOWN LIMITATION: TUI's `ActiveUiRequest` doesn't carry the
        // slot_id today, so a multi-slot run fanning per-slot
        // `ui_update`s on a shared-phase prompt has all updates land
        // on whatever prompt happens to be active. Web reducer gates
        // on slot match (`applyUiUpdateToRequest`); fixing here
        // requires threading `slot_id` through `ActiveUiRequest::new`
        // + `UiRequestData`. Documented to revisit once multi-slot
        // CLI runs render in the TUI.
        let _ = slot_id;
        for req in self.pending_ui.iter_mut() {
            if req.phase_key != phase_key {
                continue;
            }
            if let Some(c) = req.config.components.iter_mut().find(|c| c.key == id) {
                c.value = Some(value.clone());
            }
        }
    }

    pub fn set_ui_request(&mut self, request: &UiRequestData) {
        if self.active_ui.is_none() {
            self.active_ui = Some(ActiveUiRequest::new(request));
        } else {
            self.pending_ui.push_back(request.clone());
        }
    }

    pub fn promote_pending_ui(&mut self) {
        if self.active_ui.is_none() {
            if let Some(next) = self.pending_ui.pop_front() {
                self.active_ui = Some(ActiveUiRequest::new(&next));
            }
        }
    }

    pub fn completed_count(&self) -> usize {
        self.phases
            .iter()
            .filter(|p| {
                matches!(
                    p.status,
                    PhaseStatus::Pass
                        | PhaseStatus::Fail
                        | PhaseStatus::Skip
                        | PhaseStatus::Error
                        | PhaseStatus::Timeout
                        | PhaseStatus::Aborted
                )
            })
            .count()
    }

    pub fn total_count(&self) -> usize {
        self.phases.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_engine::ui::{UiConfig, UiRequestData};
    use station_protocol::PhasePlan;

    fn plan(phases: &[(&str, &str)]) -> Vec<PhasePlan> {
        phases
            .iter()
            .map(|(k, n)| PhasePlan {
                key: (*k).into(),
                name: (*n).into(),
                stage: String::new(),
            })
            .collect()
    }

    fn run_started(phases: Vec<PhasePlan>) -> StationEvent {
        StationEvent::RunStarted {
            procedure_id: "PROC-1".into(),
            procedure_name: "Test Procedure".into(),
            execution_id: "exec-test".into(),
            phases,
            slots: Vec::new(),
            plugs: Vec::new(),
            timestamp: None,
            run_id: None,
            unit: None,
        }
    }

    fn ui_request(id: &str, phase_key: &str) -> UiRequestData {
        UiRequestData {
            request_id: id.into(),
            job_id: String::new(),
            pipe_path: String::new(),
            config: UiConfig {
                components: Vec::new(),
                requires_input: Some(false),
            },
            phase_key: phase_key.into(),
            slot_id: None,
        }
    }

    #[test]
    fn run_started_populates_phases_as_pending() {
        let mut s = TuiState::new();
        s.apply(run_started(plan(&[("k1", "Phase 1"), ("k2", "Phase 2")])));
        assert_eq!(s.phases.len(), 2);
        assert!(s.phases.iter().all(|p| p.status == PhaseStatus::Pending));
        assert_eq!(s.procedure_id, "PROC-1");
    }

    #[test]
    fn phase_started_marks_running() {
        let mut s = TuiState::new();
        s.apply(run_started(plan(&[("k1", "Phase 1")])));
        s.apply(StationEvent::PhaseStarted {
            phase_key: "k1".into(),
            name: "Phase 1".into(),
            slot_id: None,
            attempt: 1,
            stage: None,
            timestamp: None,
            execution_id: None,
        });
        assert_eq!(s.phases[0].status, PhaseStatus::Running);
    }

    #[test]
    fn phase_complete_maps_outcome() {
        let mut s = TuiState::new();
        s.apply(run_started(plan(&[("k1", "P")])));
        s.apply(StationEvent::PhaseComplete {
            phase_key: "k1".into(),
            name: "P".into(),
            outcome: "PASS".into(),
            measurements: Vec::new(),
            slot_id: None,
            attempt: 1,
            started_at: None,
            ended_at: None,
            duration_ms: None,
            error: None,
            logs: Vec::new(),
            execution_id: None,
        });
        assert_eq!(s.phases[0].status, PhaseStatus::Pass);
    }

    #[test]
    fn run_complete_sets_done_and_clears_ui() {
        let mut s = TuiState::new();
        s.apply(run_started(plan(&[("k1", "P")])));
        s.set_ui_request(&ui_request("r1", "k1"));
        assert!(s.active_ui.is_some());
        let terminal = s.apply(StationEvent::RunComplete {
            outcome: "PASS".into(),
            run_id: None,
            execution_id: None,
        });
        assert!(terminal);
        assert!(s.done);
        assert!(s.active_ui.is_none());
        assert!(s.pending_ui.is_empty());
    }

    #[test]
    fn second_ui_request_queues_behind_first() {
        let mut s = TuiState::new();
        s.set_ui_request(&ui_request("r1", "k1"));
        s.set_ui_request(&ui_request("r2", "k1"));
        assert_eq!(s.active_ui.as_ref().unwrap().request_id, "r1");
        assert_eq!(s.pending_ui.len(), 1);
    }

    #[test]
    fn phase_complete_drops_queued_requests_for_that_phase() {
        // An in-flight request for a phase that completes early (e.g. timed
        // out) plus a queued follow-up should both be cleared — the phase
        // can't answer them anymore.
        let mut s = TuiState::new();
        s.apply(run_started(plan(&[("k1", "P"), ("k2", "Q")])));
        s.set_ui_request(&ui_request("r1", "k1"));
        s.set_ui_request(&ui_request("r2", "k1"));
        s.set_ui_request(&ui_request("r3", "k2"));
        s.apply(StationEvent::PhaseComplete {
            phase_key: "k1".into(),
            name: "P".into(),
            outcome: "FAIL".into(),
            measurements: Vec::new(),
            slot_id: None,
            attempt: 1,
            started_at: None,
            ended_at: None,
            duration_ms: None,
            error: None,
            logs: Vec::new(),
            execution_id: None,
        });
        // k1's active cleared, k1's queued entry dropped, k2's promoted.
        assert_eq!(s.active_ui.as_ref().unwrap().request_id, "r3");
        assert!(s.pending_ui.is_empty());
    }

    #[test]
    fn promote_noop_when_no_pending() {
        let mut s = TuiState::new();
        s.promote_pending_ui();
        assert!(s.active_ui.is_none());
    }

    #[test]
    fn completed_and_total_counts() {
        let mut s = TuiState::new();
        s.apply(run_started(plan(&[("a", "A"), ("b", "B"), ("c", "C")])));
        s.apply(StationEvent::PhaseComplete {
            phase_key: "a".into(),
            name: "A".into(),
            outcome: "PASS".into(),
            measurements: Vec::new(),
            slot_id: None,
            attempt: 1,
            started_at: None,
            ended_at: None,
            duration_ms: None,
            error: None,
            logs: Vec::new(),
            execution_id: None,
        });
        assert_eq!(s.completed_count(), 1);
        assert_eq!(s.total_count(), 3);
    }
}
