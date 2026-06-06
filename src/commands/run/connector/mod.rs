//! Bridges between the Rust run loop and Python test frameworks.
//!
//! Detects the framework in a procedure directory (OpenHTF, pytest, or Robot
//! Framework), spawns the Python connector, and parses its NDJSON event stream
//! into typed events the run loop can route. Detection prefers a bundled
//! `manifest.json` and falls back to scanning dependency manifests.

use std::path::Path;
use std::process::Stdio;

use chrono::{DateTime, Utc};
use command_group::AsyncCommandGroup;
use execution_engine::ui::{
    ComponentType, ComponentValue, UiComponent, UiConfig, UiRequestData, UI_RESPONSE_CHANNELS,
};
use station_protocol::{PhasePlan, RunMeasurement, StationEvent};
use tofupilot_sdk::types::*;
// The SDK derives these outcome enum names from the alphabetically-first
// endpoint that uses the shape; adding logs/phases shifted them off `RunGet*`.
// Alias back to the names this crate uses so the references stay stable.
use tofupilot_sdk::types::{
    LogGetOutcome as RunGetOutcome, PhaseGetOutcome as RunGetPhasesOutcome,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

use super::agent_proto::{AgentProtoCtx, AgentUiComponent, CliEvent};
use super::queue::{upload_queued_run, QueuedAttachment, QueuedRun};
use crate::commands::auth::credentials::Credentials;
use crate::commands::db;

mod events;
mod pytest;
mod robot;
use events::PythonEvent;

pub use pytest::{has_pytest, run_pytest};
pub use robot::{has_robot, run_robot};

const OPENHTF_CONNECTOR: &str = include_str!("openhtf.py");

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

pub fn has_openhtf(dir: &Path) -> bool {
    // Bundled deployments carry a manifest.json that records the framework
    // at build time. Prefer that when present — it's authoritative and
    // survives bundle stripping. Local development paths (`tofupilot run`
    // against a source tree) won't have this file and fall through to
    // dep-manifest scanning.
    if let Ok(content) = std::fs::read_to_string(dir.join("manifest.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
            if v.get("framework").and_then(|f| f.as_str()) == Some("openhtf") {
                return true;
            }
        }
    }
    for file in [
        "pyproject.toml",
        "requirements.txt",
        "uv.lock",
        "pylock.toml",
    ] {
        if let Ok(content) = std::fs::read_to_string(dir.join(file)) {
            let lower = content.to_lowercase();
            if contains_openhtf_requirement(&lower) {
                return true;
            }
        }
    }
    false
}

/// True if the dependency manifest content mentions an `openhtf` requirement
/// as a package name (not, say, a path containing the substring). Matches
/// cases like `openhtf`, `openhtf>=1.6`, `"openhtf>=1.6"`, `openhtf = "^1.6"`,
/// and `[[package]] name = "openhtf"`. Excludes things that happen to contain
/// the substring such as `/my-openhtf-fork/...` or `openhtfx`.
fn contains_openhtf_requirement(lower: &str) -> bool {
    // Split on anything that isn't a package-name character and scan tokens.
    let bytes = lower.as_bytes().iter().enumerate().peekable();
    let needle = b"openhtf";
    let n = needle.len();
    for (i, _) in bytes {
        if i + n > lower.len() {
            break;
        }
        if lower.as_bytes()[i..i + n] != *needle {
            continue;
        }
        // Char before must not be a package-name char (letter/digit/_/-).
        let before_ok = i == 0 || !is_pkgname_char(lower.as_bytes()[i - 1]);
        // Char after must not extend the identifier (so we reject `openhtfx`).
        let after_ok = i + n >= lower.len() || !is_pkgname_char(lower.as_bytes()[i + n]);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn is_pkgname_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-')
}

/// Identify-unit fields resolved by the framework's `identify(...)` step
/// before any phase runs. Mirrored into `RunCreateRequest` by
/// `build_request` and stamped on `StationEvent::RunStarted.unit`. The
/// framework's `UnitInfo` is the source of truth; this struct is just
/// the connector-side projection consumed by `build_request`.
#[derive(Debug, Default, Clone)]
struct ResolvedUnit {
    serial_number: Option<String>,
    part_number: Option<String>,
    revision_number: Option<String>,
    batch_number: Option<String>,
    /// Map of sub-unit label → serial_number, mirroring
    /// `execution_engine::unit::UnitInfo::sub_units`. v2 API accepts a
    /// flat list of serial numbers; we collect `.values()` at request
    /// build time to avoid recomputing.
    sub_units: std::collections::HashMap<String, String>,
}

/// Build a `UnitConfig` from the OpenHTF `htf.Test(...)` kwargs we
/// receive on `TestStart`. Empty-string kwargs are mapped to `None`
/// defaults so `auto_identify`-mode validation can still flag missing
/// values; `serial_number` and `part_number` always have a slot so
/// the prompt path emits the canonical components.
fn build_unit_config_from_kwargs(
    kwargs: &std::collections::HashMap<String, String>,
    auto_identify: bool,
) -> execution_engine::procedure::UnitConfig {
    use execution_engine::procedure::UnitFieldConfig;
    let field = |key: &str| {
        kwargs
            .get(key)
            .filter(|v| !v.trim().is_empty())
            .map(|v| UnitFieldConfig {
                default_value: Some(v.clone()),
                ..UnitFieldConfig::default()
            })
    };
    execution_engine::procedure::UnitConfig {
        auto_identify,
        // Always declare serial_number / part_number slots so the
        // prompt path emits them; the default_value is only set when
        // the user supplied a kwarg.
        serial_number: Some(field("serial_number").unwrap_or_default()),
        part_number: Some(field("part_number").unwrap_or_default()),
        revision_number: field("revision_number"),
        batch_number: field("batch_number"),
        sub_units: None,
    }
}

/// Convert a framework `UnitInfo` to the station-protocol wire shape.
/// Mirror of the YAML path's helper in `engine.rs`.
fn unit_info_to_wire(info: &execution_engine::unit::UnitInfo) -> station_protocol::UnitInfo {
    station_protocol::UnitInfo {
        serial_number: info.serial_number.clone(),
        part_number: info.part_number.clone(),
        revision_number: info.revision_number.clone(),
        batch_number: info.batch_number.clone(),
        sub_units: info.sub_units.clone().unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

// Args mirror `run_test` (sibling YAML driver). Single caller; param-bag
// would just rename fields.
#[allow(clippy::too_many_arguments)]
pub async fn run_openhtf(
    python_path: &Path,
    entry_file: &Path,
    procedure_dir: &Path,
    procedure_id: &str,
    procedure_name: &str,
    execution_id: &str,
    creds: Option<&Credentials>,
    // When false, the run is local-only — skip the upload spawn even
    // if `creds` happen to be present. Local `tofupilot run /path` runs
    // shouldn't surface upload events; the operator just wants to test
    // their procedure end-to-end.
    upload: bool,
    json_mode: bool,
    event_tx: broadcast::Sender<StationEvent>,
    ui_tx: Option<mpsc::Sender<UiRequestData>>,
    agent: Option<AgentProtoCtx>,
    // Whether any operator surface can answer a unit-identify prompt.
    // False on a fully headless run, which makes `identify` fail fast
    // instead of hanging on a prompt nobody can answer.
    has_ui: bool,
    // When set, skip the operator identify prompt and reply with
    // `set_unit_resolved` to the Python connector directly using
    // these values. Drives the "Run again" UX on the operator-UI
    // outcome screen — same unit as the run that just finished, no
    // re-scan.
    reuse_unit: Option<station_protocol::UnitInfo>,
    // Email forwarded to `runs.create` as `operated_by`. Set when the
    // run was triggered from the web operator UI; None for kiosk and
    // CLI-driven runs.
    operated_by: Option<String>,
    // Single cancel surface. OpenHTF doesn't distinguish graceful from
    // forced — both collapse to `graceful_shutdown` (SIGTERM, 5s, then
    // SIGKILL on `command_group`'s process tree). Either Stop or Kill
    // on the watch transitions away from `None`, which trips the
    // shutdown path; subsequent escalation is a no-op (already tearing
    // down).
    mut cancel_rx: super::cancel::Receiver,
) -> i32 {
    let connector_path = procedure_dir.join(".tofupilot_openhtf.py");
    let _ = std::fs::remove_file(&connector_path);
    if let Err(e) = std::fs::write(&connector_path, OPENHTF_CONNECTOR) {
        crate::log::error(&format!("Failed to write connector: {e}"));
        return 1;
    }
    // RAII: clean up the ephemeral connector script on every exit path,
    // including panics and early returns. The explicit `remove_file` calls
    // below are now belt-and-braces only.
    let _script_guard = ConnectorScriptGuard::new(connector_path.clone());

    let queue_id = super::queue::new_queue_id(procedure_id);

    let mut cmd = super::python::build_command(
        python_path,
        &[connector_path.as_path(), entry_file],
        procedure_dir,
        &queue_id,
    );
    cmd.stdin(Stdio::piped());

    let mut child = match cmd.group().kill_on_drop(true).spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = super::python::spawn_error_message(python_path, &e);
            crate::log::error(&msg);
            super::emit::run_crashed(
                &event_tx,
                agent.as_ref(),
                procedure_id,
                execution_id,
                "spawn_failed",
                &msg,
                1,
            );
            // `_script_guard` (`ConnectorScriptGuard`) cleans the
            // connector script on every exit path via Drop, so the
            // previous explicit `remove_file` here was redundant.
            return 1;
        }
    };

    let inner = child.inner();
    let emit_capture_fail = |what: &str| {
        crate::log::error(&format!("Failed to capture {what}"));
        super::emit::run_crashed(
            &event_tx,
            agent.as_ref(),
            procedure_id,
            execution_id,
            "spawn_failed",
            &format!("Failed to capture {what} from Python child"),
            1,
        );
    };
    let stdout = match inner.stdout.take() {
        Some(s) => s,
        None => {
            emit_capture_fail("stdout");
            return 1;
        }
    };
    let stderr = match inner.stderr.take() {
        Some(s) => s,
        None => {
            emit_capture_fail("stderr");
            return 1;
        }
    };
    let stdin = match inner.stdin.take() {
        Some(s) => s,
        None => {
            emit_capture_fail("stdin");
            return 1;
        }
    };

    let is_json = json_mode;
    let tx = event_tx.clone();
    // Upload events go on the same broadcast so operator UIs see
    // queue progress on the wire they already subscribe to.
    let upload_bus = event_tx.clone();
    // Crash branch below also publishes on the broadcast; keep the
    // original `event_tx` alive so the move into the stdout pump
    // doesn't strand the post-loop emit.
    let crash_tx = event_tx;
    let pid = procedure_id.to_string();
    let pname = procedure_name.to_string();
    // Per-run identity, owned-by-spawn so every emit inside the stdout
    // pump (RunStarted, identify-fail RunCrashed/RunComplete, normal
    // RunComplete) carries the same id the outer cancel arm uses.
    let eid = execution_id.to_string();
    let agent_for_task = agent.clone();
    // Shared router: both phase_started (inside emit_phase_started) and
    // phase_finished (inside PhaseEnd handler) delegate here, matching the
    // YAML path. Keeps the two framework sinks provably in lockstep with
    // a single (send + enqueue) pair.
    let router =
        super::event_router::EventRouter::new(tx.clone(), agent.clone(), execution_id.to_string());
    let stdout_handle = tokio::spawn(async move {
        let mut stdin_writer = stdin;
        let mut phases = Vec::new();
        let mut test_end = None;
        let mut test_start = None;
        let mut attachments = Vec::new();
        // Identify-unit fields resolved by the connector (kwargs,
        // auto_identify defaults, or operator-supplied via prompt).
        // Forwarded to build_request alongside test_end.
        let mut unit_resolved = ResolvedUnit::default();
        // Set when identify-unit fails (operator dismissed prompt,
        // validation rejected the response, etc.). The pump breaks
        // out and the outer function aborts the run cleanly without
        // waiting for `test_end`.
        let mut identify_error: Option<String> = None;
        let mut phase_keys: Vec<(String, String)> = Vec::new(); // (key, name)
                                                                // Attempt counter keyed by phase name. OpenHTF re-runs a phase on
                                                                // PhaseResult.REPEAT or retry_limit; we keep the plan key stable and
                                                                // bump the `attempt` field on each run.
        let mut attempt_by_name: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        // (phase_key, attempt) pairs we have already emitted phase_started for,
        // so a prompt + phase_end for the same attempt don't duplicate.
        let mut started_attempts: std::collections::HashSet<(String, u32)> =
            std::collections::HashSet::new();
        let mut current_phase_key = String::new();

        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(typed) = PythonEvent::from_value(&event) else {
                eprintln!("[tofupilot] unparseable connector event: {line}");
                continue;
            };
            match typed {
                PythonEvent::BridgeReady => {}
                PythonEvent::Warning { message } => {
                    eprintln!("[tofupilot-connector] {message}");
                }
                PythonEvent::Unknown => {
                    let raw_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("?");
                    eprintln!(
                        "[tofupilot] unknown connector event type (rust lacks a variant): {raw_type}",
                    );
                    if let Some(ref agent) = agent_for_task {
                        // Cap the raw event at 4KB: a malformed/huge event
                        // payload shouldn't bloat the warning that reports
                        // it. Agents have the `type` string and know the
                        // event was dropped; they don't need the body.
                        const MAX_DETAIL_BYTES: usize = 4_096;
                        let serialized_size =
                            serde_json::to_vec(&event).map(|v| v.len()).unwrap_or(0);
                        let detail = if serialized_size <= MAX_DETAIL_BYTES {
                            event.clone()
                        } else {
                            serde_json::json!({
                                "truncated": true,
                                "original_size_bytes": serialized_size,
                                "type": raw_type,
                            })
                        };
                        agent.emitter.enqueue(CliEvent::InternalWarning {
                            kind: "unknown_python_event".into(),
                            message: format!(
                                "python connector emitted event type '{raw_type}' with no matching Rust variant (CLI/connector version mismatch)"
                            ),
                            detail: Some(detail),
                        });
                    }
                }
                PythonEvent::TestStart {
                    test_name,
                    phases: names,
                    identify,
                    auto_identify,
                    unit_kwargs,
                } => {
                    let plans: Vec<PhasePlan> = names
                        .iter()
                        .enumerate()
                        .map(|(i, n)| PhasePlan {
                            key: format!("{i}_{n}"),
                            name: n.clone(),
                            stage: String::new(),
                        })
                        .collect();
                    phase_keys = plans
                        .iter()
                        .map(|p| (p.key.clone(), p.name.clone()))
                        .collect();

                    // Identify-unit handshake: build a UnitConfig from
                    // the test's kwargs and call the framework. The
                    // resolved UnitInfo populates `unit_resolved` (for
                    // upload) AND `RunStarted.unit` (so dashboards see
                    // the unit immediately on auto_identify runs).
                    // Python is blocked on stdin waiting for our reply
                    // — write `set_unit_resolved` (even on skip /
                    // failure paths) so it doesn't deadlock. On
                    // failure we also broadcast `RunCrashed` and break
                    // out of the pump; the outer function then kills
                    // Python (via child Drop's `kill_on_drop`) so the
                    // run aborts cleanly instead of proceeding with
                    // an empty serial.
                    let resolved_wire = if identify {
                        // "Run again" path: the operator UI supplied the
                        // unit from the previous run. Skip the identify
                        // prompt entirely and resolve straight from the
                        // reused fields. We still emit `identify_resolved`
                        // on the wire so consumers see one resolution
                        // event per run regardless of whether the
                        // operator scanned or reused.
                        if let Some(reused) = reuse_unit.clone() {
                            let info = execution_engine::unit::UnitInfo {
                                serial_number: reused.serial_number.clone(),
                                part_number: reused.part_number.clone(),
                                revision_number: reused.revision_number.clone(),
                                batch_number: reused.batch_number.clone(),
                                sub_units: if reused.sub_units.is_empty() {
                                    None
                                } else {
                                    Some(reused.sub_units.clone())
                                },
                                status: String::new(),
                            };
                            // Mirror the YAML path's reuse-validation
                            // (engine.rs run_yaml_procedure): a stale
                            // or malformed reused unit must not bypass
                            // the procedure's declared regex /
                            // required-field constraints.
                            let cfg = build_unit_config_from_kwargs(&unit_kwargs, auto_identify);
                            if let Err(err) =
                                execution_engine::unit::validate_unit_info(&info, &Some(cfg))
                            {
                                let msg = format!("reuse_unit failed validation: {err}");
                                crate::log::error(&msg);
                                identify_error = Some(msg.clone());
                                // Outer cleanup at `if let Some(err) = identify_error`
                                // does the agent-protocol enqueue, so pass `None` here.
                                super::emit::run_crashed(
                                    &tx,
                                    None,
                                    &pid,
                                    &eid,
                                    "identify_unit_failed",
                                    &msg,
                                    1,
                                );
                                None
                            } else {
                                unit_resolved.serial_number = info.serial_number.clone();
                                unit_resolved.part_number = info.part_number.clone();
                                unit_resolved.revision_number = info.revision_number.clone();
                                unit_resolved.batch_number = info.batch_number.clone();
                                unit_resolved.sub_units =
                                    info.sub_units.clone().unwrap_or_default();
                                router.identify_resolved(None, &reused);
                                Some(info)
                            }
                        } else {
                            let cfg = build_unit_config_from_kwargs(&unit_kwargs, auto_identify);
                            let host = super::identify_host::CliIdentifyHost {
                                router: router.clone(),
                                ui_tx: ui_tx.clone(),
                                agent: agent_for_task.clone(),
                                procedure_id: pid.clone(),
                                has_ui,
                            };
                            match execution_engine::identify(&cfg, None, &host).await {
                                Ok(info) => {
                                    unit_resolved.serial_number = info.serial_number.clone();
                                    unit_resolved.part_number = info.part_number.clone();
                                    unit_resolved.revision_number = info.revision_number.clone();
                                    unit_resolved.batch_number = info.batch_number.clone();
                                    unit_resolved.sub_units =
                                        info.sub_units.clone().unwrap_or_default();
                                    // Wire the dedicated `identify_resolved`
                                    // event so the OpenHTF path matches the
                                    // YAML engine's contract: every unit
                                    // resolution (operator scan, auto-
                                    // identify defaults) publishes a single
                                    // resolution event consumers can fold
                                    // into RunState.unit field-level. Without
                                    // this, OpenHTF runs on the wire show a
                                    // bare IdentifyRequest followed by
                                    // RunStarted with no resolution event in
                                    // between — operator UI hydration breaks
                                    // and the agent-protocol audit can't pair
                                    // request→resolution.
                                    router.identify_resolved(None, &unit_info_to_wire(&info));
                                    Some(info)
                                }
                                Err(err) => {
                                    let msg = format!("{err}");
                                    crate::log::error(&format!("Identify-unit failed: {msg}",));
                                    identify_error = Some(msg.clone());
                                    // Outer cleanup at `if let Some(err) = identify_error`
                                    // does the agent-protocol enqueue, so pass `None` here.
                                    super::emit::run_crashed(
                                        &tx,
                                        None,
                                        &pid,
                                        &eid,
                                        "identify_unit_failed",
                                        &msg,
                                        1,
                                    );
                                    None
                                }
                            }
                        }
                    } else {
                        None
                    };

                    // Always reply (even when identify is off / fails)
                    // so Python's `_await_unit_resolution` unblocks.
                    // sub_units is sent as a list of serial numbers
                    // matching v2 API shape (the framework's HashMap
                    // label→serial form is collapsed).
                    let sub_units_list: Option<Vec<String>> = resolved_wire
                        .as_ref()
                        .and_then(|i| i.sub_units.as_ref())
                        .map(|m| m.values().cloned().collect::<Vec<_>>())
                        .filter(|v| !v.is_empty());
                    let reply = serde_json::json!({
                        "type": "set_unit_resolved",
                        "serial_number": resolved_wire.as_ref().and_then(|i| i.serial_number.clone()),
                        "part_number": resolved_wire.as_ref().and_then(|i| i.part_number.clone()),
                        "revision_number": resolved_wire.as_ref().and_then(|i| i.revision_number.clone()),
                        "batch_number": resolved_wire.as_ref().and_then(|i| i.batch_number.clone()),
                        "sub_units": sub_units_list,
                    });
                    let mut line = serde_json::to_string(&reply).unwrap_or_default();
                    line.push('\n');
                    if let Err(e) = stdin_writer.write_all(line.as_bytes()).await {
                        eprintln!("Failed to write set_unit_resolved: {e}");
                    }
                    let _ = stdin_writer.flush().await;

                    // Identify failed — break out of the pump now so
                    // the outer function can kill Python and surface
                    // the abort. We've already replied with nulls so
                    // Python won't deadlock; if it's still alive when
                    // `child` drops it'll be killed.
                    if identify_error.is_some() {
                        break;
                    }

                    // Drop the OpenHTF-supplied `test_name` from the wire id —
                    // it's a procedure-level label that repeats across runs of
                    // the same procedure, which defeats the per-run gating in
                    // operator-UI. Use the externally-minted `execution_id`.
                    let _ = test_name;
                    // KNOWN LIMITATION: OpenHTF doesn't expose plug
                    // declarations or slot fan-out the way the YAML
                    // engine does, so `plugs: []` and `slots: []`.
                    // Operator UI's plug panel sits empty for OpenHTF
                    // runs. Live `phase_log` / `measurement_update` /
                    // `attachment_added` are likewise unavailable for
                    // OpenHTF + pytest — those connectors batch logs
                    // and measurements onto `phase_complete`. Filed
                    // for follow-up.
                    let _ = tx.send(StationEvent::RunStarted {
                        procedure_id: pid.clone(),
                        procedure_name: pname.clone(),
                        execution_id: eid.clone(),
                        phases: plans,
                        slots: Vec::new(),
                        plugs: Vec::new(),
                        timestamp: Some(chrono::Utc::now().to_rfc3339()),
                        run_id: None,
                        unit: resolved_wire.as_ref().map(unit_info_to_wire),
                    });
                    if let Some(ref agent) = agent_for_task {
                        let phases: Vec<super::agent_proto::PhasePlanPayload> = phase_keys
                            .iter()
                            .map(|(k, n)| super::agent_proto::PhasePlanPayload {
                                key: k.clone(),
                                name: n.clone(),
                            })
                            .collect();
                        agent.emitter.enqueue(CliEvent::Plan { phases });
                    }
                    test_start = Some(event);
                }
                PythonEvent::PhaseBegin { name } => {
                    let phase_key = canonical_phase_key(&phase_keys, &name);
                    let attempt = attempt_by_name
                        .entry(name.clone())
                        .and_modify(|a| *a += 1)
                        .or_insert(1);
                    let attempt = *attempt;
                    emit_phase_started(&router, &mut started_attempts, &phase_key, &name, attempt);
                }
                PythonEvent::PhaseEnd {
                    name,
                    outcome,
                    retry_count,
                    start_time_millis,
                    end_time_millis,
                    error,
                } => {
                    let phase_key = canonical_phase_key(&phase_keys, &name);
                    // Each phase_end carries its own retry_count (OpenHTF
                    // emits them post-hoc from test_record, one per real
                    // attempt). Don't trust the mutable phase_begin
                    // counter, which can race ahead for batched ends.
                    let attempt = (retry_count.min(u32::MAX as u64) as u32).saturating_add(1);
                    let measurements = extract_run_measurements(&event);

                    // Safety net: if phase_begin was missed, emit here so
                    // the agent still sees a matched pair.
                    emit_phase_started(&router, &mut started_attempts, &phase_key, &name, attempt);

                    let duration_ms = match (start_time_millis, end_time_millis) {
                        (Some(s), Some(e)) if e >= s => Some((e - s) as u64),
                        _ => None,
                    };
                    router.phase_finished(super::event_router::PhaseFinished {
                        phase_key: phase_key.clone(),
                        phase_name: name.clone(),
                        outcome: outcome.clone(),
                        attempt,
                        slot_id: None,
                        error,
                        started_at: start_time_millis.map(super::time_fmt::from_millis),
                        ended_at: end_time_millis.map(super::time_fmt::from_millis),
                        duration_ms,
                        station_measurements: measurements,
                        station_logs: Vec::new(),
                    });

                    phases.push(event);
                }
                PythonEvent::TestEnd { outcome } => {
                    super::emit::run_complete(&tx, &outcome, &eid, None);
                    test_end = Some(event);
                }
                PythonEvent::Prompt {
                    prompt_id,
                    phase_name,
                    message,
                    text_input,
                    image_url,
                    timeout_s,
                } => {
                    // Python tells us which phase this prompt belongs to.
                    // Update current_phase_key so the `ui_request` and
                    // routing use the correct key.
                    if let Some(pname) = phase_name.as_deref() {
                        let key = canonical_phase_key(&phase_keys, pname);
                        let attempt = *attempt_by_name.entry(pname.to_string()).or_insert(1);
                        emit_phase_started(&router, &mut started_attempts, &key, pname, attempt);
                        current_phase_key = key;
                    }

                    let mut components = Vec::new();

                    if let Some(ref url) = image_url {
                        components.push(make_component(
                            "image",
                            ComponentType::Image,
                            false,
                            None,
                            Some(ComponentValue::String(url.clone())),
                        ));
                    }

                    if !message.is_empty() {
                        components.push(make_component(
                            "message",
                            ComponentType::Text,
                            false,
                            None,
                            Some(ComponentValue::String(message.clone())),
                        ));
                    }

                    if text_input {
                        components.push(make_component(
                            "response",
                            ComponentType::TextInput,
                            true,
                            Some("Response".to_string()),
                            None,
                        ));
                    } else {
                        // OpenHTF confirm-only prompt (no text input). We still need
                        // *some* input for the agent/operator to acknowledge — otherwise
                        // we'd emit a `ui_request` with no components, which no agent
                        // can sensibly answer. Expose it as an `acknowledge` switch.
                        components.push(make_component(
                            "acknowledge",
                            ComponentType::Switch,
                            true,
                            Some("Acknowledge".to_string()),
                            None,
                        ));
                    }

                    let request_data = UiRequestData {
                        request_id: prompt_id.clone(),
                        job_id: String::new(),
                        pipe_path: String::new(),
                        config: UiConfig {
                            components,
                            requires_input: Some(true),
                        },
                        phase_key: current_phase_key.clone(),
                        slot_id: None,
                    };

                    // Broadcast to Centrifugo for web dashboard / local UI.
                    router.ui_request(
                        &prompt_id,
                        &current_phase_key,
                        None,
                        &request_data.config.components,
                        true,
                    );

                    // Always register the oneshot in UI_RESPONSE_CHANNELS so the
                    // run-level `ui_response_rx` pump (station web operator path) can
                    // deliver the response, regardless of whether TUI or agent-protocol
                    // is also wired. TUI / agent branches do their extra delivery on top.
                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                    {
                        let mut channels = UI_RESPONSE_CHANNELS.lock().await;
                        channels.insert(prompt_id.clone(), resp_tx);
                    }

                    if let Some(ref ui) = ui_tx {
                        let _ = ui.try_send(request_data.clone());
                    }
                    if let Some(ref agent) = agent_for_task {
                        {
                            let mut guard = agent.pending.write().await;
                            guard.insert(
                                prompt_id.clone(),
                                current_phase_key.clone(),
                                request_data.config.components.clone(),
                            );
                        }
                        let payload_components: Vec<AgentUiComponent> = request_data
                            .config
                            .components
                            .iter()
                            .map(super::agent_proto::events::to_agent_ui_component)
                            .collect();
                        agent.emitter.enqueue(CliEvent::UiRequest {
                            request_id: prompt_id.clone(),
                            phase_key: current_phase_key.clone(),
                            phase_description: Some(message.clone()),
                            requires_input: true,
                            components: payload_components,
                        });
                    }

                    // Per-prompt `timeout_s` from Python wins over the agent's
                    // `--ui-timeout` flag; agent flag is the default when unset.
                    let agent_default_timeout = agent_for_task.as_ref().and_then(|a| a.ui_timeout);
                    let effective_timeout = timeout_s
                        .filter(|t| *t > 0.0)
                        .map(std::time::Duration::from_secs_f64)
                        .or(agent_default_timeout);

                    let raw_values: std::collections::HashMap<String, String> =
                        if let Some(timeout) = effective_timeout {
                            match tokio::time::timeout(timeout, resp_rx).await {
                                Ok(Ok(values)) => values,
                                Ok(Err(_)) => Default::default(),
                                Err(_) => {
                                    if let Some(ref agent) = agent_for_task {
                                        agent.emitter.enqueue(CliEvent::UiTimeout {
                                            request_id: prompt_id.clone(),
                                            phase_key: current_phase_key.clone(),
                                        });
                                        agent.pending.write().await.remove(&prompt_id);
                                    }
                                    super::ui_response::cancel(&prompt_id).await;
                                    Default::default()
                                }
                            }
                        } else {
                            resp_rx.await.unwrap_or_default()
                        };

                    // Native OpenHTF prompts collapse to {"response": "..."}
                    // matching `CliUserInput.prompt`'s contract.
                    let resp_json = serde_json::json!({"response": response_value(&raw_values)});
                    let mut line = serde_json::to_string(&resp_json).unwrap_or_default();
                    line.push('\n');
                    if let Err(e) = stdin_writer.write_all(line.as_bytes()).await {
                        eprintln!("Failed to write prompt response: {e}");
                    }
                    let _ = stdin_writer.flush().await;
                }
                PythonEvent::Attachment {
                    name,
                    path,
                    mimetype,
                    live,
                    phase_name,
                } => {
                    if live {
                        // Fire `attachment_added` on the agent wire as soon
                        // as the user code calls `test.attach(...)`. The
                        // post-hoc output_callback emits the same record
                        // again with `live=false` for the upload-queue
                        // path — guard there so we don't double-fire.
                        if let Some(ref agent) = agent_for_task {
                            let phase_key = phase_name
                                .as_deref()
                                .map(|n| canonical_phase_key(&phase_keys, n))
                                .filter(|k| !k.is_empty())
                                .unwrap_or_else(|| current_phase_key.clone());
                            agent.emitter.enqueue(CliEvent::AttachmentAdded {
                                phase_key,
                                slot_id: None,
                                name: name.clone(),
                                path: Some(path.clone()),
                                mimetype: Some(mimetype.clone()),
                            });
                        }
                    } else {
                        attachments.push(QueuedAttachment {
                            name,
                            path,
                            mimetype,
                        });
                    }
                }
                PythonEvent::Measurement {
                    name,
                    value,
                    phase_name,
                    unit,
                } => {
                    if let Some(ref agent) = agent_for_task {
                        let phase_key = phase_name
                            .as_deref()
                            .map(|n| canonical_phase_key(&phase_keys, n))
                            .filter(|k| !k.is_empty())
                            .unwrap_or_else(|| current_phase_key.clone());
                        agent.emitter.enqueue(CliEvent::MeasurementRecorded {
                            phase_key,
                            slot_id: None,
                            name,
                            value,
                            outcome: "unset".into(),
                            unit,
                        });
                    }
                }
                PythonEvent::PhaseLog {
                    level,
                    message,
                    timestamp,
                    phase_name,
                    file,
                    line,
                } => {
                    if let Some(ref agent) = agent_for_task {
                        let phase_key = phase_name
                            .as_deref()
                            .map(|n| canonical_phase_key(&phase_keys, n))
                            .filter(|k| !k.is_empty())
                            .unwrap_or_else(|| current_phase_key.clone());
                        agent.emitter.enqueue(CliEvent::PhaseLog {
                            phase_key,
                            slot_id: None,
                            level,
                            message,
                            timestamp,
                            file,
                            line,
                        });
                    }
                }
            }
            // Pass raw OpenHTF events through only when --json is on *and* the
            // agent protocol isn't emitting its own typed stream. Mixing both
            // would produce two incompatible event shapes on the same channel.
            if is_json && agent_for_task.is_none() {
                println!("{line}");
            }
        }

        (
            phases,
            test_end,
            test_start,
            attachments,
            unit_resolved,
            identify_error,
        )
    });

    let (stderr_handle, stderr_tail) = super::python::spawn_stderr_reader_with_capture(stderr);

    let mut cancelled_by_signal = false;
    // OpenHTF has no force/graceful distinction — both Stop and Kill
    // route to `graceful_shutdown` (SIGTERM → wait → SIGKILL via the
    // process group). Race ctrl_c, the station Stop, and the station
    // Kill: any of the three triggers the same teardown.
    let exit_code = tokio::select! {
        status = child.wait() => match status {
            Ok(s) => s.code().unwrap_or(1),
            Err(e) => { crate::log::error(&format!("Process error: {e}")); 1 }
        },
        _ = tokio::signal::ctrl_c() => {
            cancelled_by_signal = true;
            crate::log::info("Interrupted, killing procedure subprocess");
            super::python::graceful_shutdown(&mut child).await
        }
        signal = cancel_rx.wait_any() => {
            cancelled_by_signal = true;
            crate::log::info(&format!(
                "{} requested, killing procedure subprocess",
                match signal {
                    super::cancel::CancelSignal::Force => "Force-kill",
                    _ => "Stop",
                },
            ));
            super::python::graceful_shutdown(&mut child).await
        }
    };

    let _ = stderr_handle.await;
    // `_script_guard` Drop will remove the connector script.

    // When Python died from a signal mid-prompt, the stdout pump is
    // still parked in `resp_rx.await` waiting for a UI answer that
    // will never come — the oneshot sender lives in
    // UI_RESPONSE_CHANNELS until the UI replies. Abort the pump so
    // we don't hang the CLI on a dead prompt, then publish a
    // terminal RunComplete so operator-ui drops the prompt screen
    // instead of staying parked on identify-unit.
    let (phases, test_end, test_start, attachments, unit_resolved, identify_error) =
        if cancelled_by_signal {
            stdout_handle.abort();
            // Drain any pending UI prompt channels (identify-unit or
            // native `prompts.prompt`). Without this, the global
            // `UI_RESPONSE_CHANNELS` HashMap leaks oneshot senders for
            // every cancelled-mid-prompt run; the leak is per-process
            // but accumulates across multi-procedure CLI invocations.
            super::ui_response::cancel_all().await;
            super::emit::run_complete(&crash_tx, super::outcomes::ABORTED, execution_id, None);
            if let Some(ref agent) = agent {
                agent.emitter.enqueue(CliEvent::RunCrashed {
                    exit_code,
                    stderr_tail: String::new(),
                });
            }
            return exit_code;
        } else {
            match stdout_handle.await {
                Ok(r) => r,
                Err(_) => return exit_code,
            }
        };

    // Identify-unit failed: pump already broadcast `RunCrashed` +
    // `RunComplete(ERROR)` for operator-UI before breaking. Notify
    // the agent-protocol stream and return without trying to upload
    // (no `test_end`, no phases). Python's still alive — `child` /
    // `_script_guard` Drop will SIGKILL it via `kill_on_drop(true)`.
    if let Some(err) = identify_error {
        crate::log::error(&format!("Identify-unit failed: {err}"));
        if let Some(ref agent) = agent {
            agent.emitter.enqueue(CliEvent::RunCrashed {
                exit_code: 1,
                stderr_tail: err,
            });
        }
        return 1;
    }

    let test_end = match test_end {
        Some(e) => e,
        None => {
            // Python subprocess died before emitting `test_end`. Surface
            // the captured stderr tail to every consumer:
            //   * UIs (broadcast) — `RunCrashed` + synthetic
            //     `RunComplete(ERROR)` so the operator sees an error
            //     screen instead of a stalled "running" state.
            //   * agent protocol — its own `RunCrashed` for headless
            //     consumers.
            //   * stderr — human operators watching the terminal.
            let tail = stderr_tail.lock().await.clone();
            crate::log::error(&format!("Procedure subprocess crashed: {tail}"));
            super::emit::run_crashed(
                &crash_tx,
                agent.as_ref(),
                procedure_id,
                execution_id,
                "subprocess_crash",
                &tail,
                exit_code,
            );
            return exit_code;
        }
    };

    // OpenHTF's default runner exits 0 regardless of test outcome. Override
    // the exit code with the actual test outcome so the CLI's exit status and
    // the agent-protocol `run_finished` event carry the real pass/fail.
    let test_outcome = json_str(&test_end, "outcome")
        .unwrap_or(super::outcomes::ERROR)
        .to_string();
    let exit_code = match test_outcome.as_str() {
        super::outcomes::PASS => 0,
        _ => 1,
    };

    let request = match build_request(
        &test_end,
        &test_start,
        &phases,
        procedure_id,
        procedure_dir,
        &unit_resolved,
        operated_by.as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => {
            crate::log::error(&format!("Failed to build run: {e}"));
            return 1;
        }
    };

    let mut queued = QueuedRun {
        request,
        attachments,
        run_id: None,
        attempt_count: 0,
        last_attempt_at: None,
        next_retry_at: None,
        parked: false,
        last_error: None,
        queued_at: None,
    };
    let db = match db::open() {
        Ok(db) => db,
        Err(e) => {
            crate::log::error(&format!("Failed to open database: {e}"));
            return 1;
        }
    };

    // Always persist the run to the local DB in TofuPilot format, regardless
    // of credentials. Uploading happens only when creds are available.
    if let Err(e) =
        crate::commands::run::queue::enqueue(&db, &queue_id, &mut queued, Some(&upload_bus))
    {
        crate::log::error(&format!("Failed to queue run: {e}"));
        return 1;
    }

    if json_mode {
        println!(
            "{}",
            serde_json::json!({"type": "upload_queued", "queue_id": queue_id})
        );
    }

    if upload {
        if let Some(c) = creds {
            let upload_creds = c.clone();
            let upload_queue_id = queue_id.clone();
            let upload_db = db.clone();
            let upload_bus_for_task = upload_bus.clone();
            let handle = tokio::spawn(async move {
                upload_queued_run(
                    crate::http::client(),
                    &upload_creds,
                    &upload_queue_id,
                    &queued,
                    &upload_db,
                    Some(&upload_bus_for_task),
                    true,
                )
                .await;
            });

            // Wait up to 10s for upload, then exit regardless. If this future is
            // dropped mid-wait (caller cancelled us), abort the upload task too --
            // it holds a soon-to-be-revoked key and would only 401 forever. The
            // queue entry was persisted before the spawn, so queue::drain retries
            // the upload at next startup with the new key.
            struct AbortGuard(tokio::task::AbortHandle);
            impl Drop for AbortGuard {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _guard = AbortGuard(handle.abort_handle());
            let _ = tokio::time::timeout(crate::config::timeouts::STDERR_READER_JOIN, handle).await;
        }
    }

    exit_code
}

// ---------------------------------------------------------------------------
// Build RunCreateRequest
// ---------------------------------------------------------------------------

fn build_request(
    test_end: &serde_json::Value,
    test_start: &Option<serde_json::Value>,
    phases: &[serde_json::Value],
    procedure_id: &str,
    procedure_dir: &Path,
    unit: &ResolvedUnit,
    operated_by: Option<&str>,
) -> crate::error::CliResult<RunCreateRequest> {
    // Final test_record.metadata mutations (user phase overrides) win
    // over CLI-resolved unit fields. Mirrors v1 plugin's
    // `data.dut_id ?? metadata.serial_number` precedence and lets a
    // `read_firmware` phase rewrite identity from EEPROM after boot.
    //
    // Coerce numeric / bool metadata to string instead of silently
    // dropping. A user setting `record.metadata["batch_number"] = 42`
    // gets `"42"` on the wire, not a fall-through to the resolved
    // unit's value (which would mask their override).
    let metadata = test_end
        .get("metadata")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let meta_str = |key: &str| -> Option<String> {
        let v = metadata.get(key)?;
        let s = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => return None,
        };
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };

    // Serial number priority: user metadata mutation > test_record.dut_id
    // > resolved-unit (operator prompt / auto_identify defaults).
    let serial = meta_str("serial_number")
        .or_else(|| {
            json_str(test_end, "dut_id")
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .or_else(|| unit.serial_number.clone())
        .unwrap_or_default();

    let mut b = RunCreateRequest::builder()
        .outcome(parse_outcome(
            json_str(test_end, "outcome").unwrap_or(super::outcomes::ERROR),
        ))
        .procedure_id(procedure_id)
        .serial_number(serial)
        .started_at(json_millis(test_end, "start_time_millis"))
        .ended_at(json_millis(test_end, "end_time_millis"))
        .phases(
            phases
                .iter()
                .filter_map(|p| build_phase(p).ok())
                .collect::<Vec<_>>(),
        );

    let logs: Vec<_> = test_end
        .get("logs")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|l| build_log(l, procedure_dir))
                .collect()
        })
        .unwrap_or_default();
    if !logs.is_empty() {
        b = b.logs(logs);
    }

    if let Some(d) = json_str(test_end, "docstring").filter(|s| !s.is_empty()) {
        b = b.docstring(d);
    }

    // Part / revision / batch / sub_units: user metadata mutations win.
    // Fall back to CLI-resolved unit, then constructor-time test_start
    // payload (part_number only). Lets a phase that reads identity
    // from EEPROM correct what the operator scanned.
    let part_number = meta_str("part_number")
        .or_else(|| unit.part_number.clone())
        .or_else(|| {
            test_start
                .as_ref()
                .and_then(|ts| json_str(ts, "part_number"))
                .map(str::to_string)
        })
        .filter(|s| !s.is_empty());
    if let Some(pn) = part_number {
        b = b.part_number(pn);
    }

    let revision_number = meta_str("revision_number")
        .or_else(|| unit.revision_number.clone())
        .filter(|s| !s.is_empty());
    if let Some(rev) = revision_number {
        b = b.revision_number(rev);
    }

    let batch_number = meta_str("batch_number")
        .or_else(|| unit.batch_number.clone())
        .filter(|s| !s.is_empty());
    if let Some(batch) = batch_number {
        b = b.batch_number(batch);
    }

    let sub_units: Option<Vec<String>> = metadata
        .get("sub_units")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .or_else(|| {
            if unit.sub_units.is_empty() {
                None
            } else {
                Some(unit.sub_units.values().cloned().collect())
            }
        });
    if let Some(sub) = sub_units {
        b = b.sub_units(sub);
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

fn build_phase(p: &serde_json::Value) -> crate::error::CliResult<RunCreatePhases> {
    let measurements: Vec<_> = p
        .get("measurements")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|m| build_measurement(m).ok()).collect())
        .unwrap_or_default();

    let mut b = RunCreatePhases::builder()
        .name(json_str(p, "name").ok_or("missing name")?)
        .outcome(parse_phase_outcome(
            json_str(p, "outcome").unwrap_or(super::outcomes::FAIL),
        ))
        .started_at(json_millis(p, "start_time_millis"))
        .ended_at(json_millis(p, "end_time_millis"))
        .measurements(measurements);

    if let Some(rc) = p.get("retry_count").and_then(|v| v.as_i64()) {
        b = b.retry_count(rc);
    }
    if let Some(d) = json_str(p, "docstring").filter(|s| !s.is_empty()) {
        b = b.docstring(d);
    }
    b.build().map_err(|e| e.to_string().into())
}

