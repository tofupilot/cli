use base64::Engine;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::ChildStderr;
use tokio::sync::RwLock;

use crate::event_sink::{EventSink, ExecutionEvent};
use crate::job::{Job, JobResult};
use crate::plugs::process::ChildProcess;
use crate::protocol;
use crate::transport;

/// Best-effort mimetype for an attachment from its filename. The
/// operator-UI gates image rendering on an `image/*` mimetype, so an
/// attachment with no mimetype never renders even when its bytes are an
/// image. `attach.data`/`attach_file` don't carry a content type, so we
/// derive one from the extension. Falls back to `application/octet-stream`
/// for unknown extensions so the upload PUT always carries a real
/// Content-Type (the server reads it back via HeadObject on finalize).
fn guess_attachment_mimetype(name: &str) -> Option<String> {
    Some(
        mime_guess::from_path(name)
            .first()
            .map(|m| m.essence_str().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string()),
    )
}

/// Write attachment bytes into the caller-supplied dir with a collision-safe
/// `<uuid8>_<name>` filename and return the stored path. Mirrors the naming
/// the removed ReportManager used so the kiosk `/attachments/*` route and the
/// CLI upload queue resolve the same basename.
fn store_attachment_bytes(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stored_name = format!("{}_{}", &uuid::Uuid::new_v4().to_string()[..8], name);
    let dest = dir.join(stored_name);
    std::fs::write(&dest, bytes)?;
    Ok(dest)
}

/// Copy an existing file into the attachment dir with the same collision-safe
/// `<uuid8>_<name>` naming. Uses `fs::copy` (kernel-level, reflink-capable on
/// CoW filesystems) rather than read-into-RAM-then-write, so a large file
/// (scope capture, video) never gets buffered whole in memory.
fn copy_attachment_file(dir: &Path, name: &str, source: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stored_name = format!("{}_{}", &uuid::Uuid::new_v4().to_string()[..8], name);
    let dest = dir.join(stored_name);
    std::fs::copy(source, &dest)?;
    Ok(dest)
}

/// Convert protocol Measurement to internal Measurement.
/// Returns None if value cannot be parsed.
fn try_measurement_from_protocol(
    m: protocol::Measurement,
) -> Option<crate::measurements::Measurement> {
    let value = serde_json::from_value(m.value).ok()?;
    let aggregations = m.aggregations.and_then(|v| serde_json::from_value(v).ok());
    Some(crate::measurements::Measurement {
        name: m.name,
        value,
        unit: m.unit,
        timestamp: m.timestamp,
        validators: None,
        aggregations,
        description: None,
        // Finalized by `evaluate_measurements` at phase complete.
        outcome: crate::procedure::schema::ValidatorOutcome::Unset,
    })
}

#[derive(Debug, Clone)]
pub struct Worker {
    pub id: usize,
    inner: Arc<RwLock<Option<ChildProcess>>>,
    procedure_dir: PathBuf,
    /// Pre-resolved Python interpreter. When set, `start()` skips the
    /// `resolve_python` walk-up and uses this path directly. CLI runs
    /// always set this (deterministic `<package_dir>/venv/python`); legacy
    /// callers (Studio, tests) leave it `None` and keep the walk-up.
    python_path: Option<PathBuf>,
    /// When `Some(port)`, the worker subprocess gets TP_DEBUG +
    /// TP_DEBUG_PORT so tp_worker.py starts the debugpy listener. Only
    /// set on pool workers; respawn and teardown workers stay None.
    debug_port: Option<u16>,
}

/// Startup stage of a dispatched Python job. Bounds each socket read
/// until user code owns the clock (see the loop in
/// `execute_python_phase`). Kept as data, not control flow, so the
/// stage → deadline / stage → error mapping is unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStage {
    /// Command written, nothing heard back yet.
    AwaitingAck,
    /// `JobAck` (or any first event) seen; phase module importing.
    Loading,
    /// `ModuleLoaded` seen; the phase body is running.
    Executing,
}

/// Marker substrings embedded in startup-stall errors. The orchestrator's
/// worker-replacement branch matches on these — keep both sides tied to
/// these consts so editing one can't silently detach pool replacement.
pub const STALL_MARKER_ACK: &str = "did not acknowledge";
pub const STALL_MARKER_IMPORT: &str = "stalled while importing";

impl JobStage {
    /// Max silence tolerated on the worker socket in this stage.
    /// `None` = unbounded (phase timeout / operator owns it).
    fn read_deadline(self) -> Option<std::time::Duration> {
        use crate::constants::timeouts::{JOB_ACK_TIMEOUT_SECS, MODULE_IMPORT_TIMEOUT_SECS};
        match self {
            JobStage::AwaitingAck => Some(std::time::Duration::from_secs(JOB_ACK_TIMEOUT_SECS)),
            JobStage::Loading => Some(std::time::Duration::from_secs(MODULE_IMPORT_TIMEOUT_SECS)),
            JobStage::Executing => None,
        }
    }

    /// Actionable operator-facing error for a stall in this stage.
    fn stall_error(self, job: &crate::job::Job) -> String {
        use crate::constants::timeouts::{JOB_ACK_TIMEOUT_SECS, MODULE_IMPORT_TIMEOUT_SECS};
        match self {
            JobStage::AwaitingAck => format!(
                "Python worker {STALL_MARKER_ACK} phase '{}' within {}s. The worker \
                 process is alive but not responding — on locked-down machines this is \
                 usually endpoint-protection (EDR/antivirus) software suspending the \
                 freshly spawned Python interpreter. Check the machine's security \
                 software logs, and ask IT to allowlist the deployment's venv Python.",
                job.phase_name, JOB_ACK_TIMEOUT_SECS
            ),
            JobStage::Loading => format!(
                "Phase '{}' {STALL_MARKER_IMPORT} module '{}': no progress for {}s. \
                 Module-level code is blocked — a hung import (endpoint protection \
                 scanning the venv), a device or network connection opened at import \
                 time, or an infinite loop at module scope. Move hardware/network \
                 opens into the phase function, or allowlist the deployment's venv in \
                 your endpoint-protection software.",
                job.phase_name, job.module, MODULE_IMPORT_TIMEOUT_SECS
            ),
            JobStage::Executing => unreachable!("Executing stage has no read deadline"),
        }
    }
}

/// Outcome of one stage-bounded read off the worker socket.
enum StageRead {
    Event(protocol::WorkerEvent),
    Eof,
    Io(String),
    /// The stage's deadline elapsed with total silence.
    Stalled,
}

