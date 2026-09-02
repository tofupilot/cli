//! Statistics calculation and outcome determination

use std::collections::HashMap;

use uuid::Uuid;
use crate::job::{JobResult, Outcome, PhaseResult};
use crate::procedure::schema::PhaseNextAction;
use crate::state::{JobInfo, ShutdownCause};

use super::{ExecutionStats, Orchestrator};

/// Key to identify a unique phase instance (phase_key + slot_id)
#[derive(Hash, Eq, PartialEq, Clone)]
struct PhaseInstanceKey {
    phase_key: String,
    slot_id: Option<String>,
}

impl Orchestrator {
    /// Get only the final attempt for each phase (highest retry_count per phase_key+slot_id)
    fn get_final_attempts<'a>(
        job_results: &'a HashMap<Uuid, JobResult>,
        job_info: &HashMap<Uuid, JobInfo>,
    ) -> Vec<&'a JobResult> {
        let mut best_per_phase: HashMap<PhaseInstanceKey, (usize, &'a JobResult)> = HashMap::new();

        for (job_id, result) in job_results {
            if let Some(info) = job_info.get(job_id) {
                let key = PhaseInstanceKey {
                    phase_key: info.phase_key.clone(),
                    slot_id: info.slot_id.clone(),
                };

                let dominated = best_per_phase
                    .get(&key)
                    .map(|(count, _)| result.retry_count <= *count)
                    .unwrap_or(false);

                if !dominated {
                    best_per_phase.insert(key, (result.retry_count, result));
                }
            }
        }

        best_per_phase.into_values().map(|(_, r)| r).collect()
    }

    /// Filter job results to only include jobs for a specific slot, returning final attempts only
    fn get_final_attempts_for_slot<'a>(
        job_results: &'a HashMap<Uuid, JobResult>,
        job_info: &HashMap<Uuid, JobInfo>,
        job_to_slot: &HashMap<Uuid, String>,
        slot_id: &str,
    ) -> Vec<&'a JobResult> {
        let mut best_per_phase: HashMap<String, (usize, &'a JobResult)> = HashMap::new();

        for (job_id, result) in job_results {
            if job_to_slot.get(job_id) != Some(&slot_id.to_string()) {
                continue;
            }

            if let Some(info) = job_info.get(job_id) {
                let dominated = best_per_phase
                    .get(&info.phase_key)
                    .map(|(count, _)| result.retry_count <= *count)
                    .unwrap_or(false);

                if !dominated {
                    best_per_phase.insert(info.phase_key.clone(), (result.retry_count, result));
                }
            }
        }

        best_per_phase.into_values().map(|(_, r)| r).collect()
    }

    pub async fn get_stats(&self) -> ExecutionStats {
        let state = self.state.read().await;
        let workers = self.workers.read().await;

        let failed_jobs = state
            .job_results
            .values()
            .filter(|r| r.is_failure())
            .count();

        let busy_workers = state.worker_state.count_busy();
        let running_jobs = busy_workers;

        let run_outcome = if state.is_complete() {
            let final_attempts = Self::get_final_attempts(&state.job_results, &state.job_info);
            Some(determine_aggregate_outcome(&final_attempts, state.shutdown_requested, state.shutdown_cause, &state.init_error))
        } else {
            None
        };

        // No local report archive anymore (ReportManager removed); the CLI
        // owns run-dir + run-id at upload time.
        let run_dir = None;

        // Per-slot outcomes still come straight from job results; the slot
        // set is the orchestrator's own `slot_jobs` keys (previously the
        // report-manager keys). `slot_run_ids` were report-archive-internal
        // UUIDs the CLI never used — dropped.
        let slot_outcomes = if state.is_complete() {
            let mut outcomes = HashMap::new();
            for slot_id in state.slot_jobs.keys() {
                let slot_final_attempts = Self::get_final_attempts_for_slot(
                    &state.job_results,
                    &state.job_info,
                    &state.job_to_slot,
                    slot_id,
                );

                let slot_outcome =
                    determine_aggregate_outcome(&slot_final_attempts, state.shutdown_requested, state.shutdown_cause, &state.init_error);
                outcomes.insert(slot_id.clone(), slot_outcome);
            }
            outcomes
        } else {
            HashMap::new()
        };
        let slot_run_ids = HashMap::new();

        ExecutionStats {
            total_jobs: state.total_jobs_submitted,
            completed_jobs: state.original_jobs_completed,
            failed_jobs,
            running_jobs,
            queued_jobs: state.job_queue.len(),
            workers_busy: busy_workers,
            workers_total: workers.len(),
            run_outcome,
            run_dir,
            run_id: Some(self.run_id.clone()),
            slot_outcomes,
            slot_run_ids,
            start_time: self.start_time,
            end_time: self.end_time,
        }
    }

    pub(super) async fn emit_stats(&self) {
        let stats = self.get_stats().await;

        self.event_sink.emit(&crate::event_sink::ExecutionEvent::Stats {
            total_jobs: stats.total_jobs,
            completed_jobs: stats.completed_jobs,
            failed_jobs: stats.failed_jobs,
            running_jobs: stats.running_jobs,
            queued_jobs: stats.queued_jobs,
            workers_busy: stats.workers_busy,
            workers_total: stats.workers_total,
            run_outcome: stats.run_outcome,
            run_dir: stats.run_dir,
            run_id: stats.run_id,
            slot_outcomes: stats.slot_outcomes,
            slot_run_ids: stats.slot_run_ids,
            start_time: stats.start_time,
            end_time: stats.end_time,
        });
    }
}

