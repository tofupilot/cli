//! Station-scope plug ownership.
//!
//! A [`StationPlugHost`] owns `scope: station` plug processes across run
//! boundaries. The per-run [`Orchestrator`](crate::orchestrator) borrows
//! instances from the host instead of spawning them, and never registers
//! them in its own [`PlugServiceManager`] — so every run-owned teardown
//! path (auto-teardown, shutdown sweep, `Drop`) stays oblivious to them
//! and the run-leak invariant from #1293 holds unchanged for run-owned
//! processes.
//!
//! The host's own `PlugServiceManager` drops with the host, which lives
//! for the station process — that is the intended teardown point, and
//! `Drop for PlugServiceManager` / `Drop for ChildProcess` guarantee the
//! plug interpreters die with it even on panic.
//!
//! Staleness: a held instance is keyed to the procedure context
//! (procedure dir + resolved Python) and a per-plug fingerprint of its
//! spawn identity (file, class, `__init__` kwargs). A procedure switch
//! changes the dir, which swaps the whole context; a plug definition
//! edit changes the fingerprint, which respawns that plug only. Reuse
//! additionally requires a live `GetStatus` probe.
//!
//! Deployment updates are INVISIBLE to those checks: the bundle is
//! swapped in place (same directory, new contents — see the CLI's
//! `pull::sync`), and the fingerprint does not hash the plug's Python
//! code. The station loop therefore calls [`StationPlugHost::shutdown`]
//! right after applying a staged swap, so the next run respawns every
//! plug from the new bundle instead of reusing instances built from
//! code that no longer exists on disk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::event_sink::EventSink;
use crate::events::{PlugScope, PlugStage, PlugStatusValue};
use crate::plugs::plug_service::{probe_plug_health, PlugServiceManager};

/// Spawn identity of a station plug instance. Two definitions with equal
/// fingerprints (within one procedure context) may share a live process;
/// anything else forces a respawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlugFingerprint(String);

impl PlugFingerprint {
    /// Built from the plug's spawn config (`{file, class, config}` as
    /// produced by `PlugDefinition::to_config_json`). The JSON is
    /// serialized with sorted keys by construction (`serde_json::Value`
    /// maps are BTree-backed), so equal configs yield equal strings.
    pub fn from_config(config_json: &serde_json::Value) -> Self {
        Self(config_json.to_string())
    }
}

#[derive(Debug)]
struct StationPlugEntry {
    port: u16,
    fingerprint: PlugFingerprint,
    display_name: String,
}

/// The procedure context a set of held plugs belongs to. Any change
/// swaps the context: a different procedure lives in a different
/// directory with its own venv, and plug code loaded from the old one
/// must not be reused.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostContext {
    procedure_dir: PathBuf,
    python_path: Option<PathBuf>,
}

#[derive(Default)]
struct HostInner {
    context: Option<HostContext>,
    manager: Option<Arc<PlugServiceManager>>,
    live: HashMap<String, StationPlugEntry>,
}

/// Owns station-scope plug processes across runs. Create one per
/// station process (the CLI station loop), share it with every run via
/// `Orchestrator::with_station_plug_host`.
#[derive(Default)]
pub struct StationPlugHost {
    inner: Mutex<HostInner>,
}

impl std::fmt::Debug for StationPlugHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StationPlugHost").finish_non_exhaustive()
    }
}