/// One read off the worker event stream, bounded by the current job
/// stage's deadline (none in debug mode or once `Executing`). Generic
/// over the reader so the stall behavior is testable against an
/// in-memory duplex without a live worker process.
async fn read_stage_event<R>(reader: &mut R, stage: JobStage, debug: bool) -> StageRead
where
    R: tokio::io::AsyncBufRead + Unpin + Send,
{
    let deadline = if debug { None } else { stage.read_deadline() };
    let read = match deadline {
        Some(d) => match tokio::time::timeout(d, transport::read_json_line(reader)).await {
            Ok(r) => r,
            Err(_elapsed) => return StageRead::Stalled,
        },
        None => transport::read_json_line(reader).await,
    };
    match read {
        Ok(Some(event)) => StageRead::Event(event),
        Ok(None) => StageRead::Eof,
        Err(e) => StageRead::Io(e),
    }
}

/// Build the subprocess env for a worker. When `debug_port` is set the
/// worker gets `TP_DEBUG=1` + `TP_DEBUG_PORT`, which makes tp_worker.py
/// start a debugpy listener. Pure so the env contract is unit-testable.
fn build_worker_env(worker_id: usize, debug_port: Option<u16>) -> Vec<(String, String)> {
    let mut env_vars = vec![("WORKER_ID".to_string(), worker_id.to_string())];
    if let Some(port) = debug_port {
        env_vars.push(("TP_DEBUG".to_string(), "1".to_string()));
        env_vars.push(("TP_DEBUG_PORT".to_string(), port.to_string()));
    }
    env_vars
}

impl Worker {
    pub fn new(id: usize, procedure_dir: PathBuf) -> Self {
        Self::new_with_python(id, procedure_dir, None)
    }

    pub fn new_with_python(
        id: usize,
        procedure_dir: PathBuf,
        python_path: Option<PathBuf>,
    ) -> Self {
        Self {
            id,
            inner: Arc::new(RwLock::new(None)),
            procedure_dir,
            python_path,
            debug_port: None,
        }
    }

    /// Builder: mark this worker as a debug worker (adds TP_DEBUG +
    /// TP_DEBUG_PORT to the subprocess env at spawn). `None` = no debug.
    pub fn with_debug_port(mut self, debug_port: Option<u16>) -> Self {
        self.debug_port = debug_port;
        self
    }

    /// Bundled worker script, embedded at compile time.
    const WORKER_SCRIPT: &'static str = include_str!("../../python/tp_worker.py");

    fn find_worker_script_cli() -> Result<PathBuf, String> {
        // First check next to the executable (Studio/packaged layout).
        if let Some(p) = super::embedded_script::next_to_exe("tp_worker.py") {
            return Ok(p);
        }
        // Otherwise extract to a versioned per-user dir. Writing to
        // `%TEMP%` triggers Windows Defender ASR rules and several
        // enterprise EDR policies, surfacing as `os error 5` on spawn.
        super::embedded_script::extract_to_runtime_dir("tp_worker.py", Self::WORKER_SCRIPT)
    }

    pub async fn start(&mut self, event_sink: &Arc<dyn EventSink>) -> Result<(), String> {
        let python_cmd =
            crate::python::resolve_or_walk(&self.python_path, &self.procedure_dir).await?;
        self.start_with_python(event_sink, &python_cmd).await
    }

    pub async fn start_with_python(
        &mut self,
        _event_sink: &Arc<dyn EventSink>,
        python_cmd: &str,
    ) -> Result<(), String> {
        // `canonicalize_for_spawn` strips Windows `\\?\` extended-length
        // prefix so `CreateProcessW` accepts the path as a working
        // directory under AV/EDR policies that reject UNC-prefixed cwd.
        let abs_procedure_dir = crate::path_utils::canonicalize_for_spawn(&self.procedure_dir)
            .map_err(|e| format!("Failed to canonicalize procedure dir: {}", e))?;

        let worker_script = Self::find_worker_script_cli()?;

        log::debug!(
            "Worker {} using NDJSON script: {}",
            self.id,
            worker_script.display()
        );

        let worker_id = self.id;

        let env_vars = build_worker_env(worker_id, self.debug_port);

        let process = ChildProcess::spawn(
            python_cmd,
            worker_script,
            vec![abs_procedure_dir.to_string_lossy().to_string()],
            Some(&abs_procedure_dir),
            env_vars,
            Some(Box::new(move |stderr| {
                Self::spawn_stderr_reader_static(worker_id, stderr);
            })),
        )
        .await?;

        log::debug!("Worker {} TCP port: {}", self.id, process.port);

        let mut inner = self.inner.write().await;
        *inner = Some(process);

        Ok(())
    }

    pub async fn execute_python_phase(
        &self,
        job: Job,
        plug_ports: HashMap<String, u16>,
        event_sink: Arc<dyn EventSink>,
        attachment_dir: Option<PathBuf>,
    ) -> Result<JobResult, String> {
        let start_time = chrono::Utc::now();

        // Emit UI request if phase has components
        let has_ui = !job.ui_config.components.is_empty();
        let requires_user_input = job.ui_config.requires_user_input();

        let ui_response_rx = if has_ui && requires_user_input {
            let request_id = format!("{}_{}", job.id, chrono::Utc::now().timestamp_millis());

            let (tx, rx) = tokio::sync::oneshot::channel();
            {
                let mut channels = crate::ui::UI_RESPONSE_CHANNELS.lock().await;
                channels.insert(request_id.clone(), tx);
            }

            let event_data = crate::ui::UiRequestData {
                request_id: request_id.clone(),
                job_id: job.id.to_string(),
                pipe_path: String::new(),
                config: job.ui_config.clone(),
                phase_key: job.phase_key.clone(),
                slot_id: job.slot_id.clone(),
            };

            event_sink.emit(&ExecutionEvent::UiRequest(event_data));
            log::debug!(
                "Sent UI request {} for Python phase {}",
                request_id,
                job.phase_name
            );

            Some((request_id, rx))
        } else if has_ui && !requires_user_input {
            // Display-only UI, emit but don't wait
            let request_id = format!("{}_{}", job.id, chrono::Utc::now().timestamp_millis());
            let event_data = crate::ui::UiRequestData {
                request_id: request_id.clone(),
                job_id: job.id.to_string(),
                pipe_path: String::new(),
                config: job.ui_config.clone(),
                phase_key: job.phase_key.clone(),
                slot_id: job.slot_id.clone(),
            };

            event_sink.emit(&ExecutionEvent::UiRequest(event_data));
            None
        } else {
            None
        };

        // Build unit_info for NDJSON if available
        let ndjson_unit_info = job.initial_unit_info.as_ref().map(|ui| protocol::UnitInfo {
            serial_number: ui.serial_number.clone(),
            part_number: ui.part_number.clone(),
            revision_number: ui.revision_number.clone(),
            batch_number: ui.batch_number.clone(),
            sub_units: ui.sub_units.clone().unwrap_or_default(),
            metadata: ui.metadata.clone(),
        });

        // Build command. The Python worker doesn't read UI fields off
        // the command (mid-phase mutations come back via UiUpdate
        // events keyed by attribute name on `ui.<key>`), so the
        // operator-UI config is omitted from the IPC payload entirely.
        let command = protocol::JobCommand {
            job_id: job.id.to_string(),
            slot_id: job
                .slot_id
                .clone()
                .unwrap_or_else(|| "<shared>".to_string()),
            phase_name: job.phase_name.clone(),
            module: job.module.clone(),
            function: job.function.clone(),
            plugs: plug_ports
                .into_iter()
                .map(|(k, v)| {
                    (
                        crate::python::to_python_identifier(&k),
                        format!("127.0.0.1:{}", v),
                    )
                })
                .collect(),
            timeout_ms: job.timeout_ms,
            retry_count: job.retry_count as u32,
            retry_limit: job.retry_limit as u32,
            unit_info: ndjson_unit_info,
            phase_results: job.phase_results.clone(),
        };

        // Connect to worker TCP and send command
        let port = {
            let inner = self.inner.read().await;
            inner.as_ref().ok_or("Worker not started")?.port
        };

        let stream = TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .map_err(|e| format!("TCP connect to worker failed: {}", e))?;

        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);