#[allow(deprecated)]
fn build_measurement(m: &serde_json::Value) -> crate::error::CliResult<RunCreateMeasurements> {
    let outcome = match json_str(m, "outcome").unwrap_or("UNSET") {
        super::outcomes::PASS => Outcome::Pass,
        super::outcomes::FAIL => Outcome::Fail,
        _ => Outcome::Unset,
    };
    let mut b = RunCreateMeasurements::builder()
        .name(json_str(m, "name").ok_or("missing name")?)
        .outcome(outcome);

    if m.get("x_axis").is_some() && m.get("y_axis").is_some() {
        if let Some(Ok(xa)) = m.get("x_axis").map(build_x_axis) {
            b = b.x_axis(xa);
        }
        if let Some(ya) = m.get("y_axis").and_then(|v| v.as_array()) {
            let y: Vec<_> = ya.iter().filter_map(|y| build_y_axis(y).ok()).collect();
            if !y.is_empty() {
                b = b.y_axis(y);
            }
        }
    } else if let Some(mv) = m.get("measured_value") {
        b = b.measured_value(mv.clone());
        if let Some(u) = m.get("units").filter(|v| !v.is_null()) {
            b = b.units(u.clone());
        }
    }

    if let Some(arr) = m.get("validators").and_then(|v| v.as_array()) {
        let vs: Vec<_> = arr.iter().filter_map(build_validator).collect();
        if !vs.is_empty() {
            b = b.validators(vs);
        }
    }
    if let Some(d) = json_str(m, "docstring").filter(|s| !s.is_empty()) {
        b = b.docstring(d);
    }
    b.build().map_err(|e| e.to_string().into())
}

