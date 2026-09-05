//! Process-level shutdown requests routed into a run's cancel token.
//!
//! A run must never end without its teardown: an operator's Ctrl-C or
//! Ctrl-Break, a supervisor's SIGTERM or a station stop has to stop the
//! running phases, run the execution-scoped teardown and enqueue the
//! partial runs, with a bounded escalation past the engine's teardown
//! cap. Unlike the TUI's first Ctrl-X, which lets a running phase
//! finish, these interrupt it: an hours-long burn-in phase is not
//! something a signal waits for.
//!
//! Supported stop paths: Ctrl-C, Ctrl-Break, SIGTERM / SIGHUP (Unix),
//! and a station stop (`tofupilot service stop`, Stop / Exit from the
//! operator UI). Closing the console window on Windows is best effort
//! only: the OS gives the process about five seconds
//! (`SPI_GETHUNGAPPTIMEOUT`) once the close handler returns, and logoff
//! and shutdown events are not delivered to interactive console
//! programs at all (services only). The hurried path fits what it can
//! into that window; fixture safety on the bench must come from the
//! instrument's own watchdog (`OUTP:PROT:WDOG` on the PSU), never from
//! this process being given time to power it down.
//!
//! Sources:
//!   * Unix SIGTERM / SIGHUP: `main.rs` owns the one process-wide
//!     listener (tokio swallows a signal for the process lifetime once
//!     any stream for it exists) and forwards it through [`request`].
//!   * Windows console close / logoff / shutdown / Ctrl-Break: polled by
//!     [`Listener`] itself. tokio's handler thread parks for the first
//!     three, so Windows waits (up to its own timeout) for this process
//!     to exit on its own instead of terminating it when the handler
//!     returns.
//!   * Ctrl-C (SIGINT / CTRL_C_EVENT): `tokio::signal::ctrl_c`, selected
//!     alongside the listener by [`drive_cancel`] and the station loop.
//!
//! Ladder ([`escalate`], shared by the one-shot run and the station):
//!   1. first signal: interrupt. Running main phases are killed, a
//!      teardown phase already running is left to finish, then the
//!      engine's teardown runs. Uncapped on purpose: a running teardown
//!      with a `timeout` bounds itself, one without it is bounded only
//!      by a second signal (after [`SECOND_SIGNAL_DEBOUNCE`]).
//!   2. the run reports its teardown started: [`TEARDOWN_CAP`] arms.
//!      Cap elapsed, or a second signal: force kill (no more teardown).
//!   3. third signal, or [`FORCE_GRACE`] elapsed: the process exits.
//!
//! Exit-code contract: a run stopped by any OS signal or console event
//! exits [`EXIT_SIGNALLED`] (130), Ctrl-C included, whether teardown
//! completed or the ladder had to escalate. Keystroke cancels inside the
//! TUI (Ctrl-C / Ctrl-X under raw mode) are not signals and keep the
//! run's own outcome code.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::cancel::CancelToken;

/// Exit code of a run stopped by an OS signal or console event.
pub const EXIT_SIGNALLED: i32 = 130;

/// What Windows leaves a console program after its close handler
/// returns before terminating it (`SPI_GETHUNGAPPTIMEOUT`, 5 s default).
/// Only the Windows console forwarder builds a stop that carries it.
#[cfg_attr(not(windows), allow(dead_code))]
pub const CONSOLE_CLOSE_BUDGET: Duration = Duration::from_secs(5);

/// Time past the engine's teardown cap for releasing the plugs: a
/// Cleanup RPC (5 s) plus Shutdown (1 s) per plug in sequence, so three
/// wedged plugs would need more, but a healthy release takes
/// milliseconds and the cap is there for the wedged case.
const PLUG_RELEASE_MARGIN: Duration = Duration::from_secs(15);

/// How long the teardown may take once the run reports it started
/// before the ladder escalates to a force kill: the engine's own cap on
/// the teardown phases plus the plug release.
pub const TEARDOWN_CAP: Duration =
    execution_engine::orchestrator::TEARDOWN_TIMEOUT.saturating_add(PLUG_RELEASE_MARGIN);

/// After a force kill the run task unwinds in well under this; past it
/// the process exits regardless, and the child registry's exit hook
/// reaps whatever is still alive.
const FORCE_GRACE: Duration = Duration::from_secs(10);

/// A second signal inside this window of the first is ignored. An
/// operator's reflex double Ctrl-C must not force-kill a power-off that
/// takes 2-3 s on real instruments; the escalation is for a teardown that
/// is actually stuck, and three seconds is long enough to tell the two
/// apart.
const SECOND_SIGNAL_DEBOUNCE: Duration = Duration::from_secs(3);

/// Resolve on the first signal that arrives after the debounce window.
async fn second_signal(listener: &mut Listener, armed_at: Instant, json_mode: bool) {
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = listener.wait() => {},
        }
        if armed_at.elapsed() >= SECOND_SIGNAL_DEBOUNCE {
            return;
        }
        if !json_mode {
            crate::log::warn("Teardown in progress; press again in a moment to force");
        }
    }
}