        // Send command as JSON line
        transport::write_json_line(&mut write_half, &command).await?;

        // Startup-stage deadlines. The Python-side phase timeout is
        // enforced from inside the worker interpreter, so it cannot fire
        // when that interpreter is wedged before the phase body runs — a
        // suspended process (EDR) or a blocked module-level import used
        // to hang the run forever with no diagnostic. Each read is
        // bounded by the current stage until user code owns the clock:
        //   AwaitingAck  — command sent, nothing heard: pure liveness.
        //   Loading      — acked, module importing: any streamed event
        //                  (log line, etc.) resets the window.
        //   Executing    — `ModuleLoaded` seen: no Rust-side deadline;
        //                  the phase timeout / operator owns it.
        // Debug workers skip all deadlines — a breakpoint pause is
        // indistinguishable from a stall.
        let mut stage = JobStage::AwaitingAck;

        // Read streaming events until EOF
        loop {
            let debug = self.debug_port.is_some();
            let event = match read_stage_event(&mut reader, stage, debug).await {
                StageRead::Event(e) => e,
                StageRead::Eof => break,
                StageRead::Io(e) => return Err(e),
                StageRead::Stalled => {
                    // Alive-but-silent worker. Kill it so it can't
                    // linger wedged behind the pool, then fail the
                    // job with a stage-specific, actionable error —
                    // this propagates as a phase ERROR and turns the
                    // former infinite spinner into a diagnosis.
                    self.kill_wedged().await;
                    return Err(stage.stall_error(&job));
                }
            };

            // Any traffic proves the interpreter is executing our
            // connector code, so an un-acked stage can advance on any
            // first event (robust to reordering), and events streamed
            // mid-import (module-level prints) reset the import window
            // simply by arriving.
            if matches!(stage, JobStage::AwaitingAck) {
                stage = JobStage::Loading;
            }

            match event {
                protocol::WorkerEvent::JobAck(_) => {
                    // Stage already advanced above. Surface the transition
                    // as a phase log so the operator UI's Advanced tab
                    // shows startup progress instead of dead air — the
                    // exact signal Sander's stalled run was missing.
                    self.emit_stage_log(&job, &event_sink, "Python worker acknowledged phase, importing module...");
                }
                protocol::WorkerEvent::ModuleLoaded(_) => {
                    stage = JobStage::Executing;
                    self.emit_stage_log(&job, &event_sink, "Module imported, running phase function");
                }
                protocol::WorkerEvent::JobComplete(result) => {
                    // Check phase result to determine if we should wait for UI
                    let phase_result = result
                        .phase_result_json
                        .as_ref()
                        .and_then(|json| serde_json::from_str(json).ok())
                        .and_then(|pr: crate::ui::PythonPhaseResult| {
                            crate::job::PhaseResult::from_python_result(&pr).ok()
                        })
                        .unwrap_or(crate::job::PhaseResult::Continue);

                    let is_terminal = matches!(
                        phase_result,
                        crate::job::PhaseResult::Skip
                            | crate::job::PhaseResult::Stop
                            | crate::job::PhaseResult::Fail
                    ) || result.error.is_some();

                    let mut ui_unit_info: Option<crate::unit::UnitInfo> = None;
                    let mut ui_bound_measurements: Option<HashMap<String, serde_json::Value>> =
                        None;
                    // True when the UI was required but we never got a value.
                    // Forces the phase to ERROR so a silent timeout can't pass.
                    let mut ui_missing_required = false;
                    if let Some((request_id, mut rx)) = ui_response_rx {
                        match rx.try_recv() {
                            Ok(ui_values) => {
                                log::debug!("UI already submitted for phase {}", job.phase_name);
                                if let Some((unit_info, bound)) =
                                    extract_bound_measurements(&ui_values)
                                {
                                    ui_unit_info = unit_info;
                                    ui_bound_measurements = Some(bound);
                                }
                            }
                            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                                if is_terminal {
                                    log::debug!(
                                        "Python phase {} returned terminal result {:?}, dismissing UI",
                                        job.phase_name, phase_result
                                    );
                                    drop(rx);
                                    let mut channels = crate::ui::UI_RESPONSE_CHANNELS.lock().await;
                                    channels.remove(&request_id);
                                } else {
                                    log::debug!(
                                        "Python phase {} finished, waiting for UI submission",
                                        job.phase_name
                                    );
                                    match rx.await {
                                        Ok(ui_values) => {
                                            log::debug!(
                                                "Received UI submission for phase {}",
                                                job.phase_name
                                            );
                                            if let Some((unit_info, bound)) =
                                                extract_bound_measurements(&ui_values)
                                            {
                                                ui_unit_info = unit_info;
                                                ui_bound_measurements = Some(bound);
                                            }
                                        }
                                        Err(_) => {
                                            log::warn!(
                                                "UI response channel closed for phase {}",
                                                job.phase_name
                                            );
                                            ui_missing_required = true;
                                            let mut channels =
                                                crate::ui::UI_RESPONSE_CHANNELS.lock().await;
                                            channels.remove(&request_id);
                                        }
                                    }
                                }
                            }
                            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                                log::warn!(
                                    "UI response channel closed for phase {}",
                                    job.phase_name
                                );
                                ui_missing_required = true;
                                let mut channels = crate::ui::UI_RESPONSE_CHANNELS.lock().await;
                                channels.remove(&request_id);
                            }
                        }
                    }

                    let end_time = chrono::Utc::now();

                    let phase_measurements = job.phase_measurements.clone();

                    let mut job_result = convert_job_result(result, start_time, end_time, &job)?;

                    // A required UI input that was never answered (channel
                    // closed) must fail the phase — the framework can't trust
                    // the body's PASS result if the operator never confirmed.
                    // Don't clobber a pre-existing Error/Fail/Timeout diagnostic
                    // from the phase body (e.g. "measurement X failed"); only
                    // override outcomes that would otherwise look successful.
                    if ui_missing_required
                        && matches!(
                            job_result.phase_outcome,
                            crate::job::Outcome::Pass | crate::job::Outcome::Retry
                        )
                    {
                        job_result.phase_outcome = crate::job::Outcome::Error;
                        // Surface the orchestrator's cancel reason when set
                        // (e.g. "Run aborted by phase 'X': <traceback>") so
                        // the operator sees the *cause* of the cancellation
                        // instead of a generic "cancelled or timed out".
                        // Falls back to the generic message when the channel
                        // closed for an unrelated reason (operator timeout,
                        // direct cancel from the agent stdin reader, etc.).
                        let reason = crate::ui::channels::CANCEL_REASON.read().await.clone();
                        job_result.error = Some(match reason {
                            Some(r) => r,
                            None => "Required UI input was cancelled or timed out — phase cannot complete without operator response".to_string(),
                        });
                    }

                    // Merge UI bound measurements
                    if let Some(bound) = ui_bound_measurements {
                        let existing_names: std::collections::HashSet<String> = job_result
                            .measurements
                            .iter()
                            .map(|m| m.name.clone())
                            .collect();
                        let bound_measurements = convert_bound_to_measurements(bound);

                        let phase_config = crate::procedure::schema::PhaseDefinition {
                            measurements: phase_measurements,
                            key: String::new(),
                            name: String::new(),
                            scope: None,
                            python: None,
                            executable: None,
                            description: None,
                            depends_on: Vec::new(),
                            ui: None,
                            enabled: true,
                            result: None,
                            timeout: None,
                            retry: None,
                            then: None,
                        };
                        let evaluated_bound = crate::measurements::auto_evaluate_measurements(
                            bound_measurements,
                            &phase_config,
                        );

                        for m in evaluated_bound {
                            if !existing_names.contains(&m.name) {
                                job_result.measurements.push(m);
                            }
                        }
                    }

                    if let Some(ui_unit) = ui_unit_info {
                        let merged = merge_unit_info(job_result.unit, ui_unit);
                        // Mid-run unit update: re-publish the resolved
                        // unit through the same event used pre-run so
                        // every consumer (operator-UI, dashboard,
                        // agent stream) updates its `RunState.unit`
                        // field-level. Without this the dashboard
                        // sees the new unit at upload time but the
                        // operator-UI's running screen stays stale.
                        event_sink.emit(&ExecutionEvent::UnitIdentified {
                            slot_id: job.slot_id.clone(),
                            unit_info: merged.clone(),
                        });
                        job_result.unit = Some(merged);
                    }

                    return Ok(job_result);
                }
                protocol::WorkerEvent::Error(err) => {
                    return Err(err.message);
                }
                protocol::WorkerEvent::AttachFile(attach_event) => {
                    let source_path = attach_event.source_path.clone();
                    let attachment_name = attach_event.attachment_name.clone();

