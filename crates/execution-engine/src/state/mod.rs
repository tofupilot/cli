use crate::job::{Job, JobResult};
use std::collections::{HashMap, HashSet, VecDeque};
use uuid::Uuid;

mod worker_state;
pub use worker_state::WorkerStateTracker;

#[derive(Debug, Clone)]
pub struct JobInfo {
    pub phase_key: String,
    pub phase_name: String,
    pub function: String,
    pub slot_id: Option<String>,
    pub dependency_id: Uuid,
}

impl JobInfo {
    pub fn from_job(job: &Job) -> Self {
        Self {
            phase_key: job.phase_key.clone(),
            phase_name: job.phase_name.clone(),
            function: job.function.clone(),
            slot_id: job.slot_id.clone(),
            dependency_id: job.dependency_id,
        }
    }
}

/// Information about a pending delayed retry task
#[derive(Debug)]
pub struct PendingDelayedRetry {
    pub handle: tokio::task::JoinHandle<()>,
    pub phase_key: String,
    pub phase_name: String,
    pub function: String,
    pub slot_id: Option<String>,
    pub job_id: Uuid,
    pub dependency_id: Uuid,
    /// Attempt number of the retry that was waiting to run.
    pub retry_count: usize,
}

impl PendingDelayedRetry {
    fn job_info(&self) -> JobInfo {
        JobInfo {
            phase_key: self.phase_key.clone(),
            phase_name: self.phase_name.clone(),
            function: self.function.clone(),
            slot_id: self.slot_id.clone(),
            dependency_id: self.dependency_id,
        }
    }
}

/// Who interrupted a run that did not reach its last phase. The canonical
/// explanation for the flag/cause pair; the other doc blocks point here.
///
/// `shutdown_requested` alone once carried five meanings (operator Stop,
/// operator Kill, the CLI's graceful cancel, a plug init failure, and the
/// engine stopping itself after a failed phase). The aggregation could not
/// tell them apart, so a failing unit under the default
/// `on_first_failure: stop` uploaded as `ABORTED`; the first fix then
/// guessed the cause inside `cancel_all_jobs` and an operator-cancelled run
/// uploaded as `PASS` (TP-957).
///
/// - `PhaseFailure`: the sequence stopping itself after a phase failed, or a
///   failed setup. Not an abort: the run outcome must be `Fail`.
/// - `Operator`: a human or supervisor stopping the run from outside, kill
///   button graceful or forced. That is what `ABORTED` means.
/// - `InitFailure`: a plug that could not initialize. `init_error` is set
///   alongside and wins the aggregation; this only keeps the cause honest.
///
/// Set through `request_shutdown` wherever someone actually asks for a
/// stop. `Orchestrator::shutdown()`, the end-of-run teardown the CLI runs
/// after every run, raises the bare flag without a cause on purpose. A
/// raised flag with no cause reads as an interruption, never as a PASS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownCause {
    Operator,
    PhaseFailure,
    InitFailure,
}

/// A slot cut short before its last phase. The per-slot twin of the
/// `shutdown_requested` / `shutdown_cause` pair: presence in
/// `slot_stops` is the flag, `cause` reads like `shutdown_cause`.
///
/// Stop scope follows job scope. A job with `slot_id = Some(s)` that
/// stops (`on_first_failure: stop`, `then: {…: stop}`, `phase.stop()`,
/// Error, Timeout) cancels slot `s` only and records it here; a shared
/// job (SetupAll, TeardownAll) or an operator stops the execution, which
/// marks every slot. A bool would not do: after a SetupAll failure a
/// slot's own results are only SKIPs and would aggregate to PASS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotStop {
    pub cause: Option<ShutdownCause>,
    /// Quoted on the cancelled UI phases of that slot, so the operator
    /// reads why their prompt vanished.
    pub reason: String,
}

/// What a cancellation removed from the run: queued jobs, and delayed
/// retries that were waiting to be re-enqueued. Both need a Skipped
/// event from the caller.
#[derive(Debug, Default)]
pub struct CancelledWork {
    pub jobs: Vec<Job>,
    pub retries: Vec<PendingDelayedRetry>,
}

