//! Studio RPC surface on the loopback server.
//!
//! Serves `POST /studio/rpc` (a `StudioRequest` JSON body → one
//! `StudioResponse` JSON reply) for the dashboard Studio page. This is
//! deliberately request/response over HTTP, not the event WebSocket:
//! payload-bearing replies (file contents, diagnostics) must not ride
//! the broadcast channels — they would fan out to every kiosk tab and
//! pollute the hydration ring.
//!
//! Security model (stricter than the kiosk WS):
//!   * The surface is OFF unless a studio root was explicitly enabled
//!     (`tofupilot studio`). Station daemons and `run --kiosk` never
//!     enable it, so file access is not reachable on those processes.
//!   * Every request must carry `Authorization: Bearer <session token>`;
//!     the token is generated per-process at server start and printed
//!     only to the operator's terminal. Origin checks are NOT the
//!     boundary here — the token is.
//!   * All paths are root-relative, clamped to `Normal` components and
//!     canonicalize-confined under the studio root (same posture as
//!     `/files/*`), with a text-extension allow-list and size caps.

use std::path::{Path, PathBuf};

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use sha2::{Digest, Sha256};
use station_protocol::{
    StudioDiagnostic, StudioDiagnosticSeverity, StudioEntryKind, StudioErrorCode, StudioFileEntry,
    StudioRequest, StudioResponse, StudioSequence, StudioSequenceAggregation, StudioSequenceAxis,
    StudioSequenceMeasurement, StudioSequencePhase, StudioSequencePlug, StudioSequenceRetry,
    StudioSequenceSubUnit, StudioSequenceUi, StudioSequenceUnit, StudioSequenceUnitField,
    StudioSequenceValidator,
};

use super::AppState;

/// Editable/readable text file extensions. Everything else is refused:
/// Studio edits procedure sources, not binaries or secrets.
const TEXT_EXTENSIONS: &[&str] = &[
    "yaml", "yml", "py", "md", "json", "txt", "toml", "cfg", "ini", "robot", "csv",
];

/// Directory names never listed nor readable. Keeps venvs, caches and
/// VCS internals off the wire.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".venv",
    "venv",
    "node_modules",
    "__pycache__",
    "target",
    "dist",
    ".tofupilot",
];

/// Read cap. Procedure sources are KBs; anything above this is not a
/// file Studio should be editing.
const MAX_READ_BYTES: u64 = 1024 * 1024;
/// Write cap, matching the read cap.
const MAX_WRITE_BYTES: usize = 1024 * 1024;

/// Binary resources live under this subtree only. Confining
/// `write_resource` keeps binary payloads from ever replacing a
/// procedure source file.
const RESOURCE_DIR: &str = "resources";
/// Extensions `write_resource` accepts: integration assets a phase
/// consumes at runtime (reference audio, calibration tables, firmware
/// images, reference pictures). Text sources stay on `write_file`.
const RESOURCE_EXTENSIONS: &[&str] = &[
    "wav", "mp3", "csv", "tsv", "bin", "hex", "dat", "png", "jpg", "jpeg", "webp", "bmp",
];
/// Decoded-size cap for `write_resource`. Firmware images and audio
/// references are MBs; procedure sources stay under `MAX_WRITE_BYTES`.
const MAX_RESOURCE_BYTES: usize = 16 * 1024 * 1024;

/// Studio configuration installed by `tofupilot studio` after
/// `Server::start`. Absent on every other invocation, which keeps the
/// whole RPC surface 403.
#[derive(Clone)]
pub struct StudioConfig {
    /// Canonicalized project root (`enable_studio` canonicalizes).
    /// Storing it canonical lets every request skip re-walking the
    /// root's realpath and makes `starts_with` confinement sound.
    pub root: PathBuf,
}

fn err(code: StudioErrorCode, message: impl Into<String>) -> StudioResponse {
    StudioResponse::Error {
        code,
        message: message.into(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn has_text_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some(ext) if TEXT_EXTENSIONS.contains(&ext)
    )
}

fn is_skipped_name(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name)
}

/// Clamp a client-supplied relative path to `Normal` components so
/// `..` and absolute prefixes collapse inside the root. Same pattern
/// as `files_handler`. Rejects paths that traverse a skipped dir
/// (e.g. `venv/pyvenv.cfg`) so the allow-list can't be sidestepped by
/// direct addressing.
// The Err IS the wire response callers send back, not an error to
// shrink — boxing it would just move the copy to every call site.
#[allow(clippy::result_large_err)]
fn clamp_rel(path: &str) -> Result<PathBuf, StudioResponse> {
    let safe: PathBuf = Path::new(path)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    if safe.as_os_str().is_empty() {
        return Err(err(StudioErrorCode::Invalid, "empty path"));
    }
    for comp in safe.components() {
        if let std::path::Component::Normal(s) = comp {
            if let Some(name) = s.to_str() {
                if is_skipped_name(name) {
                    return Err(err(
                        StudioErrorCode::Forbidden,
                        format!("path traverses excluded entry {name:?}"),
                    ));
                }
            }
        }
    }
    Ok(safe)
}

/// Post-resolution policy check on the CANONICAL target. The request
/// path already passed `clamp_rel` + `has_text_extension`, but those
/// ran on the client-supplied name — an in-root symlink
/// (`link.yaml -> .env`, `cfg.yaml -> venv/pyvenv.cfg`) stays confined
/// under the root while pointing at something the allow-list exists to
/// keep off the wire. Re-run the same rules on the path the filesystem
/// will actually touch.
// Same rationale as clamp_rel: the Err is the wire response itself.
#[allow(clippy::result_large_err)]
fn check_canonical_policy(
    canon_root: &Path,
    canon_target: &Path,
    require_text: bool,
) -> Result<(), StudioResponse> {
    let rel = canon_target
        .strip_prefix(canon_root)
        .map_err(|_| err(StudioErrorCode::Forbidden, "path escapes studio root"))?;
    for comp in rel.components() {
        if let std::path::Component::Normal(s) = comp {
            if s.to_str().map(is_skipped_name).unwrap_or(true) {
                return Err(err(
                    StudioErrorCode::Forbidden,
                    "target resolves into an excluded entry",
                ));
            }
        }
    }
    if require_text && !has_text_extension(canon_target) {
        return Err(err(StudioErrorCode::Forbidden, "not an editable text file"));
    }
    Ok(())
}