                    // Copy the source into the caller-owned attachment dir so
                    // the emitted path points at a file the CLI controls and
                    // deletes after upload. Without a dir (legacy callers),
                    // emit the original source path unchanged.
                    let emitted_path = match attachment_dir {
                        Some(ref dir) => {
                            match copy_attachment_file(dir, &attachment_name, &source_path) {
                                Ok(dest) => Some(dest.to_string_lossy().into_owned()),
                                Err(e) => {
                                    log::warn!(
                                        "Failed to copy attachment file {}: {}",
                                        attachment_name,
                                        e
                                    );
                                    Some(source_path.clone())
                                }
                            }
                        }
                        None => Some(source_path.clone()),
                    };

                    event_sink.emit(&ExecutionEvent::AttachmentAdded {
                        phase_key: job.phase_key.clone(),
                        slot_id: job.slot_id.clone(),
                        name: attachment_name.clone(),
                        path: emitted_path,
                        // Guess from the filename so the operator-UI can tell
                        // images apart (it gates `<img>` rendering on an
                        // `image/*` mimetype).
                        mimetype: guess_attachment_mimetype(&attachment_name),
                    });
                }
                protocol::WorkerEvent::AttachData(attach_event) => {
                    // Write the bytes to the attachment dir FIRST so the live
                    // event can carry the stored on-disk path — the kiosk
                    // serves attachment images from it (`/attachments/*`)
                    // until the upload queue deletes the file. Emitting
                    // before the write would force `path: None` and leave
                    // the kiosk unable to render `attach.data` images.
                    let mut stored_path: Option<String> = None;
                    if let Some(ref dir) = attachment_dir {
                        match base64::engine::general_purpose::STANDARD.decode(&attach_event.data) {
                            Ok(bytes) => {
                                match store_attachment_bytes(
                                    dir,
                                    &attach_event.attachment_name,
                                    &bytes,
                                ) {
                                    Ok(dest) => {
                                        stored_path = Some(dest.to_string_lossy().into_owned());
                                    }
                                    Err(e) => {
                                        log::warn!(
                                            "Failed to store attachment data {}: {}",
                                            attach_event.attachment_name,
                                            e
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to decode base64 for {}: {}",
                                    attach_event.attachment_name,
                                    e
                                );
                            }
                        }
                    }
                    // Emit after the write so `path` points at the stored
                    // file (None if no attachment dir was active or the decode
                    // failed — the remote UI still resolves via upload id).
                    event_sink.emit(&ExecutionEvent::AttachmentAdded {
                        phase_key: job.phase_key.clone(),
                        slot_id: job.slot_id.clone(),
                        name: attach_event.attachment_name.clone(),
                        path: stored_path,
                        mimetype: guess_attachment_mimetype(&attach_event.attachment_name),
                    });
                }
                protocol::WorkerEvent::UiUpdate(ui_event) => {
                    let update_event = crate::events::UiUpdateEvent {
                        job_id: job.id.to_string(),
                        slot_id: job.slot_id.as_deref().unwrap_or("<shared>").to_string(),
                        phase_key: job.phase_key.clone(),
                        worker_id: self.id,
                        action: ui_event.action.clone(),
                        data: serde_json::from_str(&ui_event.data_json).unwrap_or_default(),
                    };

                    event_sink.emit(&ExecutionEvent::UiUpdate(update_event));
                }
                protocol::WorkerEvent::PhaseLogLine(line_event) => {
                    event_sink.emit(&ExecutionEvent::PhaseLogLine {
                        job_id: line_event.job_id.clone(),
                        phase_key: job.phase_key.clone(),
                        slot_id: job.slot_id.clone(),
                        level: line_event.level.clone(),
                        message: line_event.message.clone(),
                        timestamp: line_event.timestamp.clone(),
                        file: line_event.file.clone(),
                        line: line_event.line,
                    });
                }
                protocol::WorkerEvent::MeasurementRecorded(m) => {
                    event_sink.emit(&ExecutionEvent::MeasurementRecorded {
                        job_id: m.job_id.clone(),
                        phase_key: job.phase_key.clone(),
                        slot_id: job.slot_id.clone(),
                        name: m.name.clone(),
                        value: serde_json::from_str(&m.value_json)
                            .unwrap_or(serde_json::Value::Null),
                        unit: m.unit.clone(),
                        timestamp: m.timestamp.clone(),
                    });
                }
            }
        }

