// Rust 1.98's clippy extended `result_large_err` to async fns, flagging
// every helper here that returns `Result<_, StudioResponse>` (the Err is
// ~672 bytes). The real fix is boxing `StudioResponse` in
// `station_protocol` — a signature change across this file and its
// callers, not a lint-appeasement edit. Allowed until that pass happens;
// these are per-RPC-request paths, not hot loops.
#![allow(clippy::result_large_err)]

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
    StudioProject, StudioRequest, StudioResponse, StudioSequence, StudioSequenceAggregation,
    StudioSequenceAxis, StudioSequenceMeasurement, StudioSequencePhase, StudioSequencePlug,
    StudioSequencePlugConfigEntry, StudioSequenceRetry, StudioSequenceSubUnit, StudioSequenceUi,
    StudioSequenceUnit, StudioSequenceUnitField, StudioSequenceValidator,
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

/// How deep procedure discovery walks below the project root. A repo
/// with one procedure per subdirectory needs 1; the extra levels cover
/// a `procedures/<name>/` grouping. Bounded because the open folder can
/// be anything a human picked in the dialog — a home directory or
/// Downloads must not turn `project_info` into a full-disk walk.
const MAX_PROCEDURE_DEPTH: usize = 3;
/// Cap on procedures reported. Far above any real bench repo; exists so
/// a pathological tree cannot produce an unbounded reply.
const MAX_PROCEDURES: usize = 64;

/// A project root a human deliberately handed to this session, either
/// by launching `tofupilot studio` there or by picking it in the OS
/// folder dialog. The browser can never mint one: it may only select
/// among roots already in this list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantedRoot {
    /// Canonicalized at grant time, so `starts_with` confinement is
    /// sound and no request has to re-walk the realpath.
    pub path: PathBuf,
    /// The directory's own name, for the project switcher.
    pub name: String,
}

impl GrantedRoot {
    /// `path` should be canonical — `enable_studio` and the picker
    /// canonicalize before calling. `with_recents_file` is the third
    /// caller and feeds paths straight from JSON WITHOUT canonicalizing:
    /// that fails closed (every resolver rejects a non-canonical root),
    /// but it means this constructor cannot assume canonicality — only
    /// the resolvers' checks make the invariant hold.
    fn new(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .to_string();
        Self { path, name }
    }
}

/// Studio configuration installed by `tofupilot studio` after
/// `Server::start`. Absent on every other invocation, which keeps the
/// whole RPC surface 403.
#[derive(Clone)]
pub struct StudioConfig {
    /// Granted roots, most recently opened first. The head is the
    /// active project, so "most recent" and "active" cannot drift
    /// apart the way a separate index would.
    ///
    /// INVARIANT: never empty. `new` seeds it and nothing removes the
    /// last entry, so `active()` is total.
    granted: Vec<GrantedRoot>,
    /// Where this session reads and writes its recents. A field rather
    /// than a call to `studio_recents::recents_path()` at each use: the
    /// location is part of the daemon's configuration, and a test that
    /// exercises a project switch must not rewrite the developer's own
    /// `~/.tofupilot/studio-recents.json`.
    recents_file: PathBuf,
    /// Root-relative path of the procedure the session is working on,
    /// `None` until one is chosen (or when the project holds none).
    /// A hint rather than an authority: `project_info` re-discovers on
    /// every call and drops this if the file is gone, so a deleted or
    /// renamed procedure cannot leave the session pointing at nothing.
    active_procedure: Option<PathBuf>,
}

impl StudioConfig {
    /// Seeds the granted set with `root` at the head, followed by the
    /// previously opened projects found in `recents_file`.
    ///
    /// Past roots count as granted: each was designated by a human at
    /// least once, and re-asking on every launch would rebuild exactly
    /// the friction the switcher removes. What a past root does NOT
    /// grant is anything new — only the folder dialog does that.
    ///
    /// What this convenience now costs, stated honestly: since the
    /// surface gained delete/move/copy, a session token reaches the
    /// last MAX_RECENTS trees the operator ever opened, not just the
    /// one they launched. Two caps keep that acceptable: deletes go to
    /// the OS trash (recoverable), and the token is ephemeral,
    /// loopback-bound and Origin-allow-listed. Revisit if either cap
    /// weakens.
    pub fn with_recents_file(root: PathBuf, recents_file: PathBuf) -> Self {
        let mut granted = vec![GrantedRoot::new(root)];
        for past in crate::commands::studio_recents::existing_from(&recents_file) {
            if !granted.iter().any(|g| g.path == past) {
                granted.push(GrantedRoot::new(past));
            }
        }
        Self {
            granted,
            recents_file,
            active_procedure: None,
        }
    }

    /// The project every file operation is confined to.
    pub fn active(&self) -> &Path {
        &self.granted[0].path
    }

    /// Roots the switcher may offer, active first.
    pub fn granted(&self) -> &[GrantedRoot] {
        &self.granted
    }

    /// Promote an already-granted root to active.
    ///
    /// Returns `None` for anything not in the set — the check is
    /// membership, never a filesystem lookup, so a caller holding the
    /// session token cannot use this to learn what exists on the host.
    pub fn activate(&mut self, path: &Path) -> Option<GrantedRoot> {
        let index = self.granted.iter().position(|g| g.path == path)?;
        let root = self.granted.remove(index);
        self.granted.insert(0, root.clone());
        // The selection is root-relative, so it means something else
        // under a different root — carrying it over would silently
        // point the session at another project's file of the same name.
        self.active_procedure = None;
        Some(root)
    }

    /// The procedure the session is working on, root-relative.
    pub fn active_procedure(&self) -> Option<&Path> {
        self.active_procedure.as_deref()
    }

    /// Directory a run should execute: the active procedure's holding
    /// directory, or the root itself when none is selected (the shape
    /// every single-procedure project has).
    pub fn active_procedure_dir(&self) -> PathBuf {
        let root = self.active();
        match self
            .active_procedure
            .as_deref()
            .and_then(|rel| rel.parent())
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            Some(parent) => root.join(parent),
            None => root.to_path_buf(),
        }
    }

    /// Point the session at `rel` (root-relative). Callers must have
    /// checked it against discovery first — this is bookkeeping, not
    /// the authorization step.
    pub fn set_active_procedure(&mut self, rel: PathBuf) {
        self.active_procedure = Some(rel);
    }

    /// Drop a selection that discovery no longer reports (deleted or
    /// renamed procedure), so the session falls back to the first one
    /// found instead of pointing at a file that is gone.
    pub fn forget_active_procedure(&mut self) {
        self.active_procedure = None;
    }

    /// Add a new root to the granted set and make it active — or, when
    /// it is already granted, just promote it. The ONLY caller is
    /// `pick_project`, carrying a path a human chose in the OS folder
    /// dialog: this method growing the set is exactly the capability
    /// the dialog exists to gate. `path` must be canonical, like every
    /// other entry (the caller canonicalizes).
    pub fn grant(&mut self, path: PathBuf) -> GrantedRoot {
        if let Some(existing) = self.activate(&path) {
            return existing;
        }
        let root = GrantedRoot::new(path);
        self.granted.insert(0, root.clone());
        self.active_procedure = None;
        root
    }
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
/// safe). `root` is already canonical (every granted root is stored
/// canonicalized — see `StudioConfig::grant`), so only the target is
/// walked.
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

