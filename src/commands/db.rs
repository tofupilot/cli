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
/// Per-credential counter behind the run-upload idempotency reference.
/// Keyed by credential id so a re-login (which mints a fresh credential)
/// starts a fresh namespace instead of reusing consumed values.
const RUN_REF_COUNTER: TableDefinition<&str, &[u8]> = TableDefinition::new("run.ref_counter");
/// When the last `credential_id` backfill probe failed, per credential slot
/// ("station" / "user"), unix seconds. Its own table rather than a key in
/// `station.config`: that table is what `tofupilot config` prints verbatim,
/// and an epoch timestamp with an internal name is not a setting an operator
/// should see, let alone try to change.
const CREDENTIAL_PROBE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("credential.probe_failed_at");

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

/// Allocate the next idempotency counter for `credential_id`.
///
/// Read-modify-write inside ONE redb write transaction. `enqueue` mints a
/// reference on every run it queues and can be reached from several tasks at
/// once (the station event loop produces a run while a drain is running); two
/// callers reading the same value would mint one reference for two different
/// runs, and the API would then treat the second as a retry of the first and
/// swallow it. redb serialises write transactions, so no interleaving is
/// possible here.
///
/// Seeded from the wall clock on first use rather than from 0: if the state
/// file is deleted while `credentials.json` survives, the counter resumes far
/// above every value it already used instead of replaying them. Correctness
/// never depends on the clock being *right*, only on it not going backwards by
/// days — station clocks drift, TP-1012 observed a 2-minute skew in the field.
///
/// Free function rather than a method so it can be tested against a bare
/// `Database` in a temp dir: `StateDb`'s inner `Drop` touches the real pid file.
fn next_run_ref_in(db: &Database, credential_id: &str) -> CliResult<u64> {
    let txn = db.begin_write().map_err(|e| format!("Write txn: {e}"))?;
    let next = {
        let mut tbl = txn
            .open_table(RUN_REF_COUNTER)
            .map_err(|e| format!("Open table: {e}"))?;
        let current = tbl
            .get(credential_id)
            .map_err(|e| format!("Read counter: {e}"))?
            .and_then(|guard| {
                <[u8; 8]>::try_from(guard.value())
                    .ok()
                    .map(u64::from_be_bytes)
            });
        let next = match current {
            Some(n) => n.saturating_add(1),
            None => chrono::Utc::now().timestamp_millis().max(0) as u64,
        };
        tbl.insert(credential_id, next.to_be_bytes().as_slice())
            .map_err(|e| format!("Write counter: {e}"))?;
        next
    };
    txn.commit().map_err(|e| format!("Commit: {e}"))?;
    Ok(next)
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

/// Identity slot for the whoami cache, mirroring the two credential files
/// (`credentials.json` / `station.json`). The cache used to be a single
/// last-writer-wins row, so a user-side `whoami` refresh silently evicted
/// the station identity the daemon banner, Web-UI line and kiosk analytics
/// rely on (TP-1040). One row per identity ends that interference the same
/// way the credential-file split (#1573) did for API keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhoamiSlot {
    User,
    Station,
}

impl WhoamiSlot {
    fn key(self) -> &'static str {
        match self {
            WhoamiSlot::User => "user",
            WhoamiSlot::Station => "station",
        }
    }

    /// The slot a fetched identity belongs to. The server's `auth_type` is
    /// authoritative on which kind of key made the call; anything else
    /// (including the parse default `"user"`) routes to the user slot.
    /// Private: outside callers go through [`WhoamiCache::slot`].
    fn for_auth_type(auth_type: &str) -> Self {
        if auth_type == "station" {
            WhoamiSlot::Station
        } else {
            WhoamiSlot::User
        }
    }
}

/// Pre-split installs stored a single row under this key, written by
/// whichever login or `whoami` refresh ran last.
const LEGACY_WHOAMI_KEY: &str = "current";

/// Row selection for [`StateDb::get_whoami`], split out so the legacy
/// fallback is unit-testable without a real redb file (same idiom as
/// `credentials::pick_station_first`). The slotted row always wins; the
/// legacy row is served only to the slot matching its own `auth_type`,
/// never across identities — cross-serving was the TP-1040 hazard.
fn pick_whoami_row(
    slotted: Option<WhoamiCache>,
    legacy: Option<WhoamiCache>,
    slot: WhoamiSlot,
) -> Option<WhoamiCache> {
    slotted.or_else(|| legacy.filter(|row| row.slot() == slot))
}

