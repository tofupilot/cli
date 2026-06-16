use serde::{Deserialize, Serialize};
use specta::Type;

// ---------------------------------------------------------------------------
// Station events (CLI -> Dashboard via Centrifugo)
// ---------------------------------------------------------------------------

/// Events published by CLI to the station status channel.
#[derive(Debug, Serialize, Deserialize, Type, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StationEvent {
    // -- Station lifecycle --
    /// Hardware info, published on connect
    Hardware {
        installation_id: String,
        hostname: String,
        os: String,
        /// Platform identifier (os_arch) — linux_x86_64, linux_aarch64,
        /// macos_arm64, etc. Always sent.
        platform: String,
        mac_address: Option<String>,
        cli_version: String,
    },
    /// System telemetry, published periodically
    Telemetry {
        installation_id: String,
        cpu_percent: f32,
        memory_mb: f32,
        disk_free_mb: f32,
        temperature_c: Option<f32>,
    },

    // -- Deployment --
    /// Pull started for a procedure
    DeploymentPullStarted {
        installation_id: String,
        procedure_id: String,
        deployment_id: String,
    },
    /// Pull completed successfully
    DeploymentPullCompleted {
        installation_id: String,
        procedure_id: String,
        deployment_id: String,
        file_count: u32,
    },
    /// Pull failed
    DeploymentPullFailed {
        installation_id: String,
        procedure_id: String,
        deployment_id: String,
        error: String,
    },
    /// Stale deployment removed from station
    DeploymentRemoved {
        installation_id: String,
        procedure_id: String,
        deployment_id: String,
    },
    /// New deployment added to the station's local set. Emitted by
    /// the CLI's pull loop when a procedure appears in the active
    /// pull response that wasn't in the prior local list. Lets a
    /// live-connected operator-ui kiosk update its procedure picker
    /// without a reload — the alternative was a frozen `HelloPayload`
    /// snapshot that only refreshed on reconnect.
    DeploymentAdded {
        installation_id: String,
        procedure_id: String,
        procedure_name: String,
        deployment_id: String,
    },

    // -- Run execution --
    /// Run started with phase plan + slot plan.
    RunStarted {
        procedure_id: String,
        /// Human-readable procedure name (e.g. "Battery FVT"). Lets
        /// consumers render the run header without a reverse-lookup
        /// against a station-procedures list — older clients had no
        /// way to learn the name when the run's procedure wasn't in
        /// the locally-known set (e.g. a station running a procedure
        /// that hasn't been linked via `station_procedures` yet, or
        /// the kiosk SPA which only knows the in-flight run's id).
        /// Defaulted empty for back-compat with old emitters.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        procedure_name: String,
        /// Per-run identity, minted at `run::start()` (UUID). Stamped on
        /// every lifecycle event for this run (`RunStarted`, `RunComplete`,
        /// `RunCrashed`) so consumers can drop stale terminals from a
        /// prior run that race a fresh `RunStarted` — operator presses
        /// Stop then New Run, the cancelled run's `RunComplete(ABORTED)`
        /// can land after the new run's `RunStarted`. Aliased from the
        /// old `run_name` field for back-compat with older emitters.
        #[serde(alias = "run_name")]
        execution_id: String,
        phases: Vec<PhasePlan>,
        /// Declared slot ids (e.g. `"slot_0"`). Empty for single-slot runs
        /// where `slot_id` on phase events is `None`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        slots: Vec<String>,
        /// Plug definitions declared by the procedure. Lets consumers
        /// pre-seed their plug state instead of materializing entries
        /// on the first `plug_status` / `plug_log` event — avoids
        /// ghost entries when events arrive with mismatched keys or
        /// out of order. Empty for procedures that declare no plugs.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        plugs: Vec<PlugDefinition>,
        /// ISO 8601 timestamp at which the engine recorded the start.
        /// Lets UIs anchor their elapsed-time counter on the engine's
        /// clock instead of `Date.now()` at fold time, which drifts
        /// across a hydration replay.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        /// Optional run id assigned at start (cloud-sync stations
        /// pre-mint one). Lets UIs link to the dashboard while the
        /// run is still in flight; before this field shipped, the
        /// run_id only arrived on `RunComplete`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        /// Deployment this run executes, when the procedure came from
        /// a pulled deployment (None for local-path runs). Lets remote
        /// UIs resolve relative image paths in UI components against
        /// the deployment's stored files — the dashboard serves them
        /// from the artifact bucket keyed by this id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deployment_id: Option<String>,
        /// Unit metadata captured before the engine started. None
        /// for runs that prompt the operator at runtime — those
        /// arrive via `IdentifyRequest` / `IdentifyResolved`
        /// (`IdentifyResolved` always fires before `RunStarted` so
        /// the unit is also already on this field when the engine
        /// begins).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<UnitInfo>,
    },
    /// Phase execution began.
    PhaseStarted {
        phase_key: String,
        name: String,
        /// Slot this attempt belongs to. None for single-slot / shared
        /// lifecycle phases.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        /// 1-indexed attempt number. Second attempt = 2 (first retry).
        #[serde(default = "one")]
        attempt: u32,
        /// Lifecycle stage: `setup_all` / `setup_each` / `main` /
        /// `teardown_each` / `teardown_all`. UIs render setup/teardown
        /// stages with a small prefix; without this field they had to
        /// look up the stage via the `RunStarted.phases` plan, which
        /// breaks for late-discovered phases.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stage: Option<String>,
        /// ISO 8601 attempt-start timestamp. Mirrors
        /// `PhaseComplete.started_at` so live UIs can render an
        /// elapsed-time stopwatch from the engine's clock.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        /// Per-run identity from `RunStarted`. Lets reducers drop
        /// events from a cancelled prior run that share `phase_key`
        /// with the current run (same procedure → same phase keys).
        /// Optional for back-compat with older emitters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },
    /// A single log line from a running phase. Streams live, unlike
    /// `PhaseComplete.logs` which only ships the batch when the phase
    /// terminates. UIs prepend new lines to the active phase's log
    /// view so operators see long phases unfold without waiting for
    /// completion. Subset of `PhaseLogLine` with the phase context
    /// attached.
    PhaseLog {
        phase_key: String,
        /// 1-indexed attempt this line belongs to.
        #[serde(default = "one")]
        attempt: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        level: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        /// Source filename the log line came from, relative to the
        /// procedure dir. Optional — some emitters (plug log
        /// forwarding) don't carry source context.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        /// Per-run identity from `RunStarted`. Reducer drops cross-run leaks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },
    /// A measurement value landed mid-phase. Lets UIs render the
    /// row immediately rather than waiting for `PhaseComplete`.
    /// Updates are idempotent: a later event for the same
    /// `(phase_key, attempt, name)` replaces the prior value.
    MeasurementUpdate {
        phase_key: String,
        #[serde(default = "one")]
        attempt: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        measurement: RunMeasurement,
        /// Per-run identity from `RunStarted`. Reducer drops cross-run leaks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },
    /// A phase attached a file or data blob. Streams live for YAML
    /// procedures (engine emits at attach time); OpenHTF connector
    /// emits a batch right after each phase since OpenHTF has no
    /// public on-attach hook. Either way the event lands before the
    /// terminal `RunComplete`.
    AttachmentAdded {
        phase_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        name: String,
        /// Local filesystem path or storage URL. Optional because
        /// `attach.data()` may stage in memory before persisting.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mimetype: Option<String>,
        /// u32 caps at ~4 GB; large enough for any reasonable
        /// attachment, small enough to avoid BigInt on the TS side.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size_bytes: Option<u32>,
        /// Per-run identity from `RunStarted`. Reducer drops cross-run leaks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },
    /// Phase execution completed with results.
    PhaseComplete {
        phase_key: String,
        name: String,
        outcome: String,
        measurements: Vec<RunMeasurement>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        #[serde(default = "one")]
        attempt: u32,
        /// ISO 8601 timestamp when the phase attempt started.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at: Option<String>,
        /// ISO 8601 timestamp when the phase attempt ended.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ended_at: Option<String>,
        /// Duration in milliseconds (ended_at - started_at). May be absent
        /// when the engine couldn't parse one of the timestamps. u32 caps at
        /// ~49 days — plenty for a phase; avoids BigInt on the TS side.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u32>,
        /// Error diagnostic string, populated when the phase errored or a
        /// measurement validator failed decisively.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Captured stdout/stderr log lines for this attempt, in order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        logs: Vec<PhaseLogLine>,
        /// Per-run identity from `RunStarted`. Critical: a stray
        /// `PhaseComplete{outcome:FAIL}` from a cancelled prior run
        /// matching `phase_key` would otherwise stamp FAIL on the new
        /// run's same-named phase. Reducer drops cross-run leaks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },
    /// Run execution completed
    RunComplete {
        outcome: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        /// Matches the `execution_id` from `RunStarted`. Lets consumers
        /// drop a terminal event whose run is no longer the active one
        /// (e.g. a cancelled run's `RunComplete(ABORTED)` arriving after
        /// the operator already kicked off a fresh run). Optional for
        /// back-compat with older emitters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },
    /// Run uploaded to TofuPilot API. Emitted only after a successful
    /// `POST /api/v2/runs` — never on enqueue. Consumers can take this
    /// as the authoritative "run exists in the dashboard" signal and
    /// surface the dashboard link.
    RunUploaded {
        procedure_id: String,
        run_id: String,
        /// Absolute URL the operator can hit to view the run in the
        /// dashboard. Built by the CLI from its `base_url` so the
        /// client doesn't have to guess between cloud and self-hosted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dashboard_url: Option<String>,
    },

    // -- Upload queue --
    //
    // Emitted by the CLI's background queue drain so operator UIs can
    // surface what's still in flight, what failed, and why. Each event
    // is keyed by `queue_id` (`<procedure_id>_<unix_millis>`) so
    // consumers maintain a single map.
    /// Run was queued for upload. Emitted once at enqueue time —
    /// always the first event for a given `queue_id`.
    RunUploadQueued {
        queue_id: String,
        procedure_id: String,
        outcome: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        serial_number: Option<String>,
        attachment_count: u32,
        /// ISO-8601 wall-clock timestamp the run was enqueued.
        queued_at: String,
    },
    /// A new upload attempt is starting. Emitted before each retry so
    /// UIs can flip the row to "uploading" and show the attempt
    /// number.
    RunUploadStarted {
        queue_id: String,
        attempt: u32,
    },
    /// Upload finished successfully. Mirrors `RunUploaded` for the
    /// queue map; consumers may key off either.
    RunUploadSucceeded {
        queue_id: String,
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dashboard_url: Option<String>,
    },
    /// Upload attempt failed. Carries the classified error so UIs can
    /// distinguish "client error → don't retry" from "transient → will
    /// retry". `next_retry_at` is null for client errors (4xx) which
    /// the CLI parks until the operator manually retries.
    RunUploadFailed {
        queue_id: String,
        attempt: u32,
        /// One of `http_4xx` / `http_5xx` / `network` / `unknown`.
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        /// Server response body or transport-level error message,
        /// truncated to a sane length on the CLI side.
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_retry_at: Option<String>,
    },
    /// Entry removed from the queue. Either the operator dropped it
    /// (`reason = "manual"`) or the CLI gave up (`reason = "ttl"` /
    /// `reason = "invalid"`).
    RunUploadDropped {
        queue_id: String,
        reason: String,
    },
    /// A single attachment finished uploading to storage. Emitted
    /// per-attachment from the upload queue once the server has minted an
    /// `upload_id` and the bytes are in object storage. Lets a remote
    /// operator-UI (which only ever saw a station-disk `path` on
    /// `AttachmentAdded`) resolve the attachment to a fetchable URL
    /// (`/api/attachments/{upload_id}`) and swap its pending placeholder
    /// for the real image. Correlated to the originating `AttachmentAdded`
    /// by `(phase_key, name)`: the queue carries the phase key on each
    /// attachment so the join is unique even when two phases attach the
    /// same file name. `phase_key` may be empty for legacy queue entries
    /// persisted before it was carried — consumers fall back to name.
    AttachmentUploaded {
        run_id: String,
        /// Phase the attachment belongs to. Matches
        /// `AttachmentAdded.phase_key`. Empty for legacy queue entries.
        #[serde(default)]
        phase_key: String,
        /// Matches `AttachmentAdded.name` for the same attachment.
        name: String,
        /// Server-minted upload id; the operator-UI builds
        /// `/api/attachments/{upload_id}` from it.
        upload_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mimetype: Option<String>,
    },
    /// Run failed to start, or aborted mid-flight before reaching a normal
    /// `RunComplete`. UIs should land on a terminal "errored" view that
    /// surfaces `error` to the operator. Always followed by a `RunComplete`
    /// with outcome `"ERROR"` so reducers that only key off completeness
    /// still terminate cleanly; consumers that distinguish "load failure"
    /// from "phase failure" should prefer this event's `error_kind`.
    RunCrashed {
        /// Procedure that failed. Stamped so UIs subscribed before any
        /// `RunStarted` (e.g. dashboard tab opened during station boot)
        /// can attribute the error.
        procedure_id: String,
        /// Operator-facing diagnostic. Multi-line is allowed; the UI
        /// renders it as monospace below the outcome banner.
        error: String,
        /// Coarse taxonomy. Lets UIs colour / group the screen without
        /// pattern-matching on free-form text. Open-set: unrecognised
        /// values render as a generic error.
        ///
        /// Today's values:
        ///   * `load_error`      — YAML / Python load-time failure (parse,
        ///                         missing dep, validator).
        ///   * `init_error`      — Engine setup failure after load.
        ///   * `submit_error`    — Procedure submission rejected.
        ///   * `execution_error` — Engine crashed during execution.
        ///   * `subprocess_crash`— Python process died unexpectedly
        ///                         (OpenHTF connector path).
        error_kind: String,
        /// Matches the `execution_id` from `RunStarted`. Lets consumers
        /// drop crash events that race ahead of a freshly-started run.
        /// Optional for back-compat with older emitters and for crashes
        /// that happen before any `RunStarted` has been emitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },
    /// Execution progress snapshot
    RunProgress {
        completed: u32,
        failed: u32,
        running: u32,
        total: u32,
    },

    // -- Plugs --
    /// Plug lifecycle status change
    PlugStatus {
        plug_key: String,
        plug_name: String,
        stage: String,
        status: String,
        /// `"all"` or `"each"` — plug lifecycle scope. Defaults empty for
        /// pre-scope emitters.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        scope: String,
        /// Slot this plug transition applies to. None for scope = all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        /// Per-run identity from `RunStarted`. Reducer drops cross-run leaks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },
    /// Plug log line (stdout/stderr from plug lifecycle calls).
    PlugLog {
        plug_key: String,
        plug_name: String,
        level: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        /// Lifecycle stage at the time the line was emitted —
        /// `"setup"` / `"teardown"` / `"manual"`. None when the
        /// engine can't disambiguate (the plug subprocess streams
        /// across stages, so it's the consumer's job to bucket
        /// using the latest `plug_status.stage`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stage: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        /// Per-run identity from `RunStarted`. Reducer drops cross-run leaks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },

    // -- Operator UI --
    /// Operator UI prompt requested
    UiRequest {
        request_id: String,
        phase_key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        components: Option<Vec<UiComponent>>,
        requires_input: bool,
        /// Wall-clock the engine emitted this request (RFC 3339).
        /// Used by the UI to anchor the auto-submit countdown so a
        /// reconnect / hydration replay doesn't reset the window.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        /// Per-run identity from `RunStarted`. Reducer drops cross-run leaks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },
    /// Operator UI display update. Emitted when Python code mutates a
    /// running prompt's component values via `ui.<key> = value`. Reducer
    /// finds the active `UiRequest` by `phase_key` (+ `slot_id` when
    /// present) and applies `data` per `action`:
    ///   * `set_value` — `data` is `{ id, value }`; mutate that
    ///     component's runtime value.
    /// Older engines (pre-runtime-update support) don't emit this.
    /// `data` is opaque JSON so future actions (`add_component`,
    /// `add_log`) extend without a wire break.
    UiUpdate {
        phase_key: String,
        action: String,
        /// JSON-encoded payload. Shape depends on `action`. Optional so
        /// pre-payload engines that emitted `UiUpdate` without a body
        /// still deserialize.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        /// Worker job id. Lets multi-slot runs disambiguate when the
        /// same phase fans out across slots. Optional for legacy emits.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_id: Option<String>,
        /// Slot the update targets. Optional for single-slot runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        /// Per-run identity from `RunStarted`. Reducer drops cross-run leaks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },

    // -- Identify-unit lifecycle --
    //
    // Identity is run metadata, not a phase. These three events form
    // the single contract for "the unit is being / has been
    // identified", regardless of source:
    //
    //   * `IdentifyRequest`  — pre-run operator prompt opens. Only
    //                           emitted when an operator scan is
    //                           required (no `auto_identify`). UIs
    //                           render the form on receipt.
    //   * `IdentifyResolved` — the unit (or any subset of its fields)
    //                           became known. Fires from every source:
    //                           pre-run prompt response, `auto_identify`
    //                           default resolution (no preceding
    //                           `IdentifyRequest`), mid-run operator
    //                           prompt response, and mid-run Python
    //                           bound-measurement updates. Field-level
    //                           merge — non-null fields overwrite, sub-
    //                           units merge by key.
    //   * `IdentifyTimeout`  — the pre-run prompt expired before the
    //                           operator answered; the CLI cancels the
    //                           run.
    /// Pre-run identify-unit operator prompt. Receipt of this event is
    /// the unambiguous signal "the operator must scan the next unit"
    /// — there is no shape heuristic on components and no synthetic
    /// phase wrapping.
    IdentifyRequest {
        request_id: String,
        /// Procedure the engine is identifying for. Carried so a UI
        /// hydrating mid-identify (page refresh, late attach) can show
        /// the procedure name in its topbar without waiting for
        /// `RunStarted`. Optional for back-compat with older CLIs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        procedure_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        components: Option<Vec<UiComponent>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        /// Identity of the run that will start once the operator
        /// answers. Pre-allocated by the CLI at `run::start()` so the
        /// UI can correlate this prompt with the upcoming `RunStarted`.
        /// Reducer drops a stale `IdentifyResolved` from a cancelled
        /// prior run that would otherwise poison `pendingUnitRef`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },
    /// Unit identity (or a subset of its fields) became known. Fires
    /// pre-run AND mid-run; the UI does a field-level merge so a
    /// mid-run scan that only fills `sub_unit:wifi:serial_number`
    /// doesn't clobber the pre-run-set `serial_number`.
    IdentifyResolved {
        #[serde(skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
        unit: UnitInfo,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        /// Per-run identity. Reducer drops a stale resolution from a
        /// cancelled prior run that would otherwise seed
        /// `pendingUnitRef` with the wrong unit between New Run click
        /// and the upcoming `RunStarted` — uploading the new run with
        /// a foreign serial number.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },
    /// Pre-run identify prompt timed out.
    IdentifyTimeout {
        request_id: String,
        /// Per-run identity. Reducer drops cross-run leaks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_id: Option<String>,
    },

    // -- Config --
    /// Station config applied (or failed to apply)
    ConfigApplied {
        installation_id: String,
        key: String,
        value: String,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    // -- CLI self-update --
    /// CLI update started (downloading or applying)
    UpdateStarted {
        installation_id: String,
        from_version: String,
        to_version: String,
    },
    /// CLI update applied; published by the new process after re-exec
    UpdateApplied {
        installation_id: String,
        from_version: String,
        to_version: String,
    },
    /// CLI update failed
    UpdateFailed {
        installation_id: String,
        from_version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        to_version: Option<String>,
        error: String,
    },

    /// Server-published acknowledgement that an installation was logged
    /// out. Mirrors the `StationCommand::Logout` (CLI-bound) but on the
    /// status channel so dashboard tabs can refresh activity / setup
    /// state without polling. Reason mirrors `LogoutReason` literals.
    LoggedOut {
        installation_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// Collaborative presence: who's looking at what, and what (if anything)
    /// they're currently typing. Fired by every participant (CLI, each
    /// dashboard tab) on focus/blur/keystroke. Consumers keep a per-user
    /// map keyed by `user_id`; Centrifugo `leave` events drop entries so a
    /// closed tab stops showing up without needing an explicit "goodbye"
    /// event. Designed to extend to pointer / x-y cursor collaboration
    /// without another variant — add fields to `focus` / `draft`.
    Presence(PresencePayload),
}

fn one() -> u32 {
    1
}

/// Commands published by dashboard to the commands channel (Dashboard -> CLI).
#[derive(Debug, Serialize, Deserialize, Type, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StationCommand {
    Run {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        procedure_id: Option<String>,
        /// Pre-resolved unit data for the next run. When set, the CLI
        /// skips the identify-unit prompt entirely and jumps straight
        /// to phase execution with this unit. Used by the operator
        /// UI's "Run again" button on the outcome screen — same unit
        /// as the run that just finished, no re-scan needed. None on
        /// "New run" (and on any first-run command), which triggers
        /// the normal identify flow.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reuse_unit: Option<UnitInfo>,
        /// Email of the dashboard user who pressed Run from the web
        /// operator UI. Forwarded to `runs.create` v2 as `operated_by`
        /// so the resulting run is attributed to that user. None when
        /// the run is triggered from a kiosk-mode operator UI (no
        /// browser session) or from the CLI directly.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operated_by: Option<String>,
    },
    UiResponse {
        request_id: String,
        values: std::collections::HashMap<String, String>,
    },
    ConfigUpdate {
        key: String,
        value: String,
    },
    Pull {},
    /// Stop the in-flight run. The engine cancels the current
    /// phase, runs teardown, and emits a `RunComplete` with outcome
    /// `"ABORTED"`. UIs render the abort screen on receipt.
    Stop {
        /// Optional operator-provided reason for the abort, surfaced
        /// in the run timeline.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Force-kill the in-flight run. Skips teardown, SIGKILLs all
    /// workers and plug services in parallel, emits `RunComplete`
    /// with outcome `"ABORTED"`. Used as the escalation step when a
    /// `Stop` is hung in teardown. Sending `Kill` after `Stop` is
    /// idempotent — graceful shutdown is abandoned mid-flight.
    Kill {
        /// Optional operator-provided reason for the force-kill.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Skip the currently-running phase and continue with the next.
    /// Phase ends with outcome `"SKIP"`. No-op if no phase is
    /// running.
    SkipPhase {
        phase_key: String,
    },
    /// Force a retry of a phase that has terminated. Phase status
    /// flips back to running and a new attempt starts. Bounded by
    /// the procedure's retry limit; otherwise rejected by the
    /// engine with no event change.
    RetryPhase {
        phase_key: String,
    },
    /// Server-initiated logout: the CLI should clear credentials and exit.
    /// Used when a new installation replaces this one.
    Logout {
        /// Display-only reason, e.g. "replaced", "user". Optional so future
        /// server paths can publish a Logout without inventing a string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Optional installation id the command is intended for; CLIs ignore
        /// the command if it doesn't match their own installation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        installation_id: Option<String>,
    },
    /// Force-retry an upload that's parked in the queue (4xx errors
    /// don't auto-retry; transient errors with a future
    /// `next_retry_at` skip ahead). No-op if the queue id is unknown.
    QueueRetry {
        queue_id: String,
    },
    /// Drop a queued upload outright. Attachments are deleted from
    /// disk; the queue entry is gone.
    QueueDrop {
        queue_id: String,
    },
    /// Operator-initiated graceful shutdown of the CLI process.
    /// Cancels any active run, asks the OS supervisor (systemd /
    /// launchd) to stop the unit without disabling launch_on_boot,
    /// then exits. Next reboot brings the station back. No-op
    /// (clean process exit) when running outside a supervisor.
    Exit {},
}

/// Presence payload shared by `StationEvent::Presence` +
/// `StationCommand::Presence`. `seq` is monotonic per `user_id` so
/// consumers drop out-of-order deliveries. `updated_at` lets consumers
/// enforce a stale-lock timeout independent of wire clock skew — pick
/// the max of (own_now - updated_at) against the lock budget.
#[derive(Debug, Serialize, Deserialize, Type, Clone)]
pub struct PresencePayload {
    pub user_id: String,
    pub display_name: String,
    /// Stable hex color per user (e.g. `"#EA580C"`). Assigned by the
    /// publisher; receivers don't re-derive so badges stay consistent
    /// across participants.
    pub color: String,
    /// Which UI request + component the user is focused on. None means
    /// "no focus" (user clicked away, tab is background, etc.). A leaf
    /// is still distinct from the user being offline — Centrifugo
    /// `leave` drops the row entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<PresenceFocus>,
    /// Live draft for a text input. Soft lock: other participants
    /// shouldn't type into this component until either the owner
    /// submits (publishes a presence without `draft`) or the draft goes
    /// stale (see `updated_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<PresenceDraft>,
    /// Monotonic per-user sequence. Consumers keep the highest seen
    /// per `user_id` and discard smaller/equal ones, so an out-of-order
    /// WebSocket delivery doesn't flip focus state backward. `u32`
    /// rolls over at ~4B publishes per session — an always-typing user
    /// at 10 Hz would take 13 years; we'll accept the upper bound.
    pub seq: u32,
    /// Wall-clock seconds since epoch when the publisher built this
    /// payload. Consumers enforce the 5s stale-lock rule off their own
    /// clock against this stamp — the publisher's clock may be skewed
    /// but re-broadcasts bring it forward, so a long gap still looks
    /// stale locally even if the absolute timestamp is wrong. Seconds
    /// rather than ms to dodge the i32/u32 JS number ceiling cleanly
    /// through year 2106; 1s presence granularity is fine because the
    /// 5s stale budget swamps it.
    pub updated_at: u32,
}

#[derive(Debug, Serialize, Deserialize, Type, Clone)]
pub struct PresenceFocus {
    pub request_id: String,
    pub component_key: String,
}

#[derive(Debug, Serialize, Deserialize, Type, Clone)]
pub struct PresenceDraft {
    pub request_id: String,
    pub component_key: String,
    pub value: String,
    /// Caret position within `value` (UTF-16 code units to match
    /// browser text inputs). Optional for publishers that don't track
    /// caret — e.g. a future TUI without selection-aware input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_pos: Option<u32>,
}

// Legacy `fn one() -> u32` lives above in this module; nothing else
// to add for presence types.

// ---------------------------------------------------------------------------
// Shared sub-types
// ---------------------------------------------------------------------------

/// A planned phase (name known upfront, outcome unknown).
#[derive(Debug, Serialize, Deserialize, Type, Clone)]
pub struct PhasePlan {
    pub key: String,
    pub name: String,
    /// Stage scope — `"setup_all"`, `"setup_each"`, `"main"`, `"teardown_each"`,
    /// `"teardown_all"`. Empty for pre-scope emitters.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stage: String,
}

/// Plug definition advertised on `RunStarted.plugs`. Lets consumers
/// know which plugs the procedure declared before any `plug_status`
/// or `plug_log` event lands, so they can render an empty-state row
/// (`pending`) and never have to upsert a fresh entry on the fly.
#[derive(Debug, Serialize, Deserialize, Type, Clone)]
pub struct PlugDefinition {
    /// Stable key (e.g. `"dmm"`). Matches `plug_status.plug_key` and
    /// `plug_log.plug_key` for downstream identity.
    pub key: String,
    /// Display name as configured in the procedure.
    pub name: String,
    /// `"all"` | `"each"`. `"each"` plugs are instantiated per slot;
    /// `"all"` plugs once per run.
    pub scope: String,
}

/// Unit metadata captured before a run starts. Optional inside
/// `RunStarted.unit` because runs that prompt for identification at
/// runtime resolve the unit via the identify-unit `UiRequest`
/// instead. Field names mirror the dashboard's run table columns
/// so consumers can render without remapping.
#[derive(Debug, Serialize, Deserialize, Type, Clone)]
pub struct UnitInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_number: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_number: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_number: Option<String>,
    /// Sub-unit slots keyed by slot name. Empty when the procedure
    /// doesn't declare sub-units.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub sub_units: std::collections::HashMap<String, String>,
}

/// A single captured log line from a phase attempt.
#[derive(Debug, Serialize, Deserialize, Type, Clone)]
pub struct PhaseLogLine {
    pub level: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Source filename the log line originated from (relative to the
    /// procedure dir). Optional because some emitters — notably plug
    /// log forwards — don't carry source context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// A measurement result emitted with PhaseComplete.
#[derive(Debug, Serialize, Deserialize, Type, Clone)]
pub struct RunMeasurement {
    pub name: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    /// Per-validator results as evaluated at phase completion. Populated
    /// when the engine has validator metadata (YAML-defined measurements);
    /// left empty for ad-hoc measurements without declared validators.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validators: Vec<ValidatorResult>,
}

/// One validator outcome attached to a `RunMeasurement`. Mirrors the web
/// `ValidatorInfo` schema but kept minimal for the live wire payload.
#[derive(Debug, Serialize, Deserialize, Type, Clone)]
pub struct ValidatorResult {
    /// Human-readable expression for display (e.g. "x >= 3.0").
    pub expression: String,
    /// `"PASS"` / `"FAIL"` / `"UNSET"` — same vocabulary as the outer
    /// measurement outcome.
    pub outcome: String,
    /// If false, a FAIL here doesn't flip the measurement outcome
    /// (indicative/marginal validator). Null when the engine couldn't
    /// determine decisiveness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_decisive: Option<bool>,
}

/// Operator-UI component, single source of truth across the engine,
/// the Centrifugo wire, the local-websocket wire, and the operator-UI
/// React renderer.
///
/// Why `value` lives on the wire alongside `ui_update`: the engine
/// emits a `UiRequest` once at phase start (with `value: None`) and
/// then per-component `UiUpdate{set_value}` deltas as Python mutates
/// `ui.<key> = v`. Clients fold the deltas into their cached
/// `UiRequest` (see `applyUiUpdateToRequest` in
/// `packages/operator-ui/src/run-state.ts`), so a hydration replay
/// from any prefix of the event log renders the latest value. The
/// field is on the struct — not just on `UiUpdate` — so a snapshot
/// taken after a `set_value` can re-emit a self-sufficient
/// `UiRequest` without depending on `ui_update` event-order replay.
///
/// `is_input` is materialized at construction (see `UiComponent::new`)
/// so operator-UI consumers don't reimplement the input-vs-display
/// classification — adding a new `ComponentType` variant only updates
/// `ComponentType::is_input` and the rest of the stack reads it.
#[derive(Debug, Serialize, Deserialize, Type, Clone)]
pub struct UiComponent {
    pub key: String,
    #[serde(rename = "type")]
    pub component_type: ComponentType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub is_input: bool,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<UiOption>>,
    /// Default value declared in YAML. Pre-fills the input at request time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<ComponentValue>,
    /// Runtime value: `None` at phase start, populated by clients
    /// folding `UiUpdate{set_value}` deltas into their cached
    /// `UiRequest` so a hydration replay carries the latest value
    /// without depending on `ui_update` event-order replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<ComponentValue>,
    /// Bind component value to a measurement (`measurements.X`) or unit
    /// field (`unit.X`). Without this on the wire, the web submit can't
    /// build the `__bound_measurements__` payload the engine consumes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    // Image and image-grid (radio/checklist with image options) sizing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fit: Option<String>,
    // Text input validation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    // Textarea-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u32>,
    // Text input affordances.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
    #[serde(default = "default_trim")]
    pub trim: bool,
    // Text styling (display components).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<TextSize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<TextColor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font: Option<FontFamily>,
}

fn default_trim() -> bool { true }

impl UiComponent {
    /// Construct a component with `is_input` derived from the type.
    /// All optional fields default to `None` / sensible defaults; use
    /// struct-update syntax (`UiComponent { key: …, ..UiComponent::new(t) }`)
    /// or set fields after the call.
    pub fn new(component_type: ComponentType) -> Self {
        Self {
            key: String::new(),
            component_type,
            label: None,
            is_input: component_type.is_input(),
            required: true,
            description: None,
            placeholder: None,
            options: None,
            default_value: None,
            value: None,
            bind: None,
            min: None,
            max: None,
            step: None,
            columns: None,
            width: None,
            height: None,
            aspect: None,
            fit: None,
            min_length: None,
            max_length: None,
            pattern: None,
            rows: None,
            prefix: None,
            suffix: None,
            trim: true,
            size: None,
            color: None,
            font: None,
        }
    }
}

/// Type of UI component. Wire format is snake_case (`text_input`,
/// `number_input`, …). Adding a variant here forces a recompile of
/// every consumer that matches on it.
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComponentType {
    // Input components (require user interaction)
    TextInput,
    NumberInput,
    Switch,
    Textarea,
    Radio,
    Select,
    Multiselect,
    Checklist,
    Slider,

    // Display components (output only)
    Text,
    Image,
    Progress,
}

impl ComponentType {
    /// True for component types that require operator interaction.
    /// Source of truth for the `is_input` field on `UiComponent` and
    /// for the operator-UI's input-vs-display filter.
    pub const fn is_input(self) -> bool {
        matches!(
            self,
            ComponentType::TextInput
                | ComponentType::NumberInput
                | ComponentType::Textarea
                | ComponentType::Radio
                | ComponentType::Select
                | ComponentType::Multiselect
                | ComponentType::Checklist
                | ComponentType::Switch
                | ComponentType::Slider
        )
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            ComponentType::TextInput => "text_input",
            ComponentType::NumberInput => "number_input",
            ComponentType::Textarea => "textarea",
            ComponentType::Radio => "radio",
            ComponentType::Select => "select",
            ComponentType::Multiselect => "multiselect",
            ComponentType::Checklist => "checklist",
            ComponentType::Switch => "switch",
            ComponentType::Slider => "slider",
            ComponentType::Text => "text",
            ComponentType::Image => "image",
            ComponentType::Progress => "progress",
        }
    }
}

/// Runtime value carried by a `UiComponent`. Untagged so the wire
/// matches the JSON literal shape the operator-UI submits and Python
/// `ui.<key> = v` produces.
#[derive(Debug, Serialize, Deserialize, Type, Clone, PartialEq)]
#[serde(untagged)]
pub enum ComponentValue {
    Boolean(bool),
    Number(f64),
    String(String),
    Array(Vec<String>),
}

/// Text size token. Wire format mirrors Tailwind's text-size scale so
/// a YAML literal like `size: 2xl` round-trips end-to-end.
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
pub enum TextSize {
    #[serde(rename = "xs")]
    Xs,
    #[serde(rename = "sm")]
    Sm,
    #[serde(rename = "base")]
    Base,
    #[serde(rename = "lg")]
    Lg,
    #[serde(rename = "xl")]
    Xl,
    #[serde(rename = "2xl")]
    Xl2,
    #[serde(rename = "3xl")]
    Xl3,
    #[serde(rename = "4xl")]
    Xl4,
}

#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TextColor {
    Default,
    Zinc,
    Red,
    Orange,
    Yellow,
    Green,
    Lime,
    Sky,
    Blue,
    Violet,
    Purple,
    Pink,
}

#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FontFamily {
    Default,
    Monospace,
}

#[derive(Debug, Serialize, Deserialize, Type, Clone)]
pub struct UiOption {
    pub label: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

/// Operator-runtime screen states. Both the web operator UI (React)
/// and the CLI kiosk TUI (Ratatui) derive their current screen from
/// the same precedence ladder against `(runState, identifyRequest,
/// committedProcedureId, unlinkedProcedureName, focusedPhase)`.
/// Promoting the enum into the shared protocol crate keeps the two
/// runtimes in lockstep — adding a new screen state here forces the
/// TS reducer and the Rust TUI's render branch to acknowledge it.
///
/// Precedence (highest first):
///   1. `ProcedureUnlinked` — committed procedure was removed mid-flow.
///   2. `Idle` — no commit; show picker.
///   3. `IdentifyUnit` — pre-run operator identify prompt active.
///   4. `Starting` — `runState.executionId == "pending"` (handleRun seed
///      before `RunStarted` lands).
///   5. `Outcome` — `runState.outcome` set (terminal screen).
///   6. `Running` — `runState` + a focused phase (active or last
///      completed).
///   7. `Waiting` — `runState` exists but no focused phase yet.
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Screen {
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "starting")]
    Starting,
    #[serde(rename = "identify-unit")]
    IdentifyUnit,
    #[serde(rename = "waiting")]
    Waiting,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "outcome")]
    Outcome,
    #[serde(rename = "procedure-unlinked")]
    ProcedureUnlinked,
}

/// Why an upload attempt failed. Drives operator-UI bucketing of the
/// queue row's status — `Http4xx` is parked (won't auto-retry), the
/// rest are transient with a `next_retry_at`. Wire format is
/// snake_case lower (`http_4xx` etc.) to match the existing `kind`
/// strings producers ship.
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
pub enum RunUploadFailedKind {
    #[serde(rename = "http_4xx")]
    Http4xx,
    #[serde(rename = "http_5xx")]
    Http5xx,
    #[serde(rename = "network")]
    Network,
    #[serde(rename = "unknown")]
    Unknown,
}

/// Why a queue entry was dropped. `Manual` means the operator clicked
/// Drop. `Ttl` / `Invalid` are reserved (CLI doesn't auto-drop today)
/// — kept on the wire so future enforcement doesn't need a protocol
/// bump. Wire format mirrors existing string literals.
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RunUploadDroppedReason {
    Manual,
    Ttl,
    Invalid,
}

/// Why a `Logout` was issued. `User` = operator-initiated from the
/// dashboard. `Replaced` = a fresh install took over the
/// installation_id. `Uninstalled` = backend marked the station
/// uninstalled. Wire format is snake_case to match existing strings.
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogoutReason {
    User,
    Replaced,
    Uninstalled,
}

/// Phase lifecycle stage. Wire format mirrors existing string
/// literals so the typed enum can replace `String` in producers
/// over time without a breaking wire change.
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStage {
    SetupAll,
    SetupEach,
    Main,
    TeardownEach,
    TeardownAll,
}

/// Plug lifecycle stage scope. `Setup` and `Teardown` mirror the
/// engine's plug lifecycle; `Manual` is for plugs invoked outside
/// the standard setup/teardown lattice.
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlugStage {
    Setup,
    Teardown,
    Manual,
}

/// Plug lifecycle status. Mirrors the engine's `PlugStatusValue`
/// plus a `Pending` placeholder consumers use until the first
/// `plug_status` event lands.
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlugLifecycleStatus {
    Pending,
    Idle,
    Initializing,
    Active,
    Destructing,
    Error,
    Skipped,
}

/// Measurement validator outcome. Mirrors the engine's per-validator
/// results and matches the outer `RunMeasurement.outcome` vocabulary.
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum MeasurementOutcome {
    Pass,
    Fail,
    Unset,
}

/// Run-level terminal outcome. `Skip` is reserved for procedures that
/// return early without phase failures; `Error` is engine-level
/// failure (load, init, subprocess crash); `Aborted` is operator-
/// initiated `Stop` / `Kill`.
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum RunOutcome {
    Pass,
    Fail,
    Error,
    Aborted,
    Skip,
}

/// Mid-prompt mutation action. Today only `set_value` is implemented;
/// the open-set was leaving room for `add_component` / `add_log`. Kept
/// as an enum so a producer typo fails to deserialize (and an unknown
/// future variant from a newer CLI is rejected by older operator UIs).
#[derive(Debug, Serialize, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiUpdateAction {
    SetValue,
}

/// Sentinel `executionId` for the operator-UI's pending-seed RunState.
/// Mirror of `PENDING_EXECUTION_ID` in
/// `packages/operator-ui/src/run-state.ts`. Both runtimes treat events
/// stamped against this id as "accepted, not stale" — see
/// `is_stale_for_execution`. Never appears on the wire; lives in the
/// shared crate so a Rust-side reducer can apply the same gate.
pub const PENDING_EXECUTION_ID: &str = "pending";

impl StationEvent {
    /// Per-run identity stamped on this event, when it carries one.
    /// Returns `None` for station-lifecycle events (Hardware,
    /// Telemetry, Deployment*, Update*, Config*, Presence) and for
    /// older emitters that predate the `execution_id` field.
    pub fn execution_id(&self) -> Option<&str> {
        match self {
            Self::RunStarted { execution_id, .. } => Some(execution_id.as_str()),
            Self::PhaseStarted { execution_id, .. }
            | Self::PhaseLog { execution_id, .. }
            | Self::MeasurementUpdate { execution_id, .. }
            | Self::AttachmentAdded { execution_id, .. }
            | Self::PhaseComplete { execution_id, .. }
            | Self::RunComplete { execution_id, .. }
            | Self::PlugStatus { execution_id, .. }
            | Self::PlugLog { execution_id, .. }
            | Self::UiRequest { execution_id, .. }
            | Self::UiUpdate { execution_id, .. }
            | Self::IdentifyRequest { execution_id, .. }
            | Self::IdentifyResolved { execution_id, .. }
            | Self::IdentifyTimeout { execution_id, .. }
            | Self::RunCrashed { execution_id, .. } => execution_id.as_deref(),
            _ => None,
        }
    }
}

impl UnitInfo {
    /// Field-level merge: non-null fields on `update` overwrite, sub-units
    /// merge by key. Mirrors the wire contract documented at
    /// `IdentifyResolved` and the TS `mergeUnit` helper. Promoted to
    /// the protocol crate so a Rust-side reducer can apply identical
    /// semantics — previously the contract lived only in TS.
    pub fn merge(&mut self, update: &UnitInfo) {
        if update.serial_number.is_some() {
            self.serial_number = update.serial_number.clone();
        }
        if update.part_number.is_some() {
            self.part_number = update.part_number.clone();
        }
        if update.revision_number.is_some() {
            self.revision_number = update.revision_number.clone();
        }
        if update.batch_number.is_some() {
            self.batch_number = update.batch_number.clone();
        }
        for (k, v) in &update.sub_units {
            self.sub_units.insert(k.clone(), v.clone());
        }
    }
}

/// Inputs the `Screen` derivation reads. Decoupled from any concrete
/// `RunState` shape so both the TS reducer and a Rust TUI can build
/// it from their own structures and call the shared derivation.
#[derive(Debug, Clone, Copy)]
pub struct ScreenInputs<'a> {
    /// `true` when the operator is committed to a procedure (clicked
    /// Run, mid-identify, or hydrated mid-run).
    pub committed: bool,
    /// `true` when an identify-unit prompt is in flight.
    pub identify_active: bool,
    /// `true` when a `RunState` exists and its `executionId` equals
    /// `PENDING_EXECUTION_ID` — the synchronous seed planted on
    /// handleRun before the engine's `RunStarted` arrives.
    pub run_pending: bool,
    /// `true` when a `RunState` exists at all (live, terminal, or
    /// pending). Mirrors `runState != null`.
    pub run_active: bool,
    /// `true` when the active run carries a non-null `outcome` (PASS,
    /// FAIL, ERROR, ABORTED, SKIP).
    pub run_terminal: bool,
    /// `true` when a focused phase exists for the running screen.
    pub focused_phase: bool,
    /// `true` when the committed procedure was unlinked mid-flow.
    pub unlinked: bool,
    /// Phantom lifetime so callers can borrow without forcing
    /// `'static`. Not currently read.
    pub _phantom: std::marker::PhantomData<&'a ()>,
}

/// Derive the operator-UI screen from the runtime inputs. Single
/// source of truth for the precedence ladder — both the TS hook
/// (`use-operator-ui-state.ts::screen`) and the Rust TUI must call
/// this to stay in lockstep on which screen to render.
pub fn derive_screen(inputs: ScreenInputs<'_>) -> Screen {
    if inputs.unlinked {
        return Screen::ProcedureUnlinked;
    }
    if !inputs.committed {
        return Screen::Idle;
    }
    if inputs.identify_active {
        return Screen::IdentifyUnit;
    }
    if inputs.run_pending {
        return Screen::Starting;
    }
    if inputs.run_terminal {
        return Screen::Outcome;
    }
    if inputs.run_active && inputs.focused_phase {
        return Screen::Running;
    }
    if inputs.run_active {
        return Screen::Waiting;
    }
    Screen::Starting
}

/// Cross-run gate predicate. Returns `true` when an event's
/// `execution_id` doesn't match the active run's id and both are
/// known — caller drops the event to avoid corrupting the current
/// run's state with leftovers from a cancelled prior run. Returns
/// `false` (event is OK) when:
///   * The active id is missing or `PENDING_EXECUTION_ID` (pre-run
///     seed) — every event is accepted into the gap.
///   * The event has no `execution_id` (older emitters /
///     station-lifecycle events).
///   * The two ids match.
///
/// Mirror of `isStaleForExecution` in
/// `packages/operator-ui/src/run-state.ts`. Same semantics; promoted
/// here so a Rust reducer can apply the gate without re-deriving it.
pub fn is_stale_for_execution(active: Option<&str>, event: Option<&str>) -> bool {
    let Some(active) = active else { return false };
    if active.is_empty() || active == PENDING_EXECUTION_ID {
        return false;
    }
    let Some(event) = event else { return false };
    event != active
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Screen serialisation must match the TS reducer's string literals
    /// 1:1. The TS side imports `Screen` from
    /// `packages/shared/types/generated/station-protocol.ts`
    /// (regenerated by `cargo run --bin export-types`); a rename here
    /// without a regen + re-deploy of the operator UI would split the
    /// runtimes.
    #[test]
    fn is_stale_drops_cross_run_event() {
        assert!(is_stale_for_execution(Some("a"), Some("b")));
    }

    #[test]
    fn is_stale_accepts_match() {
        assert!(!is_stale_for_execution(Some("a"), Some("a")));
    }

    #[test]
    fn is_stale_accepts_pending_seed() {
        assert!(!is_stale_for_execution(Some(PENDING_EXECUTION_ID), Some("anything")));
    }

    #[test]
    fn is_stale_accepts_no_active() {
        assert!(!is_stale_for_execution(None, Some("anything")));
    }

    #[test]
    fn is_stale_accepts_eventless() {
        assert!(!is_stale_for_execution(Some("a"), None));
    }

    #[test]
    fn execution_id_extracts_from_run_started() {
        let ev = StationEvent::RunStarted {
            procedure_id: "p".into(),
            procedure_name: String::new(),
            execution_id: "e".into(),
            phases: vec![],
            slots: vec![],
            plugs: vec![],
            timestamp: None,
            run_id: None,
            deployment_id: None,
            unit: None,
        };
        assert_eq!(ev.execution_id(), Some("e"));
    }

    #[test]
    fn execution_id_none_for_lifecycle() {
        let ev = StationEvent::Hardware {
            installation_id: "i".into(),
            hostname: "h".into(),
            os: "o".into(),
            platform: "p".into(),
            mac_address: None,
            cli_version: "v".into(),
        };
        assert_eq!(ev.execution_id(), None);
    }

    #[test]
    fn merge_overwrites_non_null_fields() {
        let mut base = UnitInfo {
            serial_number: Some("SN1".into()),
            part_number: Some("P1".into()),
            revision_number: None,
            batch_number: None,
            sub_units: Default::default(),
        };
        let update = UnitInfo {
            serial_number: None,
            part_number: Some("P2".into()),
            revision_number: Some("R1".into()),
            batch_number: None,
            sub_units: Default::default(),
        };
        base.merge(&update);
        assert_eq!(base.serial_number.as_deref(), Some("SN1"));
        assert_eq!(base.part_number.as_deref(), Some("P2"));
        assert_eq!(base.revision_number.as_deref(), Some("R1"));
        assert_eq!(base.batch_number, None);
    }

    fn inputs() -> ScreenInputs<'static> {
        ScreenInputs {
            committed: false,
            identify_active: false,
            run_pending: false,
            run_active: false,
            run_terminal: false,
            focused_phase: false,
            unlinked: false,
            _phantom: std::marker::PhantomData,
        }
    }

    #[test]
    fn screen_idle_default() {
        assert_eq!(derive_screen(inputs()), Screen::Idle);
    }

    #[test]
    fn screen_unlinked_overrides() {
        let mut i = inputs();
        i.unlinked = true;
        i.run_active = true;
        i.run_terminal = true;
        assert_eq!(derive_screen(i), Screen::ProcedureUnlinked);
    }

    #[test]
    fn screen_identify_beats_pending() {
        let mut i = inputs();
        i.committed = true;
        i.identify_active = true;
        i.run_pending = true;
        assert_eq!(derive_screen(i), Screen::IdentifyUnit);
    }

    #[test]
    fn screen_outcome_when_terminal() {
        let mut i = inputs();
        i.committed = true;
        i.run_active = true;
        i.run_terminal = true;
        assert_eq!(derive_screen(i), Screen::Outcome);
    }

    #[test]
    fn screen_running_when_focused_phase() {
        let mut i = inputs();
        i.committed = true;
        i.run_active = true;
        i.focused_phase = true;
        assert_eq!(derive_screen(i), Screen::Running);
    }

    #[test]
    fn screen_waiting_when_active_no_focus() {
        let mut i = inputs();
        i.committed = true;
        i.run_active = true;
        assert_eq!(derive_screen(i), Screen::Waiting);
    }

    #[test]
    fn screen_wire_format() {
        let cases = [
            (Screen::Idle, "\"idle\""),
            (Screen::Starting, "\"starting\""),
            (Screen::IdentifyUnit, "\"identify-unit\""),
            (Screen::Waiting, "\"waiting\""),
            (Screen::Running, "\"running\""),
            (Screen::Outcome, "\"outcome\""),
            (Screen::ProcedureUnlinked, "\"procedure-unlinked\""),
        ];
        for (screen, expected) in cases {
            let actual = serde_json::to_string(&screen).expect("serialize");
            assert_eq!(actual, expected, "wire format for {screen:?}");
            let round: Screen = serde_json::from_str(&actual).expect("deserialize");
            assert_eq!(round, screen, "round-trip for {screen:?}");
        }
    }
}