/// Refuse when `rel`'s FINAL component is itself a symlink.
/// `resolve_existing` canonicalizes through it, so a destructive op
/// would land on the pointee while the reply names the link — a
/// project pinning `procedure.yaml -> procedures/v2.yaml` would lose
/// `procedures/v2.yaml` and keep a dangling link. Checked on the
/// un-canonicalized path: `symlink_metadata` follows intermediate
/// links but not the last component, and where intermediates land is
/// already the canonical policy's job. Not reachable from the sidebar
/// today (`list_files` skips symlinks), so this guards direct callers.
async fn refuse_final_symlink(canon_root: &Path, rel: &Path) -> Result<(), StudioResponse> {
    let full = canon_root.join(rel);
    if let Ok(meta) = tokio::fs::symlink_metadata(&full).await {
        if meta.file_type().is_symlink() {
            return Err(err(
                StudioErrorCode::Forbidden,
                "the entry is a symlink; Studio refuses to touch its target through it",
            ));
        }
    }
    Ok(())
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

    let reply = dispatch(&state, &config, request).await;
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

/// `config` is a snapshot taken at authorization. Ops that only read
/// the filesystem work off it; the two that change which project is
/// active go back through `state`, because the snapshot is a clone and
/// writing to it would be lost.
async fn dispatch(
    state: &AppState,
    config: &StudioConfig,
    request: StudioRequest,
) -> StudioResponse {
    let root = config.active();
    match request {
        StudioRequest::ListProjects {} => list_projects(config).await,
        StudioRequest::OpenProject { path } => open_project(state, &path).await,
        StudioRequest::OpenProcedure { path } => open_procedure(state, root, &path).await,
        StudioRequest::PickProject {} => pick_project(state).await,
        StudioRequest::ConfirmPick {} => confirm_pick(state).await,
        StudioRequest::DiscardPick {} => discard_pick(state).await,
        StudioRequest::ProjectInfo {} => project_info(state, root).await,
        StudioRequest::ListFiles { dir } => list_files(root, dir.as_deref()).await,
        StudioRequest::ReadFile { path } => read_file(root, &path).await,
        StudioRequest::WriteFile {
            path,
            content,
            expected_sha256,
        } => write_file(root, &path, &content, expected_sha256.as_deref()).await,
        StudioRequest::CreateDir { path } => create_dir(root, &path).await,
        StudioRequest::DeleteEntry { path } => delete_entry(root, &path).await,
        StudioRequest::MoveEntry { from, to } => move_entry(root, &from, &to).await,
        StudioRequest::CopyEntry { from, to } => copy_entry(root, &from, &to).await,
        StudioRequest::Validate { path } => validate(config, root, path.as_deref()).await,
        StudioRequest::ValidateContent { path, content } => validate_content(&path, content).await,
        StudioRequest::WriteResource {
            path,
            content_base64,
            overwrite,
        } => write_resource(root, &path, &content_base64, overwrite).await,
        StudioRequest::GetSequence {} => get_sequence(config, root).await,
    }
}

fn to_wire(root: &GrantedRoot) -> StudioProject {
    StudioProject {
        path: root.path.display().to_string(),
        name: root.name.clone(),
    }
}

/// The switcher's list. Roots whose directory has vanished are dropped
/// from the reply but kept in the granted set: a disconnected network
/// share should not permanently forget a project. The stats run in
/// spawn_blocking for the same reason: a recents entry on a vanished
/// SMB/NFS mount blocks `is_dir()` for the mount's timeout, and doing
/// that on a tokio worker parks the runtime exactly when the switcher
/// is open on "Loading…".
async fn list_projects(config: &StudioConfig) -> StudioResponse {
    let active = config.active().display().to_string();
    let granted: Vec<GrantedRoot> = config.granted().to_vec();
    let projects = match tokio::task::spawn_blocking(move || {
        granted
            .iter()
            .filter(|g| g.path.is_dir())
            .map(to_wire)
            .collect()
    })
    .await
    {
        Ok(projects) => projects,
        // A panicked stat task is a failure to report, not an empty
        // account: defaulting here made the switcher claim the session
        // holds no projects at all.
        Err(join_err) => {
            return err(
                StudioErrorCode::Internal,
                format!("could not stat the granted roots: {join_err}"),
            )
        }
    };
    StudioResponse::Projects { projects, active }
}

/// Switch the active project. Selection only — see `StudioConfig::activate`.
async fn open_project(state: &AppState, path: &str) -> StudioResponse {
    // A run pins the root it started against (the dispatcher captured
    // it), so moving the active project mid-run would leave the engine
    // and the UI describing two different projects.
    if state
        .studio_run_active
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return err(
            StudioErrorCode::Busy,
            "a run is in progress; stop it before switching project",
        );
    }

    let mut guard = state.studio.lock().await;
    let Some(config) = guard.as_mut() else {
        return err(StudioErrorCode::Forbidden, "studio surface is not enabled");
    };
    match config.activate(Path::new(path)) {
        Some(root) => {
            // Persist so the next launch reopens it, and so the head of
            // the recents file and the head of the granted set agree.
            crate::commands::studio_recents::record_in_or_warn(&config.recents_file, &root.path);
            StudioResponse::Opened {
                project: to_wire(&root),
            }
        }
        // Deliberately the same answer for "never granted" and "does
        // not exist": the reply must not tell a token holder which
        // paths are real.
        None => err(
            StudioErrorCode::Forbidden,
            "not an opened project; pick the folder first",
        ),
    }
}

/// Grant a NEW root, through the OS folder dialog on the daemon's own
/// machine. The page only triggers the dialog; the choice — and with it
/// the grant — belongs to the human at the machine, through a window
/// no browser message can drive.
async fn pick_project(state: &AppState) -> StudioResponse {
    let busy_msg = "a run is in progress; stop it before opening a project";
    if state
        .studio_run_active
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return err(StudioErrorCode::Busy, busy_msg);
    }
    // Opening a fresh dialog invalidates any stale parked offer FIRST,
    // on every exit of this function including Cancelled: a pending
    // pick must never outlive the dialog interaction that created it,
    // or a later bare `confirm_pick` grants a folder whose warning the
    // human may never have answered.
    *state.studio_pending_pick.lock().await = None;

    // One dialog at a time — reentrance, not security (two Studio tabs
    // or a double-click must not stack OS windows). The gate is an
    // atomic OWNED BY THE JOB: it releases in the job's Drop on the
    // host side, i.e. only once the native panel actually closed. A
    // gate held by this request future would release when the page's
    // 120s give-up aborts it — while the panel is still on screen —
    // letting a second dialog queue behind a window the human still
    // has to answer.
    if state
        .studio_dialog_open
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        return err(StudioErrorCode::Busy, "a folder dialog is already open");
    }
    // From here every early return must clear the flag itself: the job
    // that would otherwise own it has not been created yet.
    let host = state.studio_dialog_tx.lock().await;
    let Some(tx) = host.as_ref() else {
        // No host loop was installed (not a `tofupilot studio`
        // process). Typed refusal, not a hang.
        state
            .studio_dialog_open
            .store(false, std::sync::atomic::Ordering::Release);
        return err(
            StudioErrorCode::Unsupported,
            "this daemon cannot open a folder dialog",
        );
    };

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let job = crate::local_ws::StudioDialogJob::new(reply_tx, state.studio_dialog_open.clone());
    if tx.send(job).await.is_err() {
        // The send moved the job in and dropped it on failure, which
        // already cleared the flag — nothing more to unwind.
        return err(StudioErrorCode::Internal, "the dialog host is gone");
    }
    drop(host);
    let picked = match reply_rx.await {
        Ok(choice) => choice,
        Err(_) => {
            return err(
                StudioErrorCode::Internal,
                "the dialog host dropped the request",
            )
        }
    };

    let Some(path) = picked else {
        return err(StudioErrorCode::Cancelled, "no folder was chosen");
    };
    // The dialog returns a real path, but canonicalize anyway: every
    // entry in the granted set must be canonical for `starts_with`
    // confinement to hold, aliases and symlinks included.
    let canon = match tokio::fs::canonicalize(&path).await {
        Ok(c) => c,
        Err(e) => {
            return err(
                StudioErrorCode::Internal,
                format!("could not resolve the picked folder: {e}"),
            )
        }
    };

    // Re-check: a run may have started while the dialog sat open. A
    // pick is an open, and opens are refused mid-run — grant nothing
    // rather than leave a granted-but-inactive root nobody asked for.
    if state
        .studio_run_active
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return err(StudioErrorCode::Busy, busy_msg);
    }

    // A folder with no procedure is almost always a mis-click ($HOME,
    // Documents), and a grant is permanent full read/write for every
    // file op INCLUDING the agent's read tool. Park it and make the
    // page ask a second, explicit yes — the launch path applies the
    // same rule (`resolve_without_path` gates on a procedure existing).
    // Not a refusal: opening an empty folder to create a procedure in
    // it is a supported flow, it just does not happen by accident.
    if discover_procedures(&canon).await.is_empty() {
        let name = canon
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| canon.to_string_lossy().into_owned());
        *state.studio_pending_pick.lock().await = Some(canon.clone());
        return StudioResponse::PickedEmpty {
            path: canon.to_string_lossy().into_owned(),
            name,
        };
    }
    *state.studio_pending_pick.lock().await = None;

    grant_and_open(state, canon).await
}

