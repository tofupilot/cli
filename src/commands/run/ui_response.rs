//! Single chokepoint for delivering a `ui_response` payload back to the
//! waiting phase. Pre-helper, six call sites across TUI, agent, station
//! bridge, engine, and connector each duplicated the same lock-and-take
//! shape against `UI_RESPONSE_CHANNELS`. Centralizing here eliminates
//! that drift and makes the lifecycle invariant ("a request id can be
//! resolved exactly once") easier to enforce later if we ever need it.
//!
//! The channel is owned by the `execution_engine::ui` module — we just
//! wrap the unlock-take-send-drop sequence so callers don't have to.

use std::collections::HashMap;

use execution_engine::ui::UI_RESPONSE_CHANNELS;

/// Resolve the pending UI request with the given `request_id`. No-ops
/// when the request isn't pending (already answered, timed out, or
/// engine moved past it).
pub async fn send(request_id: &str, values: HashMap<String, String>) {
    let mut channels = UI_RESPONSE_CHANNELS.lock().await;
    if let Some(sender) = channels.remove(request_id) {
        // Receiver dropped → phase cancelled / timed out while our
        // response was in flight. Nothing to do.
        let _ = sender.send(values);
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