/// Aggregate run outcome from the final attempt of every phase.
///
/// Priority: ERROR → STOP → TIMEOUT → FAIL → PASS. `Stop` means *this run
/// was interrupted*, never *the DUT failed*.
///
/// Invariants:
/// - A raised `shutdown_requested` never aggregates to PASS. Work the stop
///   prevented is not in `job_results`, so an empty or partial set cannot
///   be told from a complete one. Only a failure IN THIS SET, under a
///   `PhaseFailure` cause, earns the fall-through to TIMEOUT / FAIL.
/// - A phase whose outcome is Stop because the flag was up (in flight, or
///   `new_interrupted`) did not ask to stop. Only `phase.stop()` and
///   `then: {…: stop}` on a passing or skipped phase are requests.
/// - ERROR outranks every interruption. The operator paths record the
///   phase they interrupt as `new_interrupted` (no error), so an `error`
///   here is one a phase actually raised.
///
/// Not derived from `should_stop_test()`: that reads the *decision* to
/// halt, which the default `on_first_failure: stop` also gives a merely
/// failing phase. Free function: pure in its four arguments and testable.
/// Why the cause travels with the flag is on `ShutdownCause`.
fn determine_aggregate_outcome(
    job_results: &[&JobResult],
    shutdown_requested: bool,
    shutdown_cause: Option<ShutdownCause>,
    init_error: &Option<String>,
) -> Outcome {
    if init_error.is_some() {
        return Outcome::Error;
    }

    // ERROR outranks every interruption. That is safe only because the
    // operator paths no longer manufacture an error for the phase they
    // interrupt (`JobResult::new_interrupted`), so an `error` here is one
    // a phase actually raised.
    let has_error = job_results.iter().any(|r| r.error.is_some());
    if has_error {
        return Outcome::Error;
    }

    let has_timeout = job_results.iter().any(|r| r.timeout_secs.is_some());
    // Includes retry limit exceeded.
    let has_failure = job_results
        .iter()
        .any(|r| r.is_failure() || matches!(r.phase_outcome, Outcome::Fail));

    // Interrupted unless proven failed, with no exemption. Work the stop
    // prevented never enters `job_results`, so an empty or partial set is
    // indistinguishable from a set that ran everything: nothing can be
    // concluded from the results alone, and a PASS under a raised flag
    // is the one answer that must never come out. Only a failure that is
    // in THIS set earns the fall-through.
    if shutdown_requested {
        let own_failure = has_timeout || has_failure;
        match shutdown_cause {
            Some(ShutdownCause::PhaseFailure) if own_failure => {}
            _ => return Outcome::Stop,
        }
    }


    // Only a phase that ASKED to stop counts: an explicit `phase.stop()`,
    // or `then: {…: stop}` on a phase that passed or skipped. A phase
    // whose own outcome is Stop got it from the raised flag (it was in
    // flight, or `new_interrupted`); `next_action` is Stop for those too,
    // so it is not a request and must not turn a FAIL run into STOP.
    let has_requested_stop = job_results.iter().any(|r| {
        matches!(r.phase_result, PhaseResult::Stop)
            || (r.next_action == Some(PhaseNextAction::Stop)
                && matches!(r.phase_outcome, Outcome::Pass | Outcome::Skip))
    });
    if has_requested_stop {
        return Outcome::Stop;
    }

    if has_timeout {
        return Outcome::Timeout;
    }
    if has_failure {
        return Outcome::Fail;
    }

    Outcome::Pass
}

