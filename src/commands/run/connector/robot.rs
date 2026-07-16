//! Robot Framework connector — Rust-side runner.
//!
//! Mirror of `connector::run_pytest`: spawn the embedded Robot listener
//! script under the deployment's Python interpreter, drain NDJSON
//! events, run the framework identify-unit handshake, and bridge
//! lifecycle events into the shared `event_router::EventRouter`.
//!
//! Shape parity with the pytest path is intentional: both surfaces
//! speak the same wire enum (`PythonEvent`) and emit the same
//! `RunCreateRequest` shape. The helper functions that build that
//! request (`build_request`, `build_phase`, `build_measurement`,
//! `parse_outcome`, `canonical_phase_key`, …) are currently duplicated
//! between this file and `pytest.rs` — ~75% identical. The duplication
//! is tracked as tech debt; a future change should extract them into
//! `connector::common` (or re-export from `pytest::*`). Until that
//! lands, edits to either copy must be mirrored to the other.
//!
//! The intentional differences live in:
//!   * the listener script (Robot's API v3 hooks vs. pytest's plugin
//!     hooks) — completely separate code path,
//!   * `parse_phase_outcome` — Robot has no `XFAIL` / `XPASS`,
//!   * the framework-detection function (`has_robot`).

use std::path::Path;
use std::process::Stdio;

use chrono::{DateTime, Utc};
use command_group::AsyncCommandGroup;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

use execution_engine::ui::UiRequestData;
use station_protocol::{PhasePlan, RunMeasurement, StationEvent};
use tofupilot_sdk::types::*;
// SDK enum names track the alphabetically-first endpoint; alias back to the
// names this crate uses (see connector/mod.rs).
use tofupilot_sdk::types::{
    LogGetOutcome as RunGetOutcome, PhaseGetOutcome as RunGetPhasesOutcome,
};

use super::super::agent_proto::{AgentProtoCtx, CliEvent};
use super::super::queue::{upload_queued_run, QueuedRun};
use super::events::PythonEvent;
use crate::commands::auth::credentials::Credentials;
use crate::commands::db;

const ROBOT_LISTENER: &str = include_str!("robot.py");
const ROBOT_LIBRARY: &str = include_str!("tofupilot_robot.py");

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// True when `dir` looks like a Robot Framework procedure.
///
/// Detection strategy (any single hit wins):
///   1. `manifest.json` records `framework == "robot"`.
///   2. `pyproject.toml` / `requirements.txt` / `uv.lock` /
///      `pylock.toml` declares `robotframework` as a dep.
///   3. Any `*.robot` file at the dir root.
///
/// Robot has no canonical config file (no `robot.ini` / `robot.toml`
/// equivalent of `pytest.ini`). Some shops use `[tool.robot]` in
/// `pyproject.toml`, but it's a community convention, not a hard
/// signal — we don't sniff for it. A `.robot` leaf file is the
/// authoritative marker.
///
/// The framework precedence in `Framework::detect` is
/// yaml > openhtf > pytest > robot > plain. A repo that ships both
/// pytest test files and `.robot` cases is treated as pytest-driven —
/// that's the more common case where Robot is auxiliary tooling.
pub fn has_robot(dir: &Path) -> bool {
    if let Ok(content) = std::fs::read_to_string(dir.join("manifest.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
            if v.get("framework").and_then(|f| f.as_str()) == Some("robot") {
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
            if contains_robotframework_requirement(&content.to_lowercase()) {
                return true;
            }
        }
    }

    has_robot_file(dir)
}

/// True if `dir` (single-level) contains any `*.robot` file.
fn has_robot_file(dir: &Path) -> bool {
    let Ok(read) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if s.ends_with(".robot") {
            return true;
        }
    }
    false
}