/// Grant the folder the last dialog picked, after `PickedEmpty`. No
/// path in the request on purpose: the browser must never name a root,
/// so this can only confirm what the human already chose in the native
/// dialog — the daemon holds that choice in `studio_pending_pick`.
async fn confirm_pick(state: &AppState) -> StudioResponse {
    if state
        .studio_run_active
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return err(
            StudioErrorCode::Busy,
            "a run is in progress; stop it before switching projects",
        );
    }
    // take(): a confirmation is single-use, whether it succeeds or the
    // grant below refuses — replaying it must not re-grant.
    let Some(canon) = state.studio_pending_pick.lock().await.take() else {
        return err(
            StudioErrorCode::Invalid,
            "no folder pick is awaiting confirmation",
        );
    };
    grant_and_open(state, canon).await
}

/// The human declined the `PickedEmpty` warning: drop the parked path.
/// A no-op when nothing is parked — declining twice, or after a newer
/// pick already replaced the offer, must not error.
async fn discard_pick(state: &AppState) -> StudioResponse {
    *state.studio_pending_pick.lock().await = None;
    StudioResponse::PickDiscarded {}
}

/// The shared tail of both grant paths: insert the canonical root at
/// the head of the granted set, persist it to the recents file, and
/// answer `Opened`.
async fn grant_and_open(state: &AppState, canon: PathBuf) -> StudioResponse {
    let mut guard = state.studio.lock().await;
    let Some(config) = guard.as_mut() else {
        return err(StudioErrorCode::Forbidden, "studio surface is not enabled");
    };
    let root = config.grant(canon);
    crate::commands::studio_recents::record_in_or_warn(&config.recents_file, &root.path);
    StudioResponse::Opened {
        project: to_wire(&root),
    }
}

/// Absolute path of the procedure the session is working on: the
/// selected one, else the root's own. Confined like every other path —
/// the selection came from discovery under this root, and the canonical
/// check refuses a symlink that leaves it.
async fn active_procedure_yaml(
    config: &StudioConfig,
    root: &Path,
) -> Result<PathBuf, StudioResponse> {
    let candidate = match config.active_procedure() {
        Some(rel) => root.join(rel),
        None => crate::commands::run::engine::find_procedure_yaml(root).ok_or_else(|| {
            err(
                StudioErrorCode::NotFound,
                "no procedure found in the active project",
            )
        })?,
    };
    let canon = tokio::fs::canonicalize(&candidate).await.map_err(|_| {
        err(
            StudioErrorCode::NotFound,
            "no procedure found in the active project",
        )
    })?;
    check_canonical_policy(root, &canon, true)?;
    Ok(canon)
}

/// A plug's `config:` mapping as ordered display entries.
///
/// `kind` is what the Builder edits on: only a scalar can be
/// rewritten in place by the line editor, so a list or mapping is
/// reported as `complex` with a summary instead of its contents —
/// the row shows it and links to Code mode rather than offering an
/// inline field that cannot commit.
fn map_plug_config(
    config: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Vec<StudioSequencePlugConfigEntry> {
    let Some(config) = config else {
        return Vec::new();
    };
    config
        .iter()
        .map(|(key, value)| {
            let (kind, rendered) = match value {
                // Unquoted: the field edits the string itself, and
                // the write path re-quotes it.
                serde_json::Value::String(s) => ("string", s.clone()),
                serde_json::Value::Number(n) => ("number", n.to_string()),
                serde_json::Value::Bool(b) => ("bool", b.to_string()),
                serde_json::Value::Null => ("null", "null".to_string()),
                serde_json::Value::Array(items) => (
                    "complex",
                    format!(
                        "[{} item{}]",
                        items.len(),
                        if items.len() == 1 { "" } else { "s" }
                    ),
                ),
                serde_json::Value::Object(map) => (
                    "complex",
                    format!(
                        "{{{} key{}}}",
                        map.len(),
                        if map.len() == 1 { "" } else { "s" }
                    ),
                ),
            };
            StudioSequencePlugConfigEntry {
                key: key.clone(),
                value: rendered,
                kind: kind.to_string(),
            }
        })
        .collect()
}

async fn get_sequence(config: &StudioConfig, root: &Path) -> StudioResponse {
    let yaml_path = match active_procedure_yaml(config, root).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    // Both the parsed definition and the text it came from: the
    // projection needs the text to tell a stated `scope:` from an
    // inherited one, which the parse has already collapsed. One read
    // feeds both — parsing the text we already hold, never the path —
    // so the two cannot disagree: a second read could land after a
    // write and report a scope the returned definition does not have.
    let loaded = tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&yaml_path)
            .map_err(|e| format!("Failed to read {}: {}", yaml_path.display(), e))?;
        execution_engine::procedure::loader::load_procedure_definition_from_str(&content)
            .map(|def| (def, content))
    })
    .await;
    let (def, yaml_text) = match loaded {
        Ok(Ok(pair)) => pair,
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

    /// The canonical YAML spelling of a scope. Deliberately not
    /// `serde_json::to_string`: the wire contract is the bare word the
    /// file contains, and the legacy aliases (`each`/`all`/`run`) are
    /// parse-only — everything emits the new spellings.
    fn scope_str(scope: execution_engine::procedure::schema::Scope) -> &'static str {
        use execution_engine::procedure::schema::Scope;
        match scope {
            Scope::Slot => "slot",
            Scope::Execution => "execution",
            Scope::Station => "station",
        }
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

    let explicit_scopes =
        execution_engine::procedure::loader::plugs_with_explicit_scope(&yaml_text);

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
                    // Presence from the file, spelling from the parse.
                    scope: explicit_scopes
                        .contains(pl.name.as_str())
                        .then(|| scope_str(pl.scope).to_string()),
                    // Empty and absent are the same thing to the
                    // schema (`description` defaults to ""), so the
                    // projection collapses them rather than sending an
                    // empty string the page would have to special-case.
                    description: Some(pl.description.clone()).filter(|d| !d.is_empty()),
                    config: map_plug_config(pl.config.as_ref()),
                })
                .collect(),
            setup: def.setup.iter().map(map_phase).collect(),
            main: def.main.iter().map(map_phase).collect(),
            teardown: def.teardown.iter().map(map_phase).collect(),
        },
    }
}

/// The definition's own `name:`, read cheaply. Deliberately NOT the
/// engine loader: this runs for every discovered procedure on every
/// `project_info`, and a display label must not depend on the whole
/// definition (phases, imports, validators) being valid — a procedure
/// mid-edit still needs to be listed and selectable so it can be fixed.
async fn procedure_display_name(yaml_path: &Path) -> Option<String> {
    let text = tokio::fs::read_to_string(yaml_path).await.ok()?;
    execution_engine::procedure::loader::procedure_name_from_str(&text)
}

/// Every procedure under `root`, root-first then by path.
///
/// A directory holding `procedure.yaml`/`.yml` IS a procedure — the
/// same canonical-name rule the CLI and the git integration use. The
/// walk deliberately does NOT anchor on `pyproject.toml` the way the
/// git repo audit does: procedure subdirectories in a real monorepo
/// share the root's pyproject and have none of their own, so anchoring
/// there would miss exactly the layout this exists to support.
///
/// Bounded on both depth and count, and `SKIP_DIRS`/dotfiles are
/// pruned, so a venv or `.git` is never descended into.
async fn discover_procedures(root: &Path) -> Vec<station_protocol::StudioProcedure> {
    // Breadth-first so the shallowest procedures are found first and
    // the cap, if ever reached, keeps the ones nearest the root.
    let mut found: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> =
        std::collections::VecDeque::from([(PathBuf::new(), 0)]);

    while let Some((rel_dir, depth)) = queue.pop_front() {
        if found.len() >= MAX_PROCEDURES {
            break;
        }
        let abs_dir = root.join(&rel_dir);
        for name in ["procedure.yaml", "procedure.yml"] {
            if tokio::fs::try_exists(abs_dir.join(name))
                .await
                .unwrap_or(false)
            {
                found.push((rel_dir.clone(), rel_dir.join(name)));
                // One procedure per directory: `.yaml` wins over
                // `.yml`, matching `find_procedure_yaml`'s order.
                break;
            }
        }
        if depth >= MAX_PROCEDURE_DEPTH {
            continue;
        }
        let Ok(mut read_dir) = tokio::fs::read_dir(&abs_dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let entry_name = entry.file_name().to_string_lossy().into_owned();
            if is_skipped_name(&entry_name) {
                continue;
            }
            // `file_type` reports the dirent's own type, so a symlink
            // reads as a symlink and is skipped here — discovery never
            // leaves the root through one.
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                queue.push_back((rel_dir.join(entry_name), depth + 1));
            }
        }
    }

    let mut procedures = Vec::with_capacity(found.len());
    for (rel_dir, rel_yaml) in found {
        let name = match procedure_display_name(&root.join(&rel_yaml)).await {
            Some(name) => name,
            // No readable `name:`: the holding directory is what the
            // author chose, and for the root it is the project itself.
            None => rel_dir
                .file_name()
                .or_else(|| root.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("procedure")
                .to_string(),
        };
        procedures.push(station_protocol::StudioProcedure {
            path: rel_yaml.to_string_lossy().replace('\\', "/"),
            dir: rel_dir.to_string_lossy().replace('\\', "/"),
            name,
        });
    }
    // Root procedure first, then by path. The head is what a session
    // opens on, and the root's own procedure is what `tofupilot studio
    // <dir>` used to serve — a subdirectory winning that spot on
    // alphabetical luck would change the default under existing users.
    procedures.sort_by(|a, b| (!a.dir.is_empty(), &a.path).cmp(&(!b.dir.is_empty(), &b.path)));
    procedures
}

