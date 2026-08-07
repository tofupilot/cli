//! Orchestrator initialization and job graph creation
//!
//! This module handles:
//! - Orchestrator initialization
//! - Procedure submission and job graph creation
//! - Job dependency resolution

use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::constants::limits;
use crate::events::PlugScope;
use crate::job::Job;

use super::jobs;
use super::{ExecutionStrategy, Orchestrator};

impl Orchestrator {
    pub async fn initialize(&mut self) -> Result<(), String> {
        // Use the orchestrator's pre-resolved Python path when available
        // (CLI runs always set it). Fall back to the engine's walk-up
        // resolver for legacy callers that don't pass one in.
        let python_cmd =
            crate::python::resolve_or_walk(&self.python_path, &self.procedure_dir).await?;

        // Start all workers in parallel with the resolved python path
        let mut workers = self.workers.write().await;
        let start_futures: Vec<_> = workers
            .iter_mut()
            .map(|worker| worker.start_with_python(&self.event_sink, &python_cmd))
            .collect();

        let results = futures::future::join_all(start_futures).await;

        // Check for any errors
        for result in results {
            result?;
        }

        Ok(())
    }

    pub async fn submit_procedure(
        &mut self,
        slots: Vec<String>,
        execution_strategy: ExecutionStrategy,
        initial_unit_infos: std::collections::HashMap<String, crate::unit::UnitInfo>,
        only_phase: Option<&str>,
    ) -> Result<(), String> {
        // Partial run: resolve the target's transitive dependency closure
        // once, before any state is touched. Setup and teardown phases
        // always run; only the main stage is filtered by this set.
        let partial_main_set = only_phase
            .map(|target| partial_main_phase_set(&self.procedure_definition, target))
            .transpose()?;

        // Partial run: introspect phase signatures once (one extra Python
        // process start) and narrow the run's plug set to the union of
        // what the executing phases can touch. Full runs keep today's
        // start-everything behaviour — narrowing them would change
        // scheduling for every station in the field.
        let partial_required_plugs: Option<Vec<String>> = match &partial_main_set {
            Some(set) => Some(
                match crate::procedure::introspect_procedure(
                    &self.procedure_dir,
                    &self.python_path,
                    &self.procedure_definition,
                )
                .await
                {
                    Ok(introspection) => {
                        union_required_plugs(&self.procedure_definition, set, &introspection)
                    }
                    Err(e) => {
                        log::warn!(
                            "Phase introspection failed ({}); starting all plugs as a safe fallback",
                            e
                        );
                        self.procedure_definition
                            .plugs
                            .iter()
                            .map(|p| p.key.clone())
                            .collect()
                    }
                },
            ),
            None => None,
        };

        // Store procedure definition

        // Store initial unit infos FIRST before anything else uses it
        self.initial_unit_infos = initial_unit_infos.clone();

        // Extract plug scopes and pass to ResourceManager
        {
            let mut scopes = HashMap::new();
            for plug_def in &self.procedure_definition.plugs {
                let scope = match plug_def.scope {
                    crate::procedure::schema::Scope::Execution => PlugScope::Execution,
                    crate::procedure::schema::Scope::Slot => PlugScope::Slot,
                    crate::procedure::schema::Scope::Station => {
                        if self.station_plug_host.is_some() {
                            PlugScope::Station
                        } else {
                            // No station context to own the instance (one-shot
                            // run, Studio): degrade to execution scope so the plug
                            // still gets a single shared instance for this
                            // execution and is torn down with it.
                            log::info!(
                                "Plug '{}' has scope: station but no station \
                                 context is present; behaving as execution-scoped \
                                 for this execution",
                                plug_def.key
                            );
                            PlugScope::Execution
                        }
                    }
                };
                scopes.insert(plug_def.key.clone(), scope);
            }
            let resource_manager = self.resource_manager.write().await;
            resource_manager.set_plug_scopes(scopes).await;

            // NOTE: All-scope plugs will be created before first SetupAll phase runs
        }

        let mut state = self.state.write().await;

        // Set should_stop_on_first_failure flag from procedure configuration
        state.should_stop_on_first_failure = self.procedure_definition
            .execution
            .as_ref()
            .map(|e| matches!(e.on_first_failure, crate::procedure::schema::FirstFailureAction::Stop))
            .unwrap_or(true);
        if state.should_stop_on_first_failure {
            log::info!("on_first_failure is set to STOP - test will stop on first phase failure");
        }

        // Validate configuration consistency and emit warnings
        if let Some(exec_config) = &self.procedure_definition.execution {
            let all_phases = self.procedure_definition.get_all_phases_with_stage_scope();
            let phase_defs: Vec<_> = all_phases.iter().map(|(_, phase)| *phase).collect();
            let warnings = exec_config.validate_consistency(&phase_defs);
            for warning in warnings {
                log::warn!("Configuration warning: {}", warning);
            }
        }

        // Initialize display based on CLI mode preferences
        {
            let _total_phases = self.procedure_definition
                .get_all_phases_with_stage_scope()
                .into_iter()
                .filter(|(_, phase)| !phase.should_skip())
                .count();
        }

        // Check queue size limit using total phase count
        let total_phases = self.procedure_definition.total_phase_count();
        if state.job_queue.len() + (slots.len() * total_phases) > limits::MAX_JOB_QUEUE_SIZE {
            return Err(format!(
                "Job queue size limit exceeded ({})",
                limits::MAX_JOB_QUEUE_SIZE
            ));
        }

        // Create global job mapping for all slots/phases
        let mut global_job_map: HashMap<String, Uuid> = HashMap::new();
        let mut all_jobs = Vec::new();

        // Track setup_procedure job IDs for implicit dependencies
        let mut setup_procedure_job_ids: HashSet<Uuid> = HashSet::new();
        // Track setup_slot job IDs per slot for implicit dependencies
        let mut setup_slot_job_ids: HashMap<String, HashSet<Uuid>> = HashMap::new();
        // Track main phase job IDs per slot for implicit dependencies
        let mut main_phase_job_ids: HashMap<String, HashSet<Uuid>> = HashMap::new();
        // Track each-slot teardown job IDs per slot for implicit dependencies
        let mut teardown_slot_job_ids: HashMap<String, HashSet<Uuid>> = HashMap::new();
        // Track ALL each-slot teardown job IDs across all slots for all-slots teardown dependencies
        let mut all_teardown_slot_job_ids: HashSet<Uuid> = HashSet::new();

        // First pass: create all jobs for all stage/scope combinations and store their IDs for dependency resolution

        // Cache the phase list to avoid re-iteration
        let all_phases_with_stage = self.procedure_definition.get_all_phases_with_stage_scope();

        // Create all-slots phases once (shared across all slots)
        // No partial-run filter here: this loop only creates SetupAll /
        // TeardownAll jobs, and setup/teardown phases always run.
        for &(stage_scope, phase) in all_phases_with_stage.iter() {
            if phase.should_skip() {
                continue;
            }

            match stage_scope {
                StageScope::SetupAll | StageScope::TeardownAll => {
                    // Build dependencies including implicit ones
                    let dependencies = phase.depends_on.clone();

                    // All-slots teardown must wait for all each-slot teardown phases
                    // (will be updated in second pass after we create each-slot teardown jobs)

                    // Create all-slots phases with no slot (shared)
                    let job = jobs::create_job_for_phase(
                        phase,
                        None, // No slot = shared across all slots
                        stage_scope,
                        dependencies,
                        &global_job_map,
                        &self.procedure_dir,
                        &self.procedure_definition,
                        partial_required_plugs.as_deref(),
                    );

                    // Store mapping for dependency resolution (use key for matching)
                    let key = format!("SHARED:{}", phase.key);
                    global_job_map.insert(key, job.id);

                    // Track setup_procedure jobs
                    if matches!(stage_scope, StageScope::SetupAll) {
                        setup_procedure_job_ids.insert(job.id);
                    }

                    all_jobs.push(job);
                }
                _ => {
                    // Skip slot-level phases in this first loop - we'll handle them per-slot below
                }
            }
        }

        // Create slot-level phases for each slot
        for slot_id in &slots {
            for &(stage_scope, phase) in all_phases_with_stage.iter() {
                if phase.should_skip() {
                    continue;
                }

                // Partial run: main phases outside the target's dependency
                // closure get no job at all. The implicit stage wiring
                // below operates on whatever jobs exist and stays correct.
                if let Some(set) = &partial_main_set {
                    if matches!(stage_scope, StageScope::Main) && !set.contains(&phase.key) {
                        continue;
                    }
                }

                match stage_scope {
                    StageScope::SetupEach | StageScope::Main | StageScope::TeardownEach => {
                        // Create slot-specific phases (implicit dependencies added later)
                        let mut job = jobs::create_job_for_phase(
                            phase,
                            Some(slot_id.clone()),
                            stage_scope,
                            phase.depends_on.clone(),
                            &global_job_map,
                            &self.procedure_dir,
                            &self.procedure_definition,
                            partial_required_plugs.as_deref(),
                        );

                        // Add implicit dependencies based on stage/scope
                        match stage_scope {
                            StageScope::SetupEach => {
                                // Each-slot setup phases must wait for ALL all-slots setup phases
                                job.depends_on
                                    .extend(setup_procedure_job_ids.iter().copied());
                            }
                            StageScope::Main => {
                                // Main phases must wait for:
                                // 1. ALL all-slots setup phases
                                job.depends_on
                                    .extend(setup_procedure_job_ids.iter().copied());
                                // 2. Their slot's each-slot setup phases (will be added after we create them)
                            }
                            StageScope::TeardownEach => {
                                // Each-slot teardown phases must wait for ALL all-slots setup phases
                                // (Main phase dependencies will ensure proper ordering)
                                job.depends_on
                                    .extend(setup_procedure_job_ids.iter().copied());
                            }
                            _ => {}
                        }

                        // Store mapping for dependency resolution (use key for matching)
                        let key = format!("{}:{}", slot_id, phase.key);
                        global_job_map.insert(key, job.id);

                        // Track jobs by type for dependency management
                        match stage_scope {
                            StageScope::SetupEach => {
                                setup_slot_job_ids
                                    .entry(slot_id.clone())
                                    .or_default()
                                    .insert(job.id);
                            }
                            StageScope::Main => {
                                main_phase_job_ids
                                    .entry(slot_id.clone())
                                    .or_default()
                                    .insert(job.id);
                            }
                            StageScope::TeardownEach => {
                                teardown_slot_job_ids
                                    .entry(slot_id.clone())
                                    .or_default()
                                    .insert(job.id);
                                all_teardown_slot_job_ids.insert(job.id);
                            }
                            _ => {}
                        }

                        all_jobs.push(job);
                    }
                    _ => {
                        // Skip all-slots phases - already created above
                    }
                }
            }
        }

        // Second pass: Update phase dependencies to include implicit cross-phase dependencies
        for job in &mut all_jobs {
            match job.stage_scope {
                StageScope::SetupEach => {
                    // Each-slot setup phases must wait for ALL all-slots setup phases to complete
                    job.depends_on
                        .extend(setup_procedure_job_ids.iter().copied());
                }
                StageScope::Main => {
                    // Main phases need their slot's each-slot setup phases as dependencies
                    if let Some(slot_id) = &job.slot_id {
                        if let Some(setup_jobs) = setup_slot_job_ids.get(slot_id) {
                            job.depends_on.extend(setup_jobs.iter().copied());
                        }
                    }
                }
                StageScope::TeardownEach => {
                    // Each-slot teardown phases need their slot's Main phases as dependencies
                    if let Some(slot_id) = &job.slot_id {
                        if let Some(main_jobs) = main_phase_job_ids.get(slot_id) {
                            job.depends_on.extend(main_jobs.iter().copied());
                        }
                    }
                }
                StageScope::TeardownAll => {
                    // All-slots teardown phases must wait for ALL Main phases AND all TeardownEach phases
                    // This ensures teardown runs after all main work is complete, even if no TeardownEach phases exist
                    for main_jobs in main_phase_job_ids.values() {
                        job.depends_on.extend(main_jobs.iter().copied());
                    }
                    job.depends_on
                        .extend(all_teardown_slot_job_ids.iter().copied());
                }
                _ => {}
            }
        }

        // Third pass: enqueue jobs in proper execution order based on stage/scope combinations
        use crate::procedure::schema::StageScope;

        match execution_strategy {
            ExecutionStrategy::SlotFirst => {
                // Slot-first: complete all phases for each slot before moving to next
                log::info!("Using SLOT-FIRST execution model");

                // Setup procedure phases (run once for all slots)
                for job in &all_jobs {
                    if matches!(job.stage_scope, StageScope::SetupAll) && job.is_shared() {
                        state.enqueue_job(job.clone());
                    }
                }

                // Store slot jobs for deferred queueing
                let mut slot_jobs: Vec<(String, Vec<Job>)> = Vec::new();

                // Group jobs by slot
                for slot_id in &slots {
                    let mut current_slot_jobs = Vec::new();

                    // Collect all jobs for this slot in execution order
                    current_slot_jobs.extend(jobs::filter_jobs_by_slot_and_type(
                        &all_jobs,
                        slot_id,
                        StageScope::SetupEach,
                    ));
                    current_slot_jobs.extend(jobs::filter_jobs_by_slot_and_type(
                        &all_jobs,
                        slot_id,
                        StageScope::Main,
                    ));
                    current_slot_jobs.extend(jobs::filter_jobs_by_slot_and_type(
                        &all_jobs,
                        slot_id,
                        StageScope::TeardownEach,
                    ));

                    if !current_slot_jobs.is_empty() {
                        slot_jobs.push((slot_id.clone(), current_slot_jobs));
                    }
                }

                // Store slot jobs for deferred execution
                // Only the first slot's jobs are enqueued initially
                if let Some((first_slot_id, first_slot_jobs)) = slot_jobs.first() {
                    log::trace!("📦 Starting with slot: {}", first_slot_id);
                    for job in first_slot_jobs {
                        state.enqueue_job(job.clone());
                    }
                }

                // Store remaining slots for later
                if slot_jobs.len() > 1 {
                    state.pending_slot_jobs = slot_jobs.into_iter().skip(1).collect();
                    log::info!(
                        "{} slots queued for sequential processing",
                        state.pending_slot_jobs.len()
                    );
                }

                // Teardown procedure phases will be enqueued after all slots complete
                let mut teardown_procedure_jobs = Vec::new();
                for job in &all_jobs {
                    if matches!(job.stage_scope, StageScope::TeardownAll) && job.is_shared() {
                        teardown_procedure_jobs.push(job.clone());
                    }
                }
                state.teardown_procedure_jobs = teardown_procedure_jobs;
            }
            ExecutionStrategy::PhaseFirst => {
                // Phase-first: run same phase across all slots before moving to next phase
                jobs::enqueue_jobs_by_stage_scope(
                    &mut state,
                    &self.procedure_definition,
                    &all_jobs,
                    StageScope::SetupAll,
                    true,
                );
                jobs::enqueue_jobs_by_stage_scope(
                    &mut state,
                    &self.procedure_definition,
                    &all_jobs,
                    StageScope::SetupEach,
                    false,
                );
                jobs::enqueue_jobs_by_stage_scope(
                    &mut state,
                    &self.procedure_definition,
                    &all_jobs,
                    StageScope::Main,
                    false,
                );
                jobs::enqueue_jobs_by_stage_scope(
                    &mut state,
                    &self.procedure_definition,
                    &all_jobs,
                    StageScope::TeardownEach,
                    false,
                );
                jobs::enqueue_jobs_by_stage_scope(
                    &mut state,
                    &self.procedure_definition,
                    &all_jobs,
                    StageScope::TeardownAll,
                    true,
                );
            }
        }

        // Add plug scope operations to total job count for progress tracking
        // emit_plug_scope_event fires once per scope-batch, not per-plug:
        //   init:     1 event if all-scope plugs/SetupAll exist, 1 per slot if each-scope plugs/SetupEach exist
        //   teardown: 1 event if all-scope plugs exist, 1 per slot if each-scope plugs exist
        // Station plugs held by a host count like execution-scope for the init
        // batch (one acquire event) but contribute no teardown event —
        // they outlive the run. Without a host they degrade to execution scope
        // (see set_plug_scopes above) and count as execution-scope on both
        // sides.
        //
        // Reservations must agree with the emit gates: on a partial run the
        // creation/acquisition/teardown paths only ever see plugs in the
        // introspected union, so a declared plug outside it emits nothing
        // and must not reserve an event slot — a reserved-but-never-emitted
        // pair leaves run_progress permanently short of total. Full runs
        // have no union and count every declared plug, as before.
        let in_union = |key: &String| {
            partial_required_plugs
                .as_ref()
                .map_or(true, |union| union.contains(key))
        };
        let has_station_host = self.station_plug_host.is_some();
        let has_station_scope_plugs = self
            .procedure_definition
            .plugs
            .iter()
            .any(|p| p.scope == crate::procedure::schema::Scope::Station && in_union(&p.key));
        let has_all_scope_plugs = self
            .procedure_definition
            .plugs
            .iter()
            .any(|p| p.scope == crate::procedure::schema::Scope::Execution && in_union(&p.key))
            || (has_station_scope_plugs && !has_station_host);
        let has_each_scope_plugs = self
            .procedure_definition
            .plugs
            .iter()
            .any(|p| p.scope == crate::procedure::schema::Scope::Slot && in_union(&p.key));
        let has_hosted_station_plugs = has_station_scope_plugs && has_station_host;

        let all_phases = self.procedure_definition.get_all_phases_with_stage_scope();
        let has_setup_all = all_phases.iter().any(|(s, p)| matches!(s, crate::procedure::schema::StageScope::SetupAll) && !p.should_skip());
        let has_setup_each = all_phases.iter().any(|(s, p)| matches!(s, crate::procedure::schema::StageScope::SetupEach) && !p.should_skip());

        // The station branch and the execution branch in
        // ensure_plugs_created_for_job are INDEPENDENT emitters — each
        // fires its own terminal plug-scope event. A procedure with a
        // hosted station plug AND a SetupAll trigger produces two init
        // events, so they need two reserved slots, not a shared one
        // (sharing made progress overshoot 100%).
        let init_events =
            (if has_all_scope_plugs || has_setup_all { 1 } else { 0 })
            + (if has_hosted_station_plugs { 1 } else { 0 })
            + (if has_each_scope_plugs || has_setup_each { slots.len() } else { 0 });
        let teardown_events =
            (if has_all_scope_plugs { 1 } else { 0 })
            + (if has_each_scope_plugs { slots.len() } else { 0 });
        let plug_scope_operations = init_events + teardown_events;
        state.total_jobs_submitted += plug_scope_operations;

        // Emit execution plan to frontend, narrowed to what this run
        // will actually execute — the UI paints anything planned but
        // never run with the final outcome, and seeds a card for every
        // planned plug.
        self.emit_execution_plan(
            &self.procedure_definition,
            &state,
            &slots,
            partial_main_set.as_ref(),
            partial_required_plugs.as_deref(),
        )
        .await;

        Ok(())
    }
}

