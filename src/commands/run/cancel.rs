//! Single-channel cancellation for a run.
//!
//! Before this module, three independent oneshot pairs (`cancel_tx`,
//! `engine_stop_tx`, `engine_force_tx`) plus a bridge task converted
//! `StationCommand::Stop` / `Kill` into firings on the right oneshot.
//! The result was that every framework path (YAML, OpenHTF, plain
//! python, agent abort) had to thread its own oneshot receiver through
//! a bespoke select, and adding a new path meant wiring a new oneshot.
//!
//! The watch-based token here gives the run a single source of truth
//! for cancellation. Anyone holding a [`Receiver`] can:
//!
//!   * `await` on a state change,
//!   * read the latest signal at any time (`borrow`),
//!   * tell whether escalation has happened (`Force` is a strict
//!     superset of `Graceful`).
//!
//! Idempotent: writing `Graceful` after `Graceful` is a no-op; writing
//! `Force` after `Graceful` escalates and unblocks any task waiting on
//! `wait_force`. No double-fire panics, no `Option::take()` dance.

use tokio::sync::watch;

/// Cancellation state for a run.
///
/// `Graceful` flips engine `shutdown_requested` flags and lets teardown
/// phases finish. `Force` invokes the parallel-SIGKILL path on YAML
/// runs and drops the OpenHTF subprocess immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelSignal {
    None,
    Graceful,
    Force,
}

/// Sender side of the cancellation channel. Held by `RunHandle` so the
/// public `cancel`/`kill` methods write straight here. Cheap to clone
/// (one `Arc` internally) — clones are also held inside the run task
/// for the outer cancel arm.
#[derive(Clone)]
pub struct CancelToken {
    tx: watch::Sender<CancelSignal>,
}

impl CancelToken {
    pub fn new() -> (Self, Receiver) {
        let (tx, rx) = watch::channel(CancelSignal::None);
        (Self { tx }, Receiver { rx })
    }

    /// Request a graceful stop. Idempotent — if Force has already been
    /// requested, this is a no-op (Force already implies Graceful).
    pub fn cancel(&self) {
        let _ = self.tx.send_if_modified(|state| match state {
            CancelSignal::None => {
                *state = CancelSignal::Graceful;
                true
            }
            _ => false,
        });
    }

    /// Request a force kill. Always escalates from None or Graceful.
    pub fn kill(&self) {
        let _ = self.tx.send_if_modified(|state| match state {
            CancelSignal::Force => false,
            _ => {
                *state = CancelSignal::Force;
                true
            }
        });
    }
}

/// Receiver side. Clone-cheap. Each subscriber polls independently —
/// the watch channel keeps them in lockstep with the latest value.
#[derive(Clone)]
pub struct Receiver {
    rx: watch::Receiver<CancelSignal>,
}

impl Receiver {
    /// Returns when cancellation transitions away from `None`. Resolves
    /// immediately if cancellation has already fired.
    pub async fn wait_any(&mut self) -> CancelSignal {
        loop {
            let current = *self.rx.borrow();
            if current != CancelSignal::None {
                return current;
            }
            if self.rx.changed().await.is_err() {
                // Sender dropped — no more cancellations possible. Treat
                // as `Force` so the run task winds down (drop generally
                // means the RunHandle was abandoned).
                return CancelSignal::Force;
            }
        }
    }

    /// Returns when cancellation reaches `Force`. Resolves immediately
    /// if Force is already set.
    pub async fn wait_force(&mut self) -> CancelSignal {
        loop {
            let current = *self.rx.borrow();
            if current == CancelSignal::Force {
                return current;
            }
            if self.rx.changed().await.is_err() {
                return CancelSignal::Force;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn graceful_then_force_escalates() {
        let (tx, mut rx_any) = CancelToken::new();
        let mut rx_force = rx_any.clone();

        tx.cancel();
        assert_eq!(rx_any.wait_any().await, CancelSignal::Graceful);
        // `wait_force` must NOT resolve on graceful.
        let force_pending =
            tokio::time::timeout(std::time::Duration::from_millis(20), rx_force.wait_force()).await;
        assert!(force_pending.is_err(), "wait_force resolved on graceful");

        tx.kill();
        assert_eq!(rx_force.wait_force().await, CancelSignal::Force);
    }

    #[tokio::test]
    async fn kill_alone_resolves_both_arms() {
        let (tx, mut rx_any) = CancelToken::new();
        let mut rx_force = rx_any.clone();
        tx.kill();
        assert_eq!(rx_any.wait_any().await, CancelSignal::Force);
        assert_eq!(rx_force.wait_force().await, CancelSignal::Force);
    }

    #[tokio::test]
    async fn drop_sender_treats_as_force() {
        let (tx, mut rx) = CancelToken::new();
        drop(tx);
        // Receiver treats a dropped sender as Force — someone
        // abandoned the run task and the consumer needs to wind down.
        assert_eq!(rx.wait_any().await, CancelSignal::Force);
    }

    #[tokio::test]
    async fn graceful_after_force_is_noop() {
        // Force is the strongest state; a subsequent graceful must not
        // downgrade it. Two separate clones so we can observe both
        // arms independently.
        let (tx, mut rx_force) = CancelToken::new();
        let mut rx_any = rx_force.clone();
        tx.kill();
        // Both receivers see Force.
        assert_eq!(rx_any.wait_any().await, CancelSignal::Force);
        assert_eq!(rx_force.wait_force().await, CancelSignal::Force);
        // Now graceful — must not flip the state back to Graceful.
        tx.cancel();
        // Fresh receiver still sees Force, not Graceful.
        let mut fresh = rx_force.clone();
        assert_eq!(fresh.wait_any().await, CancelSignal::Force);
    }
}
