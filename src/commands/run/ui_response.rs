//! Single chokepoint for delivering a `ui_response` payload back to the
//! waiting phase. Pre-helper, six call sites across TUI, agent, station
//! bridge, engine, and connector each duplicated the same lock-and-take
//! shape against `UI_RESPONSE_CHANNELS`. Centralizing here eliminates
//! that drift and makes the lifecycle invariant ("a request id can be
//! resolved exactly once") easier to enforce later if we ever need it.
//!
//! The channel is owned by the `execution_engine::ui` module — we just
//! wrap the unlock-take-send-drop sequence so callers don't have to.
//!
//! Two delivery flavors:
//! * `send_validated` — operator submissions (kiosk, web console,
//!   dashboard) go through the one shared validator; a failing
//!   submission leaves the prompt pending and hands back per-field
//!   messages for a `StationEvent::UiResponseRejected` broadcast.
//! * `send` — unchecked, for paths that validated already (agent
//!   protocol, TUI) or that deliberately bypass (display-only
//!   auto-continue via `send_empty`).

use std::collections::HashMap;

use execution_engine::ui::{validate_response, UI_RESPONSE_CHANNELS};
use station_protocol::StationEvent;

/// Outcome of a validated delivery attempt.
pub enum Delivery {
    /// Values accepted; the waiting phase received them.
    Delivered,
    /// Values failed validation against the prompt's component spec.
    /// The prompt is STILL PENDING — the caller reports `errors`
    /// (component key → operator-facing message) back to the submitter.
    Rejected(HashMap<String, String>),
    /// No prompt with this request id is pending (already answered,
    /// timed out, or the engine moved past it). No-op, like `send`.
    NotPending,
}

/// Validate `values` against the pending prompt's components, then
/// resolve it — or reject and keep it pending. This is the gate that
/// makes the engine's derived messages ("Character 6 (space) is not
/// allowed — …") reach the kiosk instead of a run-crashing
/// `identify_unit_failed`, and the reason browser surfaces don't
/// validate anything themselves (TP-760).
pub async fn send_validated(request_id: &str, values: HashMap<String, String>) -> Delivery {
    let mut channels = UI_RESPONSE_CHANNELS.lock().await;
    let Some(pending) = channels.get(request_id) else {
        return Delivery::NotPending;
    };
    // Validate under the lock: cheap (string walks, one regex compile)
    // and it serializes concurrent submits on the same prompt.
    let errors = validate_response(&pending.components, &values);
    if !errors.is_empty() {
        return Delivery::Rejected(errors);
    }
    // `get` proved presence; take ownership to consume the oneshot.
    let pending = channels.remove(request_id).expect("checked above");
    // Receiver dropped → phase cancelled / timed out while our
    // response was in flight. Nothing to do.
    let _ = pending.sender.send(values);
    Delivery::Delivered
}

/// `send_validated` plus the verdict every remote surface expects back.
/// One function for both operator-submission doors (local WS pump and
/// the standalone-run stream bridge) so neither can drift into the
/// unchecked `send` again — that drift is how the dashboard lost
/// validation on both sides once browsers stopped validating locally.
///
/// `NotPending` maps to `Accepted`: the prompt is gone server-side
/// (answered elsewhere, timed out, run moved on) and holding the client
/// in "submitting" would strand it.
pub async fn submit(request_id: String, values: HashMap<String, String>) -> StationEvent {
    match send_validated(&request_id, values).await {
        Delivery::Rejected(errors) => StationEvent::UiResponseRejected { request_id, errors },
        Delivery::Delivered | Delivery::NotPending => {
            StationEvent::UiResponseAccepted { request_id }
        }
    }
}

/// Resolve the pending UI request with the given `request_id`,
/// unchecked. No-ops when the request isn't pending (already answered,
/// timed out, or engine moved past it). Callers either validated
/// already (agent protocol, TUI) or bypass on purpose (`send_empty`).
pub async fn send(request_id: &str, values: HashMap<String, String>) {
    let mut channels = UI_RESPONSE_CHANNELS.lock().await;
    if let Some(pending) = channels.remove(request_id) {
        // Receiver dropped → phase cancelled / timed out while our
        // response was in flight. Nothing to do.
        let _ = pending.sender.send(values);
    }
}

