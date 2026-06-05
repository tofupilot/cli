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
}