impl CancelledWork {
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty() && self.retries.is_empty()
    }
}

/// Centralized state for the orchestrator to reduce lock complexity
///
/// Lock ordering convention:
/// 1. OrchestratorState (this struct)
/// 2. ResourceManager (if needed)
/// 3. Individual Workers (if needed)
///
/// Never acquire locks in reverse order to prevent deadlocks
#[derive(Debug)]
pub struct OrchestratorState {
    pub job_queue: VecDeque<Job>,
    pub completed_jobs: HashSet<Uuid>,
    pub job_results: HashMap<Uuid, JobResult>,
    pub job_info: HashMap<Uuid, JobInfo>,
    pub worker_state: WorkerStateTracker,
    pub total_jobs_submitted: usize, // Track the initial total job count (not repeats)
    pub original_jobs_completed: usize, // Track completed original jobs (not repeats)
    pub job_to_slot: HashMap<Uuid, String>, // Map job IDs to slot IDs
    pub slot_jobs: HashMap<String, HashSet<Uuid>>, // Map slot IDs to their job IDs
    pub shutdown_requested: bool,    // Flag to signal shutdown
    pub shutdown_cause: Option<ShutdownCause>, // Who raised it, see `ShutdownCause`
    pub force_kill_requested: bool,  // Flag to signal immediate force kill
    pub should_stop_on_first_failure: bool, // Stop execution on first phase failure
    pub pending_slot_jobs: Vec<(String, Vec<Job>)>, // For slot-first: remaining slots to process
    pub teardown_procedure_jobs: Vec<Job>, // Teardown procedure jobs to run after all slots
    pub pending_delayed_retry_handles: Vec<PendingDelayedRetry>, // Handles to spawned retry delay tasks with job info
    pub init_error: Option<String>, // Error that occurred during initialization (e.g., plug init failure)
    /// Every slot the procedure was submitted with, in declaration order.
    /// `slot_jobs` only knows slots that have had a job enqueued, which
    /// under slot-first leaves the not-yet-started ones invisible.
    pub slots: Vec<String>,
    /// Slots cut short, see `SlotStop`. First writer wins, like
    /// `request_shutdown`.
    pub slot_stops: HashMap<String, SlotStop>,
}

impl OrchestratorState {
    pub fn new(num_workers: usize) -> Self {
        Self {
            job_queue: VecDeque::new(),
            completed_jobs: HashSet::new(),
            job_results: HashMap::new(),
            job_info: HashMap::new(),
            worker_state: WorkerStateTracker::new(num_workers),
            total_jobs_submitted: 0,
            original_jobs_completed: 0,
            job_to_slot: HashMap::new(),
            slot_jobs: HashMap::new(),
            shutdown_requested: false,
            shutdown_cause: None,
            force_kill_requested: false,
            should_stop_on_first_failure: false,
            pending_slot_jobs: Vec::new(),
            teardown_procedure_jobs: Vec::new(),
            pending_delayed_retry_handles: Vec::new(),
            init_error: None,
            slots: Vec::new(),
            slot_stops: HashMap::new(),
        }
    }

    /// Raise the shutdown flag with its cause. First cause wins: an
    /// operator kill that landed first stays the reason, whatever the
    /// engine does afterwards to wind down. See `ShutdownCause`.
    pub fn request_shutdown(&mut self, cause: ShutdownCause) {
        self.shutdown_requested = true;
        self.shutdown_cause.get_or_insert(cause);
    }

    /// Check if execution is complete. Work deferred under slot-first
    /// (`pending_slot_jobs`, `teardown_procedure_jobs`) counts: the queue
    /// is empty between two slots, and a slot stop no longer raises the
    /// flag that used to end the loop there.
    pub fn is_complete(&self) -> bool {
        (self.job_queue.is_empty()
            && self.worker_state.count_busy() == 0
            && self.pending_delayed_retry_handles.is_empty()
            && self.pending_slot_jobs.is_empty()
            && self.teardown_procedure_jobs.is_empty())
            || self.shutdown_requested
    }