async fn project_info(state: &AppState, root: &Path) -> StudioResponse {
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();
    let procedures = discover_procedures(root).await;

    // Re-anchor the session on what discovery actually found: a
    // selection whose file disappeared falls back to the first
    // procedure, and a project with exactly one needs no click to be
    // workable. Written back so `get_sequence`, `validate` and the run
    // dispatcher all resolve the same procedure this reply names.
    let mut guard = state.studio.lock().await;
    let procedure_path = if let Some(config) = guard.as_mut() {
        let still_there = config
            .active_procedure()
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .filter(|active| procedures.iter().any(|p| &p.path == active));
        match still_there {
            Some(active) => Some(active),
            None => {
                config.forget_active_procedure();
                match procedures.first() {
                    Some(first) => {
                        config.set_active_procedure(PathBuf::from(&first.path));
                        Some(first.path.clone())
                    }
                    None => None,
                }
            }
        }
    } else {
        procedures.first().map(|p| p.path.clone())
    };
    drop(guard);

    StudioResponse::ProjectInfo {
        root: root.to_string_lossy().into_owned(),
        name,
        procedure_path,
        procedures: Some(procedures),
    }
}

/// Select among the discovered procedures. Selection only, mirroring
/// `open_project`: an unlisted path is refused whether or not it exists,
/// so the reply cannot be used to probe the disk.
async fn open_procedure(state: &AppState, root: &Path, path: &str) -> StudioResponse {
    // A run pinned the procedure it started against — switching under
    // it would leave the engine and the UI on two different procedures.
    if state
        .studio_run_active
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return err(
            StudioErrorCode::Busy,
            "a run is in progress; stop it before switching procedure",
        );
    }

    let procedures = discover_procedures(root).await;
    let Some(procedure) = procedures.into_iter().find(|p| p.path == path) else {
        return err(
            StudioErrorCode::Forbidden,
            "not a procedure of this project",
        );
    };

    let mut guard = state.studio.lock().await;
    let Some(config) = guard.as_mut() else {
        return err(StudioErrorCode::Forbidden, "studio surface is not enabled");
    };
    config.set_active_procedure(PathBuf::from(&procedure.path));
    StudioResponse::ProcedureOpened { procedure }
}

/// Move `target` to the OS trash, blocking.
///
/// On macOS the crate's DEFAULT is `DeleteMethod::Finder`, which drives
/// osascript to ask the Finder application to do it. That is wrong for a
/// daemon three ways: it needs Automation permission (the "wants to
/// control Finder" prompt), it plays the trash sound, and with no
/// interactive session to answer the prompt it simply hangs — a delete
/// test sat for over 60 seconds before failing, which is how this was
/// found. `NsFileManager` calls `trashItemAtURL` directly: no
/// AppleScript, no permission, no prompt.
///
/// Documented cost of that choice: on some systems the Finder's "Put
/// Back" entry does not appear for items trashed this way (a macOS bug
/// the crate links). The file IS in the Trash and can be dragged out,
/// which is the recoverability this op promises.
fn trash_it(target: &Path) -> Result<(), trash::Error> {
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut ctx = trash::TrashContext::default();
        ctx.set_delete_method(DeleteMethod::NsFileManager);
        ctx.delete(target)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Linux follows the XDG trash spec and Windows the shell API;
        // neither needs a method choice.
        trash::delete(target)
    }
}

/// Move a file or directory to the OS trash.
///
/// Confinement is the whole risk, so it reuses the same spine as every
/// other write: `clamp_rel` for the shape of the path, `resolve_existing`
/// to canonicalize (symlink-escape safe), `check_canonical_policy` on the
/// resolved target. `require_text` is false — directories have no
/// extension, and refusing a `.csv` fixture the user wants gone would be
/// a rule protecting nothing: reading it is what the allow-list guards.
async fn delete_entry(root: &Path, path: &str) -> StudioResponse {
    let rel = match clamp_rel(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // Deleting the project root itself is not a file operation: it would
    // take the session's own confinement anchor with it.
    if rel.as_os_str().is_empty() {
        return err(StudioErrorCode::Forbidden, "cannot delete the project root");
    }
    if let Err(e) = refuse_final_symlink(root, &rel).await {
        return e;
    }
    let canon = match resolve_existing(root, &rel).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = check_canonical_policy(root, &canon, false) {
        return e;
    }
    // NOT redundant with the check above: a symlink inside the project
    // pointing at the project is made of Normal components, passes
    // `clamp_rel`, and canonicalizes to the root itself. Covered by
    // `delete_refuses_a_symlink_that_resolves_back_to_the_root`.
    if canon == root {
        return err(StudioErrorCode::Forbidden, "cannot delete the project root");
    }

    // Blocking: the trash backends are synchronous (Foundation on macOS,
    // the XDG spec on Linux, the shell API on Windows), and a directory
    // can take real time.
    let target = canon.clone();
    match tokio::task::spawn_blocking(move || trash_it(&target)).await {
        Ok(Ok(())) => StudioResponse::Deleted {
            path: rel.to_string_lossy().replace('\\', "/"),
        },
        Ok(Err(e)) => err(
            StudioErrorCode::Internal,
            format!("could not move it to the trash: {e}"),
        ),
        Err(join_err) => err(
            StudioErrorCode::Internal,
            format!("trash task failed: {join_err}"),
        ),
    }
}

/// Recursive copy, blocking, used by `copy_entry`.
///
/// Iterative with an explicit stack rather than recursion: a deep tree
/// would otherwise be bounded by the thread's stack, and a copy is
/// exactly where someone points at a directory they have never counted.
///
/// Skips symlinks and excluded directories. Symlinks because following
/// one would copy data from outside the project into it; excluded dirs
/// because they are invisible in the tree, and duplicating a procedure
/// folder must not duplicate its virtualenv.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to copy a symlink",
        ));
    }
    if meta.is_file() {
        // create_new, not a bare copy: the clobber guard ran before
        // this blocking task was scheduled, so an existing `to` here
        // means a concurrent request won the name in between —
        // `fs::copy` would silently truncate the winner's file.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)?;
        std::fs::copy(from, to)?;
        return Ok(());
    }

    let mut pending = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src_dir, dst_dir)) = pending.pop() {
        std::fs::create_dir(&dst_dir)?;
        for entry in std::fs::read_dir(&src_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_str().map(is_skipped_name).unwrap_or(true) {
                continue;
            }
            let kind = entry.file_type()?;
            // A symlink reports as a symlink here (the dirent's own
            // type), so this both skips it and never follows it.
            if kind.is_dir() {
                pending.push((entry.path(), dst_dir.join(&name)));
            } else if kind.is_file() {
                std::fs::copy(entry.path(), dst_dir.join(&name))?;
            }
        }
    }
    Ok(())
}

