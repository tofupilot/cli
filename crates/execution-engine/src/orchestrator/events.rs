//! Event emission for UI communication

use crate::event_sink::ExecutionEvent;
use crate::job::{Job, JobStatus, Outcome};
use crate::state::OrchestratorState;
use crate::procedure::schema::ProcedureDefinition;
use crate::{PlannedPhase, PlannedPlug};

use super::{ExecutionPlan, JobProgress, Orchestrator};

impl Orchestrator {
    pub(super) fn emit_job_progress(
        &self,
        job_id: String,
        original_job: &Job,
        status: JobStatus,
        outcome: Option<Outcome>,
        error: Option<String>,
        worker_id: Option<usize>,
    ) {
        let progress = JobProgress {
            job_id: job_id.clone(),
            slot_id: original_job.slot_id.clone(),
            phase_key: original_job.phase_key.clone(),
            phase_name: original_job.phase_name.clone(),
            stage_scope: original_job.stage_scope.clone(),
            status: status,
            worker_id,
            started_at: None,
            timeout_ms: original_job.timeout_ms,
            outcome: outcome,
            retry_count: original_job.retry_count,
            error: error.clone(),
        };
        self.event_sink.emit(&ExecutionEvent::JobProgress {
            job_id: progress.job_id,
            slot_id: progress.slot_id,
            phase_key: progress.phase_key,
            phase_name: progress.phase_name,
            stage_scope: progress.stage_scope,
            status: progress.status,
            worker_id: progress.worker_id,
            started_at: progress.started_at,
            timeout_ms: progress.timeout_ms,
            outcome: progress.outcome,
            retry_count: progress.retry_count,
            error: progress.error,
        });
    }

    pub(super) async fn emit_cancelled_jobs(
        &self,
        cancelled_jobs: &[Job],
        reason: &str,
        status: JobStatus,
        outcome: Outcome,
    ) {
        if !cancelled_jobs.is_empty() {
            for job in cancelled_jobs {
                self.emit_job_progress(
                    job.id.to_string(),
                    job,
                    status,
                    Some(outcome),
                    Some(reason.to_string()),
                    None,
                );
            }
        }
    }

    pub(super) async fn emit_plug_scope_event(&self, status: &str) {
        if status == "pass" || status == "error" {
            let mut state = self.state.write().await;
            state.original_jobs_completed += 1;
        }
    }

    pub(super) async fn emit_execution_plan(
        &self,
        procedure: &ProcedureDefinition,
        state: &OrchestratorState,
        slots: &[String],
    ) {
        let mut phases = Vec::new();
        for (stage_scope, phase) in procedure.get_all_phases_with_stage_scope() {
            if phase.should_skip() {
                continue;
            }
            phases.push(PlannedPhase {
                phase_key: phase.key.clone(),
                phase_name: phase.name.clone(),
                stage_scope,
            });
        }

        let (plugs_all, plugs_each): (Vec<_>, Vec<_>) = procedure
            .plugs
            .iter()
            .partition(|p| p.scope == crate::procedure::schema::Scope::All);

        let plugs_all: Vec<PlannedPlug> = plugs_all
            .into_iter()
            .map(|p| PlannedPlug {
                plug_key: p.key.clone(),
                plug_name: p.name.clone(),
                scope: "all".to_string(),
            })
            .collect();

        let plugs_each: Vec<PlannedPlug> = plugs_each
            .into_iter()
            .map(|p| PlannedPlug {
                plug_key: p.key.clone(),
                plug_name: p.name.clone(),
                scope: "each".to_string(),
            })
            .collect();

        let plan = ExecutionPlan {
            phases: phases.clone(),
            plugs_all: plugs_all.clone(),
            plugs_each: plugs_each.clone(),
            slots: slots.to_vec(),
            total_expected_jobs: state.total_jobs_submitted as u32,
        };

        self.event_sink.emit(&ExecutionEvent::Plan {
            phases,
            plugs_all,
            plugs_each,
            slots: plan.slots,
            total_expected_jobs: plan.total_expected_jobs,
        });
    }
}