/// Union of `plug_keys_for_phase` over every phase a partial run will
/// execute: all setup, the main-stage partial set, all teardown — not the
/// target phase alone, or the setup phase that initialises an instrument
/// runs without it.
///
/// A `None` from introspection (import error) makes the whole union fall
/// back to every declared plug — the degradation over-starts instead of
/// silently under-starting. `Some(vec![])` contributes nothing, which is
/// what lets a play on a callable-less phase start no plug at all.
///
/// Keys come back in declaration order.
fn union_required_plugs(
    procedure: &crate::procedure::schema::ProcedureDefinition,
    partial_main_set: &HashSet<String>,
    introspection: &crate::procedure::Introspection,
) -> Vec<String> {
    let all_keys = || {
        procedure
            .plugs
            .iter()
            .map(|p| p.key.clone())
            .collect::<Vec<_>>()
    };

    let mut needed: HashSet<String> = HashSet::new();
    for (stage_scope, phase) in procedure.iter_phases_with_stage() {
        if phase.should_skip() {
            continue;
        }
        let runs = match stage_scope {
            crate::procedure::schema::StageScope::Main => partial_main_set.contains(&phase.key),
            // Setup and teardown phases always run.
            _ => true,
        };
        if !runs {
            continue;
        }
        match procedure.plug_keys_for_phase(phase, introspection) {
            Some(keys) => needed.extend(keys),
            None => return all_keys(),
        }
    }

    procedure
        .plugs
        .iter()
        .filter(|p| needed.contains(&p.key))
        .map(|p| p.key.clone())
        .collect()
}