    /// Clean up finished delayed retry task handles
    pub fn cleanup_finished_retry_handles(&mut self) {
        self.pending_delayed_retry_handles
            .retain(|pending| !pending.handle.is_finished());
    }

    /// Get the next ready job from the queue
    pub fn pop_ready_job(&mut self, check_deps: impl Fn(&Job) -> bool) -> Option<Job> {
        let mut checked_jobs = Vec::new();
        let mut ready_job = None;

        // Find first job with satisfied dependencies
        while let Some(job) = self.job_queue.pop_front() {
            if job.dependencies_satisfied(&self.completed_jobs) && check_deps(&job) {
                ready_job = Some(job);
                break;
            } else {
                checked_jobs.push(job);
            }
        }

        // Put non-ready jobs back
        for job in checked_jobs.into_iter().rev() {
            self.job_queue.push_front(job);
        }

        ready_job
    }

    /// Mark a job as active
    pub fn mark_job_active(&mut self, job_id: Uuid, worker_id: usize) -> Result<(), String> {
        self.worker_state.assign_job(worker_id, job_id)
    }

    /// Complete a job and resolve its dependency_id to unblock dependents
    pub fn complete_job(&mut self, job_id: Uuid, result: JobResult) {
        self.completed_jobs.insert(job_id);
        // Also insert dependency_id so dependents waiting on the original UUID get unblocked
        if let Some(info) = self.job_info.get(&job_id) {
            if info.dependency_id != job_id {
                self.completed_jobs.insert(info.dependency_id);
            }
        }
        self.job_results.insert(job_id, result);
        self.worker_state.release_by_job(&job_id);
    }

    /// Record a retry attempt without satisfying dependencies.
    /// Stores result and releases the worker, but does NOT add to completed_jobs.
    pub fn record_retry_attempt(&mut self, job_id: Uuid, result: JobResult) {
        self.job_results.insert(job_id, result);
        self.worker_state.release_by_job(&job_id);
    }

    /// Complete an original (non-repeat) job
    pub fn complete_original_job(&mut self, job_id: Uuid, result: JobResult) {
        self.complete_job(job_id, result);
        self.original_jobs_completed += 1;
    }

    pub fn complete_job_with_info(&mut self, job_id: Uuid, job: &Job, result: JobResult) {
        self.job_info.insert(job_id, JobInfo::from_job(job));
        self.complete_original_job(job_id, result);
    }

    /// Remove a job from active status without completing it (for repeats)
    pub fn remove_active_job(&mut self, job_id: &Uuid) {
        self.worker_state.release_by_job(job_id);
    }

    /// Record a slot as cut short. First writer wins: a slot that
    /// stopped on its own failure keeps `PhaseFailure` when the operator
    /// later stops the whole run.
    pub fn mark_slot_stopped(&mut self, slot_id: &str, cause: Option<ShutdownCause>, reason: &str) {
        self.slot_stops
            .entry(slot_id.to_string())
            .or_insert_with(|| SlotStop {
                cause,
                reason: reason.to_string(),
            });
    }

    pub fn is_slot_stopped(&self, slot_id: &str) -> bool {
        self.slot_stops.contains_key(slot_id)
    }

    /// The slot a job belongs to is stopping, or the whole execution is.
    /// Shared jobs only ever stop with the execution.
    pub fn is_stopping_for(&self, slot_id: Option<&str>) -> bool {
        self.shutdown_requested || slot_id.is_some_and(|s| self.is_slot_stopped(s))
    }

