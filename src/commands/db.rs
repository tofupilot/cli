//! Local persistent state in an embedded redb store at
//! `~/.tofupilot/state.redb`.
//!
//! Holds the whoami cache, update cache, pull-sync state, station config, and
//! the offline run queue. Access is guarded by an exclusive per-process lock
//! with a PID-liveness probe to clear stale locks.

use redb::{Database, DatabaseError, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::CliResult;

const PULL_SYNC: TableDefinition<&str, &[u8]> = TableDefinition::new("pull.sync");
const LOGIN_WHOAMI: TableDefinition<&str, &[u8]> = TableDefinition::new("login.whoami");
const UPDATE_CACHE: TableDefinition<&str, &[u8]> = TableDefinition::new("update.cache");
const UPDATE_PENDING: TableDefinition<&str, &[u8]> = TableDefinition::new("update.pending");
const RUN_QUEUE: TableDefinition<&str, &[u8]> = TableDefinition::new("run.queue");
const STATION_CONFIG: TableDefinition<&str, &[u8]> = TableDefinition::new("station.config");

static DB: std::sync::RwLock<Option<Arc<Database>>> = std::sync::RwLock::new(None);

/// User home directory. Centralized so the "No home directory" error
/// message stays uniform and a future change (e.g. respecting
/// `TOFUPILOT_HOME`) lands in one place.
pub fn home_dir() -> CliResult<std::path::PathBuf> {
    Ok(directories::BaseDirs::new()
        .ok_or("No home directory")?
        .home_dir()
        .to_path_buf())
}

pub fn tofupilot_dir() -> CliResult<std::path::PathBuf> {
    let dir = home_dir()?.join(".tofupilot");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Create .tofupilot dir: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    Ok(dir)
}

pub fn deployments_dir() -> CliResult<std::path::PathBuf> {
    Ok(tofupilot_dir()?.join("deployments"))
}

/// Filesystem path to the redb state file. Centralized so the
/// uninstaller and `open()` agree on what to remove / open.
pub fn state_path() -> CliResult<std::path::PathBuf> {
    Ok(tofupilot_dir()?.join("state.redb"))
}

/// Remove all local deployment directories and their DB state.
pub fn clear_deployments() -> CliResult<()> {
    let dir = deployments_dir()?;
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("Remove deployments: {e}"))?;
    }
    // Clear pull state and manifests from DB
    if let Ok(db) = open() {
        db.clear_all_pull_state()?;
    }
    Ok(())
}

/// Sidecar pidfile recording which CLI holds the redb lock — lets
/// `open()` distinguish a live conflict from a stale lock.
fn pid_path() -> CliResult<std::path::PathBuf> {
    Ok(tofupilot_dir()?.join("state.redb.pid"))
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // `kill(pid, 0)` is the standard pid-liveness probe: returns 0
    // if signal could be sent, ESRCH if the pid is gone.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    // OpenProcess(SYNCHRONIZE) + WaitForSingleObject(0) is O(1) and
    // ~µs cheap. The previous tasklist-grep approach spawned a
    // subprocess per probe, ~100-300ms each, and on a Tokio shutdown
    // path the spawned subprocess pinned `spawn_blocking` long enough
    // that operators perceived "Exit doesn't work" while runtime
    // drop awaited the call.
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return false;
        }
        let r = WaitForSingleObject(h, 0);
        CloseHandle(h);
        r == WAIT_TIMEOUT
    }
}

#[cfg(not(any(unix, windows)))]
fn pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
fn kill_pid(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) == 0 }
}