/// Whole-token `robotframework` match in a (lowercased) dependency
/// manifest. Mirrors the package-name regex used in pytest detection.
fn contains_robotframework_requirement(lower: &str) -> bool {
    let needle = b"robotframework";
    let n = needle.len();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + n <= bytes.len() {
        if &bytes[i..i + n] == needle {
            let before_ok = i == 0 || !is_pkgname_char(bytes[i - 1]);
            let after_ok = i + n >= bytes.len() || !is_pkgname_char(bytes[i + n]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_pkgname_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-')
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// Resolved unit fields from the framework identify-unit handshake.
/// Local copy of the pytest connector's `ResolvedUnit`; the wire shape
/// is identical.
#[derive(Debug, Default, Clone)]
struct ResolvedUnit {
    serial_number: Option<String>,
    part_number: Option<String>,
    revision_number: Option<String>,
    batch_number: Option<String>,
    sub_units: std::collections::HashMap<String, String>,
}

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
        serial_number: Some(field("serial_number").unwrap_or_default()),
        part_number: Some(field("part_number").unwrap_or_default()),
        revision_number: field("revision_number"),
        batch_number: field("batch_number"),
        sub_units: None,
        metadata: None,
    }
}

fn unit_info_to_wire(info: &execution_engine::unit::UnitInfo) -> station_protocol::UnitInfo {
    station_protocol::UnitInfo {
        serial_number: info.serial_number.clone(),
        part_number: info.part_number.clone(),
        revision_number: info.revision_number.clone(),
        batch_number: info.batch_number.clone(),
        sub_units: info.sub_units.clone().unwrap_or_default(),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_robot(
    python_path: &Path,
    test_path: &Path,
    procedure_dir: &Path,
    procedure_id: &str,
    procedure_name: &str,
    execution_id: &str,
    creds: Option<&Credentials>,
    upload: bool,
    json_mode: bool,
    event_tx: broadcast::Sender<StationEvent>,
    ui_tx: Option<mpsc::Sender<UiRequestData>>,
    agent: Option<AgentProtoCtx>,
    // Headless guard: false when no operator surface can answer a
    // unit-identify prompt, so `identify` fails fast instead of hanging.
    has_ui: bool,
    reuse_unit: Option<station_protocol::UnitInfo>,
    // Email forwarded to `runs.create` as `operated_by`. Set when the
    // run was triggered from the web operator UI; None for kiosk and
    // CLI-driven runs.
    operated_by: Option<String>,
    mut cancel_rx: super::super::cancel::Receiver,
) -> i32 {
    let _ = ui_tx; // Robot doesn't surface operator prompts; identify-unit only.
    let listener_path = procedure_dir.join(".tofupilot_robot.py");
    let library_path = procedure_dir.join("tofupilot_robot.py");
    // Embed both: the listener script and the keyword library next to
    // it. The listener inserts its own dir on `sys.path` so `import
    // tofupilot_robot` resolves to our embedded copy regardless of
    // whether the user pinned the library themselves.
    let _ = std::fs::remove_file(&listener_path);
    let emit_setup_fail = |err: &str| {
        super::super::emit::run_crashed(
            &event_tx,
            agent.as_ref(),
            procedure_id,
            execution_id,
            "spawn_failed",
            err,
            1,
        );
    };
    if let Err(e) = std::fs::write(&listener_path, ROBOT_LISTENER) {
        crate::log::error(&format!("Failed to write Robot listener: {e}"));
        emit_setup_fail(&format!("Failed to write Robot listener: {e}"));
        return 1;
    }
    let library_existed = library_path.exists();
    if !library_existed {
        if let Err(e) = std::fs::write(&library_path, ROBOT_LIBRARY) {
            crate::log::error(&format!("Failed to write tofupilot_robot library: {e}"));
            emit_setup_fail(&format!("Failed to write tofupilot_robot library: {e}"));
            let _ = std::fs::remove_file(&listener_path);
            return 1;
        }
    }
    let _script_guard = ConnectorScriptGuard::new(listener_path.clone());
    // Only reap the library file we created; if the user shipped their
    // own `tofupilot_robot.py`, leave it untouched.
    let _library_guard = if library_existed {
        None
    } else {
        Some(ConnectorScriptGuard::new(library_path.clone()))
    };

    let queue_id = super::super::queue::new_queue_id(procedure_id);

    let mut cmd = super::super::python::build_command(
        python_path,
        &[listener_path.as_path(), test_path],
        procedure_dir,
        &queue_id,
    );
    cmd.stdin(Stdio::piped());

    let mut child = match cmd.group().kill_on_drop(true).spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = super::super::python::spawn_error_message(python_path, &e);
            crate::log::error(&msg);
            emit_setup_fail(&msg);
            return 1;
        }
    };

    let inner = child.inner();
    let stdout = match inner.stdout.take() {
        Some(s) => s,
        None => {
            crate::log::error("Failed to capture stdout");
            emit_setup_fail("Failed to capture stdout from Python child");
            return 1;
        }
    };
    let stderr = match inner.stderr.take() {
        Some(s) => s,
        None => {
            crate::log::error("Failed to capture stderr");
            emit_setup_fail("Failed to capture stderr from Python child");
            return 1;
        }
    };
    let stdin = match inner.stdin.take() {
        Some(s) => s,
        None => {
            crate::log::error("Failed to capture stdin");
            emit_setup_fail("Failed to capture stdin from Python child");
            return 1;
        }
    };

    let is_json = json_mode;
    let tx = event_tx.clone();
    let upload_bus = event_tx.clone();
    let crash_tx = event_tx;
    let pid = procedure_id.to_string();
    let pname = procedure_name.to_string();
    let eid = execution_id.to_string();
    let agent_for_task = agent.clone();
    let router = super::super::event_router::EventRouter::new(
        tx.clone(),
        agent.clone(),
        execution_id.to_string(),
    );

    let stdout_handle = tokio::spawn(async move {
        let mut stdin_writer = stdin;
        let mut phases = Vec::new();
        let mut test_end = None;
        let mut test_start = None;
        let mut unit_resolved = ResolvedUnit::default();
        let mut identify_error: Option<String> = None;
        let mut phase_keys: Vec<(String, String)> = Vec::new();
        let mut started_attempts: std::collections::HashSet<(String, u32)> =
            std::collections::HashSet::new();
        let mut current_phase_key = String::new();
        let mut attachments: Vec<super::super::queue::QueuedAttachment> = Vec::new();

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

                    let resolved_wire = if identify {
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
                                metadata: None,
                            };
                            let cfg = build_unit_config_from_kwargs(&unit_kwargs, auto_identify);
                            if let Err(err) =
                                execution_engine::unit::validate_unit_info(&info, &Some(cfg))
                            {
                                let msg = format!("reuse_unit failed validation: {err}");
                                crate::log::error(&msg);
                                identify_error = Some(msg.clone());
                                super::super::emit::run_crashed(
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
                            let host = super::super::identify_host::CliIdentifyHost {
                                router: router.clone(),
                                ui_tx: None,
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
                                    router.identify_resolved(None, &unit_info_to_wire(&info));
                                    Some(info)
                                }
                                Err(err) => {
                                    let msg = format!("{err}");
                                    crate::log::error(&format!("Identify-unit failed: {msg}"));
                                    identify_error = Some(msg.clone());
                                    super::super::emit::run_crashed(
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

                    if identify_error.is_some() {
                        break;
                    }

                    let _ = test_name;
                    let _ = tx.send(StationEvent::RunStarted {
                        procedure_id: pid.clone(),
                        procedure_name: pname.clone(),
                        execution_id: eid.clone(),
                        phases: plans,
                        slots: Vec::new(),
                        plugs: Vec::new(),
                        timestamp: Some(chrono::Utc::now().to_rfc3339()),
                        run_id: None,
                        deployment_id: crate::commands::run::deployment_id::lookup_deployment_id(
                            &pid,
                        ),
                        unit: resolved_wire.as_ref().map(unit_info_to_wire),
                    });
                    if let Some(ref agent) = agent_for_task {
                        let phases: Vec<super::super::agent_proto::PhasePlanPayload> = phase_keys
                            .iter()
                            .map(|(k, n)| super::super::agent_proto::PhasePlanPayload {
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
                    emit_phase_started(&router, &mut started_attempts, &phase_key, &name, 1);
                    current_phase_key = phase_key;
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
                    let attempt = (retry_count.min(u32::MAX as u64) as u32).saturating_add(1);
                    let measurements = extract_run_measurements(&event);
                    emit_phase_started(&router, &mut started_attempts, &phase_key, &name, attempt);
                    let duration_ms = match (start_time_millis, end_time_millis) {
                        (Some(s), Some(e)) if e >= s => Some((e - s) as u64),
                        _ => None,
                    };
                    router.phase_finished(super::super::event_router::PhaseFinished {
                        phase_key: phase_key.clone(),
                        phase_name: name.clone(),
                        outcome: outcome.clone(),
                        attempt,
                        slot_id: None,
                        error,
                        started_at: start_time_millis.map(super::super::time_fmt::from_millis),
                        ended_at: end_time_millis.map(super::super::time_fmt::from_millis),
                        duration_ms,
                        station_measurements: measurements,
                        station_logs: Vec::new(),
                    });
                    phases.push(event);
                }
                PythonEvent::TestEnd { outcome } => {
                    super::super::emit::run_complete(&tx, &outcome, &eid, None);
                    test_end = Some(event);
                }
                PythonEvent::Prompt { .. } => {
                    // Robot connector never emits prompts; ignore.
                }
                PythonEvent::Attachment {
                    name,
                    path,
                    mimetype,
                    live,
                    phase_name,
                } => {
                    if live {
                        // Live attachment (none today; the connector
                        // only emits attachments at test_end). Mirror
                        // the openhtf wire shape so a future live
                        // emission shows up in the agent UI.
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
                        let phase_key = phase_name
                            .as_deref()
                            .map(|n| canonical_phase_key(&phase_keys, n))
                            .filter(|k| !k.is_empty())
                            .unwrap_or_else(|| current_phase_key.clone());
                        attachments.push(super::super::queue::QueuedAttachment {
                            name,
                            path,
                            mimetype,
                            phase_key,
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
            if is_json && agent_for_task.is_none() {
                println!("{line}");
            }
        }

        (
            phases,
            test_end,
            test_start,
            unit_resolved,
            identify_error,
            attachments,
        )
    });

    let (stderr_handle, stderr_tail) =
        super::super::python::spawn_stderr_reader_with_capture(stderr);

    let mut cancelled_by_signal = false;
    let exit_code = tokio::select! {
        status = child.wait() => match status {
            Ok(s) => s.code().unwrap_or(1),
            Err(e) => { crate::log::error(&format!("Process error: {e}")); 1 }
        },
        _ = tokio::signal::ctrl_c() => {
            cancelled_by_signal = true;
            crate::log::info("Interrupted, killing robot subprocess");
            super::super::python::graceful_shutdown(&mut child).await
        }
        signal = cancel_rx.wait_any() => {
            cancelled_by_signal = true;
            crate::log::info(&format!(
                "{} requested, killing robot subprocess",
                match signal {
                    super::super::cancel::CancelSignal::Force => "Force-kill",
                    _ => "Stop",
                },
            ));
            super::super::python::graceful_shutdown(&mut child).await
        }
    };

    let _ = stderr_handle.await;
    // `_script_guard` / `_library_guard` Drop will remove the listener and embedded library.

    let (phases, test_end, _test_start, unit_resolved, identify_error, attachments) =
        if cancelled_by_signal {
            stdout_handle.abort();
            super::super::ui_response::cancel_all().await;
            super::super::emit::run_complete(
                &crash_tx,
                super::super::outcomes::ABORTED,
                execution_id,
                None,
            );
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
            let tail = stderr_tail.lock().await.clone();
            crate::log::error(&format!("robot subprocess crashed: {tail}"));
            super::super::emit::run_crashed(
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

    let test_outcome = json_str(&test_end, "outcome")
        .unwrap_or(super::super::outcomes::ERROR)
        .to_string();
    let exit_code = match test_outcome.as_str() {
        super::super::outcomes::PASS => 0,
        super::super::outcomes::ERROR => 5,
        _ => 1,
    };

    let request = match build_request(
        &test_end,
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

    if let Err(e) = super::super::queue::enqueue(&db, &queue_id, &mut queued, Some(&upload_bus)) {
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
            let upload_bus_for_task = upload_bus.clone();
            let handle = tokio::spawn(async move {
                upload_queued_run(
                    crate::http::client(),
                    &upload_creds,
                    &upload_queue_id,
                    &queued,
                    Some(&upload_bus_for_task),
                    true,
                )
                .await;
            });
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
    phases: &[serde_json::Value],
    procedure_id: &str,
    procedure_dir: &Path,
    unit: &ResolvedUnit,
    operated_by: Option<&str>,
) -> crate::error::CliResult<RunCreateRequest> {
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
            json_str(test_end, "outcome").unwrap_or(super::super::outcomes::ERROR),
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

    let part_number = meta_str("part_number")
        .or_else(|| unit.part_number.clone())
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

    if let Some(version) = super::super::procedure_version::read_procedure_version(procedure_dir) {
        b = b.procedure_version(version);
    }

    if let Some(deployment_id) = super::super::deployment_id::lookup_deployment_id(procedure_id) {
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

    let raw_outcome = json_str(p, "outcome").unwrap_or(super::super::outcomes::FAIL);
    let mut b = RunCreatePhases::builder()
        .name(json_str(p, "name").ok_or("missing name")?)
        .outcome(parse_phase_outcome(raw_outcome))
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
    // The pytest connector also handles `x_axis` / `y_axis` chart
    // payloads here; the Robot keyword library (`tofupilot_robot.py`)
    // doesn't expose a chart-shaped Measure keyword, so there is no
    // producer of those payloads on the Robot side. If a future
    // `Measure Series` (or similar) keyword is added, copy the
    // pytest.rs:908-917 chart branch + the `build_x_axis`,
    // `build_y_axis`, `json_f64_vec` helpers from pytest.rs over.
    let outcome = match json_str(m, "outcome").unwrap_or("UNSET") {
        super::super::outcomes::PASS => Outcome::Pass,
        super::super::outcomes::FAIL => Outcome::Fail,
        _ => Outcome::Unset,
    };
    let mut b = RunCreateMeasurements::builder()
        .name(json_str(m, "name").ok_or("missing name")?)
        .outcome(outcome);

    if let Some(mv) = m.get("measured_value") {
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
        b = b.outcome(super::super::outcomes::validator_outcome_from_wire(s));
    }
    if let Some(d) = v.get("is_decisive").and_then(|v| v.as_bool()) {
        b = b.is_decisive(d);
    }
    b.build().ok()
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
    match (op.is_empty(), expected.is_empty()) {
        (true, true) => None,
        (false, true) => Some(format!("x {op}")),
        (true, false) => Some(expected),
        (false, false) => Some(format!("x {op} {expected}")),
    }
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

fn json_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|v| v.as_str())
}

fn json_millis(v: &serde_json::Value, key: &str) -> DateTime<Utc> {
    v.get(key)
        .and_then(|v| v.as_i64())
        .and_then(DateTime::from_timestamp_millis)
        .unwrap_or_else(Utc::now)
}

fn parse_outcome(s: &str) -> RunGetOutcome {
    use super::super::outcomes::*;
    match s {
        PASS => RunGetOutcome::Pass,
        FAIL => RunGetOutcome::Fail,
        TIMEOUT => RunGetOutcome::Timeout,
        ABORTED => RunGetOutcome::Aborted,
        _ => RunGetOutcome::Error,
    }
}

fn parse_phase_outcome(s: &str) -> RunGetPhasesOutcome {
    use super::super::outcomes::*;
    match s {
        PASS => RunGetPhasesOutcome::Pass,
        SKIP => RunGetPhasesOutcome::Skip,
        ERROR => RunGetPhasesOutcome::Error,
        _ => RunGetPhasesOutcome::Fail,
    }
}

fn canonical_phase_key(phase_keys: &[(String, String)], name: &str) -> String {
    phase_keys
        .iter()
        .find(|(_, n)| n == name)
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| name.to_string())
}

fn emit_phase_started(
    router: &super::super::event_router::EventRouter,
    started: &mut std::collections::HashSet<(String, u32)>,
    phase_key: &str,
    phase_name: &str,
    attempt: u32,
) {
    if !started.insert((phase_key.to_string(), attempt)) {
        return;
    }
    router.phase_started(phase_key, phase_name, attempt, None, None);
}

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

    #[test]
    fn robotframework_requirement_matches_whole_token_only() {
        assert!(contains_robotframework_requirement("robotframework"));
        assert!(contains_robotframework_requirement("robotframework==7.0"));
        assert!(contains_robotframework_requirement("\"robotframework>=6\""));
        // `robotframework-seleniumlibrary` is a different package.
        assert!(!contains_robotframework_requirement(
            "robotframework-seleniumlibrary"
        ));
        assert!(!contains_robotframework_requirement("myrobotframework"));
        assert!(!contains_robotframework_requirement("robot"));
    }

    #[test]
    fn detects_robot_via_robot_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tests.robot"), "*** Test Cases ***\n").unwrap();
        assert!(has_robot(dir.path()));
    }

    #[test]
    fn detects_robot_via_pyproject_dependency() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            r#"
[project]
name = "x"
version = "0.1"
dependencies = ["robotframework>=7"]
"#,
        )
        .unwrap();
        assert!(has_robot(dir.path()));
    }

    #[test]
    fn detects_robot_via_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("manifest.json"),
            r#"{"framework": "robot"}"#,
        )
        .unwrap();
        assert!(has_robot(dir.path()));
    }

    #[test]
    fn rejects_robotframework_subname() {
        // Hypothetical fork shouldn't trip detection.
        assert!(!contains_robotframework_requirement(
            "robotframework-fork>=1.0"
        ));
        assert!(contains_robotframework_requirement("robotframework>=7"));
    }

    #[test]
    fn rejects_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_robot(dir.path()));
    }
}
