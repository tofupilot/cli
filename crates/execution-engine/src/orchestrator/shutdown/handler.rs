//! Worker and slot control operations
//!
//! This module handles granular control over individual workers and slots:
//! - Force killing workers (immediate termination)
//! - Graceful worker stopping
//! - Slot-level stopping (all workers for a slot)
//! - System shutdown coordination

use futures::future::join_all;
use std::sync::Arc;
use std::time::Duration;


use tokio::sync::RwLock;
use tokio::time::timeout;

use crate::event_sink::{EventSink, ExecutionEvent};
use crate::job::{Job, JobResult, JobStatus, Outcome};
use crate::state::OrchestratorState;
use crate::worker::Worker;
use crate::procedure::schema::StageScope;

use super::super::Orchestrator;

/// Cap on the teardown phases `shutdown()` runs after a stop. Exported so
/// the CLI's signal ladder can size its own escalation from it.
pub const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Worker pool for the teardown phases `shutdown()` runs.
const NUM_TEARDOWN_WORKERS: usize = 2;

/// How much time `shutdown()` may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    /// Graceful worker stop, fresh teardown workers, Cleanup RPC on
    /// every plug.
    Normal,
    /// An OS deadline of seconds is behind the stop (Windows console
    /// close). Workers are killed outright, the teardown runs on a
    /// pool worker that is already idle (one spawn only if none is),
    /// and the plugs are killed without their Cleanup RPC: the exit
    /// hook (Unix) or the Job Object (Windows) reaps them anyway.
    Hurried,
}

impl Orchestrator {
    fn is_teardown_job(job: &Job) -> bool {
        matches!(
            job.stage_scope,
            StageScope::TeardownEach | StageScope::TeardownAll
        )
    }

    fn collect_and_complete_jobs(
        state: &mut OrchestratorState,
        reason: String,
        partition_teardown: bool,
    ) -> (
        Vec<(usize, uuid::Uuid, String, String, String)>,
        Vec<(uuid::Uuid, String, String, String)>,
        Vec<Job>,
    ) {
        let mut running_jobs_info = Vec::new();
        let mut queued_jobs_info = Vec::new();

        for worker_id in 0..state.worker_state.num_workers() {
            if let Some(job_id) = state.worker_state.get_worker_job(worker_id) {
                if let Some(info) = state.job_info.get(&job_id) {
                    running_jobs_info.push((
                        worker_id,
                        job_id,
                        info.phase_key.clone(),
                        info.phase_name.clone(),
                        info.slot_id.clone().unwrap_or_else(|| "<shared>".to_string()),
                    ));
                }
                // An operator interruption, not a failure: see `new_interrupted`.
                state.complete_job(job_id, JobResult::new_interrupted(reason.clone()));
            }
        }

        let (mut teardown_jobs, regular_jobs): (Vec<Job>, Vec<Job>) = if partition_teardown {
            state.job_queue.drain(..).partition(Self::is_teardown_job)
        } else {
            (Vec::new(), state.job_queue.drain(..).collect())
        };

        let pending_slot_jobs: Vec<Job> = state
            .pending_slot_jobs
            .drain(..)
            .flat_map(|(_, jobs)| jobs)
            .collect();

        if partition_teardown {
            teardown_jobs.append(&mut state.teardown_procedure_jobs);
        } else {
            let teardown_procedure_jobs: Vec<Job> =
                state.teardown_procedure_jobs.drain(..).collect();
            for job in teardown_procedure_jobs {
                queued_jobs_info.push((
                    job.id,
                    job.phase_key.clone(),
                    job.phase_name.clone(),
                    job.slot_id
                        .clone()
                        .unwrap_or_else(|| "<shared>".to_string()),
                ));
                state.job_info.insert(job.id, crate::state::JobInfo::from_job(&job));
                state.complete_job(job.id, JobResult::new_skip());
            }
        }

        for job in &regular_jobs {
            queued_jobs_info.push((
                job.id,
                job.phase_key.clone(),
                job.phase_name.clone(),
                job.slot_id
                    .clone()
                    .unwrap_or_else(|| "<shared>".to_string()),
            ));
            // Populate job_info so complete_job can resolve dependency_id
            state.job_info.insert(job.id, crate::state::JobInfo::from_job(&job));
            state.complete_job(job.id, JobResult::new_skip());
        }

        for job in &pending_slot_jobs {
            queued_jobs_info.push((
                job.id,
                job.phase_key.clone(),
                job.phase_name.clone(),
                job.slot_id
                    .clone()
                    .unwrap_or_else(|| "<shared>".to_string()),
            ));
            // Populate job_info so complete_job can resolve dependency_id
            state.job_info.insert(job.id, crate::state::JobInfo::from_job(&job));
            state.complete_job(job.id, JobResult::new_skip());
        }

        (running_jobs_info, queued_jobs_info, teardown_jobs)
    }