#[cfg(windows)]
fn kill_pid(pid: u32) -> bool {
    std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
fn kill_pid(_pid: u32) -> bool {
    false
}

/// Sanity-check that `pid` is a tofupilot binary before we SIGKILL it.
/// The pidfile under `~/.tofupilot/` can theoretically be tampered
/// with, and on long-running Linux hosts PID reuse means a stale
/// pidfile can name an unrelated process owned by the same user.
///
/// Uses `starts_with("tofupilot")` so future renames like
/// `tofupilot-station` / `tofupilot-agent` still pass. Linux's
/// `/proc/<pid>/comm` truncates at 15 chars (TASK_COMM_LEN), which
/// fits `tofupilot` plus a few-char suffix.
///
/// On any platform where probing the process name fails or is
/// unsupported, fall back to permissive (the original behavior) to
/// keep stale-lock recovery functional.
fn pid_is_tofupilot(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{pid}/comm");
        match std::fs::read_to_string(&path) {
            Ok(s) => s.trim().starts_with("tofupilot"),
            Err(_) => false,
        }
    }
    #[cfg(target_os = "macos")]
    {
        // `ps -p <pid> -o comm=` prints just the binary path. macOS
        // doesn't expose /proc; this is the standard probe.
        let out = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let name = String::from_utf8_lossy(&o.stdout);
                name.lines()
                    .next()
                    .map(|l| {
                        std::path::Path::new(l.trim())
                            .file_name()
                            .and_then(|s| s.to_str())
                            .map(|s| s.starts_with("tofupilot"))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            }
            _ => false,
        }
    }
    #[cfg(windows)]
    {
        // tasklist subprocess cost (~100-300ms) only paid on the
        // stale-lock recovery path, not on the hot pid_alive() probe.
        // CSV row format is `"image","pid",...` — match on the image
        // column's prefix rather than substring-scanning the whole
        // row (which would also fire on `tofupilot.exe` appearing as
        // some other column's value).
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout).to_ascii_lowercase();
                s.lines().any(|line| {
                    line.trim_start_matches('"')
                        .split('"')
                        .next()
                        .map(|name| name.starts_with("tofupilot"))
                        .unwrap_or(false)
                })
            }
            _ => false,
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        true
    }
}

/// Drop the cached singleton, releasing the redb lock + pidfile so a
/// child process (re-exec on Windows) can open it. Safe to call when no
/// DB was opened.
pub fn close() {
    if let Ok(mut guard) = DB.write() {
        *guard = None;
    }
    if let Ok(p) = pid_path() {
        let _ = std::fs::remove_file(p);
    }
}

pub fn open() -> CliResult<StateDb> {
    if let Some(db) = DB.read().ok().and_then(|g| g.clone()) {
        return Ok(StateDb { db });
    }
    let path = state_path()?;
    let pid_file = pid_path()?;

    let db = match Database::create(&path) {
        Ok(db) => db,
        Err(DatabaseError::DatabaseAlreadyOpen) => {
            // Reclaim the lock: kill the holder if it's still alive (own pid
            // skipped — singleton would have returned above), then drop the
            // pidfile and retry. Self-service: no prompt, no command for the
            // user to type. Common case is a previous process that died
            // without releasing redb's file lock or a stale child after
            // self-update reexec.
            let prev_pid = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|&p| p != std::process::id());
            if let Some(pid) = prev_pid {
                if pid_alive(pid) {
                    // Only SIGKILL when we've verified the pid still
                    // points to a tofupilot binary. On Linux, PID reuse
                    // by long-running services means a stale pidfile
                    // can name a totally unrelated process owned by the
                    // same user — killing it would be a bad day.
                    if pid_is_tofupilot(pid) {
                        let _ = kill_pid(pid);
                        // Give the OS a beat to release the file lock after
                        // process teardown — taskkill returns before handles
                        // are fully closed on Windows.
                        for _ in 0..20 {
                            if !pid_alive(pid) {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                    } else {
                        crate::log::warn(&format!(
                            "Stale pidfile names PID {pid} but it isn't a tofupilot process — skipping kill"
                        ));
                    }
                }
            }
            let _ = std::fs::remove_file(&pid_file);
            Database::create(&path)
                .map_err(|e| format!("Open database (after stale-lock cleanup): {e}"))?
        }
        Err(e) => return Err(format!("Open database: {e}").into()),
    };

    // Best-effort: a future CLI hitting a lock checks this to decide
    // whether to wait or reclaim. Write failure is non-fatal.
    let _ = std::fs::write(&pid_file, std::process::id().to_string());

    let arc = Arc::new(db);
    if let Ok(mut guard) = DB.write() {
        *guard = Some(arc.clone());
    }
    Ok(StateDb { db: arc })
}

#[derive(Clone)]
pub struct StateDb {
    db: Arc<Database>,
}

// ---------------------------------------------------------------------------
// Pull state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullState {
    pub sha: String,
    pub pulled_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub name: Option<String>,
    /// station_deployment row this bundle was installed from. Stamped on
    /// every successful pull so DeploymentRemoved events can carry the
    /// deployment_id when the procedure unlinks. Required since
    /// auto-deploy v2; legacy on-disk rows that predate the field fail
    /// deserialization and are skipped by `list_pull_state` — the next
    /// `tofupilot pull` rewrites them with a deployment_id.
    pub deployment_id: String,
}