/// One request to stop, with what the OS gives us to honour it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stop {
    pub source: &'static str,
    /// `Some` when the OS terminates the process after this budget
    /// (Windows console close): the run takes its hurried path and
    /// skips the enqueue once the budget is gone. `None` for signals,
    /// which leave the ladder its full time.
    pub budget: Option<Duration>,
}

impl Stop {
    pub const fn signal(source: &'static str) -> Self {
        Self {
            source,
            budget: None,
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub const fn console_close(source: &'static str) -> Self {
        Self {
            source,
            budget: Some(CONSOLE_CLOSE_BUDGET),
        }
    }

    /// The instant the OS kills the process, if it will.
    pub fn deadline(&self) -> Option<Instant> {
        self.budget.map(|budget| Instant::now() + budget)
    }
}

/// `(request count, last stop)`. The count lets one listener observe
/// consecutive requests (first = interrupt, second = force) on a
/// channel that only retains the latest value.
type Request = (u64, Option<Stop>);

fn channel() -> &'static watch::Sender<Request> {
    static CHANNEL: OnceLock<watch::Sender<Request>> = OnceLock::new();
    CHANNEL.get_or_init(|| watch::channel((0, None)).0)
}

fn first() -> &'static OnceLock<Stop> {
    static FIRST: OnceLock<Stop> = OnceLock::new();
    &FIRST
}

/// Record the stop that ended the run, for the exit code. Ctrl-C
/// reaches the ladder through `tokio::signal::ctrl_c`, not through
/// [`request`], so it records itself here.
pub fn note(stop: Stop) {
    let _ = first().set(stop);
}

/// Forward a shutdown request from an OS signal or console event.
/// Returns `false` when no command is listening, in which case there
/// is no run to unwind and the caller should exit right away.
pub fn request(stop: Stop) -> bool {
    if !request_on(channel(), stop) {
        return false;
    }
    note(stop);
    true
}

fn request_on(tx: &watch::Sender<Request>, stop: Stop) -> bool {
    if tx.receiver_count() == 0 {
        return false;
    }
    tx.send_modify(|state| {
        state.0 += 1;
        state.1 = Some(stop);
    });
    true
}

/// The first stop that reached a run, if any.
pub fn requested() -> Option<Stop> {
    first().get().copied()
}

/// Subscription to shutdown requests. Hold it for as long as a run can
/// be in flight: `request` only forwards while a listener exists.
pub struct Listener {
    rx: watch::Receiver<Request>,
    seen: u64,
}

impl Listener {
    pub fn new() -> Self {
        #[cfg(windows)]
        spawn_console_event_forwarder();
        Self::on(channel())
    }

    fn on(tx: &watch::Sender<Request>) -> Self {
        let rx = tx.subscribe();
        let seen = rx.borrow().0;
        Self { rx, seen }
    }