#[cfg(test)]
mod aggregate_outcome_tests {
    use super::*;

    /// A phase result in its post-completion shape: `next_action` is
    /// already computed, which is what `determine_aggregate_outcome`
    /// sees in production.
    fn result(
        phase_result: PhaseResult,
        phase_outcome: Outcome,
        next_action: Option<PhaseNextAction>,
    ) -> JobResult {
        let now = chrono::Utc::now();
        JobResult {
            phase_result,
            phase_outcome,
            next_action,
            timeout_secs: None,
            error: None,
            exit_code: None,
            measurements: vec![],
            logs: vec![],
            started_at: now,
            completed_at: now,
            resource_metrics: None,
            unit: None,
            input_unit_info: None,
            retry_count: 0,
            run_metadata: Default::default(),
            unit_metadata: Default::default(),
        }
    }

    fn pass() -> JobResult {
        result(
            PhaseResult::Continue,
            Outcome::Pass,
            Some(PhaseNextAction::Continue),
        )
    }

    fn aggregate(results: &[&JobResult], shutdown: bool) -> Outcome {
        determine_aggregate_outcome(results, shutdown, None, &None)
    }

    /// The stop-on-first-failure path as the orchestrator actually leaves
    /// it: `cancel_all_jobs` raised `shutdown_requested` AND recorded the
    /// cause.
    fn aggregate_stopped_on_failure(results: &[&JobResult]) -> Outcome {
        determine_aggregate_outcome(results, true, Some(ShutdownCause::PhaseFailure), &None)
    }

    /// An operator kill, graceful or forced.
    fn aggregate_killed(results: &[&JobResult]) -> Outcome {
        determine_aggregate_outcome(results, true, Some(ShutdownCause::Operator), &None)
    }

    /// Under the default `on_first_failure: stop` a failing phase carries
    /// `next_action == Stop`. A failing unit is FAIL, not an aborted run
    /// (the CLI uploads Stop as ABORTED).
    #[test]
    fn failing_phase_under_stop_on_first_failure_is_fail_not_stop() {
        let failed = result(
            PhaseResult::Fail,
            Outcome::Fail,
            Some(PhaseNextAction::Stop),
        );
        let p = pass();
        // Both shapes must give FAIL: phases still in flight when the
        // failure lands (`shutdown_requested` not yet raised), and the
        // settled state after `cancel_all_jobs` cancelled the rest.
        assert_eq!(aggregate(&[&p, &failed], false), Outcome::Fail);
        assert_eq!(aggregate_stopped_on_failure(&[&p, &failed]), Outcome::Fail);
    }

    /// The other half of the same decision: an explicit `phase.stop()`
    /// from the test code really is an interruption and stays STOP.
    #[test]
    fn explicit_phase_stop_is_stop() {
        let stopped = result(
            PhaseResult::Stop,
            Outcome::Stop,
            Some(PhaseNextAction::Stop),
        );
        let p = pass();
        assert_eq!(aggregate(&[&p, &stopped], false), Outcome::Stop);
    }

    /// Force kill with a phase in flight: the killed job is recorded as
    /// `new_interrupted` (outcome Stop, no error), and the run is Stop,
    /// not ERROR.
    #[test]
    fn force_killed_running_phase_reads_stop_not_error() {
        let p = pass();
        let killed = JobResult::new_interrupted("Force killed by user".to_string());
        assert!(killed.error.is_none());
        assert_eq!(aggregate_killed(&[&p, &killed]), Outcome::Stop);
    }

