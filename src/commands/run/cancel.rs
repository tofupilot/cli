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

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use tokio::sync::watch;

/// Cancellation state for a run.
///
/// `Graceful` flips engine `shutdown_requested` flags: running phases
/// finish, queued ones are skipped, teardown runs. `Interrupt` stops
/// the running phases right away and still runs teardown, for stops
/// with an external deadline (console close, SIGTERM) where waiting
/// for an hours-long phase is not an option. `Force` invokes the
/// parallel-SIGKILL path on YAML runs (no teardown) and drops the
/// OpenHTF subprocess immediately. Each is a strict superset of the
/// one before it; the ordering below is the escalation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CancelSignal {
    None,
    Graceful,
    Interrupt,
    Force,
}

/// Sender side of the cancellation channel. Held by `RunHandle` so the
/// public `cancel`/`kill` methods write straight here. Cheap to clone
/// (one `Arc` internally) — clones are also held inside the run task
/// for the outer cancel arm.
#[derive(Clone)]
pub struct CancelToken {
    tx: watch::Sender<CancelSignal>,
    shared: Arc<Shared>,
}

/// What travels beside the signal: the OS deadline behind an
/// interrupt, and the run's word that its teardown has begun.
struct Shared {
    /// Set by the first interrupt that carries an OS deadline (Windows
    /// console close); a later one cannot move it.
    deadline: OnceLock<Instant>,
    /// Flipped by the run once the main loop has drained and the
    /// engine's teardown starts. The signal ladder arms its cap here,
    /// not at the signal: interrupting the running phases has no fixed
    /// duration (a teardown already running is left to finish).
    teardown: watch::Sender<bool>,
}

impl CancelToken {
    pub fn new() -> (Self, Receiver) {
        let (tx, rx) = watch::channel(CancelSignal::None);
        let shared = Arc::new(Shared {
            deadline: OnceLock::new(),
            teardown: watch::channel(false).0,
        });
        (
            Self {
                tx,
                shared: shared.clone(),
            },
            Receiver { rx, shared },
        )
    }

    /// `interrupt` with the OS deadline behind it, if any. The run reads
    /// it through `Receiver::deadline` to pick the hurried shutdown and
    /// to skip the enqueue once no time is left.
    pub fn interrupt_by(&self, deadline: Option<Instant>) {
        if let Some(deadline) = deadline {
            let _ = self.shared.deadline.set(deadline);
        }
        self.interrupt();
    }

    /// Resolves once the run reports its teardown has started. Pends
    /// forever if the run never gets there (a force kill skips it).
    pub async fn wait_teardown_started(&self) {
        let mut rx = self.shared.teardown.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
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

    /// Stop the running phases now, keep the teardown. Escalates from
    /// None or Graceful; a no-op once Force is set.
    pub fn interrupt(&self) {
        let _ = self.tx.send_if_modified(|state| {
            if *state < CancelSignal::Interrupt {
                *state = CancelSignal::Interrupt;
                true
            } else {
                false
            }
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
    shared: Arc<Shared>,
}

impl Receiver {
    /// The OS deadline behind the interrupt, if one was given.
    pub fn deadline(&self) -> Option<Instant> {
        self.shared.deadline.get().copied()
    }

    /// The main loop has drained; the engine's teardown begins now.
    pub fn mark_teardown_started(&self) {
        self.shared.teardown.send_replace(true);
    }

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

    /// Returns when cancellation reaches `Interrupt` or beyond. Resolves
    /// immediately if already there.
    pub async fn wait_interrupt(&mut self) -> CancelSignal {
        loop {
            let current = *self.rx.borrow();
            if current >= CancelSignal::Interrupt {
                return current;
            }
            if self.rx.changed().await.is_err() {
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
    async fn interrupt_sits_between_graceful_and_force() {
        let (tx, mut rx_any) = CancelToken::new();
        let mut rx_interrupt = rx_any.clone();
        let mut rx_force = rx_any.clone();

        tx.cancel();
        assert_eq!(rx_any.wait_any().await, CancelSignal::Graceful);
        let pending = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            rx_interrupt.wait_interrupt(),
        )
        .await;
        assert!(pending.is_err(), "wait_interrupt resolved on graceful");

        tx.interrupt();
        assert_eq!(rx_interrupt.wait_interrupt().await, CancelSignal::Interrupt);
        let pending =
            tokio::time::timeout(std::time::Duration::from_millis(20), rx_force.wait_force()).await;
        assert!(pending.is_err(), "wait_force resolved on interrupt");

        // Interrupt never downgrades a force kill.
        tx.kill();
        tx.interrupt();
        assert_eq!(rx_force.wait_force().await, CancelSignal::Force);
    }

    #[tokio::test]
    async fn first_deadline_wins_and_teardown_mark_wakes_the_ladder() {
        let (tx, rx) = CancelToken::new();
        assert_eq!(rx.deadline(), None);
        let first = Instant::now() + std::time::Duration::from_secs(5);
        tx.interrupt_by(Some(first));
        tx.interrupt_by(Some(first + std::time::Duration::from_secs(60)));
        assert_eq!(rx.deadline(), Some(first));

        let pending = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            tx.wait_teardown_started(),
        )
        .await;
        assert!(pending.is_err(), "ladder armed before the teardown started");
        rx.mark_teardown_started();
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tx.wait_teardown_started(),
        )
        .await
        .expect("teardown mark wakes the ladder");
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