        Err("Worker stream ended without job completion".to_string())
    }

    pub async fn shutdown(&mut self) -> Result<(), String> {
        let mut inner = self.inner.write().await;

        if let Some(ref mut process) = *inner {
            let result = process.graceful_shutdown_signal(5).await;
            inner.take();
            result
        } else {
            Ok(())
        }
    }

    pub async fn force_shutdown(&mut self) -> Result<(), String> {
        let mut inner = self.inner.write().await;

        if let Some(ref mut process) = *inner {
            let result = process.force_kill().await;
            inner.take();
            result
        } else {
            Ok(())
        }
    }

    pub async fn interrupt_current_job(&mut self) -> Result<(), String> {
        self.force_shutdown().await
    }

    /// DEBUG-level startup-stage breadcrumb attached to the running
    /// phase. Streams through the normal `PhaseLogLine` channel so every
    /// consumer (operator-UI Advanced tab, TUI, agent protocol) shows
    /// startup progress without a new event type.
    fn emit_stage_log(&self, job: &Job, event_sink: &Arc<dyn EventSink>, message: &str) {
        event_sink.emit(&ExecutionEvent::PhaseLogLine {
            job_id: job.id.to_string(),
            phase_key: job.phase_key.clone(),
            slot_id: job.slot_id.clone(),
            level: "DEBUG".to_string(),
            message: message.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            file: None,
            line: None,
        });
    }

    /// Kill a worker whose interpreter is alive but wedged (startup-stage
    /// deadline tripped). `&self` variant of `force_shutdown`: the job
    /// loop only holds a shared ref, and interior mutability via the
    /// inner lock is all a kill needs. Best-effort — the job error it
    /// accompanies is the signal that matters.
    async fn kill_wedged(&self) {
        let mut inner = self.inner.write().await;
        if let Some(ref mut process) = *inner {
            let _ = process.force_kill().await;
        }
        inner.take();
    }

    pub async fn shutdown_with_timeout(&mut self, timeout_ms: u64) -> Result<(), String> {
        let shutdown_result = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            self.shutdown(),
        )
        .await;

        match shutdown_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => self.force_shutdown().await,
        }
    }

    fn spawn_stderr_reader_static(worker_id: usize, stderr: ChildStderr) {
        tokio::spawn(async move {
            let mut stderr_reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match stderr_reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            log::warn!("Worker {} Python stderr: {}", worker_id, trimmed);
                        }
                    }
                    Err(e) => {
                        // `unwrap_or(0)` used to mask broken-pipe and
                        // any other IO error as EOF, silencing the
                        // exact diagnostics an operator needs when AV
                        // mid-killed the python worker.
                        log::warn!("Worker {} stderr reader stopped: {}", worker_id, e);
                        break;
                    }
                }
            }
        });
    }

    pub async fn execute_job(
        &self,
        job: Job,
        plug_ports: HashMap<String, u16>,
        event_sink: Arc<dyn EventSink>,
        attachment_dir: Option<PathBuf>,
    ) -> Result<JobResult, String> {
        log::debug!(
            "Worker {} executing {} phase: {}",
            self.id,
            match job.runtime_type {
                crate::job::RuntimeType::Native => "native Rust",
                crate::job::RuntimeType::Python => "Python",
                crate::job::RuntimeType::Shell => "shell",
            },
            job.phase_name
        );

        match job.runtime_type {
            crate::job::RuntimeType::Native => self.execute_native_phase(job, event_sink).await,
            crate::job::RuntimeType::Python => {
                self.execute_python_phase(job, plug_ports, event_sink, attachment_dir)
                    .await
            }
            crate::job::RuntimeType::Shell => self.execute_shell_phase(job).await,
        }
    }

    pub async fn execute_shell_phase(&self, job: Job) -> Result<JobResult, String> {
        let start_time = chrono::Utc::now();
        let mut logs = Vec::new();

        let command = job
            .command
            .as_ref()
            .ok_or_else(|| "No command specified for shell phase".to_string())?;

        let working_dir = crate::worker::runtime::shell::resolve_working_directory(
            job.working_directory.as_deref(),
            job.procedure_dir.as_deref(),
        );

        if !working_dir.exists() {
            return Err(format!(
                "Working directory does not exist: {}",
                working_dir.display()
            ));
        }

        let shell_builder =
            crate::worker::runtime::shell::ShellCommandBuilder::new(job.shell_type.as_deref())?;
        let shell_type = shell_builder.shell_type().to_string();

        logs.push(crate::log::LogEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: "INFO".to_string(),
            message: format!("Executing command with {}: {}", shell_type, command),
            file: None,
            line: None,
        });

        logs.push(crate::log::LogEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: "INFO".to_string(),
            message: format!("Working directory: {}", working_dir.display()),
            file: None,
            line: None,
        });

        let mut resource_tracker = crate::monitoring::ResourceTracker::new();

        let child = shell_builder
            .command(command)
            .working_dir(&working_dir)
            .with_stdio(
                std::process::Stdio::piped(),
                std::process::Stdio::piped(),
                std::process::Stdio::piped(),
            )
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    format!(
                        "Shell '{}' not found. Make sure it's installed and in PATH.",
                        shell_type
                    )
                } else {
                    format!("Failed to execute command with {}: {}", shell_type, e)
                }
            })?;

        let pid = child.id();
        resource_tracker.start_tracking(pid);

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| format!("Failed to wait for command: {}", e))?;

        if !output.stdout.is_empty() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                logs.push(crate::log::LogEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    level: "INFO".to_string(),
                    message: line.to_string(),
                    file: None,
                    line: None,
                });
            }
        }

        if !output.stderr.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stderr.lines() {
                logs.push(crate::log::LogEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    level: "ERROR".to_string(),
                    message: line.to_string(),
                    file: None,
                    line: None,
                });
            }
        }

        let shell_exit_code = output.status.code();
        let (phase_result, error) = if output.status.success() {
            logs.push(crate::log::LogEntry {
                timestamp: chrono::Utc::now().to_rfc3339(),
                level: "INFO".to_string(),
                message: "Command succeeded with exit code 0".to_string(),
                file: None,
                line: None,
            });
            (crate::job::PhaseResult::Continue, None)
        } else {
            let exit_code = shell_exit_code.unwrap_or(-1);
            logs.push(crate::log::LogEntry {
                timestamp: chrono::Utc::now().to_rfc3339(),
                level: "ERROR".to_string(),
                message: format!("Command failed with exit code {}", exit_code),
                file: None,
                line: None,
            });
            (crate::job::PhaseResult::Fail, None)
        };

        let end_time = chrono::Utc::now();

        let resource_metrics = resource_tracker.collect_metrics();

        Ok(JobResult {
            phase_result,
            phase_outcome: crate::job::Outcome::PENDING_COMPLETION,
            next_action: None,
            timeout_secs: None,
            error,
            exit_code: shell_exit_code,
            measurements: Vec::new(),
            logs,
            started_at: start_time,
            completed_at: end_time,
            resource_metrics,
            unit: None,
            input_unit_info: job.initial_unit_info.clone(),
            retry_count: job.retry_count,
            run_metadata: Default::default(),
            unit_metadata: Default::default(),
        })
    }

    pub async fn execute_native_phase(
        &self,
        job: Job,
        event_sink: Arc<dyn EventSink>,
    ) -> Result<JobResult, String> {
        let start_time = chrono::Utc::now();

        let mut resource_tracker = crate::monitoring::ResourceTracker::new();
        resource_tracker.start_tracking(None);

        let has_ui = !job.ui_config.components.is_empty();
        let requires_user_input = job.ui_config.requires_user_input();

        let ui_response_rx = if has_ui {
            let request_id = format!("{}_{}", job.id, chrono::Utc::now().timestamp_millis());

            let ui_response_rx = if requires_user_input {
                let (tx, rx) = tokio::sync::oneshot::channel();
                {
                    let mut channels = crate::ui::UI_RESPONSE_CHANNELS.lock().await;
                    channels.insert(request_id.clone(), tx);
                }

                log::debug!(
                    "Created UI response channel for native phase: {}",
                    request_id
                );
                Some(rx)
            } else {
                None
            };

            let event_data = crate::ui::UiRequestData {
                request_id: request_id.clone(),
                job_id: job.id.to_string(),
                pipe_path: String::new(),
                config: job.ui_config.clone(),
                phase_key: job.phase_key.clone(),
                slot_id: job.slot_id.clone(),
            };

            event_sink.emit(&ExecutionEvent::UiRequest(event_data));

            log::debug!(
                "Sent UI request {} for native phase {}",
                request_id,
                job.phase_name
            );

            ui_response_rx
        } else {
            None
        };

        let mut bound_measurements_to_merge: Option<HashMap<String, serde_json::Value>> = None;
        let mut unit_info: Option<crate::unit::UnitInfo> = None;

        let ui_result = if has_ui && requires_user_input {
            if let Some(rx) = ui_response_rx {
                match rx.await {
                    Ok(ui_values) => {
                        if let Some((ui_unit, bound)) = extract_bound_measurements(&ui_values) {
                            // Mid-run native-phase identify: merge with
                            // the unit known when this job started so a
                            // partial scan (e.g. just a sub-unit serial)
                            // doesn't clobber pre-run-set fields. The
                            // python-phase branch above does the same;
                            // both paths must publish the merged result
                            // so wire consumers' `mergeUnit` reducer
                            // produces the same final state regardless
                            // of which framework the operator used.
                            if let Some(u) = ui_unit {
                                let merged = merge_unit_info(job.initial_unit_info.clone(), u);
                                event_sink.emit(&ExecutionEvent::UnitIdentified {
                                    slot_id: job.slot_id.clone(),
                                    unit_info: merged.clone(),
                                });
                                unit_info = Some(merged);
                            }
                            bound_measurements_to_merge = Some(bound);
                        }

                        (crate::job::PhaseResult::Continue, None)
                    }
                    Err(_) => (crate::job::PhaseResult::Stop, None),
                }
            } else {
                (
                    crate::job::PhaseResult::Continue,
                    Some("No UI response channel available".to_string()),
                )
            }
        } else {
            (crate::job::PhaseResult::Continue, None)
        };

        let (phase_result, execution_error) = ui_result;

        let end_time = chrono::Utc::now();

        let phase_config = crate::procedure::schema::PhaseDefinition {
            measurements: job.phase_measurements.clone(),
            key: String::new(),
            name: String::new(),
            scope: None,
            python: None,
            executable: None,
            description: None,
            depends_on: Vec::new(),
            ui: None,
            enabled: true,
            result: None,
            timeout: None,
            retry: None,
            then: None,
        };

        let mut all_measurements = Vec::new();
        if let Some(bound) = bound_measurements_to_merge {
            all_measurements = convert_bound_to_measurements(bound);
        }

        let evaluated_measurements =
            crate::measurements::auto_evaluate_measurements(all_measurements, &phase_config);

        let resource_metrics = resource_tracker.collect_metrics();

        Ok(JobResult {
            phase_result,
            phase_outcome: crate::job::Outcome::PENDING_COMPLETION,
            next_action: None,
            timeout_secs: None,
            error: execution_error,
            exit_code: None,
            measurements: evaluated_measurements,
            logs: Vec::new(),
            started_at: start_time,
            completed_at: end_time,
            resource_metrics,
            unit: unit_info,
            input_unit_info: job.initial_unit_info.clone(),
            retry_count: job.retry_count,
            run_metadata: Default::default(),
            unit_metadata: Default::default(),
        })
    }
}