/// Convenience read of one whoami slot through a fresh DB handle,
/// swallowing IO errors — the cache is display data, so absence and an
/// unreadable DB are the same non-event to callers.
pub fn cached_whoami(slot: WhoamiSlot) -> Option<WhoamiCache> {
    open().ok()?.get_whoami(slot).ok()?
}

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

impl WhoamiCache {
    /// The slot this row belongs to, derived from the server-reported
    /// `auth_type`. Split out (rather than inlined at each call site) so
    /// writers and the legacy-row fallback can never disagree on routing.
    pub fn slot(&self) -> WhoamiSlot {
        WhoamiSlot::for_auth_type(&self.auth_type)
    }
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

    /// Read the cached identity for one slot. Falls back to the pre-split
    /// `"current"` row when the slot is empty AND that row's `auth_type`
    /// matches the requested slot — so an existing station keeps its
    /// banner, Web-UI line and analytics identity across the upgrade
    /// without a re-login. The legacy row is shadowed by the first slotted
    /// write and removed by [`Self::clear_whoami`]. Most callers want the
    /// one-shot [`cached_whoami`]; use this directly only to share one DB
    /// handle across a read and a write (the station heal).
    pub fn get_whoami(&self, slot: WhoamiSlot) -> CliResult<Option<WhoamiCache>> {
        Ok(pick_whoami_row(
            self.read_whoami(slot.key())?,
            self.read_whoami(LEGACY_WHOAMI_KEY)?,
            slot,
        ))
    }