fn build_validator(v: &serde_json::Value) -> Option<RunCreateMeasurementsValidators> {
    let mut b = RunCreateMeasurementsValidators::builder();
    if let Some(s) = json_str(v, "operator") {
        b = b.operator(s);
    }
    if let Some(e) = v.get("expected_value") {
        b = b.expected_value(e.clone());
    }
    if let Some(s) = json_str(v, "expression") {
        b = b.expression(s);
    }
    if let Some(s) = json_str(v, "outcome") {
        b = b.outcome(super::outcomes::validator_outcome_from_wire(s));
    }
    if let Some(d) = v.get("is_decisive").and_then(|v| v.as_bool()) {
        b = b.is_decisive(d);
    }
    b.build().ok()
}

fn build_x_axis(x: &serde_json::Value) -> crate::error::CliResult<RunCreateXAxis> {
    let mut b = RunCreateXAxis::builder().data(json_f64_vec(x, "data"));
    if let Some(u) = json_str(x, "units") {
        b = b.units(u);
    }
    if let Some(d) = json_str(x, "description") {
        b = b.description(d);
    }
    b.build().map_err(|e| e.to_string().into())
}

fn build_y_axis(y: &serde_json::Value) -> crate::error::CliResult<RunCreateYAxis> {
    let mut b = RunCreateYAxis::builder().data(json_f64_vec(y, "data"));
    if let Some(u) = json_str(y, "units") {
        b = b.units(u);
    }
    b.build().map_err(|e| e.to_string().into())
}