/// Resolve a pending request with an empty value map. Display-only
/// prompts (auto-continue, prebaked-cleared) take this path so they
/// don't have to assemble a `HashMap::new()` themselves.
pub async fn send_empty(request_id: &str) {
    send(request_id, HashMap::new()).await;
}

/// Drop the pending sender without responding. Used by the agent
/// timeout path: dropping the oneshot causes the awaiting phase to
/// receive a recv error and surface a missing-required error itself,
/// rather than an empty / synthetic response.
pub async fn cancel(request_id: &str) {
    let mut channels = UI_RESPONSE_CHANNELS.lock().await;
    channels.remove(request_id);
}

/// Drop every pending sender. Used when the upstream agent closes
/// stdin and no further responses can possibly arrive — fail fast
/// instead of letting each phase wait out its full ui_timeout.
pub async fn cancel_all() {
    UI_RESPONSE_CHANNELS.lock().await.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_engine::ui::{ComponentType, PendingUi, UiComponent};

    async fn register(
        request_id: &str,
        comp: UiComponent,
    ) -> tokio::sync::oneshot::Receiver<HashMap<String, String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        UI_RESPONSE_CHANNELS.lock().await.insert(
            request_id.to_string(),
            PendingUi {
                sender: tx,
                components: vec![comp],
            },
        );
        rx
    }

    fn serial_component() -> UiComponent {
        UiComponent {
            key: "serial_number".into(),
            required: true,
            pattern: Some("^[A-Z0-9-]+$".into()),
            ..UiComponent::new(ComponentType::TextInput)
        }
    }

    /// The reject-on-submit contract (TP-760): a failing submission is
    /// rejected with the engine-derived message and the prompt STAYS
    /// pending — the waiting phase must not observe anything, and the
    /// operator's corrected retry must still find the channel.
    #[tokio::test]
    async fn rejected_submission_keeps_prompt_pending() {
        let mut rx = register("req-reject", serial_component()).await;

        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "AB 1".to_string());
        match send_validated("req-reject", values).await {
            Delivery::Rejected(errors) => {
                assert_eq!(
                    errors["serial_number"],
                    "Remove the space (character 3) — allowed: uppercase letters, digits, -"
                );
            }
            _ => panic!("expected rejection"),
        }
        // Phase saw nothing; prompt still answerable.
        assert!(rx.try_recv().is_err());
        assert!(UI_RESPONSE_CHANNELS.lock().await.contains_key("req-reject"));

        // The corrected retry goes through and resolves the oneshot.
        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "AB-1".to_string());
        assert!(matches!(
            send_validated("req-reject", values).await,
            Delivery::Delivered
        ));
        assert_eq!(rx.await.unwrap()["serial_number"], "AB-1");
    }

    /// `submit` is the verdict both operator doors publish. Reject carries
    /// the field messages; NotPending closes the form like an accept so
    /// a dashboard whose prompt vanished never sits in "submitting".
    #[tokio::test]
    async fn submit_maps_delivery_to_verdict_events() {
        let rx = register("req-verdict", serial_component()).await;

        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "ab".to_string());
        match submit("req-verdict".into(), values).await {
            StationEvent::UiResponseRejected { request_id, errors } => {
                assert_eq!(request_id, "req-verdict");
                assert!(errors.contains_key("serial_number"));
            }
            other => panic!("expected rejection, got {other:?}"),
        }

        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "AB-2".to_string());
        assert!(matches!(
            submit("req-verdict".into(), values).await,
            StationEvent::UiResponseAccepted { .. }
        ));
        assert_eq!(rx.await.unwrap()["serial_number"], "AB-2");

        assert!(matches!(
            submit("req-verdict".into(), HashMap::new()).await,
            StationEvent::UiResponseAccepted { .. }
        ));
    }

    #[tokio::test]
    async fn unknown_request_is_not_pending() {
        assert!(matches!(
            send_validated("req-ghost", HashMap::new()).await,
            Delivery::NotPending
        ));
    }

    /// `send` stays the unchecked escape hatch for pre-validated paths
    /// (agent protocol, TUI) — it must not grow validation.
    #[tokio::test]
    async fn unchecked_send_bypasses_validation() {
        let rx = register("req-raw", serial_component()).await;
        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "not valid!".to_string());
        send("req-raw", values).await;
        assert_eq!(rx.await.unwrap()["serial_number"], "not valid!");
    }
}
