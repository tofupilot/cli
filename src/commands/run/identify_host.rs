//! `IdentifyHost` implementation for the CLI.
//!
//! Owns the operator-prompt dance for the pre-run identify step:
//! register a oneshot in `UI_RESPONSE_CHANNELS`, broadcast a typed
//! `StationEvent::IdentifyRequest` (so dashboard / kiosk / station
//! bridge see the prompt without heuristics on a `UiRequest` shape),
//! forward to the TUI channel and the agent protocol when those are
//! present, and await the response.
//!
//! Identity is run metadata, not a phase. This host does not emit
//! `phase_started` / `phase_finished` for identify — UIs key off the
//! dedicated `IdentifyRequest` / `IdentifyResolved` lifecycle events.

use std::collections::HashMap;

use async_trait::async_trait;
use execution_engine::identify_unit::{IdentifyHost, IdentifyHostError, PromptRequest};
use execution_engine::ui::{UiConfig, UiRequestData, UI_RESPONSE_CHANNELS};
use tokio::sync::{mpsc, oneshot};

use super::agent_proto::AgentProtoCtx;
use super::event_router::EventRouter;

/// CLI-side host. Clone-cheap: every field is `Arc` / `mpsc::Sender`
/// underneath.
pub struct CliIdentifyHost {
    pub router: EventRouter,
    pub ui_tx: Option<mpsc::Sender<UiRequestData>>,
    pub agent: Option<AgentProtoCtx>,
    /// Carried onto every `IdentifyRequest` so a UI hydrating mid-
    /// identify can resolve the procedure name without waiting for
    /// `RunStarted`.
    pub procedure_id: String,
    /// Whether any operator-facing surface exists for this run (TUI,
    /// kiosk, agent, or station dashboard). Computed once in `start()`
    /// where all of these signals are known. When false, `identify`
    /// short-circuits to `NoUi` rather than hanging on a prompt nobody
    /// can answer.
    pub has_ui: bool,
}

#[async_trait]
impl IdentifyHost for CliIdentifyHost {
    fn can_prompt(&self) -> bool {
        self.has_ui
    }

    async fn prompt(
        &self,
        req: PromptRequest,
    ) -> Result<HashMap<String, String>, IdentifyHostError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        {
            let mut channels = UI_RESPONSE_CHANNELS.lock().await;
            channels.insert(req.request_id.clone(), resp_tx);
        }

        // Broadcast the dedicated identify event. Dashboard, kiosk,
        // and station bridge consume `IdentifyRequest` directly — no
        // phase_key correlation, no `isUnitInputRequest` heuristic on
        // component shape.
        self.router.identify_request(
            &req.request_id,
            &self.procedure_id,
            req.slot_id.clone(),
            &req.components,
        );

        // TUI: forward via the same channel that carries `ui_request`s
        // so the in-terminal renderer can show a form. The TUI renders
        // identify identically to a regular prompt today; the
        // `phase_key` is set to the framework's identify sentinel
        // (`identify_unit::IDENTIFY_PHASE_KEY`) so log/grep diagnostics
        // can still distinguish identify from a real phase prompt.
        if let Some(ref ui_tx) = self.ui_tx {
            let _ = ui_tx.try_send(UiRequestData {
                request_id: req.request_id.clone(),
                job_id: String::new(),
                pipe_path: String::new(),
                config: UiConfig {
                    components: req.components.clone(),
                    requires_input: Some(true),
                },
                phase_key: req.phase_key.clone(),
                slot_id: req.slot_id.clone(),
            });
        }

        // Agent protocol pending-request bookkeeping so `get_state`
        // can reconstruct a mid-identify in-flight prompt for
        // late-attaching consumers. The agent-side `IdentifyRequest`
        // event is fanned out by the router above.
        if let Some(ref agent) = self.agent {
            let mut guard = agent.pending.write().await;
            guard.insert(
                req.request_id.clone(),
                req.phase_key.clone(),
                req.components.clone(),
            );
        }

        // Drop guard: ensures the `agent.pending` entry and any
        // parked `UI_RESPONSE_CHANNELS` sender are released even if
        // this future is dropped mid-await (e.g. the engine's outer
        // cancel select wins). Without it, a Stop during identify
        // leaves the pending registration in place until process
        // exit.
        struct PendingGuard {
            request_id: String,
            agent: Option<super::agent_proto::AgentProtoCtx>,
            armed: bool,
        }
        impl Drop for PendingGuard {
            fn drop(&mut self) {
                if !self.armed {
                    return;
                }
                let request_id = self.request_id.clone();
                let agent = self.agent.clone();
                tokio::spawn(async move {
                    if let Some(agent) = agent {
                        agent.pending.write().await.remove(&request_id);
                    }
                    super::ui_response::cancel(&request_id).await;
                });
            }
        }
        let mut pending_guard = PendingGuard {
            request_id: req.request_id.clone(),
            agent: self.agent.clone(),
            armed: true,
        };

