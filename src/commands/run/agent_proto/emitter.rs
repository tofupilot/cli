//! Serializes `CliEvent`s to stdout as NDJSON for the agent protocol.

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncWriteExt, Stdout};
use tokio::sync::{mpsc, oneshot, Mutex, Notify};

use super::events::CliEvent;
use crate::config::timeouts::EMITTER_FLUSH as FLUSH_TIMEOUT;

/// NDJSON stdout writer for the agent protocol.
///
/// Every event flows through a single ordered mpsc channel drained by a
/// dedicated writer task, so call sites can enqueue from sync contexts
/// (`EventSink::emit` is `fn emit(&self, ...)`) in FIFO order and `flush()`
/// can deterministically wait for the drain before emitting `run_finished`.
///
/// Every emitted line carries a monotonic `seq: u64` assigned **inside the
/// writer task**, not at enqueue time. Assigning under the mpsc's FIFO
/// drain guarantees seq matches line-write order on the wire; assigning
/// at enqueue with `fetch_add` would let two concurrent enqueues observe
/// seq values that race ahead of the channel position and land out of
/// order.
///
/// After `run_finished` is emitted the emitter is **finalized**: any
/// subsequent `enqueue` drops its event and synthesizes an
/// `internal_warning` in its place so the spec invariant
/// "run_finished is the last event" holds even if a late producer fires.
#[derive(Clone)]
pub struct Emitter {
    tx: mpsc::UnboundedSender<EmitterMsg>,
    /// Woken when the writer task exits. Paired with `dead_flag` so a
    /// death that happens *before* `flush()` registers a waiter is still
    /// observable (Notify alone is racy for that case — its notification
    /// is lost if nobody is parked yet).
    dead_wake: Arc<Notify>,
    /// Set by the writer's DeadGuard on exit (normal return or panic) or
    /// on fatal stdout error. Orthogonal to `finalized`:
    ///
    /// - `dead_flag` = writer task is gone; draining stopped; enqueue
    ///   would leak memory. Purpose: memory bound.
    /// - `finalized` = we deliberately emitted run_finished; further
    ///   events must not appear on the wire. Purpose: spec invariant
    ///   ("run_finished is last").
    ///
    /// Both are checked in enqueue(); either causes a silent drop.
    dead_flag: Arc<AtomicBool>,
    /// Set by `finalize()` exactly once, right after enqueueing
    /// RunFinished. See `dead_flag` for the distinction.
    finalized: Arc<AtomicBool>,
}

enum EmitterMsg {
    Event(CliEvent),
    Flush(oneshot::Sender<()>),
}

/// Flips `dead_flag` and wakes anyone parked on `dead_wake` when the
/// writer exits (normal return or panic). Setting the flag first means
/// a `flush()` that races in AFTER this drop still sees dead=true and
/// exits immediately instead of timing out; the notify is the fast path
/// for waiters already parked at drop time.
struct DeadGuard {
    dead_wake: Arc<Notify>,
    dead_flag: Arc<AtomicBool>,
}

impl Drop for DeadGuard {
    fn drop(&mut self) {
        self.dead_flag.store(true, Ordering::Release);
        self.dead_wake.notify_waiters();
    }
}

