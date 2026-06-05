//! Async-task helpers shared across the CLI.

use std::time::Duration;

use tokio::task::JoinHandle;

/// Wait up to `timeout` for `handle` to finish, otherwise abort it and
/// log `warn_msg`. Used by the publisher / bridge teardown paths after
/// the upstream broadcast has been dropped: the task should drain
/// quickly, but a stuck Centrifugo publish must not block shutdown
/// indefinitely.
///
/// `Duration::ZERO` is the explicit "abandon now" signal — abort
/// immediately and stay quiet, since the warn would only confuse the
/// operator (no actual drain was attempted).
pub async fn drain_or_abort(handle: JoinHandle<()>, timeout: Duration, warn_msg: &str) {
    let abort = handle.abort_handle();
    if timeout.is_zero() {
        abort.abort();
        return;
    }
    if tokio::time::timeout(timeout, handle).await.is_err() {
        abort.abort();
        crate::log::warn(warn_msg);
    }
}
