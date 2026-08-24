//! Out-of-run plug debug sessions for the Studio page.
//!
//! `PlugDebugStart` spawns a plug's Python service through the engine's
//! manual-plug path (`ResourceManager::start_manual_plug`), outside any
//! run; `PlugDebugCall` drives its methods one by one; `PlugDebugStop`
//! tears it down. Sessions live in the daemon and survive the browser —
//! `PlugDebugSessions` is how a reloaded page resyncs.
//!
//! Ownership: ONE optional `Inner` for the whole daemon, bound to the
//! procedure directory it was created against. A run start or a
//! project/procedure switch invalidates every session (the run's plugs
//! would contend for the same instruments), so those paths call
//! `teardown_all`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use execution_engine::plugs::manager::ResourceManager;
use execution_engine::{EventSink, ExecutionEvent};
use station_protocol::StationEvent;
use tokio::sync::{mpsc, Mutex};

/// Bound on one debug method call. Generous — a debug call pokes real
/// hardware and the human is watching — but finite, so a wedged method
/// answers an error instead of parking the RPC forever.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Why `start` refused or failed — the two answer as different RPC
/// errors (`Busy` vs `Internal`), so a plain message string won't do.
pub enum StartError {
    /// A run took the hardware between the RPC layer's early check and
    /// the lock here.
    RunActive,
    /// The service process could not be spawned or refused to come up.
    Failed(String),
}

/// Debug-session state hung off `AppState` (behind an `Arc` — the
/// state is cloned per request).
#[derive(Default)]
pub struct PlugDebugState {
    inner: Mutex<Option<Inner>>,
    /// Where the bridge sink sends its `StationEvent`s. Installed once
    /// by `tofupilot studio` at startup; `None` on daemons that never
    /// install it, where the events are simply dropped.
    event_tx: Mutex<Option<mpsc::UnboundedSender<StationEvent>>>,
}

struct Inner {
    resources: Arc<ResourceManager>,
    procedure_dir: PathBuf,
    /// Plug keys with a live session, canonical spelling from the YAML.
    sessions: HashSet<String>,
    sink: Arc<dyn EventSink>,
}

impl PlugDebugState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the sender the bridge sink emits into.
    pub async fn set_event_sender(&self, tx: mpsc::UnboundedSender<StationEvent>) {
        *self.event_tx.lock().await = Some(tx);
    }

    /// Start a debug session for `plug_key`. Answering Ok for a plug
    /// already in session keeps a double-click (or a reconnected page
    /// replaying its intent) from erroring on the state it wanted.
    ///
    /// `python` is the interpreter every service of this manager spawns
    /// under — resolved by the caller through the same venv discipline
    /// runs use, so a debug session can never disagree with Run about
    /// which environment the plug's imports come from.
    ///
    /// `run_active` is the RPC layer's run-in-flight flag, re-checked
    /// HERE, under the lock — the RPC's own early check sits before
    /// seconds of venv resolution, and a Run accepted in that window
    /// pins the flag and fires `teardown_all` (which queues on this
    /// same lock) before we get here. Loading under the lock leaves no
    /// interleaving that spawns a service a run won't tear down.
    pub async fn start(
        &self,
        procedure_dir: PathBuf,
        python: PathBuf,
        plug_key: &str,
        config_json: serde_json::Value,
        run_active: &AtomicBool,
    ) -> Result<(), StartError> {
        let mut guard = self.inner.lock().await;

        if run_active.load(Ordering::Acquire) {
            return Err(StartError::RunActive);
        }

        // A procedure switch under live sessions would leave services
        // spawned against another directory's files and venv: tear the
        // old world down before building the new one.
        if guard
            .as_ref()
            .is_some_and(|inner| inner.procedure_dir != procedure_dir)
        {
            if let Some(inner) = guard.take() {
                teardown_inner(inner).await;
            }
        }

        if guard.is_none() {
            let sink: Arc<dyn EventSink> = Arc::new(DebugEventSink {
                tx: self.event_tx.lock().await.clone(),
            });
            *guard = Some(Inner {
                resources: Arc::new(ResourceManager::new_with_python(
                    procedure_dir.clone(),
                    Some(python),
                )),
                procedure_dir,
                sessions: HashSet::new(),
                sink,
            });
        }
        let inner = guard.as_mut().expect("just installed");

        if inner.sessions.contains(plug_key) {
            return Ok(());
        }
        inner
            .resources
            .start_manual_plug(plug_key.to_string(), config_json, &inner.sink)
            .await
            .map_err(StartError::Failed)?;
        inner.sessions.insert(plug_key.to_string());
        Ok(())
    }

    /// End `plug_key`'s session. Idempotent by contract: every owner
    /// (stop button, mode exit, tab close) fires it, and stopping what
    /// is not running must answer as stopped, not as an error.
    pub async fn stop(&self, plug_key: &str) {
        let mut guard = self.inner.lock().await;
        let Some(inner) = guard.as_mut() else {
            return;
        };
        if !inner.sessions.remove(plug_key) {
            return;
        }
        if let Err(e) = inner
            .resources
            .stop_manual_plug(plug_key, &inner.sink)
            .await
        {
            // The session bookkeeping already dropped it; a service
            // that refused a graceful stop was force-killed inside
            // stop_plug_service's ladder.
            crate::log::warn(&format!("plug debug: stop of '{plug_key}' failed: {e}"));
        }
    }

    /// Whether `plug_key` has a live session.
    pub async fn has_session(&self, plug_key: &str) -> bool {
        self.inner
            .lock()
            .await
            .as_ref()
            .is_some_and(|inner| inner.sessions.contains(plug_key))
    }

    /// Call one method on a session's service. The state lock is held
    /// only to snapshot the manager — a 60s-bounded call must not block
    /// starting or stopping other plugs.
    pub async fn call(
        &self,
        plug_key: &str,
        method: &str,
        args_json: Option<String>,
        kwargs_json: Option<String>,
    ) -> Result<execution_engine::protocol::PlugResponse, String> {
        let resources = {
            let guard = self.inner.lock().await;
            let inner = guard
                .as_ref()
                .filter(|inner| inner.sessions.contains(plug_key))
                .ok_or_else(|| format!("no debug session for plug '{plug_key}'"))?;
            Arc::clone(&inner.resources)
        };
        let port = resources
            .get_plug_service_manager()
            .get_plug_port(plug_key)
            .await
            .ok_or_else(|| format!("no debug session for plug '{plug_key}'"))?;
        execution_engine::plugs::plug_service::call_plug_method(
            port,
            method,
            args_json,
            kwargs_json,
            CALL_TIMEOUT,
        )
        .await
    }

    /// Plug keys with a live session, sorted for a stable reply.
    pub async fn session_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .inner
            .lock()
            .await
            .as_ref()
            .map(|inner| inner.sessions.iter().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        keys
    }

    /// Stop every session and drop the manager. A no-op when nothing is
    /// running, so every owner (run start, project/procedure switch,
    /// daemon shutdown) can fire it unconditionally.
    pub async fn teardown_all(&self) {
        if let Some(inner) = self.inner.lock().await.take() {
            teardown_inner(inner).await;
        }
    }
}