// ---------------------------------------------------------------------------
// Whoami cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoamiCache {
    pub fetched_at: chrono::DateTime<chrono::Utc>,
    pub auth_type: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub station_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub station_id: Option<String>,
    pub organization_name: String,
    pub organization_slug: String,
}

// ---------------------------------------------------------------------------
// Update cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCache {
    pub checked_at: chrono::DateTime<chrono::Utc>,
    pub latest: String,
    pub min: Option<String>,
    // Version that previously failed to apply on this host and should
    // be skipped by background_check until the server advertises a
    // different `latest`. Prevents the every-tick retry loop when an
    // apply hits an unrecoverable local condition (e.g. current_exe
    // resolves to a missing path with no on-disk fallback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poisoned_version: Option<String>,
    // SHA-256 of the staged binary, computed at download time. Re-hashed
    // before apply so a torn-write or partially-corrupted staged file
    // can't be exec'd into a SIGSEGV/SIGBUS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_sha256: Option<String>,
    // Version of the currently staged binary (matches `latest` at
    // stage time). Authoritative — `latest` may move on later checks
    // before the staged file is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_version: Option<String>,
}

/// Record written immediately before a self-replace + reexec, read by the new
/// process on startup to publish a matching UpdateApplied / UpdateFailed event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingUpdate {
    pub from_version: String,
    pub to_version: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

// ---------------------------------------------------------------------------
// Generic get/set on a table
// ---------------------------------------------------------------------------

