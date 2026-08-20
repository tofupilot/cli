//! Plug scope management

use std::collections::HashMap;


use crate::procedure::schema::{ProcedureDefinition, StageScope};


use crate::job::Job;

use super::Orchestrator;
impl Orchestrator {
    /// Ensure plugs are created at the appropriate scope boundaries
    ///
    /// Lifecycle boundaries:
    /// - Station (scope: station, host present): Borrowed from the
    ///   station plug host once per execution, before the first phase
    ///   that needs them. Never destroyed by the run.
    /// - Execution-wide (scope: execution): Created once before the first phase that needs them
    /// - Per-slot (scope: slot): Created per-slot before the first phase that needs them for that slot
    pub(super) async fn ensure_plugs_created_for_job(
        &self,
        job: &Job,
    ) -> Result<(), String> {
        let procedure_def = &self.procedure_definition;

        // Borrow station plugs from the host if not yet borrowed this
        // execution. Without a host, station plugs were downgraded to
        // execution scope at init and flow through the branch below instead.
        if let Some(host) = &self.station_plug_host {
            let needs_station_plugs = matches!(job.stage_scope, StageScope::SetupAll)
                || procedure_def
                    .plugs
                    .iter()
                    .any(|p| p.scope_is_station() && job.required_plugs.contains(&p.key));

            if needs_station_plugs {
                let mut acquired = self.station_plugs_acquired.write().await;
                if !*acquired {
                    // Narrowed to the run's required set like the two
                    // branches below: on a partial run only station plugs
                    // in the introspected union are borrowed, so the host
                    // never spins up instruments the selected phases can't
                    // touch (and never emits plug_status for plugs absent
                    // from the execution plan).
                    let station_plugs: Vec<_> = procedure_def
                        .plugs
                        .iter()
                        .filter(|p| p.scope_is_station() && job.required_plugs.contains(&p.key))
                        .collect();

                    if !station_plugs.is_empty() {
                        log::info!(
                            "Borrowing {} station plug(s) before phase '{}'",
                            station_plugs.len(),
                            job.phase_name
                        );
                        self.emit_plug_scope_event("running").await;

                        // Acquire first, register under the resource-
                        // manager guard second, emit last. Keeping the
                        // rm guard out of scope during `acquire` (slow:
                        // may spawn Python) and during the emit (which
                        // takes `state.write()`) preserves the global
                        // `state → resource_manager` lock order that
                        // scheduling.rs documents.
                        let mut acquired_ports = Vec::with_capacity(station_plugs.len());
                        for plug in station_plugs {
                            let config_json =
                                plug.to_config_json(&self.procedure_dir).map_err(|e| {
                                    format!("Failed to build config for plug '{}': {}", plug.key, e)
                                })?;

                            match host
                                .acquire(
                                    &self.procedure_dir,
                                    &self.python_path,
                                    &plug.key,
                                    &plug.name,
                                    config_json,
                                    &self.event_sink,
                                )
                                .await
                            {
                                Ok(port) => {
                                    acquired_ports.push((
                                        plug.key.clone(),
                                        plug.name.clone(),
                                        port,
                                    ))
                                }
                                Err(e) => {
                                    self.emit_plug_scope_event("error").await;
                                    return Err(format!(
                                        "Failed to acquire station plug '{}': {}",
                                        plug.key, e
                                    ));
                                }
                            }
                        }

                        {
                            let resource_manager = self.resource_manager.write().await;
                            for (key, name, port) in acquired_ports {
                                resource_manager
                                    .register_station_plug(key, name, port)
                                    .await;
                            }
                        }

                        self.emit_plug_scope_event("pass").await;

                        // Latch only after a real acquisition. The filtered
                        // set is per-run constant (`required_plugs` carries
                        // the same union on every job), so an empty set
                        // stays empty all run and re-entering here is a
                        // cheap no-op — while latching on empty would skip
                        // acquisition outright if that invariant ever broke.
                        *acquired = true;
                    }
                }
            }
        }

        // Create execution-wide plugs if not yet created
        // Triggered by: SetupAll phase, or any phase that requires a
        // scope:execution plug — including station plugs downgraded to execution
        // scope when no host is present (the ResourceManager's scope map
        // holds the downgraded value, so create_procedure_plugs will
        // pick them up here).
        //
        // Only the plugs the job's run actually requires are started:
        // `required_plugs` is every declared plug on a full run and the
        // introspected union on a partial run, so this is what keeps a
        // partial run from starting instruments its phases never touch.
        let hostless = self.station_plug_host.is_none();
        let wanted_procedure_keys: Vec<String> = procedure_def
            .plugs
            .iter()
            .filter(|p| {
                (p.scope_is_execution() || (hostless && p.scope_is_station()))
                    && job.required_plugs.contains(&p.key)
            })
            .map(|p| p.key.clone())
            .collect();

        let needs_procedure_plugs = matches!(job.stage_scope, StageScope::SetupAll)
            || !wanted_procedure_keys.is_empty();

        if needs_procedure_plugs {
            let mut created = self.procedure_plugs_created.write().await;
            // `None` until the first job passes this gate: the scope-batch
            // progress event pair fires exactly once per run, even when the
            // first batch has nothing to create (submit_procedure reserved
            // exactly one pair for it).
            let first_batch = created.is_none();
            let created_keys = created.get_or_insert_with(std::collections::HashSet::new);
            let missing: Vec<String> = wanted_procedure_keys
                .iter()
                .filter(|k| !created_keys.contains(*k))
                .cloned()
                .collect();

            if first_batch || !missing.is_empty() {
                log::info!("Creating all-slots plugs before phase '{}'", job.phase_name);

                let resource_manager = self.resource_manager.write().await;

                if first_batch {
                    // Clean up any manually-started plugs to prevent conflicts
                    let teardown_result =
                        resource_manager.teardown_manual_plugs(&self.event_sink).await;

                    if let Err(e) = teardown_result {
                        log::warn!("Warning during manual plug teardown: {}", e);
                        // Continue anyway - not fatal
                    }

                    self.emit_plug_scope_event("running").await;
                }

                let plug_configs: HashMap<String, serde_json::Value> = self
                    .get_all_plug_configs(procedure_def)
                    .into_iter()
                    .filter(|(key, _)| missing.contains(key))
                    .collect();
                let plug_display_names = self.get_plug_display_names(procedure_def);
                let plug_result = resource_manager
                    .create_procedure_plugs(&plug_configs, &plug_display_names, &self.event_sink)
                    .await;

                match plug_result {
                    Ok(_) => {
                        log::info!("Successfully created all-slots plugs");
                        if first_batch {
                            self.emit_plug_scope_event("pass").await;
                        }
                        created_keys.extend(missing);
                    }
                    Err(e) => {
                        let error_msg = format!("Failed to create all-slots plugs: {}", e);
                        if first_batch {
                            self.emit_plug_scope_event("error").await;
                        }
                        return Err(error_msg);
                    }
                }
            }
        }

        // Create each-slot plugs if not yet created for this slot
        // Triggered by: SetupEach phase, or any slot-scoped phase that
        // requires a scope:each plug. Narrowed to the job's required set
        // the same way as the execution-scope branch above; tracking is
        // genuinely per-slot (a multi-slot procedure with no `unit:`
        // block runs on all its slots).
        let wanted_slot_keys: Vec<String> = procedure_def
            .plugs
            .iter()
            .filter(|p| {
                matches!(p.scope, crate::procedure::schema::Scope::Slot)
                    && job.required_plugs.contains(&p.key)
            })
            .map(|p| p.key.clone())
            .collect();

        let needs_slot_plugs = job.slot_id.is_some()
            && (matches!(job.stage_scope, StageScope::SetupEach) || !wanted_slot_keys.is_empty());

        if needs_slot_plugs {
            if let Some(ref slot_id) = job.slot_id {
                let mut created_slots = self.slot_plugs_created.write().await;
                // Entry presence marks the slot's batch event pair as
                // fired, mirroring the execution-scope gate above.
                let first_batch = !created_slots.contains_key(slot_id);
                let created_keys = created_slots.entry(slot_id.clone()).or_default();
                let missing: Vec<String> = wanted_slot_keys
                    .iter()
                    .filter(|k| !created_keys.contains(*k))
                    .cloned()
                    .collect();

                if first_batch || !missing.is_empty() {
                    log::info!(
                        "Creating each-slot plugs for {} before phase '{}'",
                        slot_id,
                        job.phase_name
                    );

                    if first_batch {
                        self.emit_plug_scope_event("running").await;
                    }

                    let resource_manager = self.resource_manager.write().await;
                    let plug_configs: HashMap<String, serde_json::Value> = self
                        .get_all_plug_configs(procedure_def)
                        .into_iter()
                        .filter(|(key, _)| missing.contains(key))
                        .collect();
                    let plug_display_names = self.get_plug_display_names(procedure_def);
                    let plug_result = resource_manager
                        .create_slot_plugs(slot_id.clone(), &plug_configs, &plug_display_names, &self.event_sink)
                        .await;

                    match plug_result {
                        Ok(_) => {
                            log::info!(
                                "Successfully created each-slot plugs for {}",
                                slot_id
                            );
                            if first_batch {
                                self.emit_plug_scope_event("pass").await;
                            }
                            created_keys.extend(missing);
                        }
                        Err(e) => {
                            let error_msg =
                                format!("Failed to create each-slot plugs for {}: {}", slot_id, e);
                            if first_batch {
                                self.emit_plug_scope_event("error").await;
                            }
                            return Err(error_msg);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Get all plug configurations from the procedure definition
    pub(super) fn get_all_plug_configs(
        &self,
        procedure: &ProcedureDefinition,
    ) -> HashMap<String, serde_json::Value> {
        let mut configs = HashMap::new();
        for def in &procedure.plugs {
            match def.to_config_json(&self.procedure_dir) {
                Ok(config) => {
                    configs.insert(def.key.clone(), config);
                }
                Err(e) => {
                    log::error!("Failed to get config for plug '{}': {}", def.key, e);
                }
            }
        }
        configs
    }

    /// Get all plug display names from the procedure definition
    pub(super) fn get_plug_display_names(
        &self,
        procedure: &ProcedureDefinition,
    ) -> HashMap<String, String> {
        let mut names = HashMap::new();
        for def in &procedure.plugs {
            names.insert(def.key.clone(), def.name.clone());
        }
        names
    }

    /// Get plug configurations for a specific job from the stored procedure definition
    pub(super) fn get_plug_configs_for_job(&self, job: &Job) -> HashMap<String, serde_json::Value> {
        let mut plug_configs = HashMap::new();

        for plug_key in &job.required_plugs {
            let plug_def = self.procedure_definition.plugs.iter().find(|p| &p.key == plug_key);

            if let Some(plug_def) = plug_def {
                match plug_def.to_config_json(&self.procedure_dir) {
                    Ok(config) => {
                        plug_configs.insert(plug_key.clone(), config);
                    }
                    Err(e) => {
                        log::error!("Failed to get config for plug '{}': {}", plug_key, e);
                    }
                }
            } else {
                log::warn!(
                    "WARNING: Warning: Plug '{}' required by job '{}' not found in procedure definition",
                    plug_key, job.phase_name
                );
            }
        }

        plug_configs
    }
}
