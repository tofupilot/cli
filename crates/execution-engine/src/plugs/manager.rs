use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;


use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::event_sink::{EventSink, ExecutionEvent};
use crate::plugs::plug_service::PlugServiceManager;
use crate::events::{PlugScope, PlugStage, PlugStatusUpdateEvent, PlugStatusValue};

pub(crate) fn emit_plug_status(
    event_sink: &Arc<dyn EventSink>,
    plug_key: String,
    plug_name: String,
    scope: PlugScope,
    slot_id: Option<String>,
    stage: PlugStage,
    status: PlugStatusValue,
) {
    let event = PlugStatusUpdateEvent {
        plug_key: plug_key.clone(),
        plug_name: plug_name.clone(),
        scope,
        slot_id,
        stage,
        status,
    };

    log::debug!("PLUG [BACKEND] Emitting plug-status-update: {:?}", event);
    event_sink.emit(&ExecutionEvent::PlugStatus(event));
}

#[derive(Debug, Clone)]
pub struct ResourceAllocation {
    pub job_id: Uuid,
    pub allocated_resources: HashMap<String, String>, // resource_type -> specific_instance
    pub plug_ports: HashMap<String, u16>,             // plug_key -> port
}

#[derive(Debug, Clone)]
pub struct PlugInstance {
    pub port: u16,
    pub slot_id: Option<String>, // None for all-slots plugs
    pub ref_count: usize,        // Number of jobs using this instance
}

#[derive(Debug)]
pub struct ResourceManager {
    pools: Arc<RwLock<HashMap<String, ResourcePool>>>,
    allocations: Arc<RwLock<Vec<ResourceAllocation>>>,
    plug_service_manager: Arc<PlugServiceManager>,
    // Track plug instances by key and optionally slot
    plug_instances: Arc<RwLock<HashMap<String, PlugInstance>>>, // "plug_key" or "plug_key_slot1"
    plug_scopes: Arc<RwLock<HashMap<String, PlugScope>>>,       // plug_key -> scope
    procedure_plugs_lock: Arc<Mutex<HashSet<String>>>,          // Track all-slots plugs in use
    manual_plugs: Arc<RwLock<HashSet<String>>>, // Track manually-started plugs (debug mode)
}

#[derive(Debug)]
struct ResourcePool {
    available: HashSet<String>,
    total: HashSet<String>,
}

impl ResourceManager {
    pub fn new(procedure_dir: PathBuf) -> Self {
        Self::new_with_python(procedure_dir, None)
    }