    /// Mark every slot that still has work as stopped, with the cause the
    /// execution stop recorded. Called by the operator paths before they
    /// drain the queue: once running jobs are completed as interrupted
    /// and queued ones as skipped, nothing in the results can tell a slot
    /// that finished from one that was cut, and the cut one would read
    /// PASS.
    pub fn mark_outstanding_slots_stopped(&mut self, reason: &str) {
        let cause = self.shutdown_cause;
        let shared_outstanding = self.shared_work_outstanding();
        let slots: Vec<String> = self
            .slots
            .iter()
            .filter(|s| shared_outstanding || self.slot_has_outstanding_work(s))
            .cloned()
            .collect();
        for slot in slots {
            self.mark_slot_stopped(&slot, cause, reason);
        }
    }

    /// Whether a shared stage (SetupAll, TeardownAll) is still queued,
    /// running or deferred. Shared work belongs to every slot's outcome,
    /// so while it is outstanding no slot has finished.
    pub fn shared_work_outstanding(&self) -> bool {
        !self.teardown_procedure_jobs.is_empty()
            || self.job_queue.iter().any(|j| j.slot_id.is_none())
            || (0..self.worker_state.num_workers()).any(|w| {
                self.worker_state
                    .get_worker_job(w)
                    .is_some_and(|id| !self.job_to_slot.contains_key(&id))
            })
    }

    /// Whether anything of the slot is still queued, running, waiting on a
    /// delayed retry, or deferred under slot-first. Its own work only; see
    /// `shared_work_outstanding` for the stages every slot depends on.
    pub fn slot_has_outstanding_work(&self, slot_id: &str) -> bool {
        let is_slot = |s: &Option<String>| s.as_deref() == Some(slot_id);
        self.job_queue.iter().any(|j| is_slot(&j.slot_id))
            || self.pending_delayed_retry_handles.iter().any(|p| is_slot(&p.slot_id))
            || self.pending_slot_jobs.iter().any(|(s, _)| s == slot_id)
            || (0..self.worker_state.num_workers()).any(|w| {
                self.worker_state
                    .get_worker_job(w)
                    .is_some_and(|id| self.job_to_slot.get(&id).map(String::as_str) == Some(slot_id))
            })
    }

    fn skip_result(retry_count: usize) -> JobResult {
        JobResult {
            phase_result: crate::job::PhaseResult::Skip,
            phase_outcome: crate::job::Outcome::Skip,
            next_action: None, // Will be computed in completion handler
            timeout_secs: None,
            error: None,
            exit_code: None,
            measurements: vec![],
            logs: vec![],
            started_at: chrono::Utc::now(),
            completed_at: chrono::Utc::now(),
            resource_metrics: None,
            unit: None,
            input_unit_info: None,
            retry_count,
            run_metadata: Default::default(),
            unit_metadata: Default::default(),
        }
    }

    /// Record a job that will never run as Skip so it appears in the
    /// report and unblocks its dependents. Counts toward the original job
    /// total even for a retry attempt: `record_retry_attempt` does not
    /// count the earlier attempts, and no attempt of this phase instance
    /// ever will be.
    fn complete_as_cancelled(&mut self, job_id: Uuid, info: JobInfo, retry_count: usize) {
        self.job_info.insert(job_id, info);
        self.complete_original_job(job_id, Self::skip_result(retry_count));
    }

    /// Abort the delayed retries matching `keep_out`, resolving their
    /// dependency so dependents (the slot's TeardownEach) are not blocked
    /// forever. Without this a retried main phase re-enqueues after the
    /// slot was cancelled and runs after TeardownEach destroyed its plugs.
    fn abort_pending_retries(
        &mut self,
        mut matches: impl FnMut(&PendingDelayedRetry) -> bool,
    ) -> Vec<PendingDelayedRetry> {
        let (aborted, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_delayed_retry_handles)
            .into_iter()
            .partition(|p| matches(p));
        self.pending_delayed_retry_handles = kept;
        for pending in &aborted {
            pending.handle.abort();
            // The attempt that asked for the retry is in `job_results`
            // already (`record_retry_attempt`); the retry itself never ran.
            self.complete_as_cancelled(pending.job_id, pending.job_info(), pending.retry_count);
        }
        aborted
    }