    fn read_whoami(&self, key: &str) -> CliResult<Option<WhoamiCache>> {
        self.get(LOGIN_WHOAMI, key)?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|e| format!("Deserialize: {e}")))
            .transpose()
            .map_err(Into::into)
    }

    /// Store a fetched identity in the slot matching its own `auth_type`.
    /// Routing lives here (not at call sites) so a login and a `whoami`
    /// refresh can never file the same response under different slots.
    pub fn set_whoami(&self, cache: &WhoamiCache) -> CliResult<()> {
        let bytes = serde_json::to_vec(cache).map_err(|e| format!("Serialize: {e}"))?;
        self.set(LOGIN_WHOAMI, cache.slot().key(), &bytes)
    }

    /// Remove every cached identity — both slots and the legacy row.
    /// Logout is a full reset, matching `credentials::clear()`.
    pub fn clear_whoami(&self) -> CliResult<()> {
        let txn = self
            .inner
            .begin_write()
            .map_err(|e| format!("Write txn: {e}"))?;
        {
            if let Ok(mut tbl) = txn.open_table(LOGIN_WHOAMI) {
                for key in [
                    WhoamiSlot::User.key(),
                    WhoamiSlot::Station.key(),
                    LEGACY_WHOAMI_KEY,
                ] {
                    let _ = tbl.remove(key);
                }
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

    // -- Run upload idempotency --

    /// Allocate the next idempotency counter for `credential_id`.
    /// See [`next_run_ref_in`] for why the read-modify-write is one
    /// transaction and why the counter is clock-seeded.
    pub fn next_run_ref(&self, credential_id: &str) -> CliResult<u64> {
        next_run_ref_in(&self.inner.db, credential_id)
    }

    /// Unix seconds of the last failed `credential_id` probe for `slot`, if
    /// any. See [`CREDENTIAL_PROBE`].
    pub fn credential_probe_failed_at(&self, slot: &str) -> CliResult<Option<i64>> {
        Ok(self
            .get(CREDENTIAL_PROBE, slot)?
            .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_slice()).ok())
            .map(i64::from_be_bytes))
    }

    pub fn set_credential_probe_failed_at(&self, slot: &str, at: i64) -> CliResult<()> {
        self.set(CREDENTIAL_PROBE, slot, &at.to_be_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- Run upload idempotency counter --
    //
    // The counter is what makes an idempotency reference unique by
    // construction instead of by chance, so its two load-bearing properties
    // are tested rather than asserted in a comment: concurrent callers never
    // get the same value, and a wiped state file does not replay consumed
    // ones.

    fn temp_db() -> (Database, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("tp-refcounter-{}.redb", uuid::Uuid::new_v4()));
        (Database::create(&path).expect("create temp redb"), path)
    }

    #[test]
    fn next_run_ref_increments_by_one_per_call() {
        let (db, path) = temp_db();
        let a = next_run_ref_in(&db, "cred_a").unwrap();
        let b = next_run_ref_in(&db, "cred_a").unwrap();
        let c = next_run_ref_in(&db, "cred_a").unwrap();
        assert_eq!(b, a + 1);
        assert_eq!(c, a + 2);
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn next_run_ref_namespaces_by_credential() {
        let (db, path) = temp_db();
        let a1 = next_run_ref_in(&db, "cred_a").unwrap();
        let b1 = next_run_ref_in(&db, "cred_b").unwrap();
        let a2 = next_run_ref_in(&db, "cred_a").unwrap();
        assert_eq!(a2, a1 + 1, "cred_b must not advance cred_a's counter");
        // Both seed from the clock, so they can start equal — what matters is
        // that they advance independently, since the credential id is part of
        // the reference and keeps the two spaces apart.
        assert_eq!(b1, next_run_ref_in(&db, "cred_b").unwrap() - 1);
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn next_run_ref_survives_a_reopen() {
        let (db, path) = temp_db();
        let before = next_run_ref_in(&db, "cred_a").unwrap();
        drop(db);
        let db = Database::create(&path).expect("reopen temp redb");
        let after = next_run_ref_in(&db, "cred_a").unwrap();
        assert_eq!(
            after,
            before + 1,
            "counter must not restart across restarts"
        );
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn next_run_ref_is_seeded_above_zero() {
        // Seeded from the clock so a deleted state file resumes above the
        // values it already consumed rather than replaying 1, 2, 3.
        let (db, path) = temp_db();
        let first = next_run_ref_in(&db, "cred_a").unwrap();
        assert!(
            first > 1_700_000_000_000,
            "expected a millisecond-epoch seed, got {first}"
        );
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn concurrent_callers_never_get_the_same_value() {
        // The property the whole design rests on: two runs queued at the same
        // moment must not share a reference, or the API merges them and one
        // run is silently lost.
        let (db, path) = temp_db();
        let threads = 8;
        let per_thread = 25;
        let mut values: Vec<u64> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        (0..per_thread)
                            .map(|_| next_run_ref_in(&db, "cred_shared").unwrap())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("thread panicked"))
                .collect()
        });
        values.sort_unstable();
        let total = values.len();
        values.dedup();
        assert_eq!(values.len(), total, "duplicate counter values were minted");
        assert_eq!(total, threads * per_thread);
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    fn row(auth_type: &str) -> WhoamiCache {
        WhoamiCache {
            fetched_at: chrono::Utc::now(),
            auth_type: auth_type.to_string(),
            user_id: None,
            user_name: None,
            user_email: None,
            station_name: None,
            station_id: None,
            organization_name: "Org".to_string(),
            organization_slug: "org".to_string(),
        }
    }

    #[test]
    fn slot_routes_station_auth_type_to_station() {
        assert_eq!(row("station").slot(), WhoamiSlot::Station);
    }

    #[test]
    fn slot_routes_user_auth_type_to_user() {
        assert_eq!(row("user").slot(), WhoamiSlot::User);
    }

    // `fetch_whoami` defaults a missing `auth_type` to "user"; anything
    // unrecognized must land in the user slot rather than shadow the
    // station identity.
    #[test]
    fn slot_routes_unknown_auth_type_to_user() {
        assert_eq!(row("something-new").slot(), WhoamiSlot::User);
    }

    #[test]
    fn pick_prefers_the_slotted_row_over_legacy() {
        let picked = pick_whoami_row(Some(row("station")), Some(row("user")), WhoamiSlot::Station);
        assert_eq!(picked.unwrap().auth_type, "station");
    }

    // Upgrade path: a pre-split station install has only the legacy row;
    // the station slot read must serve it so the daemon keeps its
    // identity without a re-login.
    #[test]
    fn pick_serves_legacy_to_its_matching_slot() {
        let picked = pick_whoami_row(None, Some(row("station")), WhoamiSlot::Station);
        assert_eq!(picked.unwrap().auth_type, "station");
    }

    // The TP-1040 hazard: a legacy row written by the other identity must
    // never answer this slot's read.
    #[test]
    fn pick_filters_legacy_from_the_other_slot() {
        assert!(pick_whoami_row(None, Some(row("station")), WhoamiSlot::User).is_none());
        assert!(pick_whoami_row(None, Some(row("user")), WhoamiSlot::Station).is_none());
    }

    #[test]
    fn pick_none_when_neither_present() {
        assert!(pick_whoami_row(None, None, WhoamiSlot::User).is_none());
    }
}