/// Resolve `rel` under the canonical `root` for READ access: the
/// target must exist and canonicalize inside the root (symlink-escape
/// safe). `root` is already canonical (see `StudioConfig::root`), so
/// only the target is walked.
async fn resolve_existing(canon_root: &Path, rel: &Path) -> Result<PathBuf, StudioResponse> {
    let full = canon_root.join(rel);
    let Ok(canon_full) = tokio::fs::canonicalize(&full).await else {
        return Err(err(StudioErrorCode::NotFound, "not found"));
    };
    if !canon_full.starts_with(canon_root) {
        return Err(err(StudioErrorCode::Forbidden, "path escapes studio root"));
    }
    Ok(canon_full)
}

/// Resolve `rel` under the canonical `root` for WRITE access. The file
/// itself may not exist yet, so the confinement check canonicalizes
/// the parent directory instead (which must exist — Studio writes into
/// existing project structure; directory creation is a separate op if
/// ever needed).
async fn resolve_for_write(canon_root: &Path, rel: &Path) -> Result<PathBuf, StudioResponse> {
    let full = canon_root.join(rel);
    let parent = full
        .parent()
        .ok_or_else(|| err(StudioErrorCode::Invalid, "path has no parent"))?;
    let Ok(canon_parent) = tokio::fs::canonicalize(parent).await else {
        return Err(err(StudioErrorCode::NotFound, "parent directory not found"));
    };
    if !canon_parent.starts_with(canon_root) {
        return Err(err(StudioErrorCode::Forbidden, "path escapes studio root"));
    }
    let Some(file_name) = full.file_name() else {
        return Err(err(StudioErrorCode::Invalid, "missing file name"));
    };
    // If the target exists it may itself be a symlink pointing
    // outside the root; canonicalize and re-confine in that case.
    let target = canon_parent.join(file_name);
    if let Ok(canon_target) = tokio::fs::canonicalize(&target).await {
        if !canon_target.starts_with(canon_root) {
            return Err(err(StudioErrorCode::Forbidden, "path escapes studio root"));
        }
        return Ok(canon_target);
    }
    Ok(target)
}

/// `POST /studio/rpc` — bearer-token gated `StudioRequest` dispatch.
pub(super) async fn rpc_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Auth first, before touching the body. 403 with no detail —
    // don't help a prober distinguish "studio off" from "bad token".
    let Some(config) = authorize(&state, &headers).await else {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    };

    let request: StudioRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            // Unknown `op` lands here too (serde can't pick a variant):
            // answer with the typed forward-compat error instead of a
            // bare 400 so a newer dashboard renders "daemon too old",
            // not a transport failure.
            let reply = err(
                StudioErrorCode::Unsupported,
                format!("unrecognized studio request: {e}"),
            );
            return Json(reply).into_response();
        }
    };

    let reply = dispatch(&config, request).await;
    Json(reply).into_response()
}

async fn authorize(state: &AppState, headers: &HeaderMap) -> Option<StudioConfig> {
    let config = state.studio.lock().await.clone()?;
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))?;
    token_matches(presented, &state.session_token).then_some(config)
}

/// Constant-time token comparison (compare digests, not strings). The
/// token is 128-bit random so a timing oracle is impractical anyway,
/// but this is one line and removes the question entirely.
pub(super) fn token_matches(presented: &str, actual: &str) -> bool {
    Sha256::digest(presented.as_bytes()) == Sha256::digest(actual.as_bytes())
}

async fn dispatch(config: &StudioConfig, request: StudioRequest) -> StudioResponse {
    match request {
        StudioRequest::ProjectInfo {} => project_info(&config.root).await,
        StudioRequest::ListFiles { dir } => list_files(&config.root, dir.as_deref()).await,
        StudioRequest::ReadFile { path } => read_file(&config.root, &path).await,
        StudioRequest::WriteFile {
            path,
            content,
            expected_sha256,
        } => write_file(&config.root, &path, &content, expected_sha256.as_deref()).await,
        StudioRequest::CreateDir { path } => create_dir(&config.root, &path).await,
        StudioRequest::Validate { path } => validate(&config.root, path.as_deref()).await,
        StudioRequest::ValidateContent { path, content } => validate_content(&path, content).await,
        StudioRequest::WriteResource {
            path,
            content_base64,
            overwrite,
        } => write_resource(&config.root, &path, &content_base64, overwrite).await,
        StudioRequest::GetSequence {} => get_sequence(&config.root).await,
    }
}