impl StateDb {
    fn get(&self, table: TableDefinition<&str, &[u8]>, key: &str) -> CliResult<Option<Vec<u8>>> {
        let txn = self.db.begin_read().map_err(|e| format!("Read txn: {e}"))?;
        let tbl = match txn.open_table(table) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(format!("Open table: {e}").into()),
        };
        match tbl.get(key) {
            Ok(Some(value)) => Ok(Some(value.value().to_vec())),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("Get: {e}").into()),
        }
    }

    fn set(&self, table: TableDefinition<&str, &[u8]>, key: &str, value: &[u8]) -> CliResult<()> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| format!("Write txn: {e}"))?;
        {
            let mut tbl = txn
                .open_table(table)
                .map_err(|e| format!("Open table: {e}"))?;
            tbl.insert(key, value).map_err(|e| format!("Insert: {e}"))?;
        }
        txn.commit().map_err(|e| format!("Commit: {e}"))?;
        Ok(())
    }

    // -- Pull state --

    pub fn get_pull_state(&self, procedure_id: &str) -> CliResult<Option<PullState>> {
        // A deserialize failure here means the on-disk row predates the
        // current PullState shape (auto-deploy v2 added deployment_id as
        // a required field). Treat it as "no pull state" — the next
        // `tofupilot pull` will overwrite the legacy row with a fresh
        // shape. Returning Err would brick the CLI on startup for any
        // user who pulled before the upgrade.
        let Some(bytes) = self.get(PULL_SYNC, procedure_id)? else {
            return Ok(None);
        };
        Ok(serde_json::from_slice(&bytes).ok())
    }

    pub fn set_pull_state(&self, procedure_id: &str, state: &PullState) -> CliResult<()> {
        let bytes = serde_json::to_vec(state).map_err(|e| format!("Serialize: {e}"))?;
        self.set(PULL_SYNC, procedure_id, &bytes)
    }

    /// All locally-pulled deployments as `(procedure_id, PullState)`.
    /// Source for the operator-UI idle screen's procedure list — what
    /// the station can actually run right now (deployment present
    /// on disk, deserves to appear as a pickable row).
    pub fn list_pull_state(&self) -> CliResult<Vec<(String, PullState)>> {
        let txn = self.db.begin_read().map_err(|e| format!("Read txn: {e}"))?;
        let tbl = match txn.open_table(PULL_SYNC) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(format!("Open table: {e}").into()),
        };
        let mut out = Vec::new();
        let iter = tbl.iter().map_err(|e| format!("Iter: {e}"))?;
        for entry in iter {
            let (k, v) = entry.map_err(|e| format!("Iter entry: {e}"))?;
            let id = k.value().to_string();
            // Skip rows that fail to deserialize — see get_pull_state's
            // comment for why. We intentionally don't propagate the
            // failure: a single legacy row would otherwise break every
            // caller that lists pulled procedures (operator UI idle
            // screen, station mode pull stage, etc.).
            match serde_json::from_slice::<PullState>(v.value()) {
                Ok(state) => out.push((id, state)),
                Err(_) => continue,
            }
        }
        Ok(out)
    }

    /// Resolve a deployment's human-readable procedure name with
    /// fallbacks. Source of truth: `procedure.name` in the dashboard
    /// DB, copied into `PullState.name` at pull time. When PullState
    /// is missing or has a null name (pre-rollout pull, manual deploy
    /// that bypassed `tofupilot pull`, etc.), fall back to the
    /// procedure id so callers always have something non-empty to
    /// render.
    pub fn resolve_procedure_name(&self, procedure_id: &str) -> String {
        self.get_pull_state(procedure_id)
            .ok()
            .flatten()
            .and_then(|ps| ps.name)
            .unwrap_or_else(|| procedure_id.to_string())
    }

    pub fn remove_pull_state(&self, procedure_id: &str) -> CliResult<()> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| format!("Write txn: {e}"))?;
        {
            if let Ok(mut tbl) = txn.open_table(PULL_SYNC) {
                let _ = tbl.remove(procedure_id);
            }
        }
        txn.commit().map_err(|e| format!("Commit: {e}"))?;
        Ok(())
    }

    pub fn clear_all_pull_state(&self) -> CliResult<()> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| format!("Write txn: {e}"))?;
        {
            Self::clear_table(&txn, PULL_SYNC);
        }
        txn.commit().map_err(|e| format!("Commit: {e}"))?;
        Ok(())
    }

    fn clear_table(txn: &redb::WriteTransaction, table: TableDefinition<&str, &[u8]>) {
        if let Ok(mut tbl) = txn.open_table(table) {
            let keys: Vec<String> = tbl
                .iter()
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok().map(|(k, _)| k.value().to_string()))
                .collect();
            for key in &keys {
                let _ = tbl.remove(key.as_str());
            }
        }
    }

    // -- Whoami cache --

    pub fn get_whoami(&self) -> CliResult<Option<WhoamiCache>> {
        self.get(LOGIN_WHOAMI, "current")?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|e| format!("Deserialize: {e}")))
            .transpose()
            .map_err(Into::into)
    }

    pub fn set_whoami(&self, cache: &WhoamiCache) -> CliResult<()> {
        let bytes = serde_json::to_vec(cache).map_err(|e| format!("Serialize: {e}"))?;
        self.set(LOGIN_WHOAMI, "current", &bytes)
    }

    pub fn clear_whoami(&self) -> CliResult<()> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| format!("Write txn: {e}"))?;
        {
            if let Ok(mut tbl) = txn.open_table(LOGIN_WHOAMI) {
                let _ = tbl.remove("current");
            }
        }
        txn.commit().map_err(|e| format!("Commit: {e}"))?;
        Ok(())
    }

    // -- Update cache --

    pub fn get_update_cache(&self) -> CliResult<Option<UpdateCache>> {
        self.get(UPDATE_CACHE, "current")?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|e| format!("Deserialize: {e}")))
            .transpose()
            .map_err(Into::into)
    }

    pub fn set_update_cache(&self, cache: &UpdateCache) -> CliResult<()> {
        let bytes = serde_json::to_vec(cache).map_err(|e| format!("Serialize: {e}"))?;
        self.set(UPDATE_CACHE, "current", &bytes)
    }

    // -- Pending update (survives self-replace + reexec) --

    pub fn get_pending_update(&self) -> CliResult<Option<PendingUpdate>> {
        self.get(UPDATE_PENDING, "current")?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|e| format!("Deserialize: {e}")))
            .transpose()
            .map_err(Into::into)
    }

    pub fn set_pending_update(&self, pending: &PendingUpdate) -> CliResult<()> {
        let bytes = serde_json::to_vec(pending).map_err(|e| format!("Serialize: {e}"))?;
        self.set(UPDATE_PENDING, "current", &bytes)
    }

    pub fn clear_pending_update(&self) -> CliResult<()> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| format!("Write txn: {e}"))?;
        {
            if let Ok(mut tbl) = txn.open_table(UPDATE_PENDING) {
                let _ = tbl.remove("current");
            }
        }
        txn.commit().map_err(|e| format!("Commit: {e}"))?;
        Ok(())
    }

    // -- Run queue (offline upload) --

    pub fn enqueue_run<T: serde::Serialize>(&self, queue_id: &str, queued: &T) -> CliResult<()> {
        let bytes = serde_json::to_vec(queued).map_err(|e| format!("Serialize: {e}"))?;
        self.set(RUN_QUEUE, queue_id, &bytes)
    }

    pub fn dequeue_run(&self, queue_id: &str) -> CliResult<()> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| format!("Write txn: {e}"))?;
        {
            if let Ok(mut tbl) = txn.open_table(RUN_QUEUE) {
                let _ = tbl.remove(queue_id);
            }
        }
        txn.commit().map_err(|e| format!("Commit: {e}"))?;
        Ok(())
    }

    pub fn list_queued_runs<T: serde::de::DeserializeOwned>(&self) -> CliResult<Vec<(String, T)>> {
        let txn = self.db.begin_read().map_err(|e| format!("Read txn: {e}"))?;
        let tbl = match txn.open_table(RUN_QUEUE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(format!("Open table: {e}").into()),
        };
        let mut runs = Vec::new();
        let mut dead_rows: Vec<(String, String)> = Vec::new();
        for entry in tbl.iter().map_err(|e| format!("Iterate: {e}"))? {
            let (key, value) = entry.map_err(|e| format!("Entry: {e}"))?;
            let id = key.value().to_string();
            // A schema bump to the queued-run wire shape would otherwise
            // hard-fail this entire list and `.unwrap_or_default()` at
            // every caller would silently drop every pending upload
            // fleet-wide on CLI upgrade.
            //
            // We collect un-deserializable rows for purge-after-drop.
            // These rows can never be uploaded (wire shape is gone), so
            // keeping them around just spams the operator on every
            // queue tick. Drop the txn first — `purge_dead_queued_rows`
            // takes a write txn and would deadlock against our read.
            match serde_json::from_slice::<T>(value.value()) {
                Ok(item) => runs.push((id, item)),
                Err(e) => dead_rows.push((id, e.to_string())),
            }
        }
        drop(tbl);
        drop(txn);

        if !dead_rows.is_empty() {
            self.purge_dead_queued_rows(&dead_rows);
        }
        Ok(runs)
    }

    /// Delete queued rows that fail to deserialize, log once per row
    /// per process. Idempotent — a row already removed by an earlier
    /// drain is a no-op.
    fn purge_dead_queued_rows(&self, dead: &[(String, String)]) {
        use std::collections::HashSet;
        use std::sync::Mutex;
        static LOGGED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

        let mut guard = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
        let logged = guard.get_or_insert_with(HashSet::new);

        if let Ok(txn) = self.db.begin_write() {
            {
                if let Ok(mut tbl) = txn.open_table(RUN_QUEUE) {
                    for (id, err) in dead {
                        let _ = tbl.remove(id.as_str());
                        if logged.insert(id.clone()) {
                            crate::log::warn(&format!(
                                "Dropped legacy queued run {id} from upload queue: {err}"
                            ));
                        }
                    }
                }
            }
            let _ = txn.commit();
        }
    }

    // -- Station config --

    pub fn get_config(&self, key: &str) -> CliResult<Option<String>> {
        self.get(STATION_CONFIG, key)?
            .map(|bytes| String::from_utf8(bytes).map_err(|e| format!("Decode: {e}")))
            .transpose()
            .map_err(Into::into)
    }

    pub fn set_config(&self, key: &str, value: &str) -> CliResult<()> {
        self.set(STATION_CONFIG, key, value.as_bytes())
    }

    pub fn list_config(&self) -> CliResult<Vec<(String, String)>> {
        let txn = self.db.begin_read().map_err(|e| format!("Read txn: {e}"))?;
        let tbl = match txn.open_table(STATION_CONFIG) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(format!("Open table: {e}").into()),
        };
        let mut items = Vec::new();
        for entry in tbl.iter().map_err(|e| format!("Iterate: {e}"))? {
            let (key, value) = entry.map_err(|e| format!("Entry: {e}"))?;
            let k = key.value().to_string();
            let v = String::from_utf8(value.value().to_vec()).unwrap_or_default();
            items.push((k, v));
        }
        Ok(items)
    }
}
