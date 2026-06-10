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

// Weak so the redb file lock is held only while at least one StateDb
// is alive. Every caller opens per-operation (`let db = open()?`), so
// the lock is released between operations and concurrent tofupilot
// processes (station daemon + CLI commands, parallel runs) interleave
// instead of starving each other for the whole process lifetime.
// In-process callers still share one Database: the Mutex serializes
// open(), and an upgrade hit reuses the live instance.
static DB: std::sync::Mutex<std::sync::Weak<DbInner>> =
    std::sync::Mutex::new(std::sync::Weak::new());

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

/// How long `open()` waits for a live concurrent CLI to release the
/// redb lock before giving up. Most holders are short-lived (a queue
/// tick, an update check); long holders (a full `tofupilot run`)
/// surface as a clean "state db busy" error instead of a kill.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Release any in-process reference and drop the pidfile so a child
/// process (re-exec on Windows) can open the store. Live `StateDb`
/// handles elsewhere keep the lock until they drop; callers on the
/// re-exec path hold none. Safe to call when no DB was opened.
pub fn close() {
    if let Ok(mut guard) = DB.lock() {
        *guard = std::sync::Weak::new();
    }
    remove_own_pidfile();
}

/// Read the lock holder recorded in the sidecar pidfile, ignoring our
/// own pid (we never contend with ourselves — the in-process path is
/// served from the cached instance).
fn holder_pid(pid_file: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&p| p != std::process::id())
}

/// Remove the pidfile only if it still names this process. A plain
/// remove could race a concurrent CLI that already acquired the lock
/// and wrote its own pid.
fn remove_own_pidfile() {
    let Ok(p) = pid_path() else { return };
    let ours = std::process::id().to_string();
    if std::fs::read_to_string(&p)
        .map(|s| s.trim() == ours)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(p);
    }
}

pub fn open() -> CliResult<StateDb> {
    let path = state_path()?;
    let pid_file = pid_path()?;
    let deadline = std::time::Instant::now() + LOCK_WAIT;

    // Another process can hold the redb lock. Never kill it — a
    // healthy concurrent `tofupilot run` (or the station daemon) is
    // the common holder, and SIGKILLing it mid-run loses the run and
    // orphans its Python workers. Holders are short-lived (the lock
    // is released when the last StateDb drops, i.e. between
    // operations), so poll for release up to LOCK_WAIT, then fail
    // with a message naming the holder.
    //
    // The DB mutex is held only inside each iteration, never across
    // the sleep: open() runs on async runtime threads (station event
    // loop, drain loop), and holding it through the wait would
    // serialize N in-process waiters to N×LOCK_WAIT while pinning
    // their worker threads.
    loop {
        {
            let mut guard = DB.lock().unwrap_or_else(|e| e.into_inner());
            // An in-process holder shares its instance — contention
            // below can only come from another process.
            if let Some(inner) = guard.upgrade() {
                return Ok(StateDb { inner });
            }
            match Database::create(&path) {
                Ok(db) => {
                    // Best-effort: a concurrent CLI hitting the lock
                    // reads this to name the holder in its busy
                    // error. Write failure is non-fatal.
                    let _ = std::fs::write(&pid_file, std::process::id().to_string());
                    let inner = Arc::new(DbInner { db });
                    *guard = Arc::downgrade(&inner);
                    return Ok(StateDb { inner });
                }
                Err(DatabaseError::DatabaseAlreadyOpen) => {}
                Err(e) => return Err(format!("Open database: {e}").into()),
            }
        }

        // Re-read the pidfile every iteration: the holder can change
        // while we wait (old holder exits, another waiter wins the
        // lock and writes its own pid). Only clear a pidfile whose
        // recorded pid is dead — and re-check the content right
        // before removing so we don't delete a fresh holder's file.
        let holder = holder_pid(&pid_file);
        if let Some(pid) = holder {
            if !pid_alive(pid) && holder_pid(&pid_file) == Some(pid) {
                let _ = std::fs::remove_file(&pid_file);
            }
        }

        if std::time::Instant::now() >= deadline {
            let holder = holder_pid(&pid_file)
                .map(|p| format!(" (held by PID {p})"))
                .unwrap_or_default();
            return Err(format!(
                "State database is busy{holder}: another tofupilot process is using it. Retry once it finishes."
            )
            .into());
        }
        std::thread::sleep(LOCK_POLL);
    }
}

/// Owns the Database so the last dropped handle releases the redb
/// lock and clears our pidfile, letting waiting processes proceed.
struct DbInner {
    db: Database,
}

impl Drop for DbInner {
    fn drop(&mut self) {
        remove_own_pidfile();
    }
}

impl std::ops::Deref for DbInner {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}

#[derive(Clone)]
pub struct StateDb {
    inner: Arc<DbInner>,
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
        let txn = self
            .inner
            .begin_read()
            .map_err(|e| format!("Read txn: {e}"))?;
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
            .inner
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
        let txn = self
            .inner
            .begin_read()
            .map_err(|e| format!("Read txn: {e}"))?;
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
            .inner
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
            .inner
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
            .inner
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
            .inner
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
            .inner
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
        let txn = self
            .inner
            .begin_read()
            .map_err(|e| format!("Read txn: {e}"))?;
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

        if let Ok(txn) = self.inner.begin_write() {
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
        let txn = self
            .inner
            .begin_read()
            .map_err(|e| format!("Read txn: {e}"))?;
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