/// Convert protocol JobResult to internal JobResult
fn convert_job_result(
    result: protocol::JobResult,
    start_time: chrono::DateTime<chrono::Utc>,
    end_time: chrono::DateTime<chrono::Utc>,
    job: &Job,
) -> Result<crate::job::JobResult, String> {
    use crate::job::PhaseResult;

    let phase_result = result
        .phase_result_json
        .as_ref()
        .and_then(|json| serde_json::from_str(json).ok())
        .and_then(|pr| PhaseResult::from_python_result(&pr).ok())
        .unwrap_or(PhaseResult::Continue);

    let measurements: Vec<crate::measurements::Measurement> = result
        .measurements
        .into_iter()
        .filter_map(try_measurement_from_protocol)
        .collect();

    let phase_config = crate::procedure::schema::PhaseDefinition {
        measurements: job.phase_measurements.clone(),
        key: String::new(),
        name: String::new(),
        scope: None,
        python: None,
        executable: None,
        description: None,
        depends_on: Vec::new(),
        ui: None,
        enabled: true,
        result: None,
        timeout: None,
        retry: None,
        then: None,
    };

    let evaluated_measurements =
        crate::measurements::auto_evaluate_measurements(measurements, &phase_config);

    let logs = result
        .logs
        .into_iter()
        .map(|l| crate::log::LogEntry {
            timestamp: l.timestamp,
            level: l.level,
            message: l.message,
            file: l.file,
            line: l.line,
        })
        .collect();

    let unit = result
        .unit_json
        .and_then(|json| match serde_json::from_str(&json) {
            Ok(u) => Some(u),
            Err(e) => {
                log::warn!("Failed to parse unit_json: {} (json: {})", e, json);
                None
            }
        });

    Ok(crate::job::JobResult {
        phase_result,
        phase_outcome: crate::job::Outcome::PENDING_COMPLETION,
        next_action: None,
        timeout_secs: None,
        error: result.error,
        exit_code: result.exit_code,
        measurements: evaluated_measurements,
        logs,
        started_at: start_time,
        completed_at: end_time,
        resource_metrics: Default::default(),
        unit,
        input_unit_info: job.initial_unit_info.clone(),
        retry_count: job.retry_count,
        run_metadata: parse_metadata_json("run_metadata_json", result.run_metadata_json),
        unit_metadata: parse_metadata_json("unit_metadata_json", result.unit_metadata_json),
    })
}