async fn teardown_inner(inner: Inner) {
    if let Err(e) = inner.resources.teardown_manual_plugs(&inner.sink).await {
        crate::log::warn(&format!("plug debug: teardown failed: {e}"));
    }
    // Dropping `inner.resources` here is the backstop: the service
    // manager's Drop force-kills anything the graceful pass missed.
}

/// Bridges the engine's plug events onto the loopback WS, with no
/// `execution_id` — the field is what tells the page's reducer these
/// belong to a debug session, not to a run. Field mapping mirrors the
/// per-run `CliEventSink` (commands/run/engine.rs); every other engine
/// event is impossible outside a run and ignored.
struct DebugEventSink {
    tx: Option<mpsc::UnboundedSender<StationEvent>>,
}

impl EventSink for DebugEventSink {
    fn emit(&self, event: &ExecutionEvent) {
        let Some(tx) = &self.tx else {
            return;
        };
        match event {
            ExecutionEvent::PlugStatus(status) => {
                let _ = tx.send(StationEvent::PlugStatus {
                    plug_key: status.plug_key.clone(),
                    plug_name: status.plug_name.clone(),
                    stage: plug_stage_str(&status.stage).to_string(),
                    status: plug_status_str(&status.status).to_string(),
                    scope: plug_scope_str(&status.scope).to_string(),
                    slot_id: status.slot_id.clone(),
                    execution_id: None,
                });
            }
            ExecutionEvent::PlugLog(log_event) => {
                let _ = tx.send(StationEvent::PlugLog {
                    plug_key: log_event.plug_key.clone(),
                    plug_name: log_event.plug_name.clone(),
                    level: log_event.level.clone(),
                    message: log_event.message.clone(),
                    slot_id: log_event.slot_id.clone(),
                    stage: log_event
                        .stage
                        .as_ref()
                        .map(|s| plug_stage_str(s).to_string()),
                    timestamp: log_event.timestamp.clone(),
                    line: log_event.line,
                    execution_id: None,
                });
            }
            _ => {}
        }
    }
}

// The wire spellings, replicated from the per-run sink in
// commands/run/engine.rs (private there; the two must stay in step).

fn plug_status_str(s: &execution_engine::events::PlugStatusValue) -> &'static str {
    use execution_engine::events::PlugStatusValue;
    match s {
        PlugStatusValue::Idle => "idle",
        PlugStatusValue::Initializing => "initializing",
        PlugStatusValue::Active => "active",
        PlugStatusValue::Destructing => "destructing",
        PlugStatusValue::Error => "error",
        PlugStatusValue::Skipped => "skipped",
    }
}

fn plug_stage_str(s: &execution_engine::events::PlugStage) -> &'static str {
    use execution_engine::events::PlugStage;
    match s {
        PlugStage::Setup => "setup",
        PlugStage::Teardown => "teardown",
        PlugStage::Manual => "manual",
    }
}

fn plug_scope_str(s: &execution_engine::events::PlugScope) -> &'static str {
    use execution_engine::events::PlugScope;
    match s {
        PlugScope::Execution => "execution",
        PlugScope::Slot => "slot",
        PlugScope::Station => "station",
    }
}