    fn is_teardown(job: &Job) -> bool {
        matches!(
            job.stage_scope,
            crate::procedure::schema::StageScope::TeardownEach
                | crate::procedure::schema::StageScope::TeardownAll
        )
    }

    /// Cancel one slot's remaining work: its queued non-teardown jobs and
    /// its delayed retries. Its TeardownEach stays queued and runs; the
    /// other slots and the shared stages are untouched. Records the slot
    /// as stopped with `cause`, see `SlotStop`.
    pub fn cancel_slot_jobs(
        &mut self,
        slot_id: &str,
        cause: Option<ShutdownCause>,
        reason: &str,
    ) -> CancelledWork {
        let mut cancelled = CancelledWork::default();

        self.job_queue.retain(|job| {
            if job.slot_id.as_deref() == Some(slot_id) && !Self::is_teardown(job) {
                cancelled.jobs.push(job.clone());
                false
            } else {
                true
            }
        });

        for job in &cancelled.jobs {
            self.complete_as_cancelled(job.id, JobInfo::from_job(job), job.retry_count);
        }

        cancelled.retries =
            self.abort_pending_retries(|p| p.slot_id.as_deref() == Some(slot_id));

        if !cancelled.is_empty() {
            log::info!(
                "Cancelling {} jobs and {} pending retries of slot {}: {}",
                cancelled.jobs.len(),
                cancelled.retries.len(),
                slot_id,
                reason
            );
        }

        self.mark_slot_stopped(slot_id, cause, reason);

        cancelled
    }

    /// Cancel every slot's remaining work (a shared stage failed, or a
    /// stop was asked for the whole execution). Teardown phases are NEVER
    /// cancelled: they must run for cleanup. Slots deferred under
    /// slot-first are cancelled whole, TeardownEach included: none of
    /// their setup ran, so there is nothing of theirs to tear down.
    ///
    /// `cause` is what the CALLER knows about why the run is stopping. It is
    /// deliberately not inferred here: a cancellation triggered by a phase
    /// that failed is a `PhaseFailure`, but the same code path also runs
    /// when a phase merely *finished* under an operator stop — and that must
    /// not be relabelled as a failure. Pass `None` when the cause was
    /// already recorded by whoever raised `shutdown_requested`.
    pub fn cancel_all_jobs(&mut self, reason: &str, cause: Option<ShutdownCause>) -> CancelledWork {
        let mut cancelled = CancelledWork::default();

        // Drain the queue but preserve teardown phases
        let all_jobs: Vec<Job> = self.job_queue.drain(..).collect();
        for job in all_jobs {
            if Self::is_teardown(&job) {
                self.job_queue.push_back(job);
            } else {
                cancelled.jobs.push(job);
            }
        }

        // Deferred slots were never enqueued, so they are not in the
        // submitted total yet; count them in before counting them done.
        for (_, jobs) in self.pending_slot_jobs.drain(..) {
            self.total_jobs_submitted += jobs.len();
            cancelled.jobs.extend(jobs);
        }

        for job in &cancelled.jobs {
            self.complete_as_cancelled(job.id, JobInfo::from_job(job), job.retry_count);
        }

        cancelled.retries = self.abort_pending_retries(|_| true);

        if !cancelled.is_empty() {
            log::info!(
                "Cancelling {} jobs and {} pending retries: {}",
                cancelled.jobs.len(),
                cancelled.retries.len(),
                reason
            );
        }

        let slots = self.slots.clone();
        for slot in &slots {
            self.mark_slot_stopped(slot, cause, reason);
        }

        // Only set shutdown flag if no teardown phases remain
        // (teardown phases must still run for cleanup)
        if self.job_queue.is_empty() && self.teardown_procedure_jobs.is_empty() {
            match cause {
                Some(cause) => self.request_shutdown(cause),
                // Caller has no cause of its own (a phase merely finished
                // under an existing stop): raise the flag, leave the cause
                // to whoever recorded it. `None` aggregates as Stop.
                None => self.shutdown_requested = true,
            }
        }

        cancelled
    }