/// Copy an entry. Same two-ended confinement as `move_entry`, and the
/// same refusal to clobber — the only difference is that the source
/// survives.
async fn copy_entry(root: &Path, from: &str, to: &str) -> StudioResponse {
    // `same_file_ok: false` — see resolve_two_ended: a copy onto its
    // own case-variant would truncate the source before reading it.
    let TwoEnded {
        from_rel,
        to_rel,
        canon_from,
        canon_to,
    } = match resolve_two_ended(root, from, to, "copy", false).await {
        Ok(ends) => ends,
        Err(e) => return e,
    };

    // Same editable-set rule as move_entry, for the same invariant: a
    // copy landing outside the editable extensions is a row the tree
    // lists and read_file refuses. Scoped to editable SOURCES — a .png
    // duplicates freely.
    match tokio::fs::metadata(&canon_from).await {
        Ok(m)
            if m.is_file() && has_text_extension(&canon_from) && !has_text_extension(&canon_to) =>
        {
            return err(
                StudioErrorCode::Invalid,
                "that extension is not editable in Studio",
            );
        }
        _ => {}
    }

    let (src, dst) = (canon_from.clone(), canon_to.clone());
    match tokio::task::spawn_blocking(move || copy_tree(&src, &dst)).await {
        Ok(Ok(())) => StudioResponse::Copied {
            from: from_rel.to_string_lossy().replace('\\', "/"),
            to: to_rel.to_string_lossy().replace('\\', "/"),
        },
        Ok(Err(e)) => {
            // A partial copy is worse than none: the tree would show a
            // half-populated folder that looks complete. EXCEPT when
            // the failure is AlreadyExists on the destination itself —
            // that means a concurrent request claimed the name after
            // our clobber guard ran and we wrote nothing, so removing
            // `canon_to` would hard-delete the winner's tree. (Nested
            // paths can't collide: they live inside the directory this
            // copy just created.)
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                let _ = tokio::fs::remove_dir_all(&canon_to).await;
                let _ = tokio::fs::remove_file(&canon_to).await;
            }
            err(StudioErrorCode::Internal, format!("cannot copy it: {e}"))
        }
        Err(join_err) => err(
            StudioErrorCode::Internal,
            format!("copy task failed: {join_err}"),
        ),
    }
}

/// Both canonical ends of a path-to-path op, prologue done.
struct TwoEnded {
    from_rel: PathBuf,
    to_rel: PathBuf,
    canon_from: PathBuf,
    canon_to: PathBuf,
}

/// True when the two paths are the same on-disk file. The case that
/// needs it: a case-only rename (`Main.py` -> `main.py`) on the
/// case-insensitive filesystems macOS and Windows default to, where
/// the destination "exists" because it IS the source.
#[cfg(unix)]
fn same_underlying_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(not(unix))]
fn same_underlying_file(_a: &std::fs::Metadata, _b: &std::fs::Metadata) -> bool {
    // Windows has no stable inode API on std; a case-only rename there
    // still reports Conflict. Wrong, but safely wrong — revisit when
    // `MetadataExt::file_index` stabilizes.
    false
}

/// The shared prologue of every path-to-path op (`move_entry`,
/// `copy_entry`): clamp both ends, refuse a symlink as the source's
/// final component, resolve and confine both ends, refuse a clobber
/// and a folder landing inside itself.
///
/// BOTH ends need confinement, and for different reasons — the source
/// must be a real entry inside the root (`resolve_existing`), while the
/// destination does not exist yet, so it is its PARENT that has to
/// resolve inside the root (`resolve_for_write`). An existing component
/// on either side can be a symlink out of the project.
///
/// Extracted because the two hand-written copies had already drifted
/// once: `copy_entry` shipped without the symlink refusal `move_entry`
/// carried. `verb` only flavours the error strings.
///
/// `same_file_ok`: move sets it — a destination that IS the source
/// (case-only rename on a case-insensitive filesystem) is not a
/// clobber, and `fs::rename` performs the case change correctly. Copy
/// must NOT set it: copying a file onto its own case-variant would
/// truncate the source before reading it.
async fn resolve_two_ended(
    root: &Path,
    from: &str,
    to: &str,
    verb: &str,
    same_file_ok: bool,
) -> Result<TwoEnded, StudioResponse> {
    let (from_rel, to_rel) = match (clamp_rel(from), clamp_rel(to)) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(e), _) | (_, Err(e)) => return Err(e),
    };
    if from_rel == to_rel {
        return Err(err(
            StudioErrorCode::Invalid,
            "source and destination are the same",
        ));
    }

    refuse_final_symlink(root, &from_rel).await?;
    let canon_from = resolve_existing(root, &from_rel).await?;
    check_canonical_policy(root, &canon_from, false)?;
    if canon_from == root {
        return Err(err(
            StudioErrorCode::Forbidden,
            format!("cannot {verb} the project root"),
        ));
    }

    let canon_to = resolve_for_write(root, &to_rel).await?;
    check_canonical_policy(root, &canon_to, false)?;
    // Never clobber: overwriting someone's file is not a rename, and
    // `fs::rename` would do it silently.
    if let Ok(to_meta) = tokio::fs::symlink_metadata(&canon_to).await {
        let case_rename = same_file_ok
            && matches!(
                tokio::fs::symlink_metadata(&canon_from).await,
                Ok(from_meta) if same_underlying_file(&from_meta, &to_meta)
            )
            && canon_to.parent() == canon_from.parent();
        if !case_rename {
            return Err(err(
                StudioErrorCode::Conflict,
                "something with that name already exists",
            ));
        }
        // Case-only rename on a case-insensitive filesystem. Both
        // canonical paths came back with the ON-DISK case (measured:
        // realpath("main.py") -> .../Main.py), so renaming to
        // `canon_to` would be a no-op REPORTED as success — the UI
        // would re-key its tab while the disk kept the old case. Aim
        // at the canonical parent + the REQUESTED final component
        // instead, and skip the containment guard below: with both
        // canonical paths identical it fires falsely, and the parent's
        // confinement is already proven.
        let requested = to_rel
            .file_name()
            .expect("clamp_rel refuses an empty final component");
        let target = canon_from
            .parent()
            .expect("the root guard above rules out a parentless source")
            .join(requested);
        return Ok(TwoEnded {
            from_rel,
            to_rel,
            canon_from,
            canon_to: target,
        });
    }
    // A directory landing inside itself would detach (move) or recurse
    // over (copy) the subtree; the OS refuses it too, but with an
    // errno the UI cannot explain.
    if canon_to.starts_with(&canon_from) {
        return Err(err(
            StudioErrorCode::Invalid,
            format!("cannot {verb} a folder inside itself"),
        ));
    }

    Ok(TwoEnded {
        from_rel,
        to_rel,
        canon_from,
        canon_to,
    })
}

