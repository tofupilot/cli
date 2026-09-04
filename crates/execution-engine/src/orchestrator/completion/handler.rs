


use crate::job::{JobResult, Outcome};
use crate::procedure::schema::{PhaseNextAction, StageScope};

use super::super::{JobCompletionEvent, Orchestrator};
use super::{error_handling, event_emitter, next_action, outcome_resolver};

impl Orchestrator {
    pub(in crate::orchestrator) async fn handle_job_completion(
        &self,
        event: JobCompletionEvent,
    ) -> bool {
        log::debug!(
            "Handling job completion for {}",
            event.original_job.phase_name
        );

        let mut job_result = match &event.result {
            Ok(result) => result.clone(),
            Err(e) => error_handling::convert_error_to_result(
                e.to_string(),
                &event.original_job,
                event.job_id,
            ),
        };

        // Stop scope = job scope: a phase finishing while ITS slot is
        // stopping (or the execution is) was cut short, whatever it
        // reports. Single slot: identical to reading the execution flag.
        let (shutdown_requested, should_stop_on_first_failure) = {
            let state = self.state.read().await;
            (
                state.is_stopping_for(event.original_job.slot_id.as_deref()),
                state.should_stop_on_first_failure,
            )
        };

        // When a stop cuts a phase, the worker surfaces the cut as an error
        // (a killed body's traceback, a cancelled operator prompt's
        // "required input missing"). That error is a consequence of the
        // stop, not a real phase failure — drop it so the outcome resolver
        // classifies the phase as STOP, not ERROR. Without this the UI
        // flickers from "aborted" to "error" when the phase reports back,
        // and a slot whose neighbour phase failed reads ERROR.
        if shutdown_requested {
            job_result.error = None;
        }

        let (phase_outcome, is_retry_limit_exceeded) =
            outcome_resolver::resolve_outcome(&job_result, &event.original_job, shutdown_requested);

        let phase_def = self.get_phase_definition(&event);

        let error_message = outcome_resolver::format_error_message(
            is_retry_limit_exceeded,
            event.original_job.retry_limit,
            &job_result,
        );

        log::debug!(
            "DEBUG Phase '{}': phase_result={:?}, phase_outcome={:?}, retry_count={}, retry_limit={}, can_retry={}",
            event.original_job.phase_name,
            job_result.phase_result,
            phase_outcome,
            event.original_job.retry_count,
            event.original_job.retry_limit,
            event.original_job.can_retry()
        );

        let next_action = next_action::determine_next_action(
            &job_result,
            &phase_outcome,
            phase_def,
            should_stop_on_first_failure,
        );

        log::debug!(
            "DEBUG Phase '{}': next_action={:?}",
            event.original_job.phase_name,
            next_action
        );

        job_result.phase_outcome = phase_outcome;
        job_result.next_action = Some(next_action.clone());

        event_emitter::log_resource_metrics(&event.original_job, &job_result);
        event_emitter::log_phase_completion(
            &event.original_job,
            &job_result,
            phase_outcome,
            &error_message,
        );

        event_emitter::emit_job_complete_event(
            &self.event_sink,
            event.job_id,
            &event.original_job,
            &job_result,
            phase_outcome,
            error_message.clone(),
            event.worker_id,
            is_retry_limit_exceeded,
        );

        self.handle_plug_teardown(&event).await;

        let mut state = self.state.write().await;

        let is_setup_failure = matches!(
            event.original_job.stage_scope,
            StageScope::SetupAll | StageScope::SetupEach
        ) && (matches!(
            phase_outcome,
            Outcome::Fail | Outcome::Error | Outcome::Timeout | Outcome::Stop
        ) || is_retry_limit_exceeded);

        if is_setup_failure {
            self.handle_setup_failure(&mut state, &event, &job_result).await;
        }

        let should_continue = self
            .apply_next_action(
                next_action,
                &mut state,
                event,
                job_result,
            )
            .await;

        drop(state);

        self.emit_stats().await;

        should_continue
    }

    fn get_phase_definition(
        &self,
        event: &JobCompletionEvent,
    ) -> Option<&crate::procedure::schema::PhaseDefinition> {
        let all_phases = self.procedure_definition.get_all_phases_with_stage_scope();
        all_phases
            .iter()
            .find(|(stage, phase)| {
                *stage == event.original_job.stage_scope
                    && phase.key == event.original_job.phase_key
            })
            .map(|(_, phase)| *phase)
    }