    /// A genuine error recorded before the operator stopped (a phase
    /// raised, the procedure continued) still surfaces as ERROR.
    #[test]
    fn genuine_error_before_operator_stop_stays_error() {
        let mut crashed = result(PhaseResult::Continue, Outcome::Error, Some(PhaseNextAction::Continue));
        crashed.error = Some("Traceback: ValueError".to_string());
        let killed = JobResult::new_interrupted("Force killed by user".to_string());
        assert_eq!(aggregate_killed(&[&crashed, &killed]), Outcome::Error);
    }

    /// Multi-slot under an operator stop: every slot reads Stop, whether
    /// or not it had finished. Same reason as the sibling-failure case.
    #[test]
    fn slot_finished_before_operator_stop_reads_stop() {
        let a = pass();
        let b = pass();
        assert_eq!(aggregate_killed(&[&a, &b]), Outcome::Stop);
        let killed = JobResult::new_interrupted("Force killed by user".to_string());
        assert_eq!(aggregate_killed(&[&a, &b, &killed]), Outcome::Stop);
    }

    /// The kill button, via the shutdown handler. Takes precedence over
    /// a phase that had already failed.
    #[test]
    fn operator_kill_is_stop_even_with_a_failed_phase() {
        let failed = result(
            PhaseResult::Fail,
            Outcome::Fail,
            Some(PhaseNextAction::Stop),
        );
        assert_eq!(aggregate_killed(&[&failed]), Outcome::Stop);
    }

    /// Defensive: a shutdown whose cause was never recorded is an
    /// interruption, never a PASS. This is the shape the CLI's graceful
    /// Stop arm produced before it went through `request_shutdown`.
    #[test]
    fn shutdown_without_a_recorded_cause_stays_stop() {
        let p = pass();
        assert_eq!(aggregate(&[&p], true), Outcome::Stop);
    }

    /// An operator stops a run whose phases had all passed: the in-flight
    /// phase resolves to Stop, the rest is cancelled. Stop whatever cause
    /// got recorded, never PASS.
    #[test]
    fn operator_stop_on_all_passing_phases_is_never_pass() {
        let p = pass();
        let interrupted = result(
            PhaseResult::Continue,
            Outcome::Stop,
            Some(PhaseNextAction::Stop),
        );
        let skipped = result(PhaseResult::Skip, Outcome::Skip, None);
        for cause in [None, Some(ShutdownCause::Operator), Some(ShutdownCause::PhaseFailure)] {
            assert_eq!(
                determine_aggregate_outcome(&[&p, &interrupted, &skipped], true, cause, &None),
                Outcome::Stop,
                "cause {cause:?}"
            );
        }
    }

    /// Multi-slot: slot A fails under `on_first_failure: stop`, slot B's
    /// queued phases are cancelled as SKIP while everything it ran passed.
    /// Slot B's aggregation sees the run-wide `PhaseFailure` cause but owns
    /// no failure: it was never tested to the end and is `Stop`, not PASS.
    #[test]
    fn sibling_slot_cancelled_by_another_slots_failure_is_stop_not_pass() {
        let p = pass();
        let skipped = result(PhaseResult::Skip, Outcome::Skip, None);
        assert_eq!(aggregate_stopped_on_failure(&[&p, &skipped]), Outcome::Stop);
    }

    /// `then: {pass: stop}` asks to end the run after a passing phase. With
    /// a teardown stage queued the flag is never raised, so it must be
    /// read off the result itself.
    #[test]
    fn then_pass_stop_is_an_interruption_with_or_without_the_flag() {
        let gate = result(
            PhaseResult::Continue,
            Outcome::Pass,
            Some(PhaseNextAction::Stop),
        );
        let skipped = result(PhaseResult::Skip, Outcome::Skip, None);
        // teardown queued: flag never raised
        assert_eq!(aggregate(&[&gate, &skipped], false), Outcome::Stop);
        // no teardown: flag raised by cancel_all_jobs with no cause
        assert_eq!(aggregate(&[&gate, &skipped], true), Outcome::Stop);
    }

    /// Multi-slot: slot B ran every phase and passed before slot A failed.
    /// B reads Stop, as on main: "nothing of B was cut short" cannot be
    /// read off the results (prevented work never enters `job_results`).
    /// `slot_outcomes` has no consumer today; a per-slot completeness
    /// signal from the queue is the way to refine this.
    #[test]
    fn slot_finished_before_sibling_failed_reads_stop() {
        let a = pass();
        let b = pass();
        assert_eq!(aggregate_stopped_on_failure(&[&a, &b]), Outcome::Stop);
    }

