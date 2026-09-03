use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex, RwLock};

/// Global map of UI response channels keyed by request_id.
///
/// # Timeout Handling
/// Note: Channels are created when a UI request is sent and removed when:
/// - A response is received from the frontend
/// - The phase times out (handled by orchestrator timeout mechanism)
///
/// If the frontend never responds AND the phase has no timeout, the channel
/// will remain in memory. This is acceptable as native UI phases are typically
/// user-facing and have configured timeouts.
pub static UI_RESPONSE_CHANNELS: Lazy<
    Arc<Mutex<HashMap<String, oneshot::Sender<HashMap<String, String>>>>>,
> = Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));

/// Slot each pending UI request belongs to (`None` = shared phase), so a
/// slot stop can close only that slot's prompts. Kept beside
/// `UI_RESPONSE_CHANNELS` rather than in its key: the CLI and Studio
/// resolve responses by bare `request_id`, and identify-time prompts
/// register straight into the map without going through the worker.
static UI_CHANNEL_SLOTS: Lazy<Arc<Mutex<HashMap<String, Option<String>>>>> =
    Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));

/// Cancellation reason per request, written by a slot-scoped close so the
/// slot's cancelled prompts quote *their* slot's cause. Read via
/// `take_cancel_reason`, which falls back to `CANCEL_REASON`.
static CANCEL_REASON_BY_REQUEST: Lazy<Arc<RwLock<HashMap<String, String>>>> =
    Lazy::new(|| Arc::new(RwLock::new(HashMap::new())));

/// Register a phase's UI response channel. Workers use this rather than
/// inserting into `UI_RESPONSE_CHANNELS` directly so the slot index stays
/// in step.
pub async fn register_ui_channel(
    request_id: String,
    slot_id: Option<String>,
    tx: oneshot::Sender<HashMap<String, String>>,
) {
    UI_CHANNEL_SLOTS.lock().await.insert(request_id.clone(), slot_id);
    UI_RESPONSE_CHANNELS.lock().await.insert(request_id, tx);
}

/// Forget a request the worker is done with (answered, dismissed, or
/// closed under it).
pub async fn unregister_ui_channel(request_id: &str) {
    UI_RESPONSE_CHANNELS.lock().await.remove(request_id);
    UI_CHANNEL_SLOTS.lock().await.remove(request_id);
}

/// Close the pending UI channels of one slot only, recording `reason` for
/// each so the cancelled phases can quote it. Prompts of other slots and
/// of shared phases keep waiting.
pub async fn close_slot_ui_channels_with_reason(slot_id: &str, reason: String) {
    let request_ids: Vec<String> = {
        let slots = UI_CHANNEL_SLOTS.lock().await;
        slots
            .iter()
            .filter(|(_, s)| s.as_deref() == Some(slot_id))
            .map(|(id, _)| id.clone())
            .collect()
    };
    if request_ids.is_empty() {
        return;
    }
    log::debug!(
        "Closing {} pending UI response channels of slot {}",
        request_ids.len(),
        slot_id
    );
    {
        let mut reasons = CANCEL_REASON_BY_REQUEST.write().await;
        for id in &request_ids {
            reasons.entry(id.clone()).or_insert_with(|| reason.clone());
        }
    }
    let mut channels = UI_RESPONSE_CHANNELS.lock().await;
    let mut slots = UI_CHANNEL_SLOTS.lock().await;
    for id in &request_ids {
        channels.remove(id);
        slots.remove(id);
    }
}

/// The reason a request's channel was closed under the waiting worker:
/// the slot-scoped one when there is one, else the run-wide one. `None`
/// means the channel closed for an unrelated reason (operator timeout,
/// agent stdin disconnect, …).
pub async fn take_cancel_reason(request_id: &str) -> Option<String> {
    if let Some(r) = CANCEL_REASON_BY_REQUEST.write().await.remove(request_id) {
        return Some(r);
    }
    CANCEL_REASON.read().await.clone()
}

/// Last cancellation reason set when `close_all_ui_channels` was called
/// with a populated reason. Workers waiting on a UI response read this
/// after their `rx.await` returns Err so the cancelled phase's error
/// message names *why* the run was aborted (e.g. "Run aborted by phase
/// 'capture_rail_settle': TypeError: ...") instead of the generic
/// "cancelled or timed out". `None` means the channel closed for an
/// unrelated reason — operator timeout, agent stdin disconnect, etc.
pub static CANCEL_REASON: Lazy<Arc<RwLock<Option<String>>>> =
    Lazy::new(|| Arc::new(RwLock::new(None)));

/// Close all pending UI response channels.
/// This unblocks any phases waiting for UI input by dropping the senders,
/// causing the receivers to get a RecvError.
pub async fn close_all_ui_channels() {
    let mut channels = UI_RESPONSE_CHANNELS.lock().await;
    let count = channels.len();
    if count > 0 {
        log::debug!("Closing {} pending UI response channels", count);
        channels.clear();
    }
    UI_CHANNEL_SLOTS.lock().await.clear();
}

/// Variant of `close_all_ui_channels` that records the reason. Workers
/// pick it up via `CANCEL_REASON` to surface a real cause on the
/// cancelled UI phase's `error` field.
pub async fn close_all_ui_channels_with_reason(reason: String) {
    {
        let mut r = CANCEL_REASON.write().await;
        // Don't clobber an earlier, more specific reason (e.g. plug
        // init failure) with a later generic one.
        if r.is_none() {
            *r = Some(reason);
        }
    }
    close_all_ui_channels().await;
}

/// Reset the cancel-reason slot. Called at run start so a stale reason
/// from a previous run can't leak into the next.
pub async fn clear_cancel_reason() {
    let mut r = CANCEL_REASON.write().await;
    *r = None;
    CANCEL_REASON_BY_REQUEST.write().await.clear();
    // No prompt of a previous run can still be waiting; drop any index
    // entry a consumer resolved without going through `unregister`.
    UI_CHANNEL_SLOTS.lock().await.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slot A's stop closes A's prompt with A's reason and leaves B's
    /// prompt waiting; B's worker later finds no reason of its own.
    #[tokio::test]
    async fn slot_close_spares_the_other_slot() {
        let (tx_a, rx_a) = oneshot::channel();
        let (tx_b, mut rx_b) = oneshot::channel();
        register_ui_channel("req-a".into(), Some("A".into()), tx_a).await;
        register_ui_channel("req-b".into(), Some("B".into()), tx_b).await;

        close_slot_ui_channels_with_reason("A", "Slot 'A' aborted by phase 'x'".into()).await;

        assert!(rx_a.await.is_err(), "A's prompt is cancelled");
        assert_eq!(
            take_cancel_reason("req-a").await.as_deref(),
            Some("Slot 'A' aborted by phase 'x'")
        );
        assert!(
            matches!(rx_b.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "B's prompt keeps waiting"
        );
        assert_eq!(take_cancel_reason("req-b").await, None);
        assert!(UI_RESPONSE_CHANNELS.lock().await.contains_key("req-b"));

        unregister_ui_channel("req-b").await;
        assert!(UI_CHANNEL_SLOTS.lock().await.is_empty(), "no index entry outlives its prompt");
    }
}