/// Main-stage phase set for a partial run: the target plus the transitive
/// `depends_on` closure of the target. `depends_on` only resolves within a
/// stage (the Builder enforces this), so the walk stays inside `main`.
/// Setup and teardown phases are not part of the set — they always run and
/// are never filtered.
///
/// Errors when the target key is unknown or names a setup/teardown phase.
/// Dependencies on unknown keys are tolerated, exactly like the job-graph
/// builder tolerates them.
fn partial_main_phase_set(
    procedure: &crate::procedure::schema::ProcedureDefinition,
    target: &str,
) -> Result<HashSet<String>, String> {
    if procedure
        .setup
        .iter()
        .chain(procedure.teardown.iter())
        .any(|p| p.key == target)
    {
        return Err(format!(
            "Phase '{}' is a setup/teardown phase; a partial run can only target a main phase",
            target
        ));
    }

    let by_key: HashMap<&str, &crate::procedure::schema::PhaseDefinition> = procedure
        .main
        .iter()
        .map(|p| (p.key.as_str(), p))
        .collect();

    if !by_key.contains_key(target) {
        return Err(format!(
            "Unknown phase '{}': not a main phase of this procedure",
            target
        ));
    }

    let mut set: HashSet<String> = HashSet::new();
    let mut stack = vec![target.to_string()];
    while let Some(key) = stack.pop() {
        if !set.insert(key.clone()) {
            // Already visited: diamonds and self-references terminate here.
            continue;
        }
        if let Some(phase) = by_key.get(key.as_str()) {
            for dep in &phase.depends_on {
                if !set.contains(dep) {
                    stack.push(dep.clone());
                }
            }
        }
    }

    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procedure::schema::{ProcedureDefinition, ProcedureYaml};

    fn procedure(yaml: &str) -> ProcedureDefinition {
        let raw: ProcedureYaml = serde_yaml::from_str(yaml).unwrap();
        ProcedureDefinition::from(raw)
    }

    fn set(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|s| s.to_string()).collect()
    }

    const PROCEDURE: &str = r#"
