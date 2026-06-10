//! Wire types for the agent protocol — the `CliEvent`s emitted to stdout and the
//! responses parsed from stdin.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Protocol version emitted on `run_started`. Bump on any breaking change
/// (field removal, rename, variant removal, semantic change). Adding
/// optional fields does NOT bump the version — agents are expected to
/// ignore unknown fields per the extension rule in PROTOCOL.md.
pub const PROTOCOL_VERSION: &str = "1.0";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CliEvent {
    /// First event in the stream. `protocol_version` lets agents detect a
    /// CLI too new or too old for them on handshake rather than failing
    /// mid-run when a schema mismatch first bites.
    RunStarted {
        procedure_id: String,
        protocol_version: &'static str,
    },
    /// Phase plan (emitted after run_started once the engine has loaded the procedure).
    Plan {
        phases: Vec<PhasePlanPayload>,
    },
    PhaseStarted {
        phase_key: String,
        /// 1-indexed attempt number for this phase. Always 1 unless the phase
        /// is being re-run (OpenHTF `PhaseResult.REPEAT` / `repeat_limit`).
        attempt: u32,
        /// Slot key for multi-slot procedures (`execution.slots`). None for
        /// single-slot runs. Agents observing parallel multi-slot execution
        /// need this to disambiguate events for the same phase_key.
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        /// RFC 3339 start time. `phase_finished` carries `started_at`/
        /// `ended_at`; agents timing in-flight phases need the start here.
        started_at: String,
    },
    UiRequest {
        request_id: String,
        phase_key: String,
        phase_description: Option<String>,
        requires_input: bool,
        components: Vec<AgentUiComponent>,
    },
    UiAutoContinue {
        request_id: String,
        phase_key: String,
        source: UiAutoContinueSource,
        values: HashMap<String, serde_json::Value>,
    },
    UiTimeout {
        request_id: String,
        phase_key: String,
    },
    /// Pre-run identify-unit operator prompt. Distinct from
    /// `UiRequest` because identity is run metadata, not a phase
    /// prompt — agents use this to know "the operator must scan the
    /// next unit" without inspecting component shapes. Auto-resolved
    /// `auto_identify` runs skip this and emit only
    /// `IdentifyResolved`.
    IdentifyRequest {
        request_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        components: Vec<AgentUiComponent>,
    },
    /// Unit identity (or a subset) became known. Fires from every
    /// resolution path: pre-run prompt, pre-run auto-resolve, mid-run
    /// prompt response, mid-run Python bound-measurement updates.
    /// Field-level merge — non-null fields overwrite, sub-units merge
    /// by key.
    IdentifyResolved {
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        unit: AgentIdentifyUnit,
    },
    /// Pre-run identify prompt timed out before the operator answered.
    IdentifyTimeout {
        request_id: String,
    },
    UiError {
        request_id: Option<String>,
        reason: UiErrorReason,
        field: Option<String>,
        got: Option<serde_json::Value>,
        expected: Option<String>,
    },
    PhaseFinished {
        phase_key: String,
        outcome: String,
        attempt: u32,
        /// Slot key for multi-slot procedures. Matches `PhaseStarted.slot_id`.
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        /// Non-null when the phase's underlying Python raised or the framework
        /// failed to execute it (e.g. `ModuleNotFoundError`, exception in the
        /// phase body, measurement validator failed). Agents can surface this
        /// without parsing stderr.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Wall-clock timestamps (ISO 8601 with timezone). Let agents compute
        /// duration and detect slow phases without timestamping events
        /// themselves.
        #[serde(skip_serializing_if = "Option::is_none")]
        started_at: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ended_at: Option<String>,
        /// Duration in milliseconds, derived from start/end when both are set.
        /// Emitted separately so agents don't have to re-parse ISO strings.
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// Phase never executed. Distinct from PhaseFinished so timeline UIs
    /// don't have to synthesize "start" events for phases that the scheduler
    /// cancelled before they could run (e.g. plug init failure upstream,
    /// `on_first_failure: stop` after an earlier phase failed).
    PhaseSkipped {
        phase_key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Live log line from a phase Python body. Emitted in real time as
    /// the phase runs; survives force-kills because it isn't batched into
    /// the phase_finished payload.
    PhaseLog {
        phase_key: String,
        level: String,
        message: String,
        timestamp: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
    },
    /// Plug lifecycle transition (idle → initializing → active →
    /// destructing → idle, or → error). Useful for debugging plug init
    /// hangs or cleanup failures.
    ///
    /// There is no separate init-started / init-finished pair: the
    /// `initializing` → `active` (or → `error`) transitions carry the
    /// same information. Agents watching multiple plugs in parallel
    /// should key by `(plug_key, slot_id)` and track the latest
    /// `status`. See PROTOCOL.md for the state-machine sketch.
    PlugStatus {
        plug_key: String,
        plug_name: String,
        status: String,
        stage: String,
        scope: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
    },
    /// Live log line from a plug's Python service process.
    PlugLog {
        plug_key: String,
        plug_name: String,
        level: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        /// Lifecycle stage at the time the line was emitted —
        /// `"setup"` / `"teardown"` / `"manual"`. None when the
        /// engine can't disambiguate.
        #[serde(skip_serializing_if = "Option::is_none")]
        stage: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
    },
    /// A measurement was recorded. Emitted live so agents can see values
    /// land as they happen, not only in the final phase_finished bundle.
    ///
    /// Replay semantics: assigning the same measurement name twice
    /// (`measurements.X = 1; measurements.X = 2`) emits two events,
    /// not one. `phase_finished.measurements[].value` carries the
    /// **last** value (write-wins); agents rendering a live panel
    /// should de-duplicate by `(phase_key, slot_id, name)` and take
    /// the latest event. `outcome` is always `"unset"` here because
    /// validators don't fire until phase close — consult
    /// `phase_finished` for pass/fail.
    MeasurementRecorded {
        phase_key: String,
        name: String,
        value: serde_json::Value,
        outcome: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
    },
    /// A phase attached a file or data blob to the run. Timing differs by
    /// framework: YAML procedures emit `attachment_added` live (at the
    /// moment of attach.data / attach.file), while OpenHTF emits them in a
    /// batch right after each phase's `phase_finished` because OpenHTF's
    /// public API has no on-attach hook. For both frameworks the event
    /// appears before the run_finished terminator. Agents rendering an
    /// "attachments" panel should treat this as append-on-see, not
    /// real-time-streaming.
    AttachmentAdded {
        phase_key: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mimetype: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
    },
    RunFinished {
        outcome: String,
        exit_code: i32,
    },
    /// The procedure subprocess terminated before emitting a complete run
    /// (e.g. SyntaxError at import time, ImportError, raise at module load,
    /// mid-phase sys.exit / segfault). Followed immediately by RunFinished.
    /// `stderr_tail` contains the last ~4KB of the subprocess's stderr so
    /// agents can diagnose without redirecting stderr themselves.
    RunCrashed {
        exit_code: i32,
        stderr_tail: String,
    },
    /// Reply to `get_state`. Enumerates every phase the CLI has seen so
    /// far and the outcome if finished. Agents use this to recover after
    /// a parse glitch or to sanity-check their own event parsing.
    ///
    /// `run_status` distinguishes "procedure hasn't booted yet" from
    /// "procedure has zero phases" (both produce an empty `phases` list
    /// otherwise). When a UI request is in flight, `active_ui_request`
    /// carries the full component spec so the agent can reconstruct the
    /// prompt without the original `ui_request` event.
    StateSnapshot {
        run_status: RunStatus,
        phases: Vec<PhaseSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        active_ui_request: Option<ActiveUiRequest>,
    },
    /// The run's results were persisted to the local queue and scheduled
    /// for upload. Agents get the `queue_id` even when they never see a
    /// `run_id` because the upload is deferred or failed. The extra
    /// fields mirror `StationEvent::RunUploadQueued` so JSON-mode
    /// agents have the same metadata surface UIs do.
    RunUploadQueued {
        queue_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        procedure_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        serial_number: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        attachment_count: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        queued_at: Option<String>,
    },
    /// Upload attempt starting. Mirrors `StationEvent::RunUploadStarted`
    /// for agent-mode debuggability — without it the CLI's upload path
    /// is opaque from the JSON stream.
    RunUploadStarted {
        queue_id: String,
        attempt: u32,
    },
    /// Upload finished successfully. Surfaces the `run_id` the API
    /// minted plus the dashboard URL so agents can hyperlink.
    RunUploadSucceeded {
        queue_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        dashboard_url: Option<String>,
    },
    /// Upload attempt failed. Carries the classified error (4xx / 5xx
    /// / network) and `next_retry_at` so agents can decide whether to
    /// alert.
    RunUploadFailed {
        queue_id: String,
        attempt: u32,
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        error: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_retry_at: Option<String>,
    },
    /// Entry removed from the queue (operator dropped or TTL expired).
    RunUploadDropped {
        queue_id: String,
        reason: String,
    },
    /// Non-fatal CLI-side anomaly the agent should surface to its user.
    /// Covers gaps the protocol can't express as a typed event — e.g. an
    /// unknown `type` from the python connector (Rust missing a variant),
    /// a measurement value too large to transmit inline. Keeping these in
    /// the stream instead of stderr means agent UIs can flag them.
    InternalWarning {
        kind: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct PhaseSnapshot {
    pub phase_key: String,
    pub status: String,
    pub attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

/// Run lifecycle state, exposed in `state_snapshot` so agents can distinguish
/// "not yet started" from "running with zero phases" from "already finished".
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    NotStarted,
    Running,
    Finished,
}

/// Full reconstruction payload for an in-flight UI prompt, embedded in
/// `state_snapshot`. Matches the shape of `ui_request` so an agent that
/// missed the original can answer straight from the snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct ActiveUiRequest {
    pub request_id: String,
    pub phase_key: String,
    pub components: Vec<AgentUiComponent>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UiAutoContinueSource {
    DisplayOnly,
    PreBaked,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UiErrorReason {
    ParseError,
    UnknownRequest,
    MissingRequired,
    InvalidValue,
    UnknownField,
    /// Control command (abort_run, get_state) rejected because the run
    /// is in the wrong lifecycle state — either not booted yet, or
    /// already finished.
    InvalidState,
}

/// Agent-protocol subset of `station_protocol::UiComponent`. Frozen
/// surface for external agent integrations (LLM tooling): only the
/// fields an agent needs to reason about a prompt — identity, label,
/// type, requiredness, options, numeric bounds. Styling and text
/// constraints (`size`, `color`, `font`, `width`, `height`, `aspect`,
/// `fit`, `min_length`, `max_length`, `pattern`, `prefix`, `suffix`,
/// `trim`, `rows`, `columns`, `bind`, `value`) are intentionally
/// dropped to keep the agent stream stable as the operator-UI wire
/// evolves. Adding a field here is a breaking change to the agent
/// surface — keep it intentional.
#[derive(Debug, Clone, Serialize)]
pub struct AgentUiComponent {
    pub key: String,
    #[serde(rename = "type")]
    pub component_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
    pub is_input: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<AgentUiOption>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentUiOption {
    pub value: String,
    pub label: String,
}

/// Wire shape for `IdentifyResolved.unit`. Mirrors
/// `station_protocol::UnitInfo` field-for-field — kept separate so the
/// agent stream isn't pinned to wire-protocol layout decisions.
#[derive(Debug, Clone, Serialize)]
pub struct AgentIdentifyUnit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_number: Option<String>,
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub sub_units: std::collections::HashMap<String, String>,
}

impl From<&station_protocol::UnitInfo> for AgentIdentifyUnit {
    fn from(u: &station_protocol::UnitInfo) -> Self {
        Self {
            serial_number: u.serial_number.clone(),
            part_number: u.part_number.clone(),
            revision_number: u.revision_number.clone(),
            batch_number: u.batch_number.clone(),
            sub_units: u.sub_units.clone(),
        }
    }
}

/// Convert an engine `UiComponent` into the agent-protocol subset.
/// Lives alongside the type so both engine and reader can reach it
/// without a layering violation (agent_proto must not depend on
/// engine).
pub fn to_agent_ui_component(c: &execution_engine::ui::UiComponent) -> AgentUiComponent {
    AgentUiComponent {
        key: c.key.clone(),
        component_type: c.component_type.as_str().to_string(),
        label: c.label.clone(),
        description: c.description.clone(),
        required: c.required,
        is_input: c.is_input,
        default_value: c
            .default_value
            .as_ref()
            .and_then(|v| serde_json::to_value(v).ok()),
        placeholder: c.placeholder.clone(),
        options: c.options.as_ref().map(|opts| {
            opts.iter()
                .map(|o| AgentUiOption {
                    value: o.value.clone(),
                    label: o.label.clone(),
                })
                .collect()
        }),
        min: c.min,
        max: c.max,
        step: c.step,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PhasePlanPayload {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CliCommand {
    UiResponse {
        request_id: String,
        values: HashMap<String, serde_json::Value>,
    },
    /// Ask the CLI to abort the entire run. The current phase's Python
    /// subprocess gets SIGTERM; subsequent phases are cancelled. The
    /// stream ends with a `run_crashed` + `run_finished` pair.
    AbortRun,
    /// Request a snapshot of the run's current lifecycle state. Reply is
    /// a `state_snapshot` event enumerating started/finished phases and
    /// any active UI request. Useful for agents joining mid-stream or
    /// re-verifying state after a parse glitch.
    GetState,
}