/// Parse a double-encoded metadata JSON object from the worker. Metadata
/// must never crash a run — malformed JSON is logged and dropped, same
/// failure mode as `unit_json`.
fn parse_metadata_json(
    label: &str,
    json: Option<String>,
) -> std::collections::HashMap<String, serde_json::Value> {
    json.and_then(|j| match serde_json::from_str(&j) {
        Ok(m) => Some(m),
        Err(e) => {
            log::warn!("Failed to parse {}: {} (json: {})", label, e, j);
            None
        }
    })
    .unwrap_or_default()
}

fn extract_unit_info_from_json(
    unit_data: &serde_json::Map<String, serde_json::Value>,
) -> crate::unit::UnitInfo {
    let serial_number = unit_data
        .get("serial_number")
        .and_then(|v| v.as_str())
        .map(String::from);
    let batch_number = unit_data
        .get("batch_number")
        .and_then(|v| v.as_str())
        .map(String::from);
    let part_number = unit_data
        .get("part_number")
        .and_then(|v| v.as_str())
        .map(String::from);
    let revision_number = unit_data
        .get("revision_number")
        .and_then(|v| v.as_str())
        .map(String::from);

    let sub_units = unit_data.get("sub_units").and_then(|v| {
        if let Some(obj) = v.as_object() {
            let parsed: std::collections::HashMap<String, String> = obj
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect();
            if parsed.is_empty() {
                None
            } else {
                Some(parsed)
            }
        } else {
            None
        }
    });

    crate::unit::UnitInfo {
        serial_number,
        batch_number,
        part_number,
        revision_number,
        sub_units,
        status: "tested".to_string(),
        // Python-set unit metadata flows via the JobComplete bundle
        // (unit_metadata_json), not the __unit__ bound payload.
        metadata: None,
    }
}

fn extract_bound_measurements(
    ui_values: &HashMap<String, String>,
) -> Option<(
    Option<crate::unit::UnitInfo>,
    HashMap<String, serde_json::Value>,
)> {
    let bound_json = ui_values.get("__bound_measurements__")?;
    let mut bound: HashMap<String, serde_json::Value> = serde_json::from_str(bound_json).ok()?;

    let unit_info = if let Some(unit_value) = bound.remove("__unit__") {
        let unit_data_opt = match &unit_value {
            serde_json::Value::Object(obj) => Some(obj.clone()),
            serde_json::Value::String(s) => serde_json::from_str(s).ok(),
            _ => None,
        };

        unit_data_opt.map(|unit_data| extract_unit_info_from_json(&unit_data))
    } else {
        None
    };

    Some((unit_info, bound))
}

fn convert_bound_to_measurements(
    bound: HashMap<String, serde_json::Value>,
) -> Vec<crate::measurements::Measurement> {
    bound
        .into_iter()
        .map(|(name, value)| {
            let measurement_value = match value {
                serde_json::Value::Null => crate::measurements::MeasurementValue::Null,
                serde_json::Value::Bool(b) => crate::measurements::MeasurementValue::Boolean(b),
                serde_json::Value::Number(n) => {
                    crate::measurements::MeasurementValue::Numeric(n.as_f64().unwrap_or(0.0))
                }
                serde_json::Value::String(s) => crate::measurements::MeasurementValue::String(s),
                serde_json::Value::Array(arr) => crate::measurements::MeasurementValue::Array(arr),
                serde_json::Value::Object(obj) => {
                    crate::measurements::MeasurementValue::Object(obj)
                }
            };

            crate::measurements::Measurement {
                name: name.clone(),
                value: measurement_value,
                unit: None,
                timestamp: chrono::Utc::now().to_rfc3339(),
                validators: None,
                aggregations: None,
                description: None,
                outcome: crate::procedure::schema::ValidatorOutcome::Unset,
            }
        })
        .collect()
}

fn merge_unit_info(
    existing: Option<crate::unit::UnitInfo>,
    ui_unit: crate::unit::UnitInfo,
) -> crate::unit::UnitInfo {
    match existing {
        Some(mut base) => {
            if let Some(ui_sub_units) = ui_unit.sub_units {
                let mut merged_sub_units = base.sub_units.unwrap_or_default();
                for (key, value) in ui_sub_units {
                    merged_sub_units.insert(key, value);
                }
                base.sub_units = Some(merged_sub_units);
            }
            if ui_unit.serial_number.is_some() {
                base.serial_number = ui_unit.serial_number;
            }
            if ui_unit.part_number.is_some() {
                base.part_number = ui_unit.part_number;
            }
            if ui_unit.revision_number.is_some() {
                base.revision_number = ui_unit.revision_number;
            }
            if ui_unit.batch_number.is_some() {
                base.batch_number = ui_unit.batch_number;
            }
            if let Some(ui_md) = ui_unit.metadata {
                base.metadata
                    .get_or_insert_with(Default::default)
                    .extend(ui_md);
            }
            base
        }
        None => ui_unit,
    }
}

#[cfg(test)]
mod debug_env_tests {
    use super::build_worker_env;

    #[test]
    fn no_debug_port_sets_only_worker_id() {
        let env = build_worker_env(3, None);
        assert_eq!(env, vec![("WORKER_ID".to_string(), "3".to_string())]);
        assert!(!env.iter().any(|(k, _)| k == "TP_DEBUG"));
    }

    #[test]
    fn debug_port_sets_tp_debug_and_port() {
        let env = build_worker_env(0, Some(5678));
        assert!(env.contains(&("TP_DEBUG".to_string(), "1".to_string())));
        assert!(env.contains(&("TP_DEBUG_PORT".to_string(), "5678".to_string())));
    }

    #[test]
    fn debug_port_honors_custom_port() {
        let env = build_worker_env(0, Some(9999));
        assert!(env.contains(&("TP_DEBUG_PORT".to_string(), "9999".to_string())));
    }

    #[test]
    fn with_debug_port_builder_threads_into_env() {
        // Pool workers opt in; the default (teardown/respawn) stays clean.
        let plain = super::Worker::new(0, std::path::PathBuf::from("/tmp"));
        assert_eq!(plain.debug_port, None);
        let dbg = plain.with_debug_port(Some(5678));
        assert_eq!(dbg.debug_port, Some(5678));
    }
}

#[cfg(test)]
mod stage_read_tests {
    use super::{read_stage_event, JobStage, StageRead};
    use tokio::io::AsyncWriteExt;

    /// Duplex-backed buffered reader plus the writer that feeds it.
    fn pipe() -> (
        tokio::io::BufReader<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
    ) {
        let (rx, tx) = tokio::io::duplex(64 * 1024);
        (tokio::io::BufReader::new(rx), tx)
    }