    /// Add a job to the queue
    pub fn enqueue_job(&mut self, job: Job) {
        // Track job-slot relationship
        let slot_id = job.slot_id.clone();
        let job_id = job.id;

        if let Some(slot_id_str) = slot_id {
            self.job_to_slot.insert(job_id, slot_id_str.clone());
            self.slot_jobs
                .entry(slot_id_str)
                .or_default()
                .insert(job_id);
        }

        self.job_queue.push_back(job);
        self.total_jobs_submitted += 1;
    }

    /// Add a retry job to the front of the queue without incrementing total count
    pub fn enqueue_retry_job(&mut self, job: Job) {
        // Track job-slot relationship
        let slot_id = job.slot_id.clone();
        let job_id = job.id;

        if let Some(slot_id_str) = slot_id {
            self.job_to_slot.insert(job_id, slot_id_str.clone());
            self.slot_jobs
                .entry(slot_id_str)
                .or_default()
                .insert(job_id);
        }

        self.job_queue.push_front(job);
        // Don't increment total_jobs_submitted for retries
    }

    /// Get all workers currently processing jobs for a specific slot
    pub fn get_workers_for_slot(&self, slot_id: &str) -> Vec<usize> {
        let mut workers = Vec::new();

        if let Some(job_ids) = self.slot_jobs.get(slot_id) {
            for worker_id in 0..self.worker_state.num_workers() {
                if let Some(job_id) = self.worker_state.get_worker_job(worker_id) {
                    if job_ids.contains(&job_id) {
                        workers.push(worker_id);
                    }
                }
            }
        }

        workers
    }