    fn emit_job_event(
        job_id: uuid::Uuid,
        slot_id: Option<String>,
        phase_key: &str,
        phase_name: &str,
        stage_scope: StageScope,
        status: JobStatus,
        outcome: Option<Outcome>,
        error: Option<String>,
        worker_id: Option<usize>,
        event_sink: &Arc<dyn EventSink>,
    ) {
        event_sink.emit(&ExecutionEvent::JobProgress {
            job_id: job_id.to_string(),
            slot_id,
            phase_key: phase_key.to_string(),
            phase_name: phase_name.to_string(),
            stage_scope,
            status,
            worker_id,
            started_at: None,
            timeout_ms: None,
            outcome,
            retry_count: 0,
            error,
        });
    }

    fn emit_job_events(
        jobs: &[(uuid::Uuid, String, String, String)],
        status: JobStatus,
        outcome: Option<Outcome>,
        error: Option<String>,
        event_sink: &Arc<dyn EventSink>,
    ) {
        for (job_id, phase_key, phase_name, slot_id) in jobs {
            Self::emit_job_event(
                *job_id,
                if slot_id == "<shared>" { None } else { Some(slot_id.to_string()) },
                phase_key,
                phase_name,
                StageScope::Main,
                status,
                outcome,
                error.clone(),
                None,
                event_sink,
            );
        }
    }

    async fn shutdown_workers_gracefully(
        workers: &mut [Worker],
        running_jobs_info: &[(usize, uuid::Uuid, String, String, String)],
        event_sink: &Arc<dyn EventSink>,
    ) {
        use std::collections::HashMap;

        let job_map: HashMap<usize, (uuid::Uuid, String, String, String)> = running_jobs_info
            .iter()
            .map(|(worker_id, job_id, phase_key, phase_name, slot_id)| {
                (
                    *worker_id,
                    (
                        *job_id,
                        phase_key.clone(),
                        phase_name.clone(),
                        slot_id.clone(),
                    ),
                )
            })
            .collect();

        // Step 1: Emit "stopping" events for all workers with jobs immediately
        for (worker_id, _) in workers.iter().enumerate() {
            if let Some((job_id, phase_key, phase_name, slot_id)) = job_map.get(&worker_id) {
                log::debug!(
                    "Emitting status=stopping for phase={}, slot={}",
                    phase_name, slot_id
                );
                Self::emit_job_event(
                    *job_id,
                    if slot_id == "<shared>" { None } else { Some(slot_id.to_string()) },
                    phase_key,
                    phase_name,
                    StageScope::Main,
                    JobStatus::Stopping,
                    None,
                    None,
                    Some(worker_id),
                    event_sink,
                );
            }
        }

        // Step 2: Stop all workers in parallel
        let shutdown_futures: Vec<_> = workers
            .iter_mut()
            .enumerate()
            .map(|(worker_id, worker)| {
                let has_job = job_map.contains_key(&worker_id);
                async move {
                    if has_job {
                        let _ = worker.interrupt_current_job().await;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }

                    let res = timeout(
                        Duration::from_millis(1000),
                        worker.shutdown_with_timeout(1000),
                    )
                    .await;

                    match res {
                        Ok(Ok(())) => {}
                        _ => {
                            let _ = worker.force_shutdown().await;
                        }
                    }
                }
            })
            .collect();

        join_all(shutdown_futures).await;

        // Step 3: Emit "stop" outcome for all workers with jobs
        for (worker_id, _) in workers.iter().enumerate() {
            if let Some((job_id, phase_key, phase_name, slot_id)) = job_map.get(&worker_id) {
                log::debug!(
                    "Emitting outcome=stop for phase={}, slot={}",
                    phase_name, slot_id
                );
                Self::emit_job_event(
                    *job_id,
                    if slot_id == "<shared>" { None } else { Some(slot_id.to_string()) },
                    phase_key,
                    phase_name,
                    StageScope::Main,
                    JobStatus::Completed,
                    Some(Outcome::Stop),
                    Some("Execution stopped by user".to_string()),
                    None,
                    event_sink,
                );
            }
        }
    }