    async fn handle_plug_teardown(
        &self,
        event: &JobCompletionEvent,
    ) {
        if let Some(ref slot_id) = event.original_job.slot_id {
            if matches!(event.original_job.stage_scope, StageScope::TeardownEach) {
                log::info!(
                    "Destroying slot-level plugs for {} after TeardownSlot phase",
                    slot_id
                );

                self.emit_plug_scope_event("running").await;

                let resource_manager = self.resource_manager.write().await;
                if resource_manager.has_each_scope_plugs(&slot_id).await {
                    match resource_manager
                        .destroy_each_scope_plugs(slot_id.clone(), &self.event_sink)
                        .await
                    {
                        Ok(_) => {
                            self.emit_plug_scope_event("pass").await;
                        }
                        Err(e) => {
                            log::warn!("Failed to destroy each-scope plugs for {}: {}", slot_id, e);
                            self.emit_plug_scope_event("error").await;
                        }
                    }

                    self.emit_stats().await;
                }
            }
        }

        if matches!(event.original_job.stage_scope, StageScope::TeardownAll) {
            log::info!("Destroying all-scope plugs after TeardownAll phase");

            self.emit_plug_scope_event("running").await;

            let resource_manager = self.resource_manager.write().await;
            if resource_manager.has_all_scope_plugs().await {
                match resource_manager.destroy_all_scope_plugs(&self.event_sink).await {
                    Ok(_) => {
                        self.emit_plug_scope_event("pass").await;
                    }
                    Err(e) => {
                        log::warn!("Failed to destroy all-scope plugs: {}", e);
                        self.emit_plug_scope_event("error").await;
                    }
                }

                self.emit_stats().await;
            }
        }
    }

    /// A failed setup stage cancels the work that depended on it, whatever
    /// `on_first_failure` says: SetupAll the execution, SetupEach its slot.
    /// Same dispatch as `handle_stop`; only the log line differs.
    async fn handle_setup_failure(
        &self,
        state: &mut crate::state::OrchestratorState,
        event: &JobCompletionEvent,
        job_result: &JobResult,
    ) {
        let job = &event.original_job;
        match job.stage_scope {
            StageScope::SetupAll => log::warn!(
                "Setup procedure failed: Cancelling all slots and ensuring teardown runs"
            ),
            StageScope::SetupEach => log::warn!(
                "Setup slot failed for {}: Skipping to teardown slot",
                job.slot_id.as_deref().unwrap_or("null")
            ),
            _ => return,
        }
        self.cancel_scope_of(
            state,
            job,
            job_result,
            Some(crate::state::ShutdownCause::PhaseFailure),
            "setup failure",
        )
        .await;
    }

    /// Stop scope = job scope. A slot job cancels its slot, a shared job
    /// cancels the execution. Emits Skipped for everything removed.
    ///
    /// `stop_reason` is the wire text quoted on the cancelled UI phases;
    /// its shape ("Run aborted by phase '…': …") is what single-slot runs
    /// have always shown, so it is kept verbatim there.
    async fn cancel_scope_of(
        &self,
        state: &mut crate::state::OrchestratorState,
        job: &crate::job::Job,
        job_result: &JobResult,
        cause: Option<crate::state::ShutdownCause>,
        why: &str,
    ) {
        let detail = job_result
            .error
            .as_ref()
            .map(|e| format!(": {e}"))
            .unwrap_or_default();
        let stop_reason = match &job.slot_id {
            Some(slot) if state.slots.len() > 1 => {
                format!("Slot '{slot}' aborted by phase '{}'{detail}", job.phase_name)
            }
            _ => format!("Run aborted by phase '{}'{detail}", job.phase_name),
        };

        let cancelled = match &job.slot_id {
            Some(slot_id) => state.cancel_slot_jobs(slot_id, cause, &stop_reason),
            None => state.cancel_all_jobs(&stop_reason, cause),
        };

        self.emit_cancelled_work(
            &cancelled,
            &format!("Cancelled due to {} in phase {}", why, job.phase_name),
        )
        .await;
    }

    async fn apply_next_action(
        &self,
        next_action: PhaseNextAction,
        state: &mut crate::state::OrchestratorState,
        event: JobCompletionEvent,
        job_result: JobResult,
    ) -> bool {
        let outcome = job_result.phase_outcome;

        if matches!(outcome, Outcome::Stop) {
            self.handle_stop(state, event, job_result).await;
            return false;
        }

        match next_action {
            PhaseNextAction::Retry => self.handle_retry(state, event, job_result).await,
            PhaseNextAction::Stop => {
                self.handle_stop(state, event, job_result).await;
                false
            }
            PhaseNextAction::Continue => {
                state.complete_job_with_info(event.job_id, &event.original_job, job_result);
                true
            }
        }
    }