    pub fn new_with_python(procedure_dir: PathBuf, python_path: Option<PathBuf>) -> Self {
        Self {
            pools: Arc::new(RwLock::new(HashMap::new())),
            allocations: Arc::new(RwLock::new(Vec::new())),
            plug_service_manager: Arc::new(PlugServiceManager::new_with_python(
                procedure_dir,
                python_path,
            )),
            plug_instances: Arc::new(RwLock::new(HashMap::new())),
            plug_scopes: Arc::new(RwLock::new(HashMap::new())),
            procedure_plugs_lock: Arc::new(Mutex::new(HashSet::new())),
            manual_plugs: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub async fn set_plug_scopes(&self, scopes: HashMap<String, PlugScope>) {
        // Store the scopes for all plugs
        let mut plug_scopes = self.plug_scopes.write().await;
        *plug_scopes = scopes;
    }

    pub async fn register_resource_pool(&self, resource_type: String, instances: Vec<String>) {
        let pool = ResourcePool {
            available: instances.iter().cloned().collect(),
            total: instances.iter().cloned().collect(),
        };

        self.pools.write().await.insert(resource_type, pool);
    }

    pub async fn can_allocate_resources(&self, required_resources: &[String]) -> bool {
        let pools = self.pools.read().await;

        for resource_type in required_resources {
            if let Some(pool) = pools.get(resource_type) {
                if pool.available.is_empty() {
                    return false;
                }
            } else {
                continue;
            }
        }

        true
    }

    pub async fn allocate_resources(
        &self,
        job_id: Uuid,
        required_resources: &[String],
    ) -> Result<ResourceAllocation, String> {
        let mut pools = self.pools.write().await;
        let mut allocated = HashMap::new();
        let mut reserved_instances = Vec::new();

        // Try to allocate all required resources
        for resource_type in required_resources {
            if let Some(pool) = pools.get_mut(resource_type) {
                if let Some(instance) = pool.available.iter().next().cloned() {
                    pool.available.remove(&instance);
                    allocated.insert(resource_type.clone(), instance.clone());
                    reserved_instances.push((resource_type.clone(), instance));
                } else {
                    for (rollback_type, rollback_instance) in reserved_instances {
                        if let Some(rollback_pool) = pools.get_mut(&rollback_type) {
                            rollback_pool.available.insert(rollback_instance);
                        }
                    }
                    return Err(format!("No available instances of {}", resource_type));
                }
            }
        }

        let allocation = ResourceAllocation {
            job_id,
            allocated_resources: allocated,
            plug_ports: HashMap::new(), // Will be populated when plugs are started
        };

        self.allocations.write().await.push(allocation.clone());

        Ok(allocation)
    }

    pub async fn release_resources(&self, job_id: Uuid) -> Result<(), String> {
        let mut allocations = self.allocations.write().await;
        let mut pools = self.pools.write().await;

        if let Some(pos) = allocations.iter().position(|a| a.job_id == job_id) {
            let allocation = allocations.remove(pos);
            for (resource_type, instance) in allocation.allocated_resources {
                if let Some(pool) = pools.get_mut(&resource_type) {
                    pool.available.insert(instance);
                }
            }

            Ok(())
        } else {
            Err(format!("No allocation found for job {}", job_id))
        }
    }

    pub async fn get_resource_stats(&self) -> HashMap<String, (usize, usize)> {
        let pools = self.pools.read().await;
        let mut stats = HashMap::new();

        for (resource_type, pool) in pools.iter() {
            stats.insert(
                resource_type.clone(),
                (pool.available.len(), pool.total.len()),
            );
        }

        stats
    }

    /// Start plug services for a job with scope awareness
    pub async fn start_plug_services(
        &self,
        job_id: Uuid,
        plug_configs: &HashMap<String, serde_json::Value>,
    ) -> Result<HashMap<String, u16>, String> {
        self.start_plug_services_for_slot(job_id, plug_configs, None)
            .await
    }

    /// Start plug services for a job, optionally in a specific slot
    pub async fn start_plug_services_for_slot(
        &self,
        job_id: Uuid,
        plug_configs: &HashMap<String, serde_json::Value>,
        slot_id: Option<String>,
    ) -> Result<HashMap<String, u16>, String> {
        let mut plug_ports = HashMap::new();
        let scopes = self.plug_scopes.read().await;
        let mut instances = self.plug_instances.write().await;

        // Start or reuse plug services based on scope
        for plug_name in plug_configs.keys() {
            let scope = scopes.get(plug_name).cloned().unwrap_or(PlugScope::Slot);

            // Determine the instance key based on scope. Run and station
            // scopes share one instance across slots, so the bare plug
            // key addresses it.
            let instance_key = match scope {
                PlugScope::Slot => {
                    // For slot-level plugs, include slot ID in the key
                    if let Some(ref slot) = slot_id {
                        format!("{}_{}", plug_name, slot)
                    } else {
                        // If no slot specified, treat as job-level (backward compat)
                        format!("{}_{}", plug_name, job_id)
                    }
                }
                PlugScope::Execution | PlugScope::Station => plug_name.clone(),
            };

            // Plug should already exist - just increment reference count
            if let Some(instance) = instances.get_mut(&instance_key) {
                instance.ref_count += 1;
                plug_ports.insert(plug_name.clone(), instance.port);
                log::debug!(
                    "Phase using plug {} (ref_count: {})",
                    instance_key, instance.ref_count
                );
                // No event needed - plug is already ready
            } else {
                return Err(format!(
                    "Plug {} should have been created at scope boundary but doesn't exist",
                    instance_key
                ));
            }

            // For all-slots plugs, track usage for locking
            if matches!(scope, PlugScope::Execution) {
                let mut lock = self.procedure_plugs_lock.lock().await;
                lock.insert(plug_name.clone());
            }
        }

        // Update the allocation with plug ports
        let mut allocations = self.allocations.write().await;
        if let Some(allocation) = allocations.iter_mut().find(|a| a.job_id == job_id) {
            allocation.plug_ports = plug_ports.clone();
        }

        Ok(plug_ports)
    }

    /// Stop plug services for a job with scope awareness
    pub async fn stop_plug_services(&self, job_id: Uuid) -> Result<(), String> {
        self.stop_plug_services_for_slot(job_id, None).await
    }

    /// Stop plug services for a job, handling scope properly
    pub async fn stop_plug_services_for_slot(
        &self,
        job_id: Uuid,
        slot_id: Option<String>,
    ) -> Result<(), String> {
        let allocations = self.allocations.read().await;
        let scopes = self.plug_scopes.read().await;

        if let Some(allocation) = allocations.iter().find(|a| a.job_id == job_id) {
            let mut instances = self.plug_instances.write().await;

            for plug_name in allocation.plug_ports.keys() {
                let scope = scopes.get(plug_name).cloned().unwrap_or(PlugScope::Slot);

                // Determine the instance key
                let instance_key = match scope {
                    PlugScope::Slot => {
                        if let Some(ref slot) = slot_id {
                            format!("{}_{}", plug_name, slot)
                        } else {
                            format!("{}_{}", plug_name, job_id)
                        }
                    }
                    PlugScope::Execution | PlugScope::Station => plug_name.clone(),
                };

                // Just decrement ref count - don't destroy plugs here
                if let Some(instance) = instances.get_mut(&instance_key) {
                    instance.ref_count -= 1;
                    log::debug!(
                        "Phase done using plug {} (ref_count: {})",
                        instance_key, instance.ref_count
                    );
                    // Plug stays ready - will be destroyed at scope boundary
                }
            }
        }
        Ok(())
    }

    /// Create all-slots plugs at procedure start
    pub async fn create_procedure_plugs(
        &self,
        plug_configs: &HashMap<String, serde_json::Value>,
        plug_display_names: &HashMap<String, String>,
        event_sink: &Arc<dyn EventSink>,
    ) -> Result<(), String> {
        let scopes = self.plug_scopes.read().await;
        let mut instances = self.plug_instances.write().await;

        for (plug_name, plug_config) in plug_configs {
            let scope = scopes.get(plug_name).cloned().unwrap_or(PlugScope::Slot);

            if matches!(scope, PlugScope::Execution) {
                // Only create all-slots plugs here
                let instance_key = plug_name.clone();

                if !instances.contains_key(&instance_key) {
                    let display_name = plug_display_names.get(plug_name).cloned().unwrap_or_else(|| plug_name.clone());

                    // Emit initializing event
                    emit_plug_status(
                        event_sink,
                        plug_name.clone(),
                        display_name.clone(),
                        scope.clone(),
                        None,
                        PlugStage::Setup,
                        PlugStatusValue::Initializing,
                    );

                    // Start the plug service (scope=All → slot_id=None)
                    let port = match self
                        .plug_service_manager
                        .start_plug_service(
                            instance_key.clone(),
                            plug_name.clone(),
                            display_name.clone(),
                            plug_config.clone(),
                            None,
                            event_sink,
                        )
                        .await
                    {
                        Ok(port) => port,
                        Err(e) => {
                            // Emit error status before returning
                            emit_plug_status(
                                event_sink,
                                plug_name.clone(),
                                display_name.clone(),
                                scope.clone(),
                                None,
                                PlugStage::Setup,
                                PlugStatusValue::Error,
                            );
                            return Err(e);
                        }
                    };

                    instances.insert(
                        instance_key.clone(),
                        PlugInstance {
                            port,
                            slot_id: None, // All-slots
                            ref_count: 0,  // Will be incremented when slots use it
                        },
                    );

                    log::info!("Created all-slots plug {} on port {}", instance_key, port);

                    // Emit ready event
                    emit_plug_status(
                        event_sink,
                        plug_name.clone(),
                        display_name.clone(),
                        scope.clone(),
                        None,
                        PlugStage::Setup,
                        PlugStatusValue::Active,
                    );
                }
            }
        }

        Ok(())
    }

    /// Register a station-owned plug instance so phases can allocate it
    /// like any other. The service process belongs to the
    /// `StationPlugHost` — it is deliberately NOT in this manager's
    /// `PlugServiceManager`, so no run-owned teardown path (auto-
    /// teardown, shutdown sweep, `Drop`) can reach it.
    pub async fn register_station_plug(&self, plug_key: String, port: u16) {
        let mut instances = self.plug_instances.write().await;
        instances.insert(
            plug_key,
            PlugInstance {
                port,
                slot_id: None, // shared across slots, like execution scope
                ref_count: 0,
            },
        );
    }

    /// Create slot-level plugs at slot start
    pub async fn create_slot_plugs(
        &self,
        slot_id: String,
        plug_configs: &HashMap<String, serde_json::Value>,
        plug_display_names: &HashMap<String, String>,
        event_sink: &Arc<dyn EventSink>,
    ) -> Result<(), String> {
        let scopes = self.plug_scopes.read().await;
        let mut instances = self.plug_instances.write().await;

        for (plug_name, plug_config) in plug_configs {
            let scope = scopes.get(plug_name).cloned().unwrap_or(PlugScope::Slot);

            if matches!(scope, PlugScope::Slot) {
                // Only create slot-level plugs here
                let instance_key = format!("{}_{}", plug_name, slot_id);

                if !instances.contains_key(&instance_key) {
                    let display_name = plug_display_names.get(plug_name).cloned().unwrap_or_else(|| plug_name.clone());

                    // Emit initializing event
                    emit_plug_status(
                        event_sink,
                        plug_name.clone(),
                        display_name.clone(),
                        scope.clone(),
                        Some(slot_id.clone()),
                        PlugStage::Setup,
                        PlugStatusValue::Initializing,
                    );

                    // Start the plug service
                    let port = match self
                        .plug_service_manager
                        .start_plug_service(
                            instance_key.clone(),
                            plug_name.clone(),
                            display_name.clone(),
                            plug_config.clone(),
                            Some(slot_id.clone()),
                            event_sink,
                        )
                        .await
                    {
                        Ok(port) => port,
                        Err(e) => {
                            // Emit error status before returning
                            emit_plug_status(
                                event_sink,
                                plug_name.clone(),
                                display_name.clone(),
                                scope.clone(),
                                Some(slot_id.clone()),
                                PlugStage::Setup,
                                PlugStatusValue::Error,
                            );
                            return Err(e);
                        }
                    };

                    instances.insert(
                        instance_key.clone(),
                        PlugInstance {
                            port,
                            slot_id: Some(slot_id.clone()),
                            ref_count: 0, // Will be incremented when phases use it
                        },
                    );

                    log::info!("Created slot-level plug {} on port {}", instance_key, port);

                    // Emit ready event
                    emit_plug_status(
                        event_sink,
                        plug_name.clone(),
                        display_name.clone(),
                        scope.clone(),
                        Some(slot_id.clone()),
                        PlugStage::Setup,
                        PlugStatusValue::Active,
                    );
                }
            }
        }

        Ok(())
    }

    /// Check if there are any each-scope plugs for a given slot
    pub async fn has_each_scope_plugs(&self, slot_id: &str) -> bool {
        let scopes = self.plug_scopes.read().await;
        let instances = self.plug_instances.read().await;

        instances.keys().any(|key| {
            if key.ends_with(&format!("_{}", slot_id)) {
                let plug_name = key.strip_suffix(&format!("_{}", slot_id)).unwrap_or(key);
                let scope = scopes.get(plug_name).cloned().unwrap_or(PlugScope::Slot);
                matches!(scope, PlugScope::Slot)
            } else {
                false
            }
        })
    }

    /// Check if there are any all-scope plugs
    pub async fn has_all_scope_plugs(&self) -> bool {
        let scopes = self.plug_scopes.read().await;
        let instances = self.plug_instances.read().await;

        instances.keys().any(|key| {
            let scope = scopes.get(key.as_str()).cloned().unwrap_or(PlugScope::Slot);
            matches!(scope, PlugScope::Execution)
        })
    }

    /// Destroy each-scope plugs at slot end
    pub async fn destroy_each_scope_plugs(
        &self,
        slot_id: String,
        event_sink: &Arc<dyn EventSink>,
    ) -> Result<(), String> {
        let scopes = self.plug_scopes.read().await;
        let mut instances = self.plug_instances.write().await;

        let keys_to_remove: Vec<String> = instances
            .keys()
            .filter(|key| {
                if key.ends_with(&format!("_{}", slot_id)) {
                    let plug_name = key.strip_suffix(&format!("_{}", slot_id)).unwrap_or(key);
                    let scope = scopes.get(plug_name).cloned().unwrap_or(PlugScope::Slot);
                    matches!(scope, PlugScope::Slot)
                } else {
                    false
                }
            })
            .cloned()
            .collect();

        for instance_key in keys_to_remove {
            if let Some(_instance) = instances.remove(&instance_key) {
                // Extract plug key by removing the "_slot_X" suffix
                let plug_key = instance_key
                    .strip_suffix(&format!("_{}", slot_id))
                    .unwrap_or(&instance_key);

                // Emit stopping event
                emit_plug_status(
                    event_sink,
                    plug_key.to_string(),
                    plug_key.to_string(),
                    PlugScope::Slot,
                    Some(slot_id.clone()),
                    PlugStage::Teardown,
                    PlugStatusValue::Destructing,
                );

                // Stop the plug service
                if let Err(e) = self
                    .plug_service_manager
                    .stop_plug_service(&instance_key)
                    .await
                {
                    log::warn!(
                        "Failed to stop plug service {}: {}",
                        instance_key, e
                    );
                }

                log::info!("Destroyed slot-level plug {}", instance_key);

                // Emit inactive event
                emit_plug_status(
                    event_sink,
                    plug_key.to_string(),
                    plug_key.to_string(),
                    PlugScope::Slot,
                    Some(slot_id.clone()),
                    PlugStage::Teardown,
                    PlugStatusValue::Idle,
                );
            }
        }

        Ok(())
    }

    /// Destroy all-scope plugs at procedure end
    pub async fn destroy_all_scope_plugs(
        &self,
        event_sink: &Arc<dyn EventSink>,
    ) -> Result<(), String> {
        let scopes = self.plug_scopes.read().await;
        let mut instances = self.plug_instances.write().await;

        let keys_to_remove: Vec<String> = instances
            .keys()
            .filter(|key| {
                let scope = scopes.get(*key).cloned().unwrap_or(PlugScope::Slot);
                matches!(scope, PlugScope::Execution)
            })
            .cloned()
            .collect();

        for instance_key in keys_to_remove {
            if let Some(_instance) = instances.remove(&instance_key) {
                // For all-scope plugs, instance_key == plug_key (no suffix).
                let plug_key = instance_key.clone();

                // Emit stopping event
                emit_plug_status(
                    event_sink,
                    plug_key.clone(),
                    plug_key.clone(),
                    PlugScope::Execution,
                    None,
                    PlugStage::Teardown,
                    PlugStatusValue::Destructing,
                );

                // Stop the plug service
                if let Err(e) = self
                    .plug_service_manager
                    .stop_plug_service(&instance_key)
                    .await
                {
                    log::warn!(
                        "Failed to stop plug service {}: {}",
                        instance_key, e
                    );
                }

                log::info!("Destroyed all-slots plug {}", instance_key);

                // Emit inactive event
                emit_plug_status(
                    event_sink,
                    plug_key.clone(),
                    plug_key.clone(),
                    PlugScope::Execution,
                    None,
                    PlugStage::Teardown,
                    PlugStatusValue::Idle,
                );
            }
        }

        Ok(())
    }

    /// Get access to the plug service manager
    pub fn get_plug_service_manager(&self) -> &Arc<PlugServiceManager> {
        &self.plug_service_manager
    }

    /// Start a manual plug (from UI debug buttons)
    pub async fn start_manual_plug(
        &self,
        plug_name: String,
        plug_config: serde_json::Value,
        event_sink: &Arc<dyn EventSink>,
    ) -> Result<u16, String> {
        let mut instances = self.plug_instances.write().await;
        let mut manual_plugs = self.manual_plugs.write().await;

        // Check if plug is already managed by orchestrator
        if instances.contains_key(&plug_name) && !manual_plugs.contains(&plug_name) {
            return Err(format!(
                "Plug '{}' is currently managed by a running procedure. Stop the procedure first.",
                plug_name
            ));
        }

        // Check if already manually started
        if manual_plugs.contains(&plug_name) {
            return Err(format!("Plug '{}' is already running manually", plug_name));
        }

        // Emit initializing event
        emit_plug_status(
            event_sink,
            plug_name.clone(),
            plug_name.clone(),
            PlugScope::Execution,
            None,
            PlugStage::Manual,
            PlugStatusValue::Initializing,
        );

        // Start the plug service using the same service manager.
        // For manual plugs, use the plug name as display name (user hasn't set a custom name).
        // `slot_id = None` here preserves the prior behavior: the
        // suffix-strip resolution this replaced returned `None` for
        // manual plugs (instance_key == plug_key, no `_<slot>`
        // suffix), so PlugLogEvent shape stays identical for
        // downstream consumers. The `PlugInstance.slot_id = "manual"`
        // tag below is internal bookkeeping in the manager's instance
        // map, not part of the wire event.
        let port = self
            .plug_service_manager
            .start_plug_service(
                plug_name.clone(),
                plug_name.clone(),
                plug_name.clone(),
                plug_config,
                None,
                event_sink,
            )
            .await?;

        // Track in the same instances map
        instances.insert(
            plug_name.clone(),
            PlugInstance {
                port,
                slot_id: Some("manual".to_string()), // Mark as manual
                ref_count: 0,                        // Manual plugs don't use ref counting
            },
        );

        // Mark as manually started
        manual_plugs.insert(plug_name.clone());

        log::debug!(
            "Started manual plug '{}' on port {}",
            plug_name, port
        );

        // Emit ready event
        emit_plug_status(
            event_sink,
            plug_name.clone(),
            plug_name.clone(),
            PlugScope::Execution,
            None,
            PlugStage::Manual,
            PlugStatusValue::Active,
        );

        Ok(port)
    }

    /// Stop a manual plug
    pub async fn stop_manual_plug(
        &self,
        plug_name: &str,
        event_sink: &Arc<dyn EventSink>,
    ) -> Result<(), String> {
        let mut instances = self.plug_instances.write().await;
        let mut manual_plugs = self.manual_plugs.write().await;

        // Check if this is actually a manual plug
        if !manual_plugs.contains(plug_name) {
            return Err(format!(
                "Plug '{}' is not a manually-started plug",
                plug_name
            ));
        }

        // Emit stopping event
        emit_plug_status(
            event_sink,
            plug_name.to_string(),
            plug_name.to_string(),
            PlugScope::Execution,
            None,
            PlugStage::Manual,
            PlugStatusValue::Destructing,
        );

        // Remove from instances
        if let Some(_instance) = instances.remove(plug_name) {
            // Stop the plug service
            self.plug_service_manager
                .stop_plug_service(plug_name)
                .await?;

            log::debug!("Stopped manual plug '{}'", plug_name);
        }

        // Remove from manual tracking
        manual_plugs.remove(plug_name);

        // Emit inactive event
        emit_plug_status(
            event_sink,
            plug_name.to_string(),
            plug_name.to_string(),
            PlugScope::Execution,
            None,
            PlugStage::Manual,
            PlugStatusValue::Idle,
        );

        Ok(())
    }

    /// Clean up all manual plugs (call on orchestrator start)
    pub async fn teardown_manual_plugs(
        &self,
        event_sink: &Arc<dyn EventSink>,
    ) -> Result<(), String> {
        let manual_plugs: Vec<String> = {
            let plugs = self.manual_plugs.read().await;
            plugs.iter().cloned().collect()
        };

        for plug_name in manual_plugs {
            log::warn!(
                "Cleaning up manually-started plug '{}' before procedure run",
                plug_name
            );
            let _ = self.stop_manual_plug(&plug_name, event_sink).await;
        }

        Ok(())
    }

    /// Force destroy all plugs (both each-scope and all-scope) without teardown
    /// Used during force kill operations
    pub async fn force_destroy_all_plugs(
        &self,
        event_sink: &Arc<dyn EventSink>,
    ) -> Result<(), String> {
        let scopes = self.plug_scopes.read().await;
        let mut instances = self.plug_instances.write().await;

        let all_keys: Vec<String> = instances.keys().cloned().collect();

        log::info!("Force destroying {} plug instances", all_keys.len());

        for instance_key in all_keys {
            // Station plugs are host-owned: their process is not in this
            // manager, must survive a run kill, and emitting a teardown
            // event for a plug that stays alive would lie to the
            // operator UI. Keep the instance entry too — the plug is
            // still live, so any phase dispatched after the force-kill
            // can still resolve its port. (Station instance keys are
            // always the bare plug key.)
            if matches!(scopes.get(&instance_key), Some(PlugScope::Station)) {
                continue;
            }

            if let Some(instance) = instances.remove(&instance_key) {
                let (plug_name, scope, slot_id) = if let Some(ref slot) = instance.slot_id {
                    let plug_name = instance_key
                        .strip_suffix(&format!("_{}", slot))
                        .unwrap_or(&instance_key);
                    let scope = scopes.get(plug_name).cloned().unwrap_or(PlugScope::Slot);
                    (plug_name, scope, Some(slot.clone()))
                } else {
                    let scope = scopes.get(&instance_key).cloned().unwrap_or(PlugScope::Execution);
                    (instance_key.as_str(), scope, None)
                };

                if let Err(e) = self
                    .plug_service_manager
                    .force_kill_plug_service(&instance_key)
                    .await
                {
                    log::warn!(
                        "Failed to force kill plug service {}: {}",
                        instance_key, e
                    );
                }

                log::info!("Force destroyed plug {}", instance_key);

                emit_plug_status(
                    event_sink,
                    instance_key.clone(),
                    plug_name.to_string(),
                    scope,
                    slot_id,
                    PlugStage::Teardown,
                    PlugStatusValue::Skipped,
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NullSink;

    fn sink() -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }

    async fn manager_with_station_plug() -> ResourceManager {
        let rm = ResourceManager::new(std::env::temp_dir());
        rm.set_plug_scopes(HashMap::from([("psu".to_string(), PlugScope::Station)]))
            .await;
        rm.register_station_plug("psu".to_string(), 45001).await;
        rm
    }

    #[tokio::test]
    async fn station_plug_resolves_by_bare_key_across_slots() {
        let rm = manager_with_station_plug().await;
        let job_a = Uuid::new_v4();
        let job_b = Uuid::new_v4();
        rm.allocate_resources(job_a, &[]).await.unwrap();
        rm.allocate_resources(job_b, &[]).await.unwrap();

        let configs = HashMap::from([("psu".to_string(), serde_json::json!({}))]);

        // Two phases in different slots share the single station instance.
        let ports_a = rm
            .start_plug_services_for_slot(job_a, &configs, Some("slot_1".into()))
            .await
            .unwrap();
        let ports_b = rm
            .start_plug_services_for_slot(job_b, &configs, Some("slot_2".into()))
            .await
            .unwrap();
        assert_eq!(ports_a.get("psu"), Some(&45001));
        assert_eq!(ports_b.get("psu"), Some(&45001));
    }

    #[tokio::test]
    async fn station_plug_survives_run_owned_teardown() {
        let rm = manager_with_station_plug().await;

        // Neither run-owned bucket claims the station instance...
        assert!(!rm.has_all_scope_plugs().await);
        assert!(!rm.has_each_scope_plugs("slot_1").await);

        // ...and neither destroy path touches it.
        rm.destroy_each_scope_plugs("slot_1".to_string(), &sink())
            .await
            .unwrap();
        rm.destroy_all_scope_plugs(&sink()).await.unwrap();

        let job = Uuid::new_v4();
        rm.allocate_resources(job, &[]).await.unwrap();
        let configs = HashMap::from([("psu".to_string(), serde_json::json!({}))]);
        let ports = rm
            .start_plug_services_for_slot(job, &configs, Some("slot_1".into()))
            .await
            .expect("station instance must still be allocatable after run teardown");
        assert_eq!(ports.get("psu"), Some(&45001));
    }

    #[tokio::test]
    async fn force_destroy_spares_station_plugs_and_kills_run_plugs() {
        let rm = ResourceManager::new(std::env::temp_dir());
        rm.set_plug_scopes(HashMap::from([
            ("psu".to_string(), PlugScope::Station),
            ("dmm".to_string(), PlugScope::Execution),
        ]))
        .await;
        rm.register_station_plug("psu".to_string(), 45001).await;
        // Fake an execution-scope instance directly (no live process needed for
        // map semantics; force_kill on the absent service just warns).
        rm.plug_instances.write().await.insert(
            "dmm".to_string(),
            PlugInstance {
                port: 45002,
                slot_id: None,
                ref_count: 0,
            },
        );

        rm.force_destroy_all_plugs(&sink()).await.unwrap();

        // The run plug is evicted; the station plug must still resolve —
        // its process belongs to the station host and survives run kill.
        let job = Uuid::new_v4();
        rm.allocate_resources(job, &[]).await.unwrap();
        let station_cfg = HashMap::from([("psu".to_string(), serde_json::json!({}))]);
        let ports = rm
            .start_plug_services_for_slot(job, &station_cfg, None)
            .await
            .expect("station plug must survive force destroy");
        assert_eq!(ports.get("psu"), Some(&45001));

        let job2 = Uuid::new_v4();
        rm.allocate_resources(job2, &[]).await.unwrap();
        let run_cfg = HashMap::from([("dmm".to_string(), serde_json::json!({}))]);
        assert!(
            rm.start_plug_services_for_slot(job2, &run_cfg, None)
                .await
                .is_err(),
            "run plug must be gone after force destroy"
        );
    }

    #[tokio::test]
    async fn missing_plug_instance_errors() {
        let rm = ResourceManager::new(std::env::temp_dir());
        rm.set_plug_scopes(HashMap::from([("psu".to_string(), PlugScope::Execution)]))
            .await;
        let job = Uuid::new_v4();
        rm.allocate_resources(job, &[]).await.unwrap();
        let configs = HashMap::from([("psu".to_string(), serde_json::json!({}))]);
        assert!(rm
            .start_plug_services_for_slot(job, &configs, None)
            .await
            .is_err());
    }
}