    /// Resolves on each request made after subscription and not yet
    /// returned by this listener: the first call answers the first
    /// signal, the next call the signal after it.
    pub async fn wait(&mut self) -> Stop {
        loop {
            let (count, stop) = *self.rx.borrow_and_update();
            if count > self.seen {
                self.seen = count;
                return stop.unwrap_or(Stop::signal("signal"));
            }
            if self.rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

/// One-shot `tofupilot run`: the first signal starts the ladder.
///
/// `listener` is subscribed by the caller before the run starts, so a
/// signal arriving while the run is being set up still reaches it.
/// Runs on its own task; the caller aborts it once the run has ended.
pub async fn drive_cancel(cancel: CancelToken, mut listener: Listener, json_mode: bool) {
    let stop = tokio::select! {
        _ = tokio::signal::ctrl_c() => Stop::signal("Ctrl-C"),
        stop = listener.wait() => stop,
    };
    note(stop);
    escalate(cancel, listener, stop, json_mode).await;
}

/// The ladder past the first stop, see the module doc. Ends the
/// process if the run is still there when the last rung is reached;
/// the caller aborts this future once the run has ended on its own.
pub async fn escalate(cancel: CancelToken, mut listener: Listener, stop: Stop, json_mode: bool) {
    cancel.interrupt_by(stop.deadline());
    let armed_at = Instant::now();
    if !json_mode {
        crate::log::warn(&format!(
            "{}: stopping the run, teardown in progress (again to force)",
            stop.source
        ));
    }

    let second = tokio::select! {
        _ = second_signal(&mut listener, armed_at, json_mode) => true,
        _ = cancel.wait_teardown_started() => false,
    };
    if !second {
        tokio::select! {
            _ = second_signal(&mut listener, armed_at, json_mode) => {},
            _ = tokio::time::sleep(TEARDOWN_CAP) => {
                if !json_mode {
                    crate::log::warn("Teardown exceeded its cap; force killing the run");
                }
            },
        }
    }
    cancel.kill();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = listener.wait() => {},
        _ = tokio::time::sleep(FORCE_GRACE) => {},
    }
    if !json_mode {
        eprintln!("\nForce exit.");
    }
    std::process::exit(EXIT_SIGNALLED);
}

/// Tell an operator at a Windows console what closing it does. Once,
/// at startup, only when stderr is that console: a service or a
/// redirected log has no window to close.
pub fn announce_console_close_budget(json_mode: bool) {
    #[cfg(windows)]
    {
        use std::io::IsTerminal;
        if !json_mode && std::io::stderr().is_terminal() {
            crate::log::info(&format!(
                "Closing this window gives {} s to power down; use Ctrl-C to stop the run",
                CONSOLE_CLOSE_BUDGET.as_secs()
            ));
        }
    }
    #[cfg(not(windows))]
    let _ = json_mode;
}

/// Windows console events, forwarded once per process. The streams are
/// created here and polled for the process lifetime, which is what
/// keeps the parked handler thread from terminating the process the
/// moment the handler returns. Close carries the OS budget; logoff and
/// shutdown are listed for completeness (a service would get them) and
/// take the same hurried path; Ctrl-Break is an ordinary signal.
#[cfg(windows)]
fn spawn_console_event_forwarder() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown};
    let (Ok(mut close), Ok(mut logoff), Ok(mut shutdown), Ok(mut brk)) =
        (ctrl_close(), ctrl_logoff(), ctrl_shutdown(), ctrl_break())
    else {
        return;
    };
    tokio::spawn(async move {
        loop {
            let stop = tokio::select! {
                _ = close.recv() => Stop::console_close("Console closing"),
                _ = logoff.recv() => Stop::console_close("Logging off"),
                _ = shutdown.recv() => Stop::console_close("System shutting down"),
                _ = brk.recv() => Stop::signal("Ctrl-Break"),
            };
            request(stop);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_without_listener_is_refused() {
        let (tx, _) = watch::channel((0, None));
        assert!(!request_on(&tx, Stop::signal("SIGTERM")));
        let mut listener = Listener::on(&tx);
        assert!(request_on(&tx, Stop::signal("SIGTERM")));
        assert_eq!(listener.wait().await, Stop::signal("SIGTERM"));
    }

    #[tokio::test]
    async fn consecutive_requests_resolve_once_each() {
        let (tx, _) = watch::channel((0, None));
        let mut listener = Listener::on(&tx);
        assert!(request_on(&tx, Stop::signal("SIGTERM")));
        assert_eq!(listener.wait().await.source, "SIGTERM");
        assert!(request_on(&tx, Stop::console_close("Console closing")));
        let second = listener.wait().await;
        assert_eq!(second.source, "Console closing");
        assert_eq!(second.budget, Some(CONSOLE_CLOSE_BUDGET));
        // Nothing new was requested: a third wait pends.
        let pending = tokio::time::timeout(Duration::from_millis(50), listener.wait()).await;
        assert!(pending.is_err());
    }

    #[tokio::test]
    async fn requests_before_subscription_are_not_replayed() {
        let (tx, keep) = watch::channel((0, None));
        assert!(request_on(&tx, Stop::signal("SIGTERM")));
        drop(keep);
        let mut listener = Listener::on(&tx);
        let pending = tokio::time::timeout(Duration::from_millis(50), listener.wait()).await;
        assert!(pending.is_err());
    }

    #[tokio::test]
    async fn second_signal_inside_the_debounce_is_ignored() {
        let (tx, _) = watch::channel((0, None));
        let mut listener = Listener::on(&tx);
        let armed_at = Instant::now();
        assert!(request_on(&tx, Stop::signal("SIGTERM")));
        let early = tokio::time::timeout(
            Duration::from_millis(100),
            second_signal(&mut listener, armed_at, true),
        )
        .await;
        assert!(
            early.is_err(),
            "a signal inside the debounce must not escalate"
        );
    }

    #[tokio::test]
    async fn second_signal_after_the_debounce_escalates() {
        let (tx, _) = watch::channel((0, None));
        let mut listener = Listener::on(&tx);
        let armed_at = Instant::now() - SECOND_SIGNAL_DEBOUNCE;
        assert!(request_on(&tx, Stop::signal("SIGTERM")));
        let late = tokio::time::timeout(
            Duration::from_millis(200),
            second_signal(&mut listener, armed_at, true),
        )
        .await;
        assert!(late.is_ok(), "a signal past the debounce must escalate");
    }

    #[test]
    fn only_a_console_close_carries_a_deadline() {
        assert!(Stop::signal("Ctrl-C").deadline().is_none());
        let close = Stop::console_close("Console closing");
        let deadline = close.deadline().expect("console close has a deadline");
        assert!(deadline <= Instant::now() + CONSOLE_CLOSE_BUDGET);
        assert!(TEARDOWN_CAP > execution_engine::orchestrator::TEARDOWN_TIMEOUT);
    }
}