name: test
version: 1.0.0
setup:
  - key: prepare
    name: Prepare
main:
  - key: a
    name: A
  - key: b
    name: B
    depends_on: [a]
  - key: c
    name: C
    depends_on: [b]
  - key: d1
    name: D1
    depends_on: [a]
  - key: d2
    name: D2
    depends_on: [a]
  - key: diamond
    name: Diamond
    depends_on: [d1, d2]
  - key: selfref
    name: Selfref
    depends_on: [selfref]
  - key: dangling
    name: Dangling
    depends_on: [does_not_exist]
teardown:
  - key: cleanup
    name: Cleanup
"#;

    #[test]
    fn chain_walks_to_fixpoint() {
        let def = procedure(PROCEDURE);
        assert_eq!(
            partial_main_phase_set(&def, "c").unwrap(),
            set(&["c", "b", "a"])
        );
    }

    #[test]
    fn diamond_visits_each_node_once() {
        let def = procedure(PROCEDURE);
        assert_eq!(
            partial_main_phase_set(&def, "diamond").unwrap(),
            set(&["diamond", "d1", "d2", "a"])
        );
    }

    #[test]
    fn self_reference_terminates() {
        let def = procedure(PROCEDURE);
        assert_eq!(
            partial_main_phase_set(&def, "selfref").unwrap(),
            set(&["selfref"])
        );
    }

    #[test]
    fn unknown_target_is_an_error() {
        let def = procedure(PROCEDURE);
        assert!(partial_main_phase_set(&def, "nope").is_err());
    }

    #[test]
    fn setup_and_teardown_targets_are_rejected() {
        let def = procedure(PROCEDURE);
        assert!(partial_main_phase_set(&def, "prepare").is_err());
        assert!(partial_main_phase_set(&def, "cleanup").is_err());
    }

    #[test]
    fn target_with_no_deps_is_just_itself() {
        let def = procedure(PROCEDURE);
        assert_eq!(partial_main_phase_set(&def, "a").unwrap(), set(&["a"]));
    }

    #[test]
    fn unknown_dependency_keys_are_tolerated() {
        let def = procedure(PROCEDURE);
        assert_eq!(
            partial_main_phase_set(&def, "dangling").unwrap(),
            set(&["dangling", "does_not_exist"])
        );
    }

    // --- union_required_plugs ---

    use crate::procedure::{Introspection, PhaseSignature};

    const THREE_PLUG_PROCEDURE: &str = r#"