    #[tokio::test(start_paused = true)]
    async fn silent_worker_stalls_at_the_ack_deadline() {
        // The field symptom: command sent, interpreter alive but never
        // executing. tokio's paused clock auto-advances past the 30s
        // deadline the moment the read has nothing else to do.
        let (mut reader, _tx) = pipe();
        let out = read_stage_event(&mut reader, JobStage::AwaitingAck, false).await;
        assert!(matches!(out, StageRead::Stalled));
    }

    #[tokio::test(start_paused = true)]
    async fn ack_before_deadline_is_delivered_not_killed() {
        // Anti-false-kill: a slow-but-alive worker whose ack lands
        // inside the window must never be treated as a stall.
        let (mut reader, mut tx) = pipe();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(25)).await;
            let _ = tx
                .write_all(b"{\"type\":\"JobAck\",\"job_id\":\"j1\"}\n")
                .await;
        });
        let out = read_stage_event(&mut reader, JobStage::AwaitingAck, false).await;
        match out {
            StageRead::Event(super::protocol::WorkerEvent::JobAck(a)) => {
                assert_eq!(a.job_id, "j1");
            }
            other => panic!("expected JobAck, got {:?}", stage_read_kind(&other)),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn executing_stage_never_deadlines() {
        // Once user code owns the clock the read must park indefinitely:
        // race an hour of virtual time against the unbounded read and
        // require the timer to win. If Executing ever grew a deadline,
        // the read would resolve first and this test would catch it.
        let (mut reader, _tx) = pipe();
        let long = tokio::time::sleep(std::time::Duration::from_secs(3600));
        tokio::select! {
            _ = long => {}
            _ = read_stage_event(&mut reader, JobStage::Executing, false) => {
                panic!("Executing-stage read resolved without input");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn debug_mode_disables_the_ack_deadline() {
        // A debugger paused before the first event must not be killed.
        let (mut reader, _tx) = pipe();
        let long = tokio::time::sleep(std::time::Duration::from_secs(3600));
        tokio::select! {
            _ = long => {}
            _ = read_stage_event(&mut reader, JobStage::AwaitingAck, true) => {
                panic!("debug-mode read resolved without input");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn eof_is_eof_not_a_stall() {
        // A crashed worker closes the socket — that's the existing
        // crash path ("stream ended without job completion"), and must
        // stay distinguishable from alive-but-silent.
        let (mut reader, tx) = pipe();
        drop(tx);
        let out = read_stage_event(&mut reader, JobStage::AwaitingAck, false).await;
        assert!(matches!(out, StageRead::Eof));
    }

    fn stage_read_kind(r: &StageRead) -> &'static str {
        match r {
            StageRead::Event(_) => "Event",
            StageRead::Eof => "Eof",
            StageRead::Io(_) => "Io",
            StageRead::Stalled => "Stalled",
        }
    }
}

#[cfg(test)]
mod job_stage_tests {
    use super::{JobStage, STALL_MARKER_ACK, STALL_MARKER_IMPORT};
    use crate::constants::timeouts::{JOB_ACK_TIMEOUT_SECS, MODULE_IMPORT_TIMEOUT_SECS};

    fn test_job() -> crate::job::Job {
        crate::job::Job::new(
            Some("default".to_string()),
            "stall_phase".to_string(),
            "Stall Phase".to_string(),
            crate::procedure::schema::StageScope::Main,
            "phases.stall".to_string(),
            "stall".to_string(),
            vec![],
            vec![],
            crate::ui::UiConfig::default(),
            None,
            None,
            None,
            &std::collections::HashMap::new(),
            vec![],
        )
    }

    #[test]
    fn awaiting_ack_is_bounded_by_ack_timeout() {
        assert_eq!(
            JobStage::AwaitingAck.read_deadline(),
            Some(std::time::Duration::from_secs(JOB_ACK_TIMEOUT_SECS))
        );
    }

    #[test]
    fn loading_is_bounded_by_module_import_timeout() {
        assert_eq!(
            JobStage::Loading.read_deadline(),
            Some(std::time::Duration::from_secs(MODULE_IMPORT_TIMEOUT_SECS))
        );
    }

    #[test]
    fn executing_is_unbounded() {
        // Once user code owns the clock, the Rust side must never
        // deadline the read — burn-in phases legitimately run for hours.
        assert_eq!(JobStage::Executing.read_deadline(), None);
    }

    #[test]
    fn stall_errors_name_phase_and_trigger_pool_replacement() {
        let job = test_job();
        let ack = JobStage::AwaitingAck.stall_error(&job);
        let load = JobStage::Loading.stall_error(&job);
        assert!(ack.contains("Stall Phase"));
        assert!(load.contains("Stall Phase"));
        assert!(load.contains("phases.stall"));
        // The orchestrator's worker-replacement branch keys on these
        // substrings (`execution.rs`); drifting them detaches the wedged
        // worker's replacement and every later phase lands on a corpse.
        assert!(ack.contains(STALL_MARKER_ACK));
        assert!(load.contains(STALL_MARKER_IMPORT));
    }
}

#[cfg(test)]
mod metadata_json_tests {
    use super::parse_metadata_json;

    #[test]
    fn valid_double_encoded_object_parses() {
        let json = r#"{"modification":"MOD-42","cycles":3,"calibrated":true}"#;
        let map = parse_metadata_json("run_metadata_json", Some(json.to_string()));
        assert_eq!(map.get("modification"), Some(&serde_json::json!("MOD-42")));
        assert_eq!(map.get("cycles"), Some(&serde_json::json!(3)));
        assert_eq!(map.get("calibrated"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn missing_field_is_empty() {
        let map = parse_metadata_json("run_metadata_json", None);
        assert!(map.is_empty());
    }

    #[test]
    fn malformed_json_is_empty_not_error() {
        let map = parse_metadata_json("unit_metadata_json", Some("{not json".to_string()));
        assert!(map.is_empty());
    }
}

#[cfg(test)]
mod attachment_mimetype_tests {
    use super::guess_attachment_mimetype;

    #[test]
    fn images_get_an_image_mimetype() {
        // The operator-UI keys `<img>` rendering on `image/*`.
        assert_eq!(
            guess_attachment_mimetype("board.png").as_deref(),
            Some("image/png")
        );
        assert_eq!(
            guess_attachment_mimetype("shot.JPG").as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            guess_attachment_mimetype("scan.webp").as_deref(),
            Some("image/webp")
        );
    }

    #[test]
    fn non_images_and_unknowns_are_not_image_typed() {
        assert_eq!(
            guess_attachment_mimetype("log.csv").as_deref(),
            Some("text/csv")
        );
        // Unknown extension / no extension → octet-stream, never None: the
        // upload PUT must carry a real Content-Type so the server's HeadObject
        // records a usable type. Non-image essence still keeps the UI from
        // rendering a broken <img>.
        assert_eq!(
            guess_attachment_mimetype("data.bin").as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(
            guess_attachment_mimetype("noext").as_deref(),
            Some("application/octet-stream")
        );
    }
}
