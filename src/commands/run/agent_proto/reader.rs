//! Stdin reader task: parses operator responses (`ui_response`, cancel) from the
//! agent protocol and routes them to the waiting request.

use std::collections::HashMap;

use execution_engine::ui::UiComponent;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::ctx::AgentProtoCtx;
use super::events::{ActiveUiRequest, CliCommand, CliEvent, RunStatus, UiErrorReason};
use super::validate;

/// A pending UI request tracked until the agent answers or the phase
/// cancels it. `phase_key` is stored alongside the components so
/// `state_snapshot` can reconstruct the full prompt, not just an id.
#[derive(Clone)]
pub struct PendingRequest {
    pub phase_key: String,
    pub components: Vec<UiComponent>,
}

#[derive(Default)]
pub struct PendingRequests {
    inner: HashMap<String, PendingRequest>,
}

impl PendingRequests {
    pub fn insert(&mut self, request_id: String, phase_key: String, components: Vec<UiComponent>) {
        self.inner.insert(
            request_id,
            PendingRequest {
                phase_key,
                components,
            },
        );
    }

    pub fn remove(&mut self, request_id: &str) -> Option<PendingRequest> {
        self.inner.remove(request_id)
    }

    pub fn get(&self, request_id: &str) -> Option<&PendingRequest> {
        self.inner.get(request_id)
    }

    pub fn first_entry(&self) -> Option<(String, PendingRequest)> {
        self.inner
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v.clone()))
    }

    pub fn clear_all(&mut self) {
        self.inner.clear();
    }
}

/// Spawn a stdin reader that parses NDJSON commands and dispatches them.
/// The reader owns the full `AgentProtoCtx` so it can answer `get_state`
/// queries, forward `abort_run` to the run's cancel channel, and resolve
/// `ui_response` commands.
pub fn spawn_stdin_reader(ctx: AgentProtoCtx) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let cmd: CliCommand = match serde_json::from_str(trimmed) {
                Ok(c) => c,
                Err(_) => {
                    ctx.emitter.enqueue(CliEvent::UiError {
                        request_id: None,
                        reason: UiErrorReason::ParseError,
                        field: None,
                        got: Some(serde_json::Value::String(trimmed.to_string())),
                        expected: Some("cli command NDJSON".to_string()),
                    });
                    continue;
                }
            };
            match cmd {
                CliCommand::UiResponse { request_id, values } => {
                    handle_ui_response(&ctx, request_id, values).await;
                }
                CliCommand::GetState => {
                    let status = ctx.lifecycle_status().await;
                    let phases = ctx
                        .history
                        .lock()
                        .map(|h| h.phases.clone())
                        .unwrap_or_default();
                    let active_ui_request =
                        ctx.pending
                            .read()
                            .await
                            .first_entry()
                            .map(|(request_id, pending)| ActiveUiRequest {
                                request_id,
                                phase_key: pending.phase_key,
                                components: pending
                                    .components
                                    .iter()
                                    .map(super::events::to_agent_ui_component)
                                    .collect(),
                            });
                    ctx.emitter.enqueue(CliEvent::StateSnapshot {
                        run_status: status,
                        phases,
                        active_ui_request,
                    });
                }
                CliCommand::AbortRun => {
                    let status = ctx.lifecycle_status().await;
                    match status {
                        RunStatus::NotStarted => {
                            ctx.emitter.enqueue(CliEvent::UiError {
                                request_id: None,
                                reason: UiErrorReason::InvalidState,
                                field: Some("abort_run".to_string()),
                                got: Some(serde_json::json!("not_started")),
                                expected: Some(
                                    "run_status=running; wait for run_started before abort".into(),
                                ),
                            });
                        }
                        RunStatus::Finished => {
                            ctx.emitter.enqueue(CliEvent::UiError {
                                request_id: None,
                                reason: UiErrorReason::InvalidState,
                                field: Some("abort_run".to_string()),
                                got: Some(serde_json::json!("finished")),
                                expected: Some("run_status=running; run already complete".into()),
                            });
                        }
                        RunStatus::Running => {
                            if let Some(tx) = ctx.abort_tx.write().await.take() {
                                let _ = tx.send(());
                            } else {
                                // abort_tx already consumed (double abort or
                                // race against natural completion). Surface
                                // rather than silently drop.
                                ctx.emitter.enqueue(CliEvent::UiError {
                                    request_id: None,
                                    reason: UiErrorReason::InvalidState,
                                    field: Some("abort_run".to_string()),
                                    got: Some(serde_json::json!("already_aborted")),
                                    expected: Some("a single abort_run per run".into()),
                                });
                            }
                        }
                    }
                    // NOTE: don't mark lifecycle=finished here — the normal
                    // terminal flow (mod.rs) owns that transition via the
                    // RunFinished enqueue + finalize. Marking here would
                    // race a legitimate in-flight RunFinished.
                }
            }
        }

        // Stdin closed. Fail fast on any pending UI requests so the
        // phase surfaces a missing-required error now, instead of
        // waiting out its full --ui-timeout. Agent is gone; no response
        // is coming.
        ctx.pending.write().await.clear_all();
        super::super::ui_response::cancel_all().await;
    })
}

async fn handle_ui_response(
    ctx: &AgentProtoCtx,
    request_id: String,
    values: HashMap<String, serde_json::Value>,
) {
    // Single write lock acquire: validate against a cloned spec while we
    // hold the write guard, and either remove-and-resolve (success) or
    // keep the spec in place (validation failed, agent may retry). A
    // concurrent duplicate resolve waits on the same lock and then sees
    // `None`, falling through to UnknownRequest.
    let result = {
        let mut guard = ctx.pending.write().await;
        let Some(pending) = guard.get(&request_id).cloned() else {
            ctx.emitter.enqueue(CliEvent::UiError {
                request_id: Some(request_id.clone()),
                reason: UiErrorReason::UnknownRequest,
                field: None,
                got: None,
                expected: Some("a pending request_id".to_string()),
            });
            return;
        };
        match validate::validate_and_coerce(&pending.components, values) {
            Ok(coerced) => {
                guard.remove(&request_id);
                Ok(coerced)
            }
            Err(err) => Err(err),
        }
    };

    match result {
        Ok(coerced) => {
            super::super::ui_response::send(&request_id, coerced).await;
        }
        Err(err) => {
            ctx.emitter.enqueue(err.into_event(&request_id));
        }
    }
}