impl StationPlugHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow a live instance for `plug_key`, spawning or respawning as
    /// needed. Returns the loopback port phases connect to.
    ///
    /// Reuse requires all three: same procedure context, same
    /// fingerprint, passing health probe. A context change tears down
    /// every held plug first (the old procedure's code and venv must
    /// not leak into the new one); a fingerprint change or failed probe
    /// respawns that plug only.
    #[allow(clippy::too_many_arguments)]
    pub async fn acquire(
        &self,
        procedure_dir: &PathBuf,
        python_path: &Option<PathBuf>,
        plug_key: &str,
        display_name: &str,
        config_json: serde_json::Value,
        event_sink: &Arc<dyn EventSink>,
    ) -> Result<u16, String> {
        let mut inner = self.inner.lock().await;

        let context = HostContext {
            procedure_dir: procedure_dir.clone(),
            python_path: python_path.clone(),
        };

        if inner.context.as_ref() != Some(&context) {
            if let Some(manager) = &inner.manager {
                if !inner.live.is_empty() {
                    log::info!(
                        "Station plug context changed (procedure switch); \
                         releasing {} held plug(s)",
                        inner.live.len()
                    );
                }
                // Graceful stop; a partially-failed stop is still safe
                // because stop_plug_service removes the entry BEFORE
                // shutting down, so the ChildProcess drops locally and
                // its Drop SIGKILLs the process group. That Drop is the
                // real backstop here — the manager replaced below drops
                // with an already-empty map.
                manager.stop_all_services().await.ok();
            }
            inner.live.clear();
            inner.manager = Some(Arc::new(PlugServiceManager::new_with_python(
                procedure_dir.clone(),
                python_path.clone(),
            )));
            inner.context = Some(context);
        }

        let manager = Arc::clone(
            inner
                .manager
                .as_ref()
                .expect("manager set with context above"),
        );

        let fingerprint = PlugFingerprint::from_config(&config_json);

        if let Some(entry) = inner.live.get(plug_key) {
            if entry.fingerprint == fingerprint {
                match probe_plug_health(entry.port).await {
                    Ok(()) => {
                        log::info!(
                            "Reusing station plug '{}' on port {}",
                            plug_key,
                            entry.port
                        );
                        return Ok(entry.port);
                    }
                    Err(e) => {
                        log::warn!(
                            "Station plug '{}' failed health probe ({}); respawning",
                            plug_key,
                            e
                        );
                    }
                }
            } else {
                log::info!(
                    "Station plug '{}' definition changed; respawning",
                    plug_key
                );
            }

            // Stale or dead — release before respawn. A dead process
            // makes the graceful path fail fast; fall back to kill.
            if manager.stop_plug_service(plug_key).await.is_err() {
                manager.force_kill_plug_service(plug_key).await.ok();
            }
            inner.live.remove(plug_key);
        }

        crate::plugs::manager::emit_plug_status(
            event_sink,
            plug_key.to_string(),
            display_name.to_string(),
            PlugScope::Station,
            None,
            PlugStage::Setup,
            PlugStatusValue::Initializing,
        );

        let port = match manager
            .start_plug_service(
                plug_key.to_string(),
                plug_key.to_string(),
                display_name.to_string(),
                config_json,
                None,
                event_sink,
            )
            .await
        {
            Ok(port) => port,
            Err(e) => {
                crate::plugs::manager::emit_plug_status(
                    event_sink,
                    plug_key.to_string(),
                    display_name.to_string(),
                    PlugScope::Station,
                    None,
                    PlugStage::Setup,
                    PlugStatusValue::Error,
                );
                return Err(e);
            }
        };

        inner.live.insert(
            plug_key.to_string(),
            StationPlugEntry {
                port,
                fingerprint,
                display_name: display_name.to_string(),
            },
        );

        log::info!("Created station plug '{}' on port {}", plug_key, port);

        crate::plugs::manager::emit_plug_status(
            event_sink,
            plug_key.to_string(),
            display_name.to_string(),
            PlugScope::Station,
            None,
            PlugStage::Setup,
            PlugStatusValue::Active,
        );

        Ok(port)
    }

    /// Gracefully release every held plug. Call at station shutdown; the
    /// `Drop` chain covers abnormal exits.
    pub async fn shutdown(&self, event_sink: Option<&Arc<dyn EventSink>>) {
        let mut inner = self.inner.lock().await;
        if let Some(manager) = &inner.manager {
            if let Some(sink) = event_sink {
                for (key, entry) in &inner.live {
                    crate::plugs::manager::emit_plug_status(
                        sink,
                        key.clone(),
                        entry.display_name.clone(),
                        PlugScope::Station,
                        None,
                        PlugStage::Teardown,
                        PlugStatusValue::Destructing,
                    );
                }
            }
            manager.stop_all_services().await.ok();
        }
        inner.live.clear();
        inner.context = None;
        inner.manager = None;
    }

    /// Number of currently held plug instances (test/diagnostic aid).
    pub async fn held_count(&self) -> usize {
        self.inner.lock().await.live.len()
    }
}