    async fn handle_retry(
        &self,
        state: &mut crate::state::OrchestratorState,
        event: JobCompletionEvent,
        job_result: JobResult,
    ) -> bool {
        // No retry of a main phase once its scope is stopping: the slot
        // (`slot_stops`) or the execution (`shutdown_requested`). A job
        // that was running during the cancel still completes after it,
        // and would otherwise re-enqueue a phase the orchestrator gave up
        // on. Teardown phases keep their retry budget: they are exactly
        // the phases that DO run during a stop.
        let is_teardown = matches!(
            event.original_job.stage_scope,
            crate::procedure::schema::StageScope::TeardownEach
                | crate::procedure::schema::StageScope::TeardownAll
        );
        let shutdown_in_progress =
            !is_teardown && state.is_stopping_for(event.original_job.slot_id.as_deref());
        let should_retry = event.original_job.can_retry() && !shutdown_in_progress;

        if !should_retry {
            state.complete_job_with_info(event.job_id, &event.original_job, job_result);
            // emit_stats is called by handle_job_completion after releasing state lock
            return true;
        }

        let retry_job = event.original_job.create_retry_job();

        let delay_msg = if let Some(ms) = retry_job.retry_delay_ms {
            format!(" (waiting {}ms before retry)", ms)
        } else {
            String::new()
        };

        let reason = if let Some(err) = &job_result.error {
            format!("error: {}", err)
        } else if let Some(secs) = job_result.timeout_secs {
            format!("timeout after {}s", secs)
        } else {
            "explicit retry".to_string()
        };

        log::info!(
            "Retrying job {} due to {} (attempt {}/{}{})",
            retry_job.phase_name,
            reason,
            retry_job.retry_count + 1,
            retry_job.retry_limit + 1,
            delay_msg
        );

        state.job_info.insert(event.job_id, crate::state::JobInfo::from_job(&event.original_job));
        // Record result without satisfying dependencies -- dependents stay blocked until retry resolves
        state.record_retry_attempt(event.job_id, job_result);

        if let Some(delay_ms) = retry_job.retry_delay_ms {
            let state_arc = self.state.clone();
            let phase_key = retry_job.phase_key.clone();
            let phase_name = retry_job.phase_name.clone();
            let function = retry_job.function.clone();
            let slot_id = retry_job.slot_id.clone();
            let retry_job_id = retry_job.id;
            let dependency_id = retry_job.dependency_id;
            let retry_count = retry_job.retry_count;

            let handle = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                let mut state = state_arc.write().await;
                // Belt and braces: a cancel aborts this task, but should
                // it land between the sleep and the lock the scope check
                // still keeps a cancelled slot's retry out of the queue.
                if !state.is_stopping_for(retry_job.slot_id.as_deref()) {
                    state.enqueue_retry_job(retry_job);
                }
            });

            state.pending_delayed_retry_handles.push(
                crate::state::PendingDelayedRetry {
                    handle,
                    phase_key,
                    phase_name,
                    function,
                    slot_id,
                    job_id: retry_job_id,
                    dependency_id,
                    retry_count,
                },
            );
        } else {
            state.enqueue_retry_job(retry_job);
        }

        true
    }

    async fn handle_stop(
        &self,
        state: &mut crate::state::OrchestratorState,
        event: JobCompletionEvent,
        job_result: JobResult,
    ) {
        let outcome = job_result.phase_outcome;
        let reason = match outcome {
            Outcome::Error => "error",
            Outcome::Timeout => "timeout",
            Outcome::Stop => "stop",
            Outcome::Fail => "failure (on_first_failure: stop)",
            _ => "terminal outcome",
        };

        log::warn!(
            "Phase '{}' resulted in {} - stopping {}",
            event.original_job.phase_name,
            reason,
            match &event.original_job.slot_id {
                Some(slot) => format!("slot {slot}"),
                None => "all execution".to_string(),
            }
        );

        // Only a phase that genuinely failed makes this a `PhaseFailure`. An
        // `Outcome::Stop` lands here too, for two reasons that are both NOT
        // failures: an explicit `phase.stop()` (caught by the aggregation on
        // `phase_result`), and a phase that simply finished while an operator
        // stop was already raised — whoever raised it recorded the cause, and
        // stamping `PhaseFailure` over it would turn that abort into a PASS.
        let cause = match outcome {
            Outcome::Fail | Outcome::Error | Outcome::Timeout => {
                Some(crate::state::ShutdownCause::PhaseFailure)
            }
            _ => None,
        };

        self.cancel_scope_of(state, &event.original_job, &job_result, cause, reason)
            .await;

        state.complete_job_with_info(event.job_id, &event.original_job, job_result);

        // Note: shutdown_requested is set by cancel_all_jobs only if no teardown phases remain
    }
}