        // Identify-unit is a hard dependency for the run, so a timeout is
        // a clean cancellation the run loop turns into a `RunCrashed`
        // (unlike a regular `ui_request`, we never fall back to an empty
        // response). An explicit `--ui-timeout` wins; otherwise we still
        // apply a generous default so an unattended or wedged prompt can't
        // park the CLI process forever — the previous `None` arm did a
        // bare `resp_rx.await` with no deadline at all.
        let timeout = self
            .agent
            .as_ref()
            .and_then(|a| a.ui_timeout)
            .unwrap_or(crate::config::timeouts::IDENTIFY_PROMPT_DEFAULT);
        let result = match tokio::time::timeout(timeout, resp_rx).await {
            Ok(Ok(values)) => Ok(values),
            Ok(Err(_)) => Err(IdentifyHostError::Cancelled(
                "operator UI channel closed before responding".to_string(),
            )),
            Err(_) => {
                self.router.identify_timeout(&req.request_id);
                if let Some(ref agent) = self.agent {
                    agent.pending.write().await.remove(&req.request_id);
                }
                super::ui_response::cancel(&req.request_id).await;
                Err(IdentifyHostError::Cancelled(
                    "identify-unit prompt timed out".to_string(),
                ))
            }
        };

        // Resolved naturally (success or local error path): the guard
        // would double-free if it ran now, so disarm. The error path
        // below handles its own cleanup synchronously to keep the
        // existing semantics (no spawn detour for a fatal error).
        pending_guard.armed = false;

        // On any error: drop the pending agent entry and the channel
        // so a late response can't resolve a stale request.
        if result.is_err() {
            if let Some(ref agent) = self.agent {
                agent.pending.write().await.remove(&req.request_id);
            }
            super::ui_response::cancel(&req.request_id).await;
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_engine::identify_unit::{IdentifyHost, IdentifyHostError, PromptRequest};
    use tokio::sync::broadcast;

    // A host with no agent and no responder — the plain kiosk shape. Its
    // `prompt()` used to `resp_rx.await` forever when `--ui-timeout` was
    // absent; now it must fall back to `IDENTIFY_PROMPT_DEFAULT`.
    fn bare_host() -> CliIdentifyHost {
        let (tx, _rx) = broadcast::channel(16);
        CliIdentifyHost {
            router: EventRouter::new(tx, None, "exec-test".to_string()),
            ui_tx: None,
            agent: None,
            procedure_id: "proc-test".to_string(),
            has_ui: true,
        }
    }

    fn req(request_id: &str) -> PromptRequest {
        PromptRequest {
            request_id: request_id.to_string(),
            slot_id: None,
            phase_key: "identify".to_string(),
            components: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn prompt_times_out_when_no_response() {
        // No `--ui-timeout`, no responder: on the paused clock the default
        // deadline elapses in virtual time and the prompt resolves as a
        // clean Cancelled instead of hanging the run forever.
        let host = bare_host();
        let result = host.prompt(req("id-timeout-1")).await;
        match result {
            Err(IdentifyHostError::Cancelled(msg)) => {
                assert!(msg.contains("timed out"), "unexpected cancel reason: {msg}");
            }
            other => panic!("expected Cancelled(timed out), got {other:?}"),
        }
    }

    // The response / channel-closed tests deliberately use the REAL clock,
    // not `start_paused`. `UI_RESPONSE_CHANNELS` is a process-global map, and
    // multiple `start_paused` runtimes running in parallel auto-advance their
    // virtual clocks independently — interleaving their touches of that shared
    // map non-deterministically. These two paths resolve in milliseconds of
    // real time (a quick poll for the prompt to register its oneshot, then an
    // answer), so a real-clock retry loop is both deterministic and fast.

    /// Spin until `request_id`'s oneshot is registered in the global map, so
    /// the responder answers a request that actually exists. Bounded so a bug
    /// can't hang the test.
    async fn await_registered(request_id: &str) {
        for _ in 0..200 {
            if UI_RESPONSE_CHANNELS.lock().await.contains_key(request_id) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("prompt never registered {request_id} in UI_RESPONSE_CHANNELS");
    }

    #[tokio::test]
    async fn prompt_returns_operator_response_before_deadline() {
        // The happy path must still work under the new timeout: an operator
        // response delivered via `ui_response::send` resolves the prompt with
        // the submitted values, well before the default deadline.
        let host = bare_host();
        let request_id = "id-response-1";
        let responder = tokio::spawn(async move {
            await_registered(request_id).await;
            let mut values = std::collections::HashMap::new();
            values.insert("serial_number".to_string(), "SN-42".to_string());
            super::super::ui_response::send(request_id, values).await;
        });
        let result = host.prompt(req(request_id)).await;
        responder.await.unwrap();
        let values = result.expect("prompt should resolve with the response");
        assert_eq!(values.get("serial_number").map(String::as_str), Some("SN-42"));
    }

    #[tokio::test]
    async fn prompt_reports_channel_closed_distinctly_from_timeout() {
        // If the response channel is dropped (UI closed) before answering,
        // the prompt must surface a channel-closed cancel, not a timeout —
        // the two are different operator situations.
        let host = bare_host();
        let request_id = "id-closed-1";
        let closer = tokio::spawn(async move {
            await_registered(request_id).await;
            // Drops the oneshot sender without sending → `resp_rx` errors.
            super::super::ui_response::cancel(request_id).await;
        });
        let result = host.prompt(req(request_id)).await;
        closer.await.unwrap();
        match result {
            Err(IdentifyHostError::Cancelled(msg)) => {
                assert!(
                    msg.contains("channel closed"),
                    "expected channel-closed reason, got: {msg}"
                );
            }
            other => panic!("expected Cancelled(channel closed), got {other:?}"),
        }
    }
}