name: test
version: 1.0.0
plugs:
  - name: p1
    python: plugs.p1:P1
  - name: p2
    python: plugs.p2:P2
  - name: p3
    python: plugs.p3:P3
setup:
  - key: prep
    name: Prep
    python: phases.s:prep
main:
  - key: a
    name: A
    python: phases.m:a
  - key: b
    name: B
  - key: c
    name: C
    python: phases.m:c
teardown:
  - key: cleanup
    name: Cleanup
    python: phases.t:cleanup
"#;

    fn signature(params: &[&str]) -> PhaseSignature {
        PhaseSignature {
            params: Some(params.iter().map(|s| s.to_string()).collect()),
            error: None,
        }
    }

    fn intr(entries: Vec<(&str, PhaseSignature)>) -> Introspection {
        Introspection {
            phases: entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    #[test]
    fn partial_set_starts_one_plug_of_three() {
        let def = procedure(THREE_PLUG_PROCEDURE);
        let i = intr(vec![
            ("prep", signature(&["phase"])),
            ("a", signature(&["measurements", "p1"])),
            ("cleanup", signature(&["phase"])),
        ]);
        assert_eq!(
            union_required_plugs(&def, &set(&["a"]), &i),
            vec!["p1".to_string()]
        );
    }

    #[test]
    fn setup_phase_pulls_in_plug_the_target_does_not_use() {
        let def = procedure(THREE_PLUG_PROCEDURE);
        let i = intr(vec![
            ("prep", signature(&["phase", "p2"])),
            ("a", signature(&["measurements", "p1"])),
            ("cleanup", signature(&["phase"])),
        ]);
        assert_eq!(
            union_required_plugs(&def, &set(&["a"]), &i),
            vec!["p1".to_string(), "p2".to_string()]
        );
    }

    #[test]
    fn phase_with_no_plugs_starts_none() {
        let def = procedure(THREE_PLUG_PROCEDURE);
        // Target `b` has no callable at all; setup/teardown take no plug.
        let i = intr(vec![
            ("prep", signature(&["phase"])),
            ("cleanup", signature(&["phase"])),
        ]);
        assert_eq!(
            union_required_plugs(&def, &set(&["b"]), &i),
            Vec::<String>::new()
        );
    }

    #[test]
    fn import_error_in_the_set_starts_all_plugs() {
        let def = procedure(THREE_PLUG_PROCEDURE);
        let i = intr(vec![
            ("prep", signature(&["phase"])),
            (
                "c",
                PhaseSignature {
                    params: None,
                    error: Some("No module named 'serial'".to_string()),
                },
            ),
            ("cleanup", signature(&["phase"])),
        ]);
        assert_eq!(
            union_required_plugs(&def, &set(&["c"]), &i),
            vec!["p1".to_string(), "p2".to_string(), "p3".to_string()]
        );
    }

    #[test]
    fn import_error_outside_the_set_is_ignored() {
        let def = procedure(THREE_PLUG_PROCEDURE);
        // `c` fails to import but isn't in the partial set: its error
        // must not widen the union (or stop the run).
        let i = intr(vec![
            ("prep", signature(&["phase"])),
            ("a", signature(&["p1"])),
            (
                "c",
                PhaseSignature {
                    params: None,
                    error: Some("No module named 'serial'".to_string()),
                },
            ),
            ("cleanup", signature(&["phase"])),
        ]);
        assert_eq!(
            union_required_plugs(&def, &set(&["a"]), &i),
            vec!["p1".to_string()]
        );
    }
}
