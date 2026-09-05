//! An operator stop must leave the execution-scope plugs up for the
//! teardown phases `shutdown()` runs afterwards.
//!
//! The end of `execute_all` used to destroy every scope plug whenever
//! its loop ended, including on `shutdown_requested` with teardown
//! phases still queued. `power_off(ps1)` then ran with an empty plug
//! map and errored, so every operator stop left the bench powered. Two
//! stops are covered: the graceful one (running phases finish first)
//! and the interrupt (`interrupt_running_jobs`: running phases are
//! killed, teardown still runs), the path console close and SIGTERM
//! take; each under both scheduling strategies, since slot-first parks
//! the shared teardown outside the queue. An interrupt that lands while
//! the teardown itself is running must leave it alone, and the hurried
//! shutdown (console close, seconds of budget) must still power off.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use execution_engine::job::Outcome;
use execution_engine::orchestrator::{Orchestrator, ShutdownMode, TEARDOWN_TIMEOUT};
use execution_engine::procedure::loader::load_procedure_definition;
use execution_engine::procedure::schema::ExecutionStrategy;
use execution_engine::state::ShutdownCause;
use execution_engine::{EventSink, NullSink};

fn python3() -> Option<PathBuf> {
    let out = std::process::Command::new("which")
        .arg("python3")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    path.exists().then_some(path)
}

struct Bed {
    dir: PathBuf,
}

impl Bed {
    /// Two slots on one shared PSU; `soak` sleeps `soak_secs` per slot.
    /// `power_off` writes `powered_off` from the phase only after the
    /// plug call returned, so the marker proves the teardown had its
    /// plug and ran to its end.
    fn new(tag: &str, soak_secs: u32) -> Self {
        Self::with_power_off(tag, soak_secs, 0)
    }