fn build_log(l: &serde_json::Value, procedure_dir: &Path) -> Option<RunCreateLogs> {
    Some(RunCreateLogs {
        level: super::outcomes::parse_log_level(json_str(l, "level").unwrap_or("INFO")),
        timestamp: json_millis(l, "timestamp_millis"),
        message: json_str(l, "message").unwrap_or("").to_string(),
        source_file: super::log_source::sanitize_source_file(
            json_str(l, "source").unwrap_or(""),
            procedure_dir,
        ),
        line_number: l.get("lineno").and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

fn json_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|v| v.as_str())
}

/// OpenHTF prompts encode the operator answer under the `"response"`
/// key of a `HashMap<String, String>`. Three sites pulled it out
/// inline; keep them aligned through one helper.
fn response_value(values: &std::collections::HashMap<String, String>) -> String {
    values.get("response").cloned().unwrap_or_default()
}

fn json_millis(v: &serde_json::Value, key: &str) -> DateTime<Utc> {
    v.get(key)
        .and_then(|v| v.as_i64())
        .and_then(DateTime::from_timestamp_millis)
        .unwrap_or_else(Utc::now)
}

fn json_f64_vec(v: &serde_json::Value, key: &str) -> Vec<f64> {
    v.get(key)
        .and_then(|d| d.as_array())
        .map(|a| a.iter().filter_map(|n| n.as_f64()).collect())
        .unwrap_or_default()
}

fn parse_outcome(s: &str) -> RunGetOutcome {
    use super::outcomes::*;
    match s {
        PASS => RunGetOutcome::Pass,
        FAIL => RunGetOutcome::Fail,
        TIMEOUT => RunGetOutcome::Timeout,
        ABORTED => RunGetOutcome::Aborted,
        _ => RunGetOutcome::Error,
    }
}

fn parse_phase_outcome(s: &str) -> RunGetPhasesOutcome {
    use super::outcomes::*;
    match s {
        PASS => RunGetPhasesOutcome::Pass,
        SKIP => RunGetPhasesOutcome::Skip,
        ERROR => RunGetPhasesOutcome::Error,
        _ => RunGetPhasesOutcome::Fail,
    }
}

fn make_component(
    key: &str,
    component_type: ComponentType,
    required: bool,
    label: Option<String>,
    default_value: Option<ComponentValue>,
) -> UiComponent {
    UiComponent {
        key: key.to_string(),
        label,
        required,
        default_value,
        ..UiComponent::new(component_type)
    }
}

fn extract_run_measurements(phase_event: &serde_json::Value) -> Vec<RunMeasurement> {
    phase_event
        .get("measurements")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(RunMeasurement {
                        name: json_str(m, "name")?.to_string(),
                        outcome: json_str(m, "outcome").unwrap_or("UNSET").to_string(),
                        measured_value: m.get("measured_value").cloned(),
                        units: json_str(m, "units").map(String::from),
                        validators: extract_validator_results(m),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_validator_results(m: &serde_json::Value) -> Vec<station_protocol::ValidatorResult> {
    m.get("validators")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let expression = json_str(v, "expression")
                        .map(String::from)
                        .or_else(|| synthesize_validator_expression(v))?;
                    let outcome = json_str(v, "outcome").unwrap_or("UNSET").to_string();
                    let is_decisive = v.get("is_decisive").and_then(|v| v.as_bool());
                    Some(station_protocol::ValidatorResult {
                        expression,
                        outcome,
                        is_decisive,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build a compact display string like "x >= 3.0" when the Python side
/// didn't send one. Keeps the live view readable without forcing the
/// connector to pre-format every validator.
fn synthesize_validator_expression(v: &serde_json::Value) -> Option<String> {
    let op = json_str(v, "operator").unwrap_or("").trim().to_string();
    let expected = match v.get("expected_value") {
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    };
    // Match web's display format: `x <op> <value>`.
    match (op.is_empty(), expected.is_empty()) {
        (true, true) => None,
        (false, true) => Some(format!("x {op}")),
        (true, false) => Some(expected),
        (false, false) => Some(format!("x {op} {expected}")),
    }
}

/// Resolve an OpenHTF phase name to its canonical plan key.
///
/// OpenHTF phase names are stable across retries of the same phase — the
/// `PhaseResult.REPEAT` / `repeat_limit` machinery re-runs the same descriptor.
/// We preserve the first plan entry's key (`<idx>_<name>`) and differentiate
/// retries via the `attempt` field on `phase_started` / `phase_finished`, not
/// via a synthetic key. Phases not in the plan (e.g. PhaseGroup setup /
/// teardown phases that aren't enumerated in `Test.__init__`) fall back to a
/// `<name>` key so the protocol remains well-formed.
fn canonical_phase_key(phase_keys: &[(String, String)], name: &str) -> String {
    phase_keys
        .iter()
        .find(|(_, n)| n == name)
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| name.to_string())
}

/// Emit `phase_started` (once per `(phase_key, attempt)`). Shared by
/// `phase_begin`, `phase_end` (safety net), and `prompt` handlers so they
/// stay in lockstep. Delegates to the shared `EventRouter` for the actual
/// fan-out; this function only owns the dedup invariant.
fn emit_phase_started(
    router: &super::event_router::EventRouter,
    started: &mut std::collections::HashSet<(String, u32)>,
    phase_key: &str,
    phase_name: &str,
    attempt: u32,
) {
    if !started.insert((phase_key.to_string(), attempt)) {
        return;
    }
    router.phase_started(phase_key, phase_name, attempt, None);
}

/// RAII cleanup for the ephemeral `.tofupilot_openhtf.py` connector script
/// we drop into the procedure directory. Covers normal return, early
/// error returns, and panics; the explicit `remove_file` calls elsewhere
/// are redundant but harmless.
struct ConnectorScriptGuard {
    path: std::path::PathBuf,
}

impl ConnectorScriptGuard {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for ConnectorScriptGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kwargs(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn openhtf_requirement_matches_bare_and_pinned() {
        // Input is always lowercased by the caller.
        assert!(contains_openhtf_requirement("openhtf"));
        assert!(contains_openhtf_requirement("openhtf>=1.6"));
        assert!(contains_openhtf_requirement("\"openhtf>=1.6\""));
        assert!(contains_openhtf_requirement("openhtf = \"^1.6\""));
        assert!(contains_openhtf_requirement(
            "[[package]]\nname = \"openhtf\""
        ));
    }

    #[test]
    fn openhtf_requirement_rejects_substring_lookalikes() {
        // A path or a longer identifier that merely contains the substring
        // must not match — that's the whole point of the boundary check.
        assert!(!contains_openhtf_requirement("/my-openhtf-fork/setup.py"));
        assert!(!contains_openhtf_requirement("openhtfx"));
        assert!(!contains_openhtf_requirement("xopenhtf"));
        assert!(!contains_openhtf_requirement("my_openhtf_helper"));
        assert!(!contains_openhtf_requirement("no framework here"));
    }

    #[test]
    fn openhtf_requirement_matches_at_string_boundaries() {
        // First and last token positions exercise the i == 0 and
        // i + n >= len boundary branches.
        assert!(contains_openhtf_requirement("openhtf\n"));
        assert!(contains_openhtf_requirement("\nopenhtf"));
    }

    #[test]
    fn pkgname_char_classifies_identifier_bytes() {
        for b in b"az09_-" {
            assert!(
                is_pkgname_char(*b),
                "{} should be a pkgname char",
                *b as char
            );
        }
        for b in b" .\"/=\n[" {
            assert!(
                !is_pkgname_char(*b),
                "{} should not be a pkgname char",
                *b as char
            );
        }
    }

    #[test]
    fn build_unit_config_propagates_kwargs() {
        let cfg = build_unit_config_from_kwargs(
            &kwargs(&[
                ("serial_number", "SN-1"),
                ("part_number", "PCB"),
                ("revision_number", "A"),
                ("batch_number", "B-2026-W17"),
            ]),
            true,
        );
        assert!(cfg.auto_identify);
        assert_eq!(
            cfg.serial_number
                .as_ref()
                .and_then(|f| f.default_value.as_deref()),
            Some("SN-1")
        );
        assert_eq!(
            cfg.part_number
                .as_ref()
                .and_then(|f| f.default_value.as_deref()),
            Some("PCB")
        );
        assert_eq!(
            cfg.revision_number
                .as_ref()
                .and_then(|f| f.default_value.as_deref()),
            Some("A")
        );
        assert_eq!(
            cfg.batch_number
                .as_ref()
                .and_then(|f| f.default_value.as_deref()),
            Some("B-2026-W17")
        );
    }

    #[test]
    fn build_unit_config_empty_strings_become_none_defaults() {
        // Python sends "" for absent kwargs. They must map to a config
        // with no `default_value` so `auto_identify`-mode validation
        // fails fast with a clear error instead of silently uploading
        // an empty serial.
        let cfg = build_unit_config_from_kwargs(
            &kwargs(&[("serial_number", ""), ("part_number", "")]),
            true,
        );
        assert!(cfg.serial_number.as_ref().unwrap().default_value.is_none());
        assert!(cfg.part_number.as_ref().unwrap().default_value.is_none());
        // `validate_auto_identify` should reject this config.
        let err = cfg.validate_auto_identify().unwrap_err();
        assert!(err.contains("serial_number.default_value"));
    }

    #[test]
    fn build_unit_config_optional_fields_omitted_when_absent() {
        // No `revision_number` / `batch_number` kwargs → no config
        // slots, so the prompt path doesn't render rows for them.
        let cfg = build_unit_config_from_kwargs(
            &kwargs(&[("serial_number", "SN"), ("part_number", "PCB")]),
            false,
        );
        assert!(cfg.revision_number.is_none());
        assert!(cfg.batch_number.is_none());
    }

    #[test]
    fn build_unit_config_keeps_required_slots_for_prompt_path() {
        // Even with zero kwargs, serial_number / part_number slots
        // exist (with empty UnitFieldConfig) so the framework prompt
        // emits the canonical identify-unit components.
        let cfg = build_unit_config_from_kwargs(&kwargs(&[]), false);
        assert!(cfg.serial_number.is_some());
        assert!(cfg.part_number.is_some());
        assert!(cfg.revision_number.is_none());
        assert!(cfg.batch_number.is_none());
    }

    #[test]
    fn build_request_user_metadata_overrides_resolved_unit() {
        // User phase mutated test_record.metadata to override identity
        // (e.g. read from EEPROM after boot). Mutation must win over
        // CLI-resolved unit fields.
        let mut sub = std::collections::HashMap::new();
        sub.insert("battery".to_string(), "BAT-RESOLVED".to_string());
        let unit = ResolvedUnit {
            serial_number: Some("SN-CLI".to_string()),
            part_number: Some("PCB-CLI".to_string()),
            revision_number: Some("A".to_string()),
            batch_number: Some("BATCH-CLI".to_string()),
            sub_units: sub,
        };
        let test_end = serde_json::json!({
            "outcome": "PASS",
            "dut_id": "SN-DUT",
            "start_time_millis": 0,
            "end_time_millis": 1,
            "metadata": {
                "serial_number": "SN-USER",
                "part_number": "PCB-USER",
                "revision_number": "B",
                "batch_number": "BATCH-USER",
                "sub_units": ["BAT-USER-1", "BAT-USER-2"],
            },
        });
        let req = build_request(
            &test_end,
            &None,
            &[],
            "proc",
            std::path::Path::new("/tmp"),
            &unit,
            None,
        )
        .unwrap();
        assert_eq!(req.serial_number, "SN-USER");
        assert_eq!(req.part_number.as_deref(), Some("PCB-USER"));
        assert_eq!(req.revision_number.as_deref(), Some("B"));
        assert_eq!(req.batch_number.as_deref(), Some("BATCH-USER"));
        assert_eq!(
            req.sub_units.as_deref(),
            Some(&["BAT-USER-1".to_string(), "BAT-USER-2".to_string()][..])
        );
    }

    #[test]
    fn build_request_falls_back_to_resolved_unit_without_metadata() {
        // No user mutations — CLI-resolved unit fields populate the
        // request directly. dut_id wins for serial_number when no
        // metadata override is present.
        let mut sub = std::collections::HashMap::new();
        sub.insert("battery".to_string(), "BAT-RESOLVED".to_string());
        let unit = ResolvedUnit {
            serial_number: Some("SN-CLI".to_string()),
            part_number: Some("PCB-CLI".to_string()),
            revision_number: Some("A".to_string()),
            batch_number: Some("BATCH-CLI".to_string()),
            sub_units: sub,
        };
        let test_end = serde_json::json!({
            "outcome": "PASS",
            "dut_id": "SN-DUT",
            "start_time_millis": 0,
            "end_time_millis": 1,
        });
        let req = build_request(
            &test_end,
            &None,
            &[],
            "proc",
            std::path::Path::new("/tmp"),
            &unit,
            None,
        )
        .unwrap();
        assert_eq!(req.serial_number, "SN-DUT");
        assert_eq!(req.part_number.as_deref(), Some("PCB-CLI"));
        assert_eq!(req.revision_number.as_deref(), Some("A"));
        assert_eq!(req.batch_number.as_deref(), Some("BATCH-CLI"));
        assert_eq!(
            req.sub_units.as_deref(),
            Some(&["BAT-RESOLVED".to_string()][..])
        );
    }

    #[test]
    fn build_request_coerces_numeric_metadata_to_string() {
        // User sets `metadata["batch_number"] = 42` — we coerce to "42"
        // instead of silently falling through to the resolved unit's
        // value (which would mask the user override).
        let unit = ResolvedUnit {
            serial_number: Some("SN-CLI".to_string()),
            part_number: None,
            revision_number: None,
            batch_number: Some("BATCH-CLI".to_string()),
            sub_units: std::collections::HashMap::new(),
        };
        let test_end = serde_json::json!({
            "outcome": "PASS",
            "dut_id": "SN-CLI",
            "start_time_millis": 0,
            "end_time_millis": 1,
            "metadata": {
                "batch_number": 42,
                "revision_number": true,
            },
        });
        let req = build_request(
            &test_end,
            &None,
            &[],
            "proc",
            std::path::Path::new("/tmp"),
            &unit,
            None,
        )
        .unwrap();
        assert_eq!(req.batch_number.as_deref(), Some("42"));
        assert_eq!(req.revision_number.as_deref(), Some("true"));
    }

    #[test]
    fn unit_info_to_wire_round_trips_fields() {
        use std::collections::HashMap;
        let mut sub = HashMap::new();
        sub.insert("battery".to_string(), "BAT-1".to_string());
        let info = execution_engine::unit::UnitInfo {
            serial_number: Some("SN".to_string()),
            part_number: Some("PCB".to_string()),
            revision_number: Some("A".to_string()),
            batch_number: None,
            sub_units: Some(sub),
            status: "complete".to_string(),
        };
        let wire = unit_info_to_wire(&info);
        assert_eq!(wire.serial_number.as_deref(), Some("SN"));
        assert_eq!(wire.part_number.as_deref(), Some("PCB"));
        assert_eq!(wire.revision_number.as_deref(), Some("A"));
        assert!(wire.batch_number.is_none());
        assert_eq!(
            wire.sub_units.get("battery").map(String::as_str),
            Some("BAT-1")
        );
    }
}