    /// Check if a slot is complete and queue next slot if using slot-first execution
    pub fn check_and_queue_next_slot(&mut self) -> bool {
        // Don't queue new slots if shutdown was requested
        if self.shutdown_requested {
            return false;
        }

        // Check if there are pending slots to queue
        if self.pending_slot_jobs.is_empty() {
            // Check if we need to queue teardown procedure jobs
            // Must also check pending_delayed_retry_handles to avoid starting teardown
            // while a retry is still waiting to be enqueued
            if !self.teardown_procedure_jobs.is_empty()
                && self.job_queue.is_empty()
                && self.worker_state.count_busy() == 0
                && self.pending_delayed_retry_handles.is_empty()
            {
                log::trace!("📋 All slots complete, enqueueing teardown procedure phases");
                // Collect jobs first to avoid borrow issues
                let teardown_jobs: Vec<Job> = self.teardown_procedure_jobs.drain(..).collect();
                for job in teardown_jobs {
                    self.enqueue_job(job);
                }
                return true;
            }
            return false;
        }

        // Check if current slot work is complete (no jobs in queue, no busy workers,
        // and no pending delayed retries)
        if self.job_queue.is_empty()
            && self.worker_state.count_busy() == 0
            && self.pending_delayed_retry_handles.is_empty()
        {
            // Queue the next slot's jobs
            if !self.pending_slot_jobs.is_empty() {
                let (slot_id, jobs) = self.pending_slot_jobs.remove(0);
                log::trace!("📦 Slot complete, starting next slot: {}", slot_id);
                for job in jobs {
                    self.enqueue_job(job);
                }
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod stop_scope_tests {
    use super::*;
    use crate::procedure::schema::StageScope;

    use crate::test_support::job;

    /// First cause wins. An operator kill that landed first must not be
    /// relabelled by the engine winding down afterwards; the first fix's
    /// blocker was exactly a later writer overwriting the cause.
    #[test]
    fn request_shutdown_keeps_the_first_cause() {
        let mut state = OrchestratorState::new(1);
        state.request_shutdown(ShutdownCause::Operator);
        state.request_shutdown(ShutdownCause::PhaseFailure);
        assert!(state.shutdown_requested);
        assert_eq!(state.shutdown_cause, Some(ShutdownCause::Operator));
    }

    /// `cancel_all_jobs` raises the flag and records the caller's cause
    /// only once no teardown phase is left to run. With a teardown phase
    /// queued, neither moves: the aggregation must then rely on the
    /// results themselves (`then: {pass: stop}` case).
    #[test]
    fn cancel_all_jobs_records_the_cause_only_when_the_queue_drains() {
        let mut with_teardown = OrchestratorState::new(1);
        with_teardown.enqueue_job(job(StageScope::Main));
        with_teardown.enqueue_job(job(StageScope::TeardownAll));
        let cancelled = with_teardown.cancel_all_jobs("x", Some(ShutdownCause::PhaseFailure));
        assert_eq!(cancelled.jobs.len(), 1, "only the main phase is cancelled");
        assert!(!with_teardown.shutdown_requested);
        assert_eq!(with_teardown.shutdown_cause, None);

        let mut no_teardown = OrchestratorState::new(1);
        no_teardown.enqueue_job(job(StageScope::Main));
        no_teardown.cancel_all_jobs("x", Some(ShutdownCause::PhaseFailure));
        assert!(no_teardown.shutdown_requested);
        assert_eq!(no_teardown.shutdown_cause, Some(ShutdownCause::PhaseFailure));

        // A caller with no cause raises the flag and leaves the cause alone.
        let mut no_cause = OrchestratorState::new(1);
        no_cause.enqueue_job(job(StageScope::Main));
        no_cause.cancel_all_jobs("x", None);
        assert!(no_cause.shutdown_requested);
        assert_eq!(no_cause.shutdown_cause, None);
    }

    /// Stop scope = job scope. Cancelling slot A leaves slot B's queued
    /// main phase alone, keeps A's TeardownEach, aborts A's delayed retry
    /// (resolving its dependency), and records A as stopped with the
    /// caller's cause. The execution flag stays down.
    #[tokio::test]
    async fn cancel_slot_jobs_touches_one_slot_only() {
        let mut state = OrchestratorState::new(1);
        state.slots = vec!["a".into(), "b".into()];
        state.enqueue_job(slot_job(StageScope::Main, "a"));
        state.enqueue_job(slot_job(StageScope::Main, "b"));
        state.enqueue_job(slot_job(StageScope::TeardownEach, "a"));
        state.enqueue_job(slot_job(StageScope::TeardownEach, "b"));
        state.enqueue_job(job(StageScope::TeardownAll));

        let retry_id = Uuid::new_v4();
        let dep_id = Uuid::new_v4();
        let handle = tokio::spawn(async { tokio::time::sleep(std::time::Duration::from_secs(60)).await });
        state.pending_delayed_retry_handles.push(PendingDelayedRetry {
            handle,
            phase_key: "k".into(),
            phase_name: "Phase".into(),
            function: "f".into(),
            slot_id: Some("a".into()),
            job_id: retry_id,
            dependency_id: dep_id,
            retry_count: 2,
        });

        let cancelled = state.cancel_slot_jobs("a", Some(ShutdownCause::PhaseFailure), "a failed");

        assert_eq!(cancelled.jobs.len(), 1, "only A's main phase is cancelled");
        assert_eq!(cancelled.retries.len(), 1, "A's pending retry is aborted");
        assert!(cancelled.retries[0].handle.is_finished() || {
            tokio::task::yield_now().await;
            cancelled.retries[0].handle.is_finished()
        });
        assert!(state.completed_jobs.contains(&dep_id), "aborted retry unblocks dependents");
        assert_eq!(
            state.job_results[&retry_id].retry_count, 2,
            "the skip records the attempt that was pending"
        );
        assert!(state.pending_delayed_retry_handles.is_empty());
        assert_eq!(state.job_queue.len(), 4, "B's main, both TeardownEach, TeardownAll stay");
        assert!(state.job_queue.iter().any(|j| j.slot_id.as_deref() == Some("b")
            && matches!(j.stage_scope, StageScope::Main)));
        assert!(state.is_slot_stopped("a"));
        assert!(!state.is_slot_stopped("b"));
        assert_eq!(state.slot_stops["a"].cause, Some(ShutdownCause::PhaseFailure));
        assert!(!state.shutdown_requested, "a slot stop never raises the execution flag");
        assert!(state.is_stopping_for(Some("a")));
        assert!(!state.is_stopping_for(Some("b")));
        assert!(!state.is_stopping_for(None));
    }

    /// An execution-wide cancel marks every slot, drains the slots still
    /// deferred under slot-first (their TeardownEach included: nothing
    /// of theirs was set up), and leaves the flag down while a shared
    /// teardown is still to run.
    #[test]
    fn cancel_all_jobs_marks_every_slot_and_drains_deferred_slots() {
        let mut state = OrchestratorState::new(1);
        state.slots = vec!["a".into(), "b".into()];
        state.enqueue_job(slot_job(StageScope::Main, "a"));
        state.enqueue_job(slot_job(StageScope::TeardownEach, "a"));
        state.pending_slot_jobs = vec![(
            "b".into(),
            vec![slot_job(StageScope::Main, "b"), slot_job(StageScope::TeardownEach, "b")],
        )];
        state.teardown_procedure_jobs = vec![job(StageScope::TeardownAll)];

        let cancelled = state.cancel_all_jobs("setup failed", Some(ShutdownCause::PhaseFailure));

        assert_eq!(cancelled.jobs.len(), 3, "A's main plus both of B's deferred jobs");
        assert_eq!(state.total_jobs_submitted, 4, "B's deferred jobs join the submitted total");
        assert_eq!(state.original_jobs_completed, 3);
        assert!(state.pending_slot_jobs.is_empty());
        assert_eq!(state.job_queue.len(), 1, "A's TeardownEach stays");
        assert!(state.is_slot_stopped("a") && state.is_slot_stopped("b"));
        assert_eq!(state.slot_stops["b"].cause, Some(ShutdownCause::PhaseFailure));
        assert!(!state.shutdown_requested, "TeardownEach and TeardownAll still have to run");
        assert!(!state.is_complete());
    }

    /// The operator path: a slot that finished every phase keeps no
    /// outstanding work and must not be marked; one with a job queued is.
    #[test]
    fn mark_outstanding_slots_stopped_spares_finished_slots() {
        let mut state = OrchestratorState::new(1);
        state.slots = vec!["done".into(), "live".into(), "never".into(), "empty".into()];
        let finished = slot_job(StageScope::Main, "done");
        let finished_id = finished.id;
        state.enqueue_job(finished);
        state.enqueue_job(slot_job(StageScope::Main, "live"));
        state.pending_slot_jobs = vec![("never".into(), vec![slot_job(StageScope::Main, "never")])];
        state.job_info.insert(finished_id, JobInfo::from_job(&state.job_queue[0]));
        let popped = state.job_queue.pop_front().unwrap();
        state.complete_job(popped.id, skipped());

        state.request_shutdown(ShutdownCause::Operator);
        state.mark_outstanding_slots_stopped("Execution stopped by user");

        assert!(!state.is_slot_stopped("done"));
        assert_eq!(state.slot_stops["live"].cause, Some(ShutdownCause::Operator));
        assert!(state.is_slot_stopped("never"), "a deferred slot that never started was cut too");
        assert!(!state.is_slot_stopped("empty"), "a slot with no job of its own has nothing to cut");

        // A shared teardown still queued: every slot, the empty one
        // included, was cut before its execution finished.
        let mut with_shared = OrchestratorState::new(1);
        with_shared.slots = vec!["empty".into()];
        with_shared.enqueue_job(job(StageScope::TeardownAll));
        with_shared.request_shutdown(ShutdownCause::Operator);
        with_shared.mark_outstanding_slots_stopped("Execution stopped by user");
        assert!(with_shared.is_slot_stopped("empty"));
    }

    fn skipped() -> JobResult {
        OrchestratorState::skip_result(0)
    }

    fn slot_job(scope: StageScope, slot: &str) -> Job {
        let mut j = job(scope);
        j.slot_id = Some(slot.to_string());
        j
    }
}