    /// An operator Stop that lands before any phase reports (startup,
    /// identify, a phase boundary) leaves `job_results` empty or partial
    /// with the flag up. Stop, never PASS: a false PASS ships an untested
    /// unit.
    #[test]
    fn operator_stop_before_any_phase_reported_is_stop() {
        assert_eq!(aggregate_killed(&[]), Outcome::Stop);
        let p = pass();
        assert_eq!(aggregate_killed(&[&p]), Outcome::Stop);
        assert_eq!(aggregate(&[], true), Outcome::Stop);
        assert_eq!(aggregate(&[&p], true), Outcome::Stop);
    }

    /// Multi-worker: a sibling phase still running when another phase
    /// failed finishes under the raised flag and is given `Outcome::Stop`
    /// by `resolve_outcome` (and `next_action == Stop` from that). That is
    /// not a request to stop, it is collateral. The run failed: FAIL.
    /// Both flag shapes, since a queued teardown keeps the flag down.
    #[test]
    fn sibling_in_flight_when_a_phase_fails_does_not_turn_fail_into_stop() {
        let failed = result(PhaseResult::Fail, Outcome::Fail, Some(PhaseNextAction::Stop));
        let sibling = result(PhaseResult::Continue, Outcome::Stop, Some(PhaseNextAction::Stop));
        assert_eq!(aggregate_stopped_on_failure(&[&failed, &sibling]), Outcome::Fail);
        assert_eq!(aggregate(&[&failed, &sibling], false), Outcome::Fail);
    }

    /// And the slot that DID fail keeps its FAIL under the same cause.
    #[test]
    fn failing_slot_under_stop_on_first_failure_is_fail() {
        let failed = result(
            PhaseResult::Fail,
            Outcome::Fail,
            Some(PhaseNextAction::Stop),
        );
        let skipped = result(PhaseResult::Skip, Outcome::Skip, None);
        assert_eq!(aggregate_stopped_on_failure(&[&failed, &skipped]), Outcome::Fail);
    }

    /// Same class as the FAIL case: a timed-out phase also gets
    /// `next_action == Stop` under `on_first_failure: stop`, and used to
    /// be reported as an abort. It belongs in TIMEOUT.
    #[test]
    fn timed_out_phase_under_stop_on_first_failure_is_timeout() {
        let mut timed_out = result(
            PhaseResult::Fail,
            Outcome::Timeout,
            Some(PhaseNextAction::Stop),
        );
        timed_out.timeout_secs = Some(30);
        assert_eq!(aggregate(&[&timed_out], false), Outcome::Timeout);
        assert_eq!(aggregate_stopped_on_failure(&[&timed_out]), Outcome::Timeout);
    }

    /// ERROR outranks every stop, operator included: a crashed phase is
    /// never masked by an interruption. The kill no longer manufactures
    /// an error, so this does not turn the kill button into a crash.
    #[test]
    fn errored_phase_outranks_everything() {
        let mut errored = result(
            PhaseResult::Fail,
            Outcome::Error,
            Some(PhaseNextAction::Stop),
        );
        errored.error = Some("boom".to_string());
        assert_eq!(aggregate_killed(&[&errored]), Outcome::Error);
        assert_eq!(aggregate_stopped_on_failure(&[&errored]), Outcome::Error);
    }

    #[test]
    fn init_error_is_error() {
        let p = pass();
        assert_eq!(
            determine_aggregate_outcome(&[&p], false, None, &Some("bad procedure".to_string())),
            Outcome::Error
        );
    }

    #[test]
    fn all_phases_passing_is_pass() {
        let a = pass();
        let b = pass();
        assert_eq!(aggregate(&[&a, &b], false), Outcome::Pass);
    }

    /// A skipped phase is not a failure. The phases cancelled after a
    /// stop-on-failure arrive here as SKIP, so this guards the fix
    /// against turning them into a FAIL of their own.
    #[test]
    fn skipped_phases_do_not_make_a_run_fail() {
        let p = pass();
        let skipped = result(PhaseResult::Skip, Outcome::Skip, None);
        assert_eq!(aggregate(&[&p, &skipped], false), Outcome::Pass);
    }
}