    /// The plug marks `powering_off` on entry and holds the call for
    /// `power_off_secs`; a worker killed meanwhile never writes
    /// `powered_off`.
    fn with_power_off(tag: &str, soak_secs: u32, power_off_secs: u32) -> Self {
        let dir = std::env::temp_dir().join(format!("tp-stop-plugs-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("phases")).unwrap();
        std::fs::create_dir_all(dir.join("instruments")).unwrap();
        std::fs::write(
            dir.join("procedure.yaml"),
            format!(
                r#"
name: Stop keeps teardown plugs
version: 1.0.0

plugs:
  - name: PSU
    key: psu
    python: instruments.psu:Psu
    scope: execution

execution:
  strategy: phase_first
  workers: 2
  slots:
    - key: s1
      name: Nest 1
    - key: s2
      name: Nest 2

setup:
  - key: power_on
    name: Power on
    scope: execution
    python: phases.rack:power_on

main:
  - key: soak
    name: Soak
    python: phases.soak

teardown:
  - key: power_off
    name: Power off
    scope: execution
    python: phases.rack:power_off
"#
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("instruments").join("psu.py"),
            format!(
                r#"
import time

class Psu:
    def output(self, on):
        if not on:
            with open({started:?}, "w") as f:
                f.write("off\n")
            time.sleep({power_off_secs})
        return on
"#,
                started = dir.join("powering_off").to_string_lossy(),
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("phases").join("rack.py"),
            format!(
                r#"
def power_on(psu):
    psu.output(True)

def power_off(psu):
    psu.output(False)
    with open({marker:?}, "w") as f:
        f.write("off\n")
"#,
                marker = dir.join("powered_off").to_string_lossy()
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("phases").join("soak.py"),
            format!(
                r#"
import time

def soak(run):
    with open({started:?}, "a") as f:
        f.write(run.slot_id + "\n")
    time.sleep({soak_secs})
"#,
                started = dir.join("soaking").to_string_lossy()
            ),
        )
        .unwrap();
        std::fs::write(dir.join("phases").join("__init__.py"), "").unwrap();
        Self { dir }
    }

    fn soaking_count(&self) -> usize {
        std::fs::read_to_string(self.dir.join("soaking"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn powered_off(&self) -> bool {
        self.dir.join("powered_off").exists()
    }

    fn powering_off(&self) -> bool {
        self.dir.join("powering_off").exists()
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn orchestrator(bed: &Bed, python: PathBuf, tag: &str) -> Orchestrator {
    let def = load_procedure_definition(&bed.dir.join("procedure.yaml")).expect("procedure loads");
    let mut orchestrator = Orchestrator::new_with_python(
        2,
        bed.dir.clone(),
        Some(python),
        None,
        format!("exec-{tag}"),
        format!("run-{tag}"),
        def,
        None,
    );
    let sink: Arc<dyn EventSink> = Arc::new(NullSink);
    orchestrator.set_event_sink(sink);
    orchestrator
}

async fn wait_for_soak(bed: &Bed) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while bed.soaking_count() < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(bed.soaking_count(), 2, "both slots should be soaking");
}

#[tokio::test]
async fn graceful_stop_leaves_the_plug_for_the_teardown() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new("graceful", 1);
    let mut orchestrator = orchestrator(&bed, python, "graceful");
    orchestrator.initialize().await.expect("initialize");
    orchestrator
        .submit_procedure(
            vec!["s1".into(), "s2".into()],
            ExecutionStrategy::PhaseFirst,
            Default::default(),
            None,
        )
        .await
        .expect("submit_procedure");

    let state = orchestrator.state.clone();
    let dir = bed.dir.clone();
    let stopper = tokio::spawn(async move {
        let bed = Bed { dir };
        wait_for_soak(&bed).await;
        state
            .write()
            .await
            .request_shutdown(ShutdownCause::Operator);
        std::mem::forget(bed);
    });

    let stats = orchestrator.execute_all().await.expect("execute_all");
    orchestrator.shutdown().await.expect("shutdown");
    stopper.abort();

    assert_eq!(stats.run_outcome, Some(Outcome::Stop));
    assert!(bed.powered_off(), "power_off ran without its execution-scope plug");
}

#[tokio::test]
async fn interrupt_kills_the_soak_and_still_powers_off() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new("interrupt", 30);
    let mut orchestrator = orchestrator(&bed, python, "interrupt");
    orchestrator.initialize().await.expect("initialize");
    orchestrator
        .submit_procedure(
            vec!["s1".into(), "s2".into()],
            ExecutionStrategy::PhaseFirst,
            Default::default(),
            None,
        )
        .await
        .expect("submit_procedure");

    let state = orchestrator.state.clone();
    let workers = orchestrator.workers.clone();
    let dir = bed.dir.clone();
    let interrupter = tokio::spawn(async move {
        let bed = Bed { dir };
        wait_for_soak(&bed).await;
        std::mem::forget(bed);
        Orchestrator::interrupt_running_jobs(state, workers).await;
        Instant::now()
    });

    let stats = orchestrator.execute_all().await.expect("execute_all");
    orchestrator.shutdown().await.expect("shutdown");
    let interrupted_at = interrupter.await.unwrap();

    assert_eq!(stats.run_outcome, Some(Outcome::Stop));
    assert!(bed.powered_off(), "power_off ran without its execution-scope plug");
    let took = interrupted_at.elapsed();
    assert!(
        took < Duration::from_secs(10),
        "interrupt waited {took:?} for a 30 s soak instead of killing it"
    );
}

async fn wait_for(deadline: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + deadline;
    while !ready() && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ready()
}

/// Slot-first parks the shared `power_off` in
/// `teardown_procedure_jobs`, not in the queue, and a stop returns early
/// from `check_and_queue_next_slot`. `has_queued_teardown` used to look
/// at the queue only, so the plugs were released before `shutdown()`
/// ran the parked teardown against an empty plug map.
#[tokio::test]
async fn slot_first_stop_keeps_the_plug_for_the_parked_teardown() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new("slot-first", 1);
    let mut orchestrator = orchestrator(&bed, python, "slot-first");
    orchestrator.initialize().await.expect("initialize");
    orchestrator
        .submit_procedure(
            vec!["s1".into(), "s2".into()],
            ExecutionStrategy::SlotFirst,
            Default::default(),
            None,
        )
        .await
        .expect("submit_procedure");

    let state = orchestrator.state.clone();
    let dir = bed.dir.clone();
    let stopper = tokio::spawn(async move {
        let bed = Bed { dir };
        // Slot-first: only the first slot soaks before the stop.
        assert!(
            wait_for(Duration::from_secs(60), || bed.soaking_count() >= 1).await,
            "first slot should be soaking"
        );
        state
            .write()
            .await
            .request_shutdown(ShutdownCause::Operator);
        std::mem::forget(bed);
    });

    let stats = orchestrator.execute_all().await.expect("execute_all");
    orchestrator.shutdown().await.expect("shutdown");
    stopper.abort();

    assert_eq!(stats.run_outcome, Some(Outcome::Stop));
    assert!(bed.powered_off(), "power_off ran without its execution-scope plug");
}

/// A signal that lands while `power_off` is already running (the run
/// reached its teardown on its own) must not kill it: the bench would
/// stay on with nothing left to re-run the power-off.
#[tokio::test]
async fn interrupt_during_a_running_teardown_lets_it_finish() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::with_power_off("teardown-running", 0, 3);
    let mut orchestrator = orchestrator(&bed, python, "teardown-running");
    orchestrator.initialize().await.expect("initialize");
    orchestrator
        .submit_procedure(
            vec!["s1".into(), "s2".into()],
            ExecutionStrategy::PhaseFirst,
            Default::default(),
            None,
        )
        .await
        .expect("submit_procedure");

    let state = orchestrator.state.clone();
    let workers = orchestrator.workers.clone();
    let dir = bed.dir.clone();
    let interrupter = tokio::spawn(async move {
        let bed = Bed { dir };
        assert!(
            wait_for(Duration::from_secs(60), || bed.powering_off()).await,
            "power_off should have started"
        );
        std::mem::forget(bed);
        Orchestrator::interrupt_running_jobs(state, workers).await;
    });

    orchestrator.execute_all().await.expect("execute_all");
    orchestrator.shutdown().await.expect("shutdown");
    interrupter.await.unwrap();

    assert!(
        bed.powered_off(),
        "the interrupt killed the running power_off instead of letting it finish"
    );
}

/// Console close on Windows leaves seconds: the hurried shutdown kills
/// the pool outright, runs the teardown on a worker that is already up
/// and skips the plug Cleanup RPCs. The bench must still be powered off.
#[tokio::test]
async fn hurried_shutdown_still_powers_off() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new("hurried", 30);
    let mut orchestrator = orchestrator(&bed, python, "hurried");
    orchestrator.initialize().await.expect("initialize");
    orchestrator
        .submit_procedure(
            vec!["s1".into(), "s2".into()],
            ExecutionStrategy::PhaseFirst,
            Default::default(),
            None,
        )
        .await
        .expect("submit_procedure");

    let state = orchestrator.state.clone();
    let workers = orchestrator.workers.clone();
    let dir = bed.dir.clone();
    let interrupter = tokio::spawn(async move {
        let bed = Bed { dir };
        wait_for_soak(&bed).await;
        std::mem::forget(bed);
        Orchestrator::interrupt_running_jobs(state, workers).await;
        Instant::now()
    });

    let stats = orchestrator.execute_all().await.expect("execute_all");
    orchestrator
        .shutdown_with(ShutdownMode::Hurried)
        .await
        .expect("shutdown");
    let interrupted_at = interrupter.await.unwrap();

    assert_eq!(stats.run_outcome, Some(Outcome::Stop));
    assert!(bed.powered_off(), "hurried shutdown skipped the power-off");
    let took = interrupted_at.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "hurried stop took {took:?}; a console close gives about five seconds"
    );
}

/// A second signal while `power_off` is running must be able to abandon
/// it: `shutdown()` reads the force flag on entry only, so a force kill
/// raised meanwhile has to stop the teardown loop and kill its workers
/// itself, well before the engine's own teardown cap.
#[tokio::test]
async fn force_kill_during_the_teardown_abandons_it() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::with_power_off("force-teardown", 30, 60);
    let mut orchestrator = orchestrator(&bed, python, "force-teardown");
    orchestrator.initialize().await.expect("initialize");
    orchestrator
        .submit_procedure(
            vec!["s1".into(), "s2".into()],
            ExecutionStrategy::PhaseFirst,
            Default::default(),
            None,
        )
        .await
        .expect("submit_procedure");

    let state = orchestrator.state.clone();
    let workers = orchestrator.workers.clone();
    let resource_manager = orchestrator.resource_manager.clone();
    let dir = bed.dir.clone();
    let driver = tokio::spawn(async move {
        let bed = Bed { dir };
        wait_for_soak(&bed).await;
        Orchestrator::interrupt_running_jobs(state.clone(), workers.clone()).await;
        assert!(
            wait_for(Duration::from_secs(60), || bed.powering_off()).await,
            "power_off should have started"
        );
        std::mem::forget(bed);
        let sink: Arc<dyn EventSink> = Arc::new(NullSink);
        Orchestrator::force_kill_immediate(state, workers, resource_manager, None, sink)
            .await
            .expect("force kill");
        Instant::now()
    });

    orchestrator.execute_all().await.expect("execute_all");
    let shutdown_started = Instant::now();
    let _ = orchestrator.shutdown().await;
    let forced_at = driver.await.unwrap();

    assert!(
        forced_at.elapsed() < Duration::from_secs(5),
        "shutdown() ran on for {:?} after the force kill",
        forced_at.elapsed()
    );
    assert!(
        shutdown_started.elapsed() < TEARDOWN_TIMEOUT,
        "the force kill did not cut the teardown short"
    );
    assert!(!bed.powered_off(), "the 60 s power_off cannot have completed");
}