impl Emitter {
    pub fn new() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<EmitterMsg>();
        let dead_wake = Arc::new(Notify::new());
        let dead_flag = Arc::new(AtomicBool::new(false));
        let wake_for_task = dead_wake.clone();
        let flag_for_task = dead_flag.clone();
        let guard = DeadGuard {
            dead_wake: dead_wake.clone(),
            dead_flag: dead_flag.clone(),
        };
        tokio::spawn(async move {
            let _guard = guard;
            let stdout: Arc<Mutex<Stdout>> = Arc::new(Mutex::new(tokio::io::stdout()));
            // seq assigned in drain order → matches wire order exactly.
            let mut seq: u64 = 0;
            while let Some(msg) = rx.recv().await {
                match msg {
                    EmitterMsg::Event(event) => {
                        let write_ok = write_event(&stdout, seq, &event).await;
                        seq += 1;
                        if !write_ok {
                            // stdout pipe broken or otherwise unusable:
                            // flip the flag first, notify second. Both
                            // are needed — flag for late-arriving
                            // flush() calls, notify for ones already
                            // parked. Then break so the channel drains
                            // into the void instead of backing up.
                            flag_for_task.store(true, Ordering::Release);
                            wake_for_task.notify_waiters();
                            break;
                        }
                    }
                    EmitterMsg::Flush(reply) => {
                        let _ = reply.send(());
                    }
                }
            }
        });
        Self {
            tx,
            dead_wake,
            dead_flag,
            finalized: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Enqueue an event for emission. Non-blocking; safe from sync contexts.
    /// Returns `false` if the writer is dead or finalized — silently drops
    /// the event so a long crash-handler path can't pile up unbounded
    /// messages in the mpsc behind a dead consumer.
    pub fn enqueue(&self, event: CliEvent) -> bool {
        if self.finalized.load(Ordering::Acquire) {
            // Post-finalization: swallow to preserve the "run_finished
            // is last" invariant.
            return false;
        }
        if self.dead_flag.load(Ordering::Acquire) {
            // Writer exited (BrokenPipe / panic). The mpsc channel is
            // still open from our side — without this check, subsequent
            // enqueues would succeed and pile up in an un-drained queue,
            // leaking memory until process exit.
            return false;
        }
        self.tx.send(EmitterMsg::Event(event)).is_ok()
    }

    /// Mark the emitter finalized. Call exactly once, right after enqueueing
    /// `run_finished`. Subsequent enqueues are dropped silently.
    pub fn finalize(&self) {
        self.finalized.store(true, Ordering::Release);
    }

    /// Wait until every event enqueued before this call has been written.
    /// Returns early if the writer is dead or unresponsive; never deadlocks.
    ///
    /// Death detection has two layers: the AtomicBool is an early-exit
    /// check that works even if the writer died before we parked; the
    /// Notify handles waiters already parked at death time.
    pub async fn flush(&self) {
        if self.dead_flag.load(Ordering::Acquire) {
            return;
        }

        let dead_wait = self.dead_wake.notified();
        tokio::pin!(dead_wait);

        let (reply_tx, reply_rx) = oneshot::channel();
        if self.tx.send(EmitterMsg::Flush(reply_tx)).is_err() {
            return;
        }

        // Re-check after registration: writer could have died in the
        // window between the first load and our notify waiter being
        // pinned.
        if self.dead_flag.load(Ordering::Acquire) {
            return;
        }

        tokio::select! {
            _ = reply_rx => {}
            _ = &mut dead_wait => {
                eprintln!("agent_proto emitter writer task died during flush");
            }
            _ = tokio::time::sleep(FLUSH_TIMEOUT) => {
                eprintln!("agent_proto emitter flush timed out after {:?}", FLUSH_TIMEOUT);
            }
        }
    }
}

/// Returns `true` if the line was written; `false` on a fatal stdout error
/// (BrokenPipe, etc.) so the caller can stop draining.
async fn write_event(stdout: &Arc<Mutex<Stdout>>, seq: u64, event: &CliEvent) -> bool {
    // Serialize to a Value, inject `seq`, then re-serialize. Keeps all
    // existing event shape intact while giving agents a monotonic counter.
    // If serialization fails (should never happen for sane events), emit
    // a minimal internal-error event instead of silently dropping.
    let mut line = match serde_json::to_value(event) {
        Ok(serde_json::Value::Object(mut map)) => {
            map.insert("seq".into(), serde_json::json!(seq));
            match serde_json::to_vec(&serde_json::Value::Object(map)) {
                Ok(v) => v,
                Err(e) => fallback_internal_error(seq, "reserialize", &e.to_string()),
            }
        }
        Ok(other) => serde_json::to_vec(&other).unwrap_or_default(),
        Err(e) => fallback_internal_error(seq, "serialize", &e.to_string()),
    };
    line.push(b'\n');
    let mut s = stdout.lock().await;
    if let Err(e) = s.write_all(&line).await {
        eprintln!("agent_proto emitter stdout write failed: {e}");
        // BrokenPipe: the consumer (agent) is gone. No point retrying;
        // subsequent writes would fail identically and each would add a
        // 5s flush() timeout. Signal fatal via false.
        return !matches!(e.kind(), ErrorKind::BrokenPipe);
    }
    if let Err(e) = s.flush().await {
        eprintln!("agent_proto emitter stdout flush failed: {e}");
        return !matches!(e.kind(), ErrorKind::BrokenPipe);
    }
    true
}

/// Last-ditch: synthesize a single-line `internal_error` event describing
/// why the real event couldn't be serialized. Never silently drops; if
/// even this fails, eprintln is the fallback.
fn fallback_internal_error(seq: u64, stage: &str, detail: &str) -> Vec<u8> {
    let msg = serde_json::json!({
        "type": "internal_error",
        "seq": seq,
        "stage": stage,
        "detail": detail,
    });
    match serde_json::to_vec(&msg) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("agent_proto emitter double-serialization failure: {e}");
            Vec::new()
        }
    }
}