/// Rename or move: one path-to-path move. Confinement lives in
/// `resolve_two_ended`.
async fn move_entry(root: &Path, from: &str, to: &str) -> StudioResponse {
    let TwoEnded {
        from_rel,
        to_rel,
        canon_from,
        canon_to,
    } = match resolve_two_ended(root, from, to, "move", true).await {
        Ok(ends) => ends,
        Err(e) => return e,
    };

    // A rename must not take an EDITABLE file out of Studio's editable
    // set: the tree would still list the row while read_file refuses
    // it — Studio producing an entry its own editor cannot open.
    // Scoped to sources that ARE editable text: resources (.png, .bin —
    // the set write_resource itself creates), extensionless files
    // (LICENSE, Makefile) and directories carry no such invariant, and
    // refusing them made every image and firmware file unrenamable.
    match tokio::fs::metadata(&canon_from).await {
        Ok(m)
            if m.is_file() && has_text_extension(&canon_from) && !has_text_extension(&canon_to) =>
        {
            return err(
                StudioErrorCode::Invalid,
                "that extension is not editable in Studio",
            );
        }
        _ => {}
    }

    match tokio::fs::rename(&canon_from, &canon_to).await {
        Ok(()) => StudioResponse::Moved {
            from: from_rel.to_string_lossy().replace('\\', "/"),
            to: to_rel.to_string_lossy().replace('\\', "/"),
        },
        Err(e) => err(StudioErrorCode::Internal, format!("cannot move it: {e}")),
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
    // target new subtrees (plugs/multimeter.py). Confined descent, not
    // `create_dir_all`: clamp_rel proves the NAME is clean, but an
    // existing intermediate can be a symlink out of the root, and
    // create_dir_all would plant the chain on its target before
    // resolve_for_write below got a chance to refuse the write.
    if let Some(parent_rel) = rel.parent() {
        if !parent_rel.as_os_str().is_empty() {
            if let Err(e) = mkdir_confined(root, parent_rel, false).await {
                return e;
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

/// Create `rel` and every intermediate under `root`, one component at
/// a time instead of `create_dir_all`. Confinement can only be checked
/// on a path that exists, so each level is created and then
/// re-canonicalized: an intermediate that already exists may be a
/// symlink out of the root (or into an excluded dir), and
/// `create_dir_all` would follow it and plant the new directory there
/// before anything got a chance to object. Shared by `create_dir` and
/// both write paths — the write paths previously used `create_dir_all`
/// and failed exactly that invariant.
///
/// `expect_fresh_leaf`: `create_dir` sets it — an already-existing
/// final component is a Conflict there, while the write paths only
/// need the chain to exist.
async fn mkdir_confined(
    root: &Path,
    rel: &Path,
    expect_fresh_leaf: bool,
) -> Result<(), StudioResponse> {
    let mut current = root.to_path_buf();
    let last = rel.components().count();
    for (index, comp) in rel.components().enumerate() {
        current.push(comp);
        let fresh = match tokio::fs::create_dir(&current).await {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => {
                return Err(err(
                    StudioErrorCode::Internal,
                    format!("cannot create directory: {e}"),
                ))
            }
        };
        if !fresh {
            // An existing FILE on the path is not something to build
            // through, and an existing final component means the
            // caller's folder is not the one we would be reporting.
            match tokio::fs::metadata(&current).await {
                Ok(m) if !m.is_dir() => {
                    return Err(err(
                        StudioErrorCode::Conflict,
                        "a file with that name exists",
                    ))
                }
                Ok(_) if expect_fresh_leaf && index + 1 == last => {
                    return Err(err(StudioErrorCode::Conflict, "directory already exists"))
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(err(
                        StudioErrorCode::Internal,
                        format!("cannot inspect directory: {e}"),
                    ))
                }
            }
        }
        let Ok(canon) = tokio::fs::canonicalize(&current).await else {
            return Err(err(
                StudioErrorCode::Internal,
                "created directory vanished before it could be checked",
            ));
        };
        if let Err(e) = check_canonical_policy(root, &canon, false) {
            // Only unwind what this request made: an escaping symlink
            // that was already there is not ours to remove.
            if fresh {
                let _ = tokio::fs::remove_dir(&current).await;
            }
            return Err(e);
        }
        current = canon;
    }
    Ok(())
}

async fn create_dir(root: &Path, path: &str) -> StudioResponse {
    let rel = match clamp_rel(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = mkdir_confined(root, &rel, true).await {
        return e;
    }
    let rel_display = rel.to_string_lossy().replace('\\', "/");
    crate::log::info(&format!("studio: created directory {rel_display}"));
    StudioResponse::DirCreated { path: rel_display }
}

async fn validate(config: &StudioConfig, root: &Path, path: Option<&str>) -> StudioResponse {
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
        // Implicit: the procedure the session is working on. Same
        // canonical policy as the explicit-path arm — a procedure.yaml
        // that is a symlink out of the root (or into an excluded dir)
        // must not be loadable implicitly when it would be refused
        // when addressed by name (`active_procedure_yaml` checks it).
        None => match active_procedure_yaml(config, root).await {
            Ok(p) => p,
            Err(e) => return e,
        },
    };
    // Root-relative path of the validated file, so the web UI can open
    // it from a diagnostic. Every diagnostic this op produces is about
    // the YAML itself (parse/validation errors, and dangling `python:`
    // refs are entries OF the yaml) — the referenced .py file does not
    // exist, so the yaml line is the only place a fix can go.
    let diag_path = yaml_path
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"));
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
                path: diag_path.clone(),
            })
            .collect(),
        Ok(Err(message)) => vec![StudioDiagnostic {
            severity: StudioDiagnosticSeverity::Error,
            message,
            path: diag_path,
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
            // The caller named the file it asked about; echo it so the
            // UI can jump there, same as `validate`.
            path: Some(rel.to_string_lossy().replace('\\', "/")),
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
    // does not exist before the first upload. Same confined descent —
    // see write_file for why create_dir_all is not safe here.
    if let Some(parent_rel) = rel.parent() {
        if !parent_rel.as_os_str().is_empty() {
            if let Err(e) = mkdir_confined(root, parent_rel, false).await {
                return e;
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
    fn plug_config_projection_kinds_and_order() {
        let config: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            r#"{
                "address": "192.168.1.100",
                "channel": 1,
                "gain": 1.5,
                "invert": true,
                "spare": null,
                "channels": [1, 2, 3],
                "limits": {"lo": 0, "hi": 5}
            }"#,
        )
        .unwrap();
        let out = map_plug_config(Some(&config));
        let seen: Vec<(&str, &str, &str)> = out
            .iter()
            .map(|e| (e.key.as_str(), e.kind.as_str(), e.value.as_str()))
            .collect();

        // A string arrives UNQUOTED: the field edits the string itself
        // and the write path re-quotes it.
        assert!(seen.contains(&("address", "string", "192.168.1.100")));
        assert!(seen.contains(&("channel", "number", "1")));
        assert!(seen.contains(&("gain", "number", "1.5")));
        assert!(seen.contains(&("invert", "bool", "true")));
        assert!(seen.contains(&("spare", "null", "null")));
        // A list or mapping is summarized, never inlined: the line editor
        // cannot rewrite it, so the row must not offer a field.
        assert!(seen.contains(&("channels", "complex", "[3 items]")));
        assert!(seen.contains(&("limits", "complex", "{2 keys}")));
        assert_eq!(out.len(), 7);
    }

    #[test]
    fn plug_config_singular_summaries_and_absent() {
        let one: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"a": [7], "b": {"k": 1}}"#).unwrap();
        let out = map_plug_config(Some(&one));
        assert_eq!(out.iter().find(|e| e.key == "a").unwrap().value, "[1 item]");
        assert_eq!(out.iter().find(|e| e.key == "b").unwrap().value, "{1 key}");

        // No `config:` at all is an empty list, never a null the wire
        // would omit — the page maps over it unconditionally.
        assert!(map_plug_config(None).is_empty());
    }

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

        // Upstream wrote this test against the single-root signature;
        // on this branch validate resolves the implicit procedure
        // through the granted-roots config.
        let config = config_with(&[root.to_str().unwrap()]);
        let res = validate(&config, &root, None).await;
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
        let res = validate(&config, &root, None).await;
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

    /// Build a config without touching the machine's recents file —
    /// `StudioConfig::new` reads it, and these tests are about the
    /// selection rule, not about what this machine has opened before.
    fn config_with(paths: &[&str]) -> StudioConfig {
        StudioConfig {
            granted: paths
                .iter()
                .map(|p| GrantedRoot::new(PathBuf::from(p)))
                .collect(),
            recents_file: PathBuf::from("/dev/null/unused-by-these-tests"),
            active_procedure: None,
        }
    }

    #[test]
    fn active_is_the_head_of_the_granted_set() {
        let config = config_with(&["/projects/alpha", "/projects/beta"]);
        assert_eq!(config.active(), Path::new("/projects/alpha"));
    }

    #[test]
    fn activating_a_granted_root_promotes_it() {
        let mut config = config_with(&["/projects/alpha", "/projects/beta"]);
        let opened = config.activate(Path::new("/projects/beta")).unwrap();

        assert_eq!(opened.name, "beta");
        assert_eq!(config.active(), Path::new("/projects/beta"));
        // Promotion reorders, it never drops the one we came from.
        let paths: Vec<&Path> = config.granted().iter().map(|g| g.path.as_path()).collect();
        assert_eq!(
            paths,
            vec![Path::new("/projects/beta"), Path::new("/projects/alpha")]
        );
    }

    /// The security-relevant case: a path the browser invents must not
    /// become active, whether or not it exists on disk. This test is
    /// the guard on `open_project`'s "selects, never grants" contract.
    #[test]
    fn activating_an_ungranted_root_is_refused_and_changes_nothing() {
        let mut config = config_with(&["/projects/alpha"]);

        assert!(config.activate(Path::new("/etc")).is_none());
        assert!(config
            .activate(Path::new("/projects/alpha/../beta"))
            .is_none());
        assert_eq!(config.active(), Path::new("/projects/alpha"));
        assert_eq!(config.granted().len(), 1);
    }

    #[test]
    fn activating_the_active_root_is_a_no_op() {
        let mut config = config_with(&["/projects/alpha", "/projects/beta"]);
        assert!(config.activate(Path::new("/projects/alpha")).is_some());
        assert_eq!(config.active(), Path::new("/projects/alpha"));
        assert_eq!(config.granted().len(), 2, "no duplicate on re-activation");
    }

    /// The monorepo shape this exists for: one procedure per named
    /// subdirectory, sharing the root's pyproject and having none of
    /// their own. Anchoring discovery on `pyproject.toml` (what the git
    /// repo audit does) would find only the root — this asserts the
    /// canonical-YAML rule instead.
    #[tokio::test]
    async fn discovery_finds_procedures_in_named_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("pyproject.toml"), "[project]\n").unwrap();
        std::fs::write(root.join("procedure.yaml"), "name: Root suite\n").unwrap();
        for sub in ["plug-rpc-60s-timeout", "ui-showcase"] {
            std::fs::create_dir(root.join(sub)).unwrap();
        }
        // A `name:` reads as the label; without one the folder does.
        std::fs::write(
            root.join("plug-rpc-60s-timeout/procedure.yaml"),
            "name: Plug RPC timeout\nversion: 1.0.0\n",
        )
        .unwrap();
        std::fs::write(root.join("ui-showcase/procedure.yaml"), "version: 1.0.0\n").unwrap();
        // Neither a procedure nor a place to descend into.
        std::fs::create_dir(root.join("phases")).unwrap();
        std::fs::write(root.join("phases/main.py"), "def main():\n    pass\n").unwrap();

        let found = discover_procedures(root).await;
        let paths: Vec<&str> = found.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                // Root first: it is the head, and the head is what a
                // session opens on. A subdirectory must not take that
                // spot on alphabetical luck.
                "procedure.yaml",
                "plug-rpc-60s-timeout/procedure.yaml",
                "ui-showcase/procedure.yaml",
            ],
        );
        let named: Vec<(&str, &str)> = found
            .iter()
            .map(|p| (p.dir.as_str(), p.name.as_str()))
            .collect();
        assert_eq!(
            named,
            vec![
                ("", "Root suite"),
                ("plug-rpc-60s-timeout", "Plug RPC timeout"),
                // No readable `name:` -> the folder the author named.
                ("ui-showcase", "ui-showcase"),
            ],
        );
    }

    /// Discovery runs on whatever folder a human picked in the dialog,
    /// so the excluded dirs are not cosmetic: a `.venv` full of vendored
    /// packages (or a `.git`) must never be descended into, and depth is
    /// bounded so a home directory cannot become a full-disk walk.
    #[tokio::test]
    async fn discovery_prunes_excluded_dirs_and_stops_at_the_depth_bound() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for excluded in [".venv", "node_modules", ".git"] {
            std::fs::create_dir(root.join(excluded)).unwrap();
            std::fs::write(root.join(excluded).join("procedure.yaml"), "name: Nope\n").unwrap();
        }
        // One level past the bound.
        let deep = root.join("a/b/c/d");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("procedure.yaml"), "name: Too deep\n").unwrap();
        // At the bound, so it must be found.
        std::fs::write(root.join("a/b/c/procedure.yaml"), "name: At the bound\n").unwrap();

        let found = discover_procedures(root).await;
        let paths: Vec<&str> = found.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, vec!["a/b/c/procedure.yaml"]);
    }

    /// A procedure whose YAML is mid-edit still has to be listed and
    /// selectable — that is how it gets fixed. The label falls back to
    /// the folder rather than the whole procedure dropping out.
    #[tokio::test]
    async fn a_procedure_with_unparseable_yaml_is_still_listed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("half-written")).unwrap();
        std::fs::write(
            root.join("half-written/procedure.yaml"),
            "name: [this is not: valid yaml\n",
        )
        .unwrap();

        let found = discover_procedures(root).await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "half-written");
        assert_eq!(found[0].dir, "half-written");
    }

    /// Switching project drops the procedure selection: the path is
    /// root-relative, so keeping it would point the session at another
    /// project's same-named file.
    #[test]
    fn switching_project_forgets_the_active_procedure() {
        let mut config = config_with(&["/projects/alpha", "/projects/beta"]);
        config.set_active_procedure(PathBuf::from("sub/procedure.yaml"));
        assert_eq!(
            config.active_procedure_dir(),
            Path::new("/projects/alpha/sub")
        );

        config.activate(Path::new("/projects/beta")).unwrap();
        assert_eq!(config.active_procedure(), None);
        // With no selection a run executes the root itself, which is
        // every single-procedure project.
        assert_eq!(config.active_procedure_dir(), Path::new("/projects/beta"));
    }

    /// Delete goes to the trash, so the assertion is that the entry left
    /// the PROJECT — not that the bytes are gone. Recovering it is the
    /// Finder's job, and that is the whole point of the choice.
    #[tokio::test]
    async fn delete_removes_files_and_whole_directories_from_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::fs::write(root.join("notes.md"), "bye\n").unwrap();
        std::fs::create_dir(root.join("phases")).unwrap();
        std::fs::write(root.join("phases/main.py"), "x = 1\n").unwrap();

        assert!(matches!(
            delete_entry(root, "notes.md").await,
            StudioResponse::Deleted { .. }
        ));
        assert!(!root.join("notes.md").exists());

        // A directory goes with its contents: that is what the gesture
        // means in every file manager.
        assert!(matches!(
            delete_entry(root, "phases").await,
            StudioResponse::Deleted { .. }
        ));
        assert!(!root.join("phases").exists());
    }

    /// The refusals that matter on a destructive op. The root case is
    /// the dangerous one: it would take the session's own confinement
    /// anchor with it.
    #[tokio::test]
    async fn delete_refuses_the_root_escapes_and_excluded_entries() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("precious.txt"), "keep\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".venv")).unwrap();
        std::fs::write(root.join(".venv/pyvenv.cfg"), "\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();

        for path in [
            ".",
            "",
            "..",
            "link/precious.txt",
            ".venv/pyvenv.cfg",
            "../",
        ] {
            let refused = delete_entry(root, path).await;
            assert!(
                matches!(refused, StudioResponse::Error { .. }),
                "deleting {path:?} was not refused: {refused:?}",
            );
        }
        // Nothing outside the project was touched through the symlink,
        // and the project itself is still there.
        assert!(outside.path().join("precious.txt").exists());
        assert!(root.exists());
        assert!(root.join(".venv/pyvenv.cfg").exists());
    }

    /// The root guard after canonicalization is not redundant with
    /// `clamp_rel`: a symlink INSIDE the project pointing at the project
    /// is made of Normal components and passes every earlier check, then
    /// resolves to the root itself. Without it, deleting that link would
    /// trash the whole project.
    #[tokio::test]
    async fn delete_refuses_a_symlink_that_resolves_back_to_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::fs::write(root.join("keep.py"), "x = 1\n").unwrap();
        std::os::unix::fs::symlink(root, root.join("self")).unwrap();

        let refused = delete_entry(root, "self").await;
        assert!(
            matches!(
                refused,
                StudioResponse::Error {
                    code: StudioErrorCode::Forbidden,
                    ..
                }
            ),
            "got {refused:?}"
        );
        assert!(root.join("keep.py").exists(), "the project must survive");
    }

    /// An in-project symlink is refused by delete and move alike: the
    /// canonical resolution would land the op on the pointee while the
    /// reply names the link (the `procedure.yaml -> procedures/v2.yaml`
    /// version-pinning pattern would lose the target).
    #[tokio::test]
    #[cfg(unix)]
    async fn delete_and_move_refuse_an_in_project_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("procedures")).unwrap();
        std::fs::write(root.join("procedures/v2.yaml"), "name: V2\n").unwrap();
        std::os::unix::fs::symlink(root.join("procedures/v2.yaml"), root.join("procedure.yaml"))
            .unwrap();

        let refused = delete_entry(root, "procedure.yaml").await;
        assert!(
            matches!(
                refused,
                StudioResponse::Error {
                    code: StudioErrorCode::Forbidden,
                    ..
                }
            ),
            "delete got {refused:?}"
        );
        assert!(
            root.join("procedures/v2.yaml").exists(),
            "the pointee must survive"
        );

        let refused = move_entry(root, "procedure.yaml", "renamed.yaml").await;
        assert!(
            matches!(
                refused,
                StudioResponse::Error {
                    code: StudioErrorCode::Forbidden,
                    ..
                }
            ),
            "move got {refused:?}"
        );
        assert!(
            root.join("procedures/v2.yaml").exists(),
            "the pointee must survive a refused move"
        );
        assert!(
            std::fs::symlink_metadata(root.join("procedure.yaml")).is_ok(),
            "the link itself must survive too"
        );
    }

    /// A case-only rename must reach the disk with the REQUESTED case:
    /// on a case-insensitive filesystem the destination "exists"
    /// because it IS the source, and both the clobber guard and the
    /// containment guard used to fire on it — then a naive fix would
    /// have renamed to the canonicalized (on-disk-case) path, a no-op
    /// reported as success. Probes the filesystem and stands down on a
    /// case-sensitive one, where the plain rename path covers it.
    #[tokio::test]
    async fn move_performs_a_case_only_rename() {
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::fs::write(root.join("Main.py"), "x = 1\n").unwrap();
        if !root.join("main.py").exists() {
            // Case-sensitive filesystem: "main.py" is simply a new
            // name and the regular rename path applies.
            return;
        }

        let moved = move_entry(root, "Main.py", "main.py").await;
        assert!(
            matches!(moved, StudioResponse::Moved { .. }),
            "got {moved:?}"
        );
        // The case change must be real on disk, not a reported no-op.
        let on_disk: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            on_disk.contains(&"main.py".to_string()),
            "disk still shows: {on_disk:?}"
        );
    }

    /// The extension rule is scoped to EDITABLE sources: a .py must not
    /// leave the editable set, while resources and extensionless files
    /// rename freely (the first version refused those too, making every
    /// image and firmware file unrenamable).
    #[tokio::test]
    async fn move_extension_rule_only_guards_editable_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::fs::write(root.join("main.py"), "x = 1\n").unwrap();
        std::fs::write(root.join("logo.png"), [137u8, 80, 78, 71]).unwrap();
        std::fs::write(root.join("LICENSE"), "MIT\n").unwrap();

        let refused = move_entry(root, "main.py", "main.py.bak").await;
        assert!(
            matches!(
                refused,
                StudioResponse::Error {
                    code: StudioErrorCode::Invalid,
                    ..
                }
            ),
            "got {refused:?}"
        );
        assert!(matches!(
            move_entry(root, "logo.png", "banner.png").await,
            StudioResponse::Moved { .. }
        ));
        assert!(matches!(
            move_entry(root, "LICENSE", "LICENSE.orig").await,
            StudioResponse::Moved { .. }
        ));
    }

    /// Rename and move are one op, and this is the shape both take.
    #[tokio::test]
    async fn move_renames_within_a_directory_and_across_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::fs::write(root.join("old.py"), "x = 1\n").unwrap();
        std::fs::create_dir(root.join("phases")).unwrap();

        // Rename: same parent, new name.
        assert!(matches!(
            move_entry(root, "old.py", "new.py").await,
            StudioResponse::Moved { .. }
        ));
        assert!(!root.join("old.py").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("new.py")).unwrap(),
            "x = 1\n"
        );

        // Move: new parent, and the content rides along.
        assert!(matches!(
            move_entry(root, "new.py", "phases/new.py").await,
            StudioResponse::Moved { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("phases/new.py")).unwrap(),
            "x = 1\n"
        );
    }

    /// A move must never destroy anything: `fs::rename` overwrites the
    /// destination silently, which would turn a mistyped rename into
    /// data loss. Verified by mutation — disabling the guard fails this.
    #[tokio::test]
    async fn move_refuses_to_clobber_an_existing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.py"), "keep me\n").unwrap();
        std::fs::write(root.join("b.py"), "do not lose me\n").unwrap();

        let refused = move_entry(root, "a.py", "b.py").await;
        assert!(
            matches!(
                refused,
                StudioResponse::Error {
                    code: StudioErrorCode::Conflict,
                    ..
                }
            ),
            "got {refused:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b.py")).unwrap(),
            "do not lose me\n",
            "the destination must be untouched",
        );
        assert!(root.join("a.py").exists(), "the source must be untouched");
    }

    /// Confinement on BOTH ends, which is what makes a move riskier than
    /// a write: a symlinked component can point out of the project on
    /// the source side or the destination side.
    #[tokio::test]
    async fn move_confines_both_ends_and_refuses_a_folder_into_itself() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("theirs.py"), "not ours\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::fs::write(root.join("ours.py"), "ours\n").unwrap();
        std::fs::create_dir(root.join("group")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();

        // Destination escapes: the file must not leave the project.
        let out = move_entry(root, "ours.py", "link/ours.py").await;
        assert!(matches!(out, StudioResponse::Error { .. }), "got {out:?}");
        assert!(root.join("ours.py").exists());
        assert!(!outside.path().join("ours.py").exists());

        // Source escapes: a file outside must not be pulled in.
        let inward = move_entry(root, "link/theirs.py", "theirs.py").await;
        assert!(
            matches!(inward, StudioResponse::Error { .. }),
            "got {inward:?}"
        );
        assert!(outside.path().join("theirs.py").exists());

        // A folder into its own subtree detaches it; refuse in our own
        // words rather than surfacing an errno.
        let cycle = move_entry(root, "group", "group/inner").await;
        assert!(
            matches!(
                cycle,
                StudioResponse::Error {
                    code: StudioErrorCode::Invalid,
                    ..
                }
            ),
            "got {cycle:?}"
        );
        assert!(root.join("group").is_dir());
    }

    /// A copy carries the tree but deliberately not everything on disk:
    /// symlinks and excluded dirs are skipped, because the tree does not
    /// show them and duplicating a procedure folder must not duplicate
    /// its virtualenv.
    #[tokio::test]
    async fn copy_duplicates_a_tree_skipping_symlinks_and_excluded_dirs() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("theirs.txt"), "not ours\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("proc/phases")).unwrap();
        std::fs::write(root.join("proc/procedure.yaml"), "name: P\n").unwrap();
        std::fs::write(root.join("proc/phases/main.py"), "x = 1\n").unwrap();
        // Both must stay out of the copy.
        std::fs::create_dir(root.join("proc/venv")).unwrap();
        std::fs::write(root.join("proc/venv/big"), "0".repeat(64)).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("theirs.txt"),
            root.join("proc/link.txt"),
        )
        .unwrap();

        let res = copy_entry(root, "proc", "proc-copy").await;
        assert!(matches!(res, StudioResponse::Copied { .. }), "got {res:?}");

        // The source is untouched — that is the whole difference from move.
        assert!(root.join("proc/procedure.yaml").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("proc-copy/phases/main.py")).unwrap(),
            "x = 1\n"
        );
        assert!(
            !root.join("proc-copy/venv").exists(),
            "venv must not follow"
        );
        assert!(
            !root.join("proc-copy/link.txt").exists(),
            "a symlink must not be followed into the copy",
        );
    }

    /// Same refusals as a move, plus the one specific to copying: a
    /// folder into its own subtree would recurse forever, since the
    /// destination keeps reappearing inside the walk.
    #[tokio::test]
    async fn copy_refuses_clobber_escapes_and_a_folder_into_itself() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = &dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.py"), "mine\n").unwrap();
        std::fs::write(root.join("b.py"), "do not lose me\n").unwrap();
        std::fs::create_dir(root.join("group")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();

        let clobber = copy_entry(root, "a.py", "b.py").await;
        assert!(
            matches!(
                clobber,
                StudioResponse::Error {
                    code: StudioErrorCode::Conflict,
                    ..
                }
            ),
            "got {clobber:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b.py")).unwrap(),
            "do not lose me\n"
        );

        // Destination outside the project.
        let out = copy_entry(root, "a.py", "link/a.py").await;
        assert!(matches!(out, StudioResponse::Error { .. }), "got {out:?}");
        assert!(!outside.path().join("a.py").exists());

        let cycle = copy_entry(root, "group", "group/inner").await;
        assert!(
            matches!(
                cycle,
                StudioResponse::Error {
                    code: StudioErrorCode::Invalid,
                    ..
                }
            ),
            "got {cycle:?}"
        );
    }
}