/// Parse the project's procedure with the engine loader and project it
/// into the display model. One parser for validation, execution, and
/// UI — the web never re-derives structure from YAML text.
async fn get_sequence(root: &Path) -> StudioResponse {
    let Some(yaml_path) = crate::commands::run::engine::find_procedure_yaml(root) else {
        return err(
            StudioErrorCode::NotFound,
            "no procedure.yaml found in the studio root",
        );
    };
    let loaded = tokio::task::spawn_blocking(move || {
        execution_engine::procedure::loader::load_procedure_definition(&yaml_path)
    })
    .await;
    let def = match loaded {
        Ok(Ok(def)) => def,
        Ok(Err(message)) => return err(StudioErrorCode::Invalid, message),
        Err(join_err) => {
            return err(
                StudioErrorCode::Internal,
                format!("sequence load failed: {join_err}"),
            )
        }
    };

    fn validator_detail(v: &execution_engine::procedure::schema::ValidatorSpec) -> String {
        if let Some(expr) = &v.expression {
            return expr.clone();
        }
        let op = v.operator.as_deref().unwrap_or("==");
        let value = v
            .expected_value
            .as_ref()
            .and_then(|ev| serde_json::to_value(ev).ok())
            .map(|j| match j {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            })
            .unwrap_or_default();
        format!("{op} {value}")
    }

    fn map_phase(p: &execution_engine::procedure::schema::PhaseDefinition) -> StudioSequencePhase {
        StudioSequencePhase {
            key: p.key.clone(),
            name: p.name.clone(),
            python: p.python.as_ref().map(|py| py.as_str().to_string()),
            description: p.description.clone(),
            enabled: p.enabled,
            depends_on: p.depends_on.clone(),
            ui_components: p
                .ui
                .as_ref()
                .and_then(|u| u.components.as_ref())
                .map(|c| c.len() as u32)
                .unwrap_or(0),
            executable: p.executable.is_some(),
            // ms values saturate into the u32 the TS codegen requires;
            // the schema caps them below that anyway.
            timeout: p.timeout.map(|t| t.min(u32::MAX as u64) as u32),
            retry: p.retry.as_ref().map(|r| StudioSequenceRetry {
                limit: r.limit.min(u32::MAX as usize) as u32,
                delay: r.delay.map(|d| d.min(u32::MAX as u64) as u32),
            }),
            // Same schema -> wire conversion the runtime uses, so the
            // Builder sees every declared field without a second
            // projection to keep in sync.
            ui: p.ui.as_ref().map(|u| StudioSequenceUi {
                requires_input: u.requires_input,
                components: u.components.iter().flatten().map(Into::into).collect(),
            }),
            measurements: p
                .measurements
                .iter()
                .map(|m| StudioSequenceMeasurement {
                    key: m.key.clone(),
                    name: m.name.clone(),
                    unit: m.unit.clone(),
                    validators: map_validators(m.validators.as_deref()),
                    aggregations: map_aggregations(m.aggregations.as_deref()),
                    title: m.title.clone(),
                    x_axis: m.x_axis.as_ref().map(map_axis),
                    y_axis: m.y_axis.iter().flatten().map(map_axis).collect(),
                })
                .collect(),
        }
    }

    fn map_validators(
        validators: Option<&[execution_engine::procedure::schema::ValidatorSpec]>,
    ) -> Vec<StudioSequenceValidator> {
        validators
            .unwrap_or_default()
            .iter()
            .map(|v| StudioSequenceValidator {
                detail: validator_detail(v),
            })
            .collect()
    }

    fn map_aggregations(
        aggregations: Option<&[execution_engine::procedure::schema::AggregationSpec]>,
    ) -> Vec<StudioSequenceAggregation> {
        aggregations
            .unwrap_or_default()
            .iter()
            .map(|a| StudioSequenceAggregation {
                aggregation_type: a.aggregation_type.clone(),
                unit: a.unit.clone(),
                validators: map_validators(a.validators.as_deref()),
            })
            .collect()
    }

    // Resolved key/legend (each derives from the other when only one is
    // in the YAML) so the Builder always has a display label.
    fn map_axis(axis: &execution_engine::procedure::schema::AxisSpec) -> StudioSequenceAxis {
        StudioSequenceAxis {
            key: axis.get_key(),
            legend: axis.get_legend(),
            unit: axis.unit.clone(),
            description: axis.description.clone(),
            aggregations: map_aggregations(axis.aggregations.as_deref()),
            validators: map_validators(axis.validators.as_deref()),
        }
    }

    fn map_unit_field(
        f: &execution_engine::procedure::schema::UnitFieldConfig,
    ) -> StudioSequenceUnitField {
        StudioSequenceUnitField {
            default_value: f.default_value.clone(),
            placeholder: f.placeholder.clone(),
            description: f.description.clone(),
            pattern: f.pattern.clone(),
            min_length: f.min_length.map(|v| v.min(u32::MAX as usize) as u32),
            max_length: f.max_length.map(|v| v.min(u32::MAX as usize) as u32),
        }
    }

    StudioResponse::Sequence {
        sequence: StudioSequence {
            name: def.name.clone(),
            version: def.version.clone(),
            description: def.description.clone(),
            unit: def.unit.as_ref().map(|u| StudioSequenceUnit {
                auto_identify: u.auto_identify,
                serial_number: u.serial_number.as_ref().map(map_unit_field),
                part_number: u.part_number.as_ref().map(map_unit_field),
                revision_number: u.revision_number.as_ref().map(map_unit_field),
                batch_number: u.batch_number.as_ref().map(map_unit_field),
                sub_units: u
                    .sub_units
                    .as_ref()
                    .map(|s| {
                        s.0.iter()
                            .map(|item| StudioSequenceSubUnit {
                                label: item.label.clone(),
                                serial_number: item.serial_number.as_ref().map(map_unit_field),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                components: {
                    // The canonical builder requires serial/part configs;
                    // they are always prompted even when the YAML omits
                    // them, so normalize before building rather than
                    // erroring on a hand-written partial `unit:` block.
                    let mut cfg = u.clone();
                    if cfg.serial_number.is_none() {
                        cfg.serial_number = Some(Default::default());
                    }
                    if cfg.part_number.is_none() {
                        cfg.part_number = Some(Default::default());
                    }
                    execution_engine::identify_unit::components::build_components(&cfg)
                        .unwrap_or_default()
                },
            }),
            plugs: def
                .plugs
                .iter()
                .map(|pl| StudioSequencePlug {
                    key: pl.key.clone(),
                    name: pl.name.clone(),
                    python: pl.python.as_str().to_string(),
                })
                .collect(),
            setup: def.setup.iter().map(map_phase).collect(),
            main: def.main.iter().map(map_phase).collect(),
            teardown: def.teardown.iter().map(map_phase).collect(),
        },
    }
}

async fn project_info(root: &Path) -> StudioResponse {
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();
    let procedure_path = crate::commands::run::engine::find_procedure_yaml(root)
        .and_then(|p| p.strip_prefix(root).ok().map(|r| r.to_path_buf()))
        .map(|r| r.to_string_lossy().replace('\\', "/"));
    StudioResponse::ProjectInfo {
        root: root.to_string_lossy().into_owned(),
        name,
        procedure_path,
    }
}

async fn list_files(root: &Path, dir: Option<&str>) -> StudioResponse {
    let rel = match dir {
        None | Some("") => PathBuf::new(),
        Some(d) => match clamp_rel(d) {
            Ok(p) => p,
            Err(e) => return e,
        },
    };
    let target = if rel.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        match resolve_existing(root, &rel).await {
            Ok(p) => p,
            Err(e) => return e,
        }
    };
    if let Err(e) = check_canonical_policy(root, &target, false) {
        return e;
    }

    let mut read_dir = match tokio::fs::read_dir(&target).await {
        Ok(rd) => rd,
        Err(_) => return err(StudioErrorCode::NotFound, "not a directory"),
    };
    let mut entries = Vec::new();
    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_skipped_name(&name) {
            continue;
        }
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        let kind = if file_type.is_dir() {
            StudioEntryKind::Dir
        } else if file_type.is_file() {
            StudioEntryKind::File
        } else {
            // Symlinks are skipped outright: listing them invites
            // read/write attempts the resolvers would refuse anyway.
            continue;
        };
        let size = match kind {
            StudioEntryKind::File => entry
                .metadata()
                .await
                .ok()
                .map(|m| u32::try_from(m.len()).unwrap_or(u32::MAX)),
            StudioEntryKind::Dir => None,
        };
        let path = if rel.as_os_str().is_empty() {
            name.clone()
        } else {
            format!("{}/{}", rel.to_string_lossy().replace('\\', "/"), name)
        };
        entries.push(StudioFileEntry {
            name,
            path,
            kind,
            size,
        });
    }
    // Dirs first, then case-insensitive name. Cached key so each name
    // is lowercased once, not per comparison.
    entries.sort_by_cached_key(|e| (e.kind != StudioEntryKind::Dir, e.name.to_lowercase()));
    StudioResponse::Files { entries }
}

async fn read_file(root: &Path, path: &str) -> StudioResponse {
    let rel = match clamp_rel(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !has_text_extension(&rel) {
        return err(StudioErrorCode::Forbidden, "not an editable text file");
    }
    let full = match resolve_existing(root, &rel).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = check_canonical_policy(root, &full, true) {
        return e;
    }
    match tokio::fs::metadata(&full).await {
        Ok(m) if m.len() > MAX_READ_BYTES => {
            return err(
                StudioErrorCode::TooLarge,
                format!("file is {} bytes (cap {MAX_READ_BYTES})", m.len()),
            )
        }
        Ok(_) => {}
        Err(_) => return err(StudioErrorCode::NotFound, "not found"),
    }
    let Ok(bytes) = tokio::fs::read(&full).await else {
        return err(StudioErrorCode::NotFound, "not found");
    };
    let Ok(content) = String::from_utf8(bytes) else {
        return err(StudioErrorCode::Invalid, "file is not valid UTF-8");
    };
    let sha256 = sha256_hex(content.as_bytes());
    StudioResponse::FileContent {
        path: rel.to_string_lossy().replace('\\', "/"),
        content,
        sha256,
    }
}

async fn write_file(
    root: &Path,
    path: &str,
    content: &str,
    expected_sha256: Option<&str>,
) -> StudioResponse {
    let rel = match clamp_rel(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !has_text_extension(&rel) {
        return err(StudioErrorCode::Forbidden, "not an editable text file");
    }
    if content.len() > MAX_WRITE_BYTES {
        return err(
            StudioErrorCode::TooLarge,
            format!("content is {} bytes (cap {MAX_WRITE_BYTES})", content.len()),
        );
    }
    // Auto-create missing parent directories: agent/user writes may
    // target new subtrees (plugs/multimeter.py). Every component
    // already passed clamp_rel (Normal-only, no excluded names), so
    // creation stays under the canonical root, and resolve_for_write
    // still canonicalize-confines the final parent afterwards.
    if let Some(parent_rel) = rel.parent() {
        if !parent_rel.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(root.join(parent_rel)).await {
                return err(
                    StudioErrorCode::Internal,
                    format!("cannot create parent directory: {e}"),
                );
            }
        }
    }

    let full = match resolve_for_write(root, &rel).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = check_canonical_policy(root, &full, true) {
        return e;
    }

    // Optimistic concurrency: refuse when the caller's baseline no
    // longer matches the disk. A missing file only conflicts if the
    // caller claimed a baseline (expected_sha256 set).
    if let Some(expected) = expected_sha256 {
        match tokio::fs::read(&full).await {
            Ok(current) => {
                if sha256_hex(&current) != expected {
                    return err(
                        StudioErrorCode::Conflict,
                        "file changed on disk since it was read",
                    );
                }
            }
            Err(_) => {
                return err(
                    StudioErrorCode::Conflict,
                    "file no longer exists on disk; re-read before writing",
                )
            }
        }
    }

    if let Err(e) = atomic_write(root, &full, content.as_bytes()).await {
        return e;
    }
    crate::log::info(&format!("studio: wrote {}", full.display()));
    StudioResponse::Written {
        path: rel.to_string_lossy().replace('\\', "/"),
        sha256: sha256_hex(content.as_bytes()),
    }
}

/// Atomic-ish write: temp file in the same directory, then rename.
/// A crash mid-write leaves the original intact. The rename
/// replaces the inode, so the temp file is CREATED with the
/// original's permission bits (unix) — chmod-after-write would
/// leave the new content world-readable (umask default) for a
/// window, and a crash in that window would strand it that way.
async fn atomic_write(root: &Path, full: &Path, bytes: &[u8]) -> Result<(), StudioResponse> {
    let parent = full.parent().unwrap_or(root);
    let tmp = parent.join(format!(
        ".tofupilot-studio-write-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if let Ok(meta) = tokio::fs::metadata(full).await {
        use std::os::unix::fs::PermissionsExt;
        opts.mode(meta.permissions().mode() & 0o7777);
    }
    let write_result = async {
        use tokio::io::AsyncWriteExt;
        let mut f = opts.open(&tmp).await?;
        f.write_all(bytes).await?;
        f.flush().await
    }
    .await;
    if let Err(e) = write_result {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err(StudioErrorCode::Internal, format!("write failed: {e}")));
    }
    if let Err(e) = tokio::fs::rename(&tmp, full).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err(
            StudioErrorCode::Internal,
            format!("rename failed: {e}"),
        ));
    }
    Ok(())
}

async fn create_dir(root: &Path, path: &str) -> StudioResponse {
    let rel = match clamp_rel(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // Descend one component at a time instead of `create_dir_all`.
    // Confinement can only be checked on a path that exists, so each
    // level is created and then re-canonicalized: an intermediate that
    // already exists may be a symlink out of the root (or into an
    // excluded dir), and `create_dir_all` would follow it and plant the
    // new directory there before anything got a chance to object.
    let mut current = root.to_path_buf();
    let last = rel.components().count();
    for (index, comp) in rel.components().enumerate() {
        current.push(comp);
        let fresh = match tokio::fs::create_dir(&current).await {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => {
                return err(
                    StudioErrorCode::Internal,
                    format!("cannot create directory: {e}"),
                )
            }
        };
        if !fresh {
            // An existing FILE on the path is not something to build
            // through, and an existing final component means the
            // caller's folder is not the one we would be reporting.
            match tokio::fs::metadata(&current).await {
                Ok(m) if !m.is_dir() => {
                    return err(StudioErrorCode::Conflict, "a file with that name exists")
                }
                Ok(_) if index + 1 == last => {
                    return err(StudioErrorCode::Conflict, "directory already exists")
                }
                Ok(_) => {}
                Err(e) => {
                    return err(
                        StudioErrorCode::Internal,
                        format!("cannot inspect directory: {e}"),
                    )
                }
            }
        }
        let Ok(canon) = tokio::fs::canonicalize(&current).await else {
            return err(
                StudioErrorCode::Internal,
                "created directory vanished before it could be checked",
            );
        };
        if let Err(e) = check_canonical_policy(root, &canon, false) {
            // Only unwind what this request made: an escaping symlink
            // that was already there is not ours to remove.
            if fresh {
                let _ = tokio::fs::remove_dir(&current).await;
            }
            return e;
        }
        current = canon;
    }
    let rel_display = rel.to_string_lossy().replace('\\', "/");
    crate::log::info(&format!("studio: created directory {rel_display}"));
    StudioResponse::DirCreated { path: rel_display }
}

async fn validate(root: &Path, path: Option<&str>) -> StudioResponse {
    let yaml_path = match path {
        Some(p) => {
            let rel = match clamp_rel(p) {
                Ok(r) => r,
                Err(e) => return e,
            };
            // Only YAML procedures are loadable; refusing other
            // extensions up front keeps the loader from parsing (and
            // quoting back) arbitrary project files.
            if !matches!(
                rel.extension().and_then(|e| e.to_str()),
                Some("yaml") | Some("yml")
            ) {
                return err(StudioErrorCode::Invalid, "not a procedure YAML file");
            }
            let full = match resolve_existing(root, &rel).await {
                Ok(full) => full,
                Err(e) => return e,
            };
            if let Err(e) = check_canonical_policy(root, &full, true) {
                return e;
            }
            full
        }
        None => match crate::commands::run::engine::find_procedure_yaml(root) {
            Some(p) => {
                // Same canonical policy as the explicit-path arm: a
                // procedure.yaml that is a symlink out of the root (or
                // into an excluded dir) must not be loadable implicitly
                // when it would be refused when addressed by name.
                let Ok(canon) = tokio::fs::canonicalize(&p).await else {
                    return err(StudioErrorCode::NotFound, "procedure file not found");
                };
                if let Err(e) = check_canonical_policy(root, &canon, true) {
                    return e;
                }
                canon
            }
            None => {
                return err(
                    StudioErrorCode::NotFound,
                    "no procedure.yaml found in the studio root",
                )
            }
        },
    };
    // Loader runs on a blocking thread: it does synchronous file IO,
    // and on success every `python:` reference is resolved against the
    // procedure directory. Structural loading alone lets a dangling ref
    // through, and it then fails only mid-run (tp_worker traceback for
    // a phase, silently omitted plug) — this op validates the project
    // as it is on disk, so here the missing file is an error.
    let result = tokio::task::spawn_blocking(move || {
        execution_engine::procedure::loader::load_procedure_definition(&yaml_path).map(|def| {
            let procedure_dir = yaml_path.parent().unwrap_or(Path::new("."));
            def.resolve_python_refs(procedure_dir, None)
        })
    })
    .await;
    let diagnostics = match result {
        Ok(Ok(ref_problems)) => ref_problems
            .into_iter()
            .map(|message| StudioDiagnostic {
                severity: StudioDiagnosticSeverity::Error,
                message,
                path: None,
            })
            .collect(),
        Ok(Err(message)) => vec![StudioDiagnostic {
            severity: StudioDiagnosticSeverity::Error,
            message,
            path: None,
        }],
        Err(join_err) => vec![StudioDiagnostic {
            severity: StudioDiagnosticSeverity::Error,
            message: format!("validation task failed: {join_err}"),
            path: None,
        }],
    };
    StudioResponse::Diagnostics { diagnostics }
}

/// Validate proposed procedure content without touching the disk. The
/// loader chain after the file read is purely structural
/// (`load_procedure_definition_from_str`), so no temp file is needed;
/// the target file need not exist yet. `path` keeps the addressing
/// rules of `validate` (clamped, YAML-only) so a proposal is refused
/// exactly where a disk validation of the same path would be.
///
/// Deliberately does NOT resolve `python:` references: proposed YAML
/// legitimately precedes the modules it references (the agent writes
/// procedure.yaml before the phase/plug files in one edit sequence),
/// and the browser pre-validation hook auto-rejects on ANY diagnostic
/// — flagging a not-yet-written module here would reject valid edits.
/// The disk-based `validate` catches dangling refs once writes land.
async fn validate_content(path: &str, content: String) -> StudioResponse {
    let rel = match clamp_rel(path) {
        Ok(r) => r,
        Err(e) => return e,
    };
    if !matches!(
        rel.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    ) {
        return err(StudioErrorCode::Invalid, "not a procedure YAML file");
    }
    if content.len() > MAX_WRITE_BYTES {
        return err(
            StudioErrorCode::TooLarge,
            format!("content is {} bytes (cap {MAX_WRITE_BYTES})", content.len()),
        );
    }
    // Blocking thread for symmetry with `validate`: parsing large YAML
    // is CPU-bound work that should not sit on the async executor.
    let result = tokio::task::spawn_blocking(move || {
        execution_engine::procedure::loader::load_procedure_definition_from_str(&content)
    })
    .await;
    let diagnostics = match result {
        Ok(Ok(_)) => Vec::new(),
        Ok(Err(message)) => vec![StudioDiagnostic {
            severity: StudioDiagnosticSeverity::Error,
            message,
            path: None,
        }],
        Err(join_err) => vec![StudioDiagnostic {
            severity: StudioDiagnosticSeverity::Error,
            message: format!("validation task failed: {join_err}"),
            path: None,
        }],
    };
    StudioResponse::Diagnostics { diagnostics }
}

/// Write a binary integration resource (base64 payload) under
/// `resources/`. Confinement + allowlist + cap keep this op from ever
/// replacing a procedure source: sources go through `write_file`
/// (text, approval-gated), resources through here. There is no diff to
/// review on a binary, so replacing an existing resource requires the
/// explicit `overwrite` flag — the refusal carries the existing file's
/// sha256 so the caller can tell an identical re-upload from a clobber.
async fn write_resource(
    root: &Path,
    path: &str,
    content_base64: &str,
    overwrite: bool,
) -> StudioResponse {
    let rel = match clamp_rel(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !rel.starts_with(RESOURCE_DIR) {
        return err(
            StudioErrorCode::Forbidden,
            format!("resources must live under {RESOURCE_DIR}/"),
        );
    }
    if !matches!(
        rel.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some(ext) if RESOURCE_EXTENSIONS.contains(&ext)
    ) {
        return err(StudioErrorCode::Forbidden, "not an allowed resource type");
    }
    // Cap the encoded payload before decoding: base64 inflates 4/3, so
    // this bounds the decode allocation too.
    if content_base64.len() > MAX_RESOURCE_BYTES / 3 * 4 + 4 {
        return err(
            StudioErrorCode::TooLarge,
            format!("resource exceeds the {MAX_RESOURCE_BYTES} byte cap"),
        );
    }
    use base64::Engine;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(content_base64) else {
        return err(
            StudioErrorCode::Invalid,
            "content_base64 is not valid base64",
        );
    };
    if bytes.len() > MAX_RESOURCE_BYTES {
        return err(
            StudioErrorCode::TooLarge,
            format!(
                "resource is {} bytes (cap {MAX_RESOURCE_BYTES})",
                bytes.len()
            ),
        );
    }
    // Same parent auto-creation as write_file: resources/ typically
    // does not exist before the first upload. Every component already
    // passed clamp_rel, so creation stays under the canonical root.
    if let Some(parent_rel) = rel.parent() {
        if !parent_rel.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(root.join(parent_rel)).await {
                return err(
                    StudioErrorCode::Internal,
                    format!("cannot create parent directory: {e}"),
                );
            }
        }
    }
    let full = match resolve_for_write(root, &rel).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    // require_text = false: this is the one binary write of the
    // surface; the allowlist above is its extension policy.
    if let Err(e) = check_canonical_policy(root, &full, false) {
        return e;
    }
    if !overwrite {
        if let Ok(existing) = tokio::fs::read(&full).await {
            return err(
                StudioErrorCode::Conflict,
                format!(
                    "resource already exists (sha256 {}); re-send with overwrite to replace it",
                    sha256_hex(&existing)
                ),
            );
        }
    }
    if let Err(e) = atomic_write(root, &full, &bytes).await {
        return e;
    }
    crate::log::info(&format!(
        "studio: wrote resource {} ({} bytes)",
        full.display(),
        bytes.len()
    ));
    StudioResponse::Written {
        path: rel.to_string_lossy().replace('\\', "/"),
        sha256: sha256_hex(&bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_extension_allow_list() {
        assert!(has_text_extension(Path::new("procedure.yaml")));
        assert!(has_text_extension(Path::new("phases/main.PY")));
        assert!(!has_text_extension(Path::new("image.png")));
        assert!(!has_text_extension(Path::new(".env")));
        assert!(!has_text_extension(Path::new("binary")));
    }

    #[test]
    fn clamp_rejects_excluded_dirs_and_empty() {
        assert!(clamp_rel("venv/pyvenv.cfg").is_err());
        assert!(clamp_rel(".git/config").is_err());
        assert!(clamp_rel("").is_err());
        assert!(clamp_rel("../..").is_err());
        let ok = clamp_rel("../phases/main.py").unwrap();
        assert_eq!(ok, PathBuf::from("phases/main.py"));
    }

    #[tokio::test]
    async fn validate_flags_dangling_python_refs_but_validate_content_does_not() {
        let dir = tempfile::tempdir().unwrap();
        // macOS: temp dirs live behind a /var -> /private/var symlink and
        // the handler canonicalizes, so the root must be canonical too.
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("phases")).unwrap();
        std::fs::write(
            root.join("phases/main.py"),
            "def check(measurements):\n    measurements.x = 1\n",
        )
        .unwrap();
        // The 2026-08-13 incident spelling: every dot is a directory, so
        // this resolves to phases/main/check.py (missing) while the author
        // meant check() in phases/main.py.
        let dangling = "name: A\nmain:\n  - key: p1\n    name: P1\n    python: phases.main.check\n";
        std::fs::write(root.join("procedure.yaml"), dangling).unwrap();

        let res = validate(&root, None).await;
        let StudioResponse::Diagnostics { diagnostics } = res else {
            panic!("expected Diagnostics, got {res:?}");
        };
        assert_eq!(diagnostics.len(), 1, "unexpected: {diagnostics:?}");
        assert_eq!(diagnostics[0].severity, StudioDiagnosticSeverity::Error);
        assert!(
            diagnostics[0].message.contains("Python file not found"),
            "unexpected message: {}",
            diagnostics[0].message
        );

        // Same content through validate_content: silent by contract — a
        // proposal's referenced modules may not be written yet, and the
        // browser hook auto-rejects on any diagnostic.
        let ok = validate_content("procedure.yaml", dangling.to_string()).await;
        let StudioResponse::Diagnostics { diagnostics } = ok else {
            panic!("expected Diagnostics, got {ok:?}");
        };
        assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");

        // The correct ':' spelling validates clean on disk.
        std::fs::write(
            root.join("procedure.yaml"),
            dangling.replace("phases.main.check", "phases.main:check"),
        )
        .unwrap();
        let res = validate(&root, None).await;
        let StudioResponse::Diagnostics { diagnostics } = res else {
            panic!("expected Diagnostics, got {res:?}");
        };
        assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");
    }

    #[tokio::test]
    async fn validate_content_checks_a_proposal_without_touching_disk() {
        // A minimal valid procedure passes with no diagnostics.
        let valid = "name: A\nmain:\n  - name: P1\n";
        let ok = validate_content("procedure.yaml", valid.to_string()).await;
        let StudioResponse::Diagnostics { diagnostics } = ok else {
            panic!("expected Diagnostics, got {ok:?}");
        };
        assert!(diagnostics.is_empty(), "unexpected: {diagnostics:?}");

        // A schema violation is reported as a diagnostic, not an error.
        let invalid = "name: A\nmain: []\n";
        let bad = validate_content("procedure.yaml", invalid.to_string()).await;
        let StudioResponse::Diagnostics { diagnostics } = bad else {
            panic!("expected Diagnostics, got {bad:?}");
        };
        assert_eq!(diagnostics.len(), 1);

        // Non-YAML targets are refused like `validate` refuses them.
        let refused = validate_content("phases/main.py", "x = 1".to_string()).await;
        assert!(matches!(
            refused,
            StudioResponse::Error {
                code: StudioErrorCode::Invalid,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn write_resource_confines_allowlists_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        use base64::Engine;
        let payload: Vec<u8> = vec![0u8, 159, 146, 150]; // invalid UTF-8 on purpose
        let b64 = base64::engine::general_purpose::STANDARD.encode(&payload);

        // Roundtrip: parent dir auto-created, bytes land verbatim.
        let w = write_resource(root, "resources/ref/cal.bin", &b64, false).await;
        let StudioResponse::Written { path, sha256 } = w else {
            panic!("expected Written, got {w:?}");
        };
        assert_eq!(path, "resources/ref/cal.bin");
        let on_disk = std::fs::read(root.join("resources/ref/cal.bin")).unwrap();
        assert_eq!(on_disk, payload);
        assert_eq!(sha256, sha256_hex(&payload));

        // Re-upload without overwrite: refused with a Conflict that
        // carries the existing sha, and the bytes are untouched.
        let other = base64::engine::general_purpose::STANDARD.encode(b"other bytes");
        let clash = write_resource(root, "resources/ref/cal.bin", &other, false).await;
        let StudioResponse::Error { code, message } = clash else {
            panic!("expected Error, got {clash:?}");
        };
        assert!(matches!(code, StudioErrorCode::Conflict));
        assert!(message.contains(&sha256_hex(&payload)));
        assert_eq!(
            std::fs::read(root.join("resources/ref/cal.bin")).unwrap(),
            payload
        );

        // Explicit overwrite replaces the bytes.
        let replaced = write_resource(root, "resources/ref/cal.bin", &other, true).await;
        assert!(matches!(replaced, StudioResponse::Written { .. }));
        assert_eq!(
            std::fs::read(root.join("resources/ref/cal.bin")).unwrap(),
            b"other bytes"
        );

        // Outside resources/: refused, even with an allowed extension.
        let outside = write_resource(root, "firmware.bin", &b64, false).await;
        assert!(matches!(
            outside,
            StudioResponse::Error {
                code: StudioErrorCode::Forbidden,
                ..
            }
        ));

        // Disallowed extension: a binary payload must not become a
        // procedure source.
        let source = write_resource(root, "resources/procedure.yaml", &b64, false).await;
        assert!(matches!(
            source,
            StudioResponse::Error {
                code: StudioErrorCode::Forbidden,
                ..
            }
        ));

        // Invalid base64 is a typed Invalid, not a daemon error.
        let garbage = write_resource(root, "resources/x.bin", "not-base64!!", false).await;
        assert!(matches!(
            garbage,
            StudioResponse::Error {
                code: StudioErrorCode::Invalid,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn write_then_read_roundtrip_with_conflict_detection() {
        let dir = tempfile::tempdir().unwrap();
        // Handlers require a canonical root (enable_studio guarantees
        // it in production); tempdirs sit behind /var symlinks on macOS.
        let root = &dir.path().canonicalize().unwrap();

        // Fresh write (no baseline).
        let w = write_file(root, "procedure.yaml", "name: A\n", None).await;
        let StudioResponse::Written { sha256, .. } = w else {
            panic!("expected Written, got {w:?}");
        };

        // Read back.
        let r = read_file(root, "procedure.yaml").await;
        let StudioResponse::FileContent {
            content,
            sha256: read_sha,
            ..
        } = r
        else {
            panic!("expected FileContent, got {r:?}");
        };
        assert_eq!(content, "name: A\n");
        assert_eq!(read_sha, sha256);

        // Write with matching baseline succeeds.
        let w2 = write_file(root, "procedure.yaml", "name: B\n", Some(&sha256)).await;
        assert!(matches!(w2, StudioResponse::Written { .. }));

        // Stale baseline now conflicts.
        let w3 = write_file(root, "procedure.yaml", "name: C\n", Some(&sha256)).await;
        assert!(matches!(
            w3,
            StudioResponse::Error {
                code: StudioErrorCode::Conflict,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn write_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();

        let w = write_file(
            root,
            "plugs/multimeter.py",
            "class Multimeter:\n    pass\n",
            None,
        )
        .await;
        assert!(matches!(w, StudioResponse::Written { .. }), "got {w:?}");
        assert!(root.join("plugs/multimeter.py").is_file());

        // Excluded dirs stay refused even via the create path.
        let bad = write_file(root, "venv/hack.py", "x", None).await;
        assert!(matches!(
            bad,
            StudioResponse::Error {
                code: StudioErrorCode::Forbidden,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn create_dir_makes_nested_dirs_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();

        let c = create_dir(root, "phases/setup").await;
        assert!(
            matches!(&c, StudioResponse::DirCreated { path } if path == "phases/setup"),
            "got {c:?}"
        );
        assert!(root.join("phases/setup").is_dir());

        // Creating it again is a conflict, not a silent success: the
        // sidebar must not report a folder it did not make.
        let again = create_dir(root, "phases/setup").await;
        assert!(matches!(
            again,
            StudioResponse::Error {
                code: StudioErrorCode::Conflict,
                ..
            }
        ));
        // An existing parent is fine as long as the leaf is new.
        let deeper = create_dir(root, "phases/setup/probe").await;
        assert!(matches!(deeper, StudioResponse::DirCreated { .. }));
    }

    #[tokio::test]
    async fn create_dir_refuses_excluded_files_and_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();

        // Excluded and hidden names are refused by the same rule the
        // rest of the surface uses.
        for path in ["venv", "node_modules/pkg", ".git", "plugs/.hidden"] {
            let bad = create_dir(root, path).await;
            assert!(
                matches!(
                    bad,
                    StudioResponse::Error {
                        code: StudioErrorCode::Forbidden,
                        ..
                    }
                ),
                "{path} was not refused: {bad:?}"
            );
        }

        // A file already holding the name is a conflict, not something
        // to build a subtree through.
        tokio::fs::write(root.join("phases.py"), "x = 1\n")
            .await
            .unwrap();
        let clash = create_dir(root, "phases.py").await;
        assert!(matches!(
            clash,
            StudioResponse::Error {
                code: StudioErrorCode::Conflict,
                ..
            }
        ));
        let through = create_dir(root, "phases.py/inner").await;
        assert!(matches!(
            through,
            StudioResponse::Error {
                code: StudioErrorCode::Conflict,
                ..
            }
        ));

        // Traversal collapses inside the root rather than escaping it.
        let up = create_dir(root, "../escape").await;
        assert!(
            matches!(&up, StudioResponse::DirCreated { path } if path == "escape"),
            "got {up:?}"
        );
        assert!(root.join("escape").is_dir());
        assert!(!dir.path().parent().unwrap().join("escape").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_dir_does_not_follow_a_symlinked_parent_out_of_the_root() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();

        let bad = create_dir(root, "link/planted").await;
        assert!(
            matches!(
                bad,
                StudioResponse::Error {
                    code: StudioErrorCode::Forbidden,
                    ..
                }
            ),
            "got {bad:?}"
        );
        assert!(!outside.path().join("planted").exists());
    }

    #[tokio::test]
    async fn escapes_and_binaries_refused() {
        let dir = tempfile::tempdir().unwrap();
        // Handlers require a canonical root (enable_studio guarantees
        // it in production); tempdirs sit behind /var symlinks on macOS.
        let root = &dir.path().canonicalize().unwrap();
        tokio::fs::write(root.join("ok.py"), "x = 1\n")
            .await
            .unwrap();

        // Traversal collapses inside the root and 404s (no such file).
        let r = read_file(root, "../../etc/passwd").await;
        assert!(matches!(r, StudioResponse::Error { .. }));

        // Non-text extension refused on read and write.
        let r = read_file(root, "logo.png").await;
        assert!(matches!(
            r,
            StudioResponse::Error {
                code: StudioErrorCode::Forbidden,
                ..
            }
        ));
        let w = write_file(root, "logo.png", "data", None).await;
        assert!(matches!(
            w,
            StudioResponse::Error {
                code: StudioErrorCode::Forbidden,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn listing_skips_hidden_and_venv() {
        let dir = tempfile::tempdir().unwrap();
        // Handlers require a canonical root (enable_studio guarantees
        // it in production); tempdirs sit behind /var symlinks on macOS.
        let root = &dir.path().canonicalize().unwrap();
        tokio::fs::create_dir(root.join("venv")).await.unwrap();
        tokio::fs::create_dir(root.join("phases")).await.unwrap();
        tokio::fs::write(root.join(".env"), "SECRET=1")
            .await
            .unwrap();
        tokio::fs::write(root.join("procedure.yaml"), "name: A\n")
            .await
            .unwrap();

        let r = list_files(root, None).await;
        let StudioResponse::Files { entries } = r else {
            panic!("expected Files, got {r:?}");
        };
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["phases", "procedure.yaml"]);
    }
}
