//! Shared teardown machinery for run dispatchers (station daemon,
//! studio session).
//!
//! Both dispatchers detach a cancelled run's task so their command loop
//! stays responsive, then need two guarantees the bare `JoinHandle`
//! doesn't give them: dropping the parked wrapper must ABORT the inner
//! run task (a dropped `JoinHandle` merely detaches it, leaving the
//! Python child running past CLI shutdown), and anything about to
//! reuse the run's resources (instrument ports, station plugs) must be
//! able to wait — bounded — for the parked teardowns to finish.

use crate::log;

/// RAII: aborts the wrapped task on Drop unless explicitly disarmed.
struct AbortOnDrop(Option<tokio::task::AbortHandle>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

/// Park a cancelled run's detached task on the dispatcher's teardown
/// JoinSet so the dispatcher returns to its select! tick instead of
/// blocking on Python teardown / publisher drain (1-3s typical).
///
/// The task is wrapped in a future that aborts the inner JoinHandle if
/// the wrapper itself is cancelled (JoinSet::Drop at dispatcher exit,
/// `drain_prior_teardowns`' deadline abort). Without this, dropping
/// the wrapper merely detaches the inner task — the Python child keeps
/// running, holding its instrument connections (VISA/TCP/serial).
///
/// Finished teardowns are reaped first so an operator hammering the
/// Run button can't pile up unbounded tasks (`try_join_next` is
/// non-blocking).
pub(crate) fn park_prior_run(
    teardowns: &mut tokio::task::JoinSet<()>,
    task: tokio::task::JoinHandle<()>,
) {
    while teardowns.try_join_next().is_some() {}
    // Arm the guard BEFORE spawning and move it into the future: a
    // future dropped before its first poll never executes its body,
    // so a guard constructed inside the async block would not exist
    // yet — the captured JoinHandle would drop bare and the inner
    // task would detach. A captured guard drops (and aborts) even
    // when the wrapper is torn down pre-poll.
    let guard = AbortOnDrop(Some(task.abort_handle()));
    teardowns.spawn(async move {
        let mut guard = guard;
        let _ = task.await;
        // Natural completion — disarm so we don't abort a JoinHandle
        // that's already finished (no-op anyway, but semantically
        // cleaner).
        guard.0 = None;
    });
}

/// Await parked prior-run teardowns, bounded. The parked runs are the
/// only borrowers of shared run resources (station plug host, local
/// instrument ports) besides the active run, so anything about to
/// reuse those resources (a new run's acquire, host shutdown, process
/// exit) must drain them first or an instrument can be torn down
/// mid-RPC. On timeout the stragglers are force-aborted — their
/// `AbortOnDrop` wrapper kills the inner run task — and we log loudly
/// rather than wait forever on a wedged Python teardown.
pub(crate) async fn drain_prior_teardowns(
    teardowns: &mut tokio::task::JoinSet<()>,
    timeout_secs: u64,
    json_mode: bool,
) {
    if teardowns.is_empty() {
        return;
    }
    if !json_mode {
        log::info(&format!(
            "Waiting for {} prior run(s) to finish teardown...",
            teardowns.len()
        ));
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while !teardowns.is_empty() {
        match tokio::time::timeout_at(deadline, teardowns.join_next()).await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                if !json_mode {
                    log::warn(&format!(
                        "{} prior-run teardown(s) still running after {}s; aborting them",
                        teardowns.len(),
                        timeout_secs
                    ));
                }
                teardowns.abort_all();
                // Cancelled tasks resolve at their next await point, so
                // this join is normally instant — but keep it bounded
                // too, or a task wedged in a compute loop between
                // awaits would violate this function's bounded
                // contract.
                let reap_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                while !teardowns.is_empty() {
                    if tokio::time::timeout_at(reap_deadline, teardowns.join_next())
                        .await
                        .is_err()
                    {
                        teardowns.detach_all();
                        break;
                    }
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{drain_prior_teardowns, park_prior_run};

    async fn wait_finished(probe: &tokio::task::AbortHandle) -> bool {
        for _ in 0..50 {
            if probe.is_finished() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        probe.is_finished()
    }

    /// The cascade both dispatchers rely on: draining past the deadline
    /// aborts the parked wrapper, whose Drop must abort the INNER run
    /// task — not detach it.
    #[tokio::test]
    async fn parked_wedged_run_is_aborted_at_the_drain_deadline() {
        let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let inner = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let probe = inner.abort_handle();
        park_prior_run(&mut set, inner);

        let start = std::time::Instant::now();
        drain_prior_teardowns(&mut set, 1, true).await;
        assert!(start.elapsed() >= std::time::Duration::from_secs(1));
        assert!(set.is_empty());
        assert!(
            wait_finished(&probe).await,
            "inner run task must be aborted by the wrapper's Drop, not left running"
        );
    }

    /// Dropping the JoinSet itself (dispatcher exit without a drain)
    /// must also cascade the abort to the inner run task.
    #[tokio::test]
    async fn parked_run_is_aborted_when_the_joinset_drops() {
        let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let inner = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let probe = inner.abort_handle();
        park_prior_run(&mut set, inner);

        drop(set);
        assert!(
            wait_finished(&probe).await,
            "JoinSet::Drop must abort the wrapper, whose Drop aborts the inner task"
        );
    }

    /// A run whose teardown completes naturally is reaped by the next
    /// park, and the disarmed guard doesn't abort anything.
    #[tokio::test]
    async fn finished_teardowns_are_reaped_before_parking() {
        let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let quick = tokio::spawn(async {});
        park_prior_run(&mut set, quick);
        drain_prior_teardowns(&mut set, 5, true).await;
        assert!(set.is_empty());

        // Reap path: park a finished wrapper, then park again.
        let quick = tokio::spawn(async {});
        park_prior_run(&mut set, quick);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let next = tokio::spawn(async {});
        park_prior_run(&mut set, next);
        assert_eq!(set.len(), 1);
        drain_prior_teardowns(&mut set, 5, true).await;
        assert!(set.is_empty());
    }
}