    async fn force_kill_workers_parallel(workers: Vec<Worker>) {
        let kill_futures: Vec<_> = workers
            .iter()
            .enumerate()
            .map(|(idx, worker)| {
                let mut worker_clone = worker.clone();
                async move {
                    log::debug!("Force killing worker {}", idx);
                    let result = worker_clone.force_shutdown().await;
                    match &result {
                        Ok(_) => {}
                        Err(e) => {
                            log::error!("Worker {} kill failed: {}", idx, e)
                        }
                    }
                    result
                }
            })
            .collect();

        futures::future::join_all(kill_futures).await;
    }

    /// Run the teardown phases on `teardown_workers`, spawning fresh
    /// ones when none was handed over. `Hurried` skips every grace
    /// period on the way out.
    async fn execute_teardown_jobs(
        &mut self,
        teardown_jobs: Vec<Job>,
        mut teardown_workers: Vec<Worker>,
        mode: ShutdownMode,
    ) -> Result<(), String> {
        if self.state.read().await.force_kill_requested {
            Self::force_kill_workers_parallel(teardown_workers).await;
            return Err("Force kill requested; teardown phases skipped".to_string());
        }

        if teardown_workers.is_empty() {
            let spawn = match mode {
                ShutdownMode::Normal => NUM_TEARDOWN_WORKERS,
                ShutdownMode::Hurried => 1,
            };
            for i in 0..spawn {
                let mut worker = Worker::new_with_python(
                    i,
                    self.procedure_dir.clone(),
                    self.python_path.clone(),
                );
                worker.start(&self.event_sink).await?;
                teardown_workers.push(worker);
            }
        }

        // Re-populate state with teardown jobs (phases stay pending until
        // actually started). A force kill that landed while the workers
        // were spawning has already drained the queue and killed the
        // plugs: nothing left to run the teardown against.
        {
            let mut state = self.state.write().await;
            if state.force_kill_requested {
                drop(state);
                Self::force_kill_workers_parallel(teardown_workers).await;
                return Err("Force kill requested; teardown phases skipped".to_string());
            }
            for job in teardown_jobs {
                state.enqueue_job(job);
            }
            state.shutdown_requested = false; // Temporarily allow execution
        }

        // Store teardown workers
        {
            let mut workers = self.workers.write().await;
            *workers = teardown_workers;
        }

        // Execute teardown jobs with timeout
        let teardown_result = tokio::time::timeout(
            TEARDOWN_TIMEOUT,
            self.run_teardown_loop(),
        )
        .await;

        // The flag was lowered above so teardown phases could run; raise it
        // again so the state does not claim the run is still live. Bare
        // flag on purpose: `shutdown()` is also the normal end-of-run
        // teardown, so it must not invent an `Operator` cause. Whoever
        // actually asked for the stop recorded the cause before this.
        self.state.write().await.shutdown_requested = true;

        // Shutdown teardown workers
        let workers = {
            let mut guard = self.workers.write().await;
            std::mem::take(&mut *guard)
        };
        match mode {
            ShutdownMode::Normal => {
                for mut worker in workers {
                    let _ = worker.shutdown_with_timeout(1000).await;
                }
            }
            ShutdownMode::Hurried => Self::force_kill_workers_parallel(workers).await,
        }

        match teardown_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("Teardown execution failed: {}", e)),
            Err(_) => {
                // Timeout - force complete remaining jobs
                let mut state = self.state.write().await;
                while let Some(job) = state.job_queue.pop_front() {
                    state.job_info.insert(job.id, crate::state::JobInfo::from_job(&job));
                    state.complete_job(
                        job.id,
                        JobResult::new_error("Teardown timeout during shutdown".to_string()),
                    );

                    // Emit timeout event for this job
                    self.event_sink.emit(&ExecutionEvent::JobProgress {
                        job_id: job.id.to_string(),
                        slot_id: job.slot_id.clone(),
                        phase_key: job.phase_key.clone(),
                        phase_name: job.phase_name.clone(),
                        stage_scope: job.stage_scope.clone(),
                        status: JobStatus::Completed,
                        worker_id: None,
                        started_at: None,
                        timeout_ms: job.timeout_ms,
                        outcome: Some(Outcome::Error),
                        retry_count: job.retry_count,
                        error: Some("Teardown timeout during shutdown".to_string()),
                    });
                }
                Err(format!(
                    "Teardown execution timed out after {}s",
                    TEARDOWN_TIMEOUT.as_secs()
                ))
            }
        }
    }

    async fn run_teardown_loop(&mut self) -> Result<(), String> {
        // Create a new channel for teardown job completions. Matches
        // the main completion channel's bounded shape (orchestrator/mod.rs).
        let (teardown_tx, mut teardown_rx) = tokio::sync::mpsc::channel(64);

        // Temporarily swap the completion_tx to use our teardown channel
        let original_tx = std::mem::replace(&mut self.completion_tx, teardown_tx);

        loop {
            let is_complete = {
                let state = self.state.read().await;
                // A force kill during the teardown (`force_kill_immediate`
                // from the CLI's second signal) killed the workers and
                // drained the queue under us; do not schedule the rest.
                state.force_kill_requested
                    || (state.job_queue.is_empty() && state.worker_state.count_busy() == 0)
            };

            if is_complete {
                break;
            }

            // Schedule available teardown jobs (reuse existing scheduling logic)
            self.schedule_teardown_jobs().await?;

            // Process completion events
            tokio::select! {
                Some(event) = teardown_rx.recv() => {
                    self.handle_job_completion(event).await;
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }

        // Restore the original completion_tx
        self.completion_tx = original_tx;

        Ok(())
    }

    async fn schedule_teardown_jobs(&self) -> Result<(), String> {
        // The tracker still counts the pool's workers; only the first
        // `teardown_pool` of those ids have a worker behind them now.
        // Scheduling on a higher id used to fail the whole teardown
        // ("Worker not found") as soon as a procedure had more teardown
        // phases than teardown workers.
        let teardown_pool = self.workers.read().await.len();
        let jobs_to_spawn = {
            let mut state = self.state.write().await;
            let num_workers = state.worker_state.num_workers().min(teardown_pool);
            let mut jobs = Vec::new();

            for worker_id in 0..num_workers {
                if !state.worker_state.is_worker_idle(worker_id) {
                    continue;
                }

                // Get next ready teardown job
                let job = match state.pop_ready_job(|_| true) {
                    Some(j) => j,
                    None => continue,
                };

                // Mark as active
                state.mark_job_active(job.id, worker_id)?;
                jobs.push((job, worker_id));
            }

            jobs
        };

        // Spawn jobs outside the lock
        for (job, worker_id) in jobs_to_spawn {
            // Get worker
            let worker = {
                let workers = self.workers.read().await;
                workers.get(worker_id).ok_or("Worker not found")?.clone()
            };

            // Spawn execution so teardown phases emit "started" events
            let permit = self.job_semaphore.clone().acquire_owned().await.unwrap();
            self.spawn_job_execution(job, worker_id, worker, permit)
                .await?;
        }

        Ok(())
    }
    /// Enhanced shutdown with graceful-to-force escalation
    pub async fn shutdown(&mut self) -> Result<(), String> {
        self.shutdown_with(ShutdownMode::Normal).await
    }

    /// `shutdown` with its time budget spelled out, see [`ShutdownMode`].
    pub async fn shutdown_with(&mut self, mode: ShutdownMode) -> Result<(), String> {
        // Check if force kill was requested
        {
            let state = self.state.read().await;
            if state.force_kill_requested {
                drop(state);
                return self.force_kill().await;
            }

            // Check if already shut down
            if state.shutdown_requested && self.workers.read().await.is_empty() {
                return Ok(());
            }
        }

        let (running_jobs_info, regular_jobs_info, teardown_jobs, pending_retry_handles) = {
            let mut state = self.state.write().await;
            // Bare flag, no cause: the CLI calls `shutdown()` after EVERY
            // run to reap workers and plugs, a clean pass included. An
            // operator stop recorded its `Operator` cause before reaching
            // here (`request_shutdown`, first cause wins).
            state.shutdown_requested = true;
            // Before anything is drained: once running jobs read as
            // interrupted and queued ones as skipped, a slot that was cut
            // is indistinguishable from one that finished (`SlotStop`).
            state.mark_outstanding_slots_stopped("Execution stopped by user");
            let handles = std::mem::take(&mut state.pending_delayed_retry_handles);
            // Resolve dependencies for pending retries that won't run
            for pending in &handles {
                state.completed_jobs.insert(pending.dependency_id);
            }
            let result = Self::collect_and_complete_jobs(
                &mut state,
                "Execution stopped by user".to_string(),
                true,
            );
            (result.0, result.1, result.2, handles)
        };

        // Abort all pending delayed retry tasks and emit stop events
        for pending in pending_retry_handles.iter() {
            pending.handle.abort();

            self.event_sink.emit(&ExecutionEvent::JobProgress {
                job_id: pending.job_id.to_string(),
                slot_id: pending.slot_id.clone(),
                phase_key: pending.phase_key.clone(),
                phase_name: pending.phase_name.clone(),
                stage_scope: StageScope::Main,
                status: JobStatus::Skipped,
                worker_id: None,
                started_at: None,
                timeout_ms: None,
                outcome: Some(Outcome::Stop),
                retry_count: 0,
                error: Some("Retry cancelled due to shutdown".to_string()),
            });
        }

        let mut workers = {
            let mut guard = self.workers.write().await;
            std::mem::take(&mut *guard)
        };

        let teardown_workers = match mode {
            ShutdownMode::Normal => {
                Self::shutdown_workers_gracefully(
                    &mut workers,
                    &running_jobs_info,
                    &self.event_sink,
                )
                .await;
                Vec::new()
            }
            ShutdownMode::Hurried => {
                Self::stop_workers_hurried(workers, &running_jobs_info, &self.event_sink).await
            }
        };

        Self::emit_job_events(
            &regular_jobs_info,
            JobStatus::Skipped,
            Some(Outcome::Skip),
            Some("Execution stopped by user".to_string()),
            &self.event_sink,
        );

        // Execute teardown jobs if any
        if !teardown_jobs.is_empty() {
            log::info!(
                "Executing {} teardown phases before shutdown",
                teardown_jobs.len()
            );

            if let Err(e) = self
                .execute_teardown_jobs(teardown_jobs, teardown_workers, mode)
                .await
            {
                log::error!("Failed to execute teardown jobs: {}", e);
            }
        } else {
            Self::force_kill_workers_parallel(teardown_workers).await;
        }

        self.release_scope_plugs(mode).await;

        // Catch-all for what the scope release above does not own
        // (manual plugs, a service whose instance entry is gone).
        let plug_service_manager = {
            let resource_manager = self.resource_manager.read().await;
            Arc::clone(resource_manager.get_plug_service_manager())
        };
        let stopped = match mode {
            ShutdownMode::Normal => plug_service_manager.stop_all_services().await,
            ShutdownMode::Hurried => plug_service_manager.force_kill_all_services().await,
        };
        if let Err(e) = stopped {
            log::error!(
                "Failed to stop plug services during shutdown: {}",
                e
            );
        }

        Ok(())
    }

    /// Release the scope plugs a stop kept up for its teardown phases
    /// (`execute_all` leaves them when teardown work is still queued).
    /// Counts each release toward the run's progress exactly like the
    /// end-of-run release does, so an operator stop still reaches 100%.
    /// A run whose plugs were already released at the end of
    /// `execute_all` finds nothing here.
    async fn release_scope_plugs(&self, mode: ShutdownMode) {
        let slot_ids: Vec<String> = {
            let state = self.state.read().await;
            state.slot_jobs.keys().cloned().collect()
        };
        let mut statuses: Vec<&'static str> = Vec::new();
        {
            let resource_manager = self.resource_manager.write().await;
            match mode {
                ShutdownMode::Normal => {
                    for slot_id in slot_ids {
                        if !resource_manager.has_each_scope_plugs(&slot_id).await {
                            continue;
                        }
                        match resource_manager
                            .destroy_each_scope_plugs(slot_id.clone(), &self.event_sink)
                            .await
                        {
                            Ok(_) => statuses.push("pass"),
                            Err(e) => {
                                log::warn!(
                                    "Failed to destroy each-scope plugs for {} at shutdown: {}",
                                    slot_id, e
                                );
                                statuses.push("error");
                            }
                        }
                    }
                    if resource_manager.has_all_scope_plugs().await {
                        match resource_manager
                            .destroy_all_scope_plugs(&self.event_sink)
                            .await
                        {
                            Ok(_) => statuses.push("pass"),
                            Err(e) => {
                                log::warn!(
                                    "Failed to destroy all-scope plugs at shutdown: {}",
                                    e
                                );
                                statuses.push("error");
                            }
                        }
                    }
                }
                ShutdownMode::Hurried => {
                    for slot_id in &slot_ids {
                        if resource_manager.has_each_scope_plugs(slot_id).await {
                            statuses.push("pass");
                        }
                    }
                    if resource_manager.has_all_scope_plugs().await {
                        statuses.push("pass");
                    }
                    if let Err(e) = resource_manager
                        .force_destroy_all_plugs(&self.event_sink)
                        .await
                    {
                        log::warn!("Failed to force destroy plugs at shutdown: {}", e);
                    }
                }
            }
        }
        // Emitted with the resource manager released: the counter takes
        // `state`, and the lock order is state before resource manager.
        for status in statuses {
            self.emit_plug_scope_event(status).await;
        }
    }

    /// The hurried counterpart of `shutdown_workers_gracefully`: same
    /// events, no grace. Idle workers whose interpreter is still up are
    /// handed back for the teardown phases (a spawn is the slowest step
    /// of a shutdown that has seconds); the rest are killed in parallel.
    async fn stop_workers_hurried(
        workers: Vec<Worker>,
        running_jobs_info: &[(usize, uuid::Uuid, String, String, String)],
        event_sink: &Arc<dyn EventSink>,
    ) -> Vec<Worker> {
        let running: Vec<(uuid::Uuid, String, String, String)> = running_jobs_info
            .iter()
            .map(|(_, job_id, phase_key, phase_name, slot_id)| {
                (*job_id, phase_key.clone(), phase_name.clone(), slot_id.clone())
            })
            .collect();
        let busy: std::collections::HashSet<usize> =
            running_jobs_info.iter().map(|(worker_id, ..)| *worker_id).collect();

        Self::emit_job_events(&running, JobStatus::Stopping, None, None, event_sink);

        let mut keep = Vec::new();
        let mut kill = Vec::new();
        for (worker_id, worker) in workers.into_iter().enumerate() {
            if keep.len() < NUM_TEARDOWN_WORKERS
                && !busy.contains(&worker_id)
                && worker.is_alive().await
            {
                keep.push(worker);
            } else {
                kill.push(worker);
            }
        }
        log::info!(
            "Hurried stop: {} idle worker(s) kept for the teardown, {} killed",
            keep.len(),
            kill.len()
        );
        Self::force_kill_workers_parallel(kill).await;

        Self::emit_job_events(
            &running,
            JobStatus::Completed,
            Some(Outcome::Stop),
            Some("Execution stopped by user".to_string()),
            event_sink,
        );
        keep
    }

    pub async fn force_kill(&mut self) -> Result<(), String> {
        log::info!("Force killing execution - no teardown phases will run");

        let (running_jobs_info, queued_jobs_info, _, pending_retry_handles) = {
            let mut state = self.state.write().await;
            state.request_shutdown(crate::state::ShutdownCause::Operator);
            state.mark_outstanding_slots_stopped("Force killed by user");
            let handles = std::mem::take(&mut state.pending_delayed_retry_handles);
            // Resolve dependencies for pending retries that won't run
            for pending in &handles {
                state.completed_jobs.insert(pending.dependency_id);
            }
            let result = Self::collect_and_complete_jobs(
                &mut state,
                "Force killed by user".to_string(),
                false,
            );
            (result.0, result.1, result.2, handles)
        };

        // Abort all pending delayed retry tasks and emit error events
        for pending in pending_retry_handles.iter() {
            pending.handle.abort();

            self.event_sink.emit(&ExecutionEvent::JobProgress {
                job_id: pending.job_id.to_string(),
                slot_id: pending.slot_id.clone(),
                phase_key: pending.phase_key.clone(),
                phase_name: pending.phase_name.clone(),
                stage_scope: StageScope::Main,
                status: JobStatus::Completed,
                worker_id: None,
                started_at: None,
                timeout_ms: None,
                outcome: Some(Outcome::Error),
                retry_count: 0,
                error: Some("Force killed by user".to_string()),
            });
        }

        let running_jobs_for_emit: Vec<(uuid::Uuid, String, String, String)> = running_jobs_info
            .iter()
            .map(|(_, job_id, phase_key, phase_name, slot_id)| {
                (
                    *job_id,
                    phase_key.clone(),
                    phase_name.clone(),
                    slot_id.clone(),
                )
            })
            .collect();

        Self::emit_job_events(
            &running_jobs_for_emit,
            JobStatus::Stopping,
            None,
            None,
            &self.event_sink,
        );

        log::info!(
            "Force killing {} workers ({} running, {} queued)",
            self.workers.read().await.len(),
            running_jobs_info.len(),
            queued_jobs_info.len()
        );

        let workers = {
            let mut guard = self.workers.write().await;
            std::mem::take(&mut *guard)
        };

        Self::force_kill_workers_parallel(workers).await;

        Self::emit_job_events(
            &running_jobs_for_emit,
            JobStatus::Completed,
            Some(Outcome::Error),
            Some("Force killed by user".to_string()),
            &self.event_sink,
        );

        Self::emit_job_events(
            &queued_jobs_info,
            JobStatus::Skipped,
            Some(Outcome::Skip),
            Some("Force killed by user".to_string()),
            &self.event_sink,
        );

        log::info!("Force killing all plug services");

        let resource_manager = self.resource_manager.read().await;
        if let Err(e) = resource_manager.force_destroy_all_plugs(&self.event_sink).await {
            log::warn!("Failed to force destroy plugs: {}", e);
        }
        drop(resource_manager);

        log::info!("Execution force killed - all processes terminated");

        Ok(())
    }

    /// Stop the phases running right now without giving up the teardown.
    ///
    /// Raises the operator stop, then kills every worker busy with a
    /// main or setup phase, in parallel: each interrupted job completes
    /// as stopped under the raised flag (no replacement worker is
    /// started), `execute_all` drains, and the caller's `shutdown()`
    /// runs the teardown phases before the plugs are released. A worker
    /// already running a teardown phase is left alone: killing it mid
    /// power-off would leave the bench on with nothing to re-run it, and
    /// `execute_all` waits for it. For stops with an external deadline
    /// (console close, SIGTERM): a graceful stop waits for an hours-long
    /// phase, a force kill skips powering the bench down.
    pub async fn interrupt_running_jobs(
        state: Arc<RwLock<OrchestratorState>>,
        workers: Arc<RwLock<Vec<Worker>>>,
    ) {
        let (busy, spared) = {
            let mut state = state.write().await;
            state.request_shutdown(crate::state::ShutdownCause::Operator);
            let mut busy = Vec::new();
            let mut spared = 0usize;
            for id in 0..state.worker_state.num_workers() {
                let Some(job_id) = state.worker_state.get_worker_job(id) else {
                    continue;
                };
                if state.job_info.get(&job_id).is_some_and(|info| info.is_teardown()) {
                    spared += 1;
                } else {
                    busy.push(id);
                }
            }
            (busy, spared)
        };
        if spared > 0 {
            log::info!("{} teardown phase(s) already running are left to finish", spared);
        }
        if busy.is_empty() {
            return;
        }
        log::info!("Interrupting {} running phase(s)", busy.len());
        let targets: Vec<(usize, Worker)> = {
            let workers = workers.read().await;
            busy.into_iter()
                .filter_map(|id| workers.get(id).cloned().map(|w| (id, w)))
                .collect()
        };
        join_all(targets.into_iter().map(|(id, mut worker)| async move {
            if let Err(e) = worker.interrupt_current_job().await {
                log::warn!("Worker {} interrupt failed: {}", id, e);
            }
        }))
        .await;
    }

    pub async fn force_kill_immediate(
        state: Arc<RwLock<OrchestratorState>>,
        workers: Arc<RwLock<Vec<Worker>>>,
        resource_manager: Arc<RwLock<crate::plugs::manager::ResourceManager>>,
        _execution_id: Option<String>,
        event_sink: Arc<dyn EventSink>,
    ) -> Result<(), String> {
        // Set shutdown flags and take pending retry handles atomically
        let pending_retry_handles = {
            let mut state_guard = state.write().await;
            state_guard.request_shutdown(crate::state::ShutdownCause::Operator);
            state_guard.mark_outstanding_slots_stopped("Force killed by user");
            state_guard.force_kill_requested = true;
            let handles = std::mem::take(&mut state_guard.pending_delayed_retry_handles);
            // Resolve dependencies for pending retries that won't run
            for pending in &handles {
                state_guard.completed_jobs.insert(pending.dependency_id);
            }
            handles
        };

        // Abort all pending delayed retry tasks
        for pending in &pending_retry_handles {
            pending.handle.abort();

            event_sink.emit(&ExecutionEvent::JobProgress {
                job_id: pending.job_id.to_string(),
                slot_id: pending.slot_id.clone(),
                phase_key: pending.phase_key.clone(),
                phase_name: pending.phase_name.clone(),
                stage_scope: StageScope::Main,
                status: JobStatus::Completed,
                worker_id: None,
                started_at: None,
                timeout_ms: None,
                outcome: Some(Outcome::Error),
                retry_count: 0,
                error: Some("Force killed by user".to_string()),
            });
        }

        log::info!("Force killing all workers immediately");

        // Kill all workers FIRST, in parallel for maximum speed
        // This prevents workers from completing teardown phases before we mark them as skipped
        let kill_tasks: Vec<_> = {
            let workers_guard = workers.read().await;
            workers_guard
                .iter()
                .map(|worker| {
                    let mut worker_clone = worker.clone();
                    tokio::spawn(async move {
                        let result = worker_clone.force_shutdown().await;
                        result
                    })
                })
                .collect()
        };

        // Wait for all kills to complete (truly in parallel)
        let _ = join_all(kill_tasks).await;

        // NOW collect and mark jobs as complete, after workers are dead
        let (running_jobs_info, queued_jobs_info, _) = {
            let mut state_guard = state.write().await;
            Self::collect_and_complete_jobs(
                &mut state_guard,
                "Force killed by user".to_string(),
                false,
            )
        };

        let running_jobs_for_emit: Vec<(uuid::Uuid, String, String, String)> = running_jobs_info
            .iter()
            .map(|(_, job_id, phase_key, phase_name, slot_id)| {
                (
                    *job_id,
                    phase_key.clone(),
                    phase_name.clone(),
                    slot_id.clone(),
                )
            })
            .collect();

        Self::emit_job_events(
            &running_jobs_for_emit,
            JobStatus::Completed,
            Some(Outcome::Stop),
            Some("Force killed by user".to_string()),
            &event_sink,
        );

        Self::emit_job_events(
            &queued_jobs_info,
            JobStatus::Skipped,
            Some(Outcome::Skip),
            Some("Force killed by user".to_string()),
            &event_sink,
        );

        log::info!("Force killing all plug services");

        let resource_manager_guard = resource_manager.read().await;
        if let Err(e) = resource_manager_guard
            .force_destroy_all_plugs(&event_sink)
            .await
        {
            log::warn!("Failed to force destroy plugs: {}", e);
        }
        drop(resource_manager_guard);

        log::info!("Execution force killed - all processes terminated");

        Ok(())
    }
}

#[cfg(test)]
mod interrupted_job_tests {
    use super::*;
    use crate::procedure::schema::StageScope;
    use crate::state::{JobInfo, OrchestratorState};

    use crate::test_support::job;

    /// The operator paths record the phase they interrupt as `Stop` with
    /// no `error`. Recording it as `new_error` once made every force kill
    /// with a phase in flight aggregate to ERROR (TP-957, protocol run 4).
    /// The reason survives as a log line on the phase.
    #[test]
    fn running_job_is_recorded_as_interrupted_not_as_an_error() {
        let mut state = OrchestratorState::new(1);
        let running = job(StageScope::Main);
        let queued = job(StageScope::Main);
        state.job_info.insert(running.id, JobInfo::from_job(&running));
        state.worker_state.assign_job(0, running.id).unwrap();
        state.enqueue_job(queued);

        let (running_info, queued_info, _) = Orchestrator::collect_and_complete_jobs(
            &mut state,
            "Force killed by user".to_string(),
            false,
        );
        assert_eq!(running_info.len(), 1);
        assert_eq!(queued_info.len(), 1);

        let result = state.job_results.get(&running.id).expect("running job completed");
        assert_eq!(result.phase_outcome, Outcome::Stop);
        assert!(result.error.is_none(), "an interruption is not an error");
        assert!(!result.is_failure());
        assert_eq!(result.logs.len(), 1);
        assert_eq!(result.logs[0].message, "Force killed by user");
    }
}
