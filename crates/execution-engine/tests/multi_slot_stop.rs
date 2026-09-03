//! Stop scope = job scope, end to end through the REAL engine: procedure
//! on disk, worker pool, Python phases, four slots on two workers.
//!
//! A job with a slot that stops (failure under `on_first_failure: stop`,
//! `phase.stop()`, error, timeout) cancels its own slot: the slot's
//! remaining main phases are skipped, its TeardownEach runs, and the other
//! slots carry on to their own outcome. A shared job (SetupAll,
//! TeardownAll) or the operator stops the execution. Each assertion reads
//! what the phases wrote to disk, not only the outcomes: a skipped phase
//! aggregates as PASS, so an outcome alone proves nothing ran.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use execution_engine::event_sink::ExecutionEvent;
use execution_engine::job::Outcome;
use execution_engine::orchestrator::{ExecutionStats, Orchestrator};
use execution_engine::procedure::loader::load_procedure_definition;
use execution_engine::procedure::schema::ExecutionStrategy;
use execution_engine::state::ShutdownCause;
use execution_engine::EventSink;

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

/// Every phase appends `<phase_key> <slot_id> <monotonic ns>` to `marks`
/// when it runs. Lines are appended atomically (O_APPEND, one short
/// write), so FILE ORDER is the cross-process execution order; the ns
/// column is per process on macOS and only orders marks of one worker. `bad_in` / `stop_in` misbehave only in the slot named
/// in `target_slot`, so one procedure can hold a failing slot and passing
/// neighbours.
struct Bed {
    dir: PathBuf,
}

impl Bed {
    fn new(tag: &str, procedure_yaml: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tp-slot-stop-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("phases")).unwrap();
        std::fs::write(dir.join("procedure.yaml"), procedure_yaml).unwrap();

        let marks = dir.join("marks").to_string_lossy().to_string();
        let target = dir.join("target_slot").to_string_lossy().to_string();
        let common = format!(
            r#"
import time

def _mark(key, run):
    with open({marks:?}, "a") as f:
        f.write(f"{{key}} {{run.slot_id}} {{time.monotonic_ns()}}\n")

def _target():
    try:
        with open({target:?}) as f:
            return f.read().strip()
    except FileNotFoundError:
        return None
"#
        );
        std::fs::write(dir.join("phases").join("_common.py"), &common).unwrap();

        let phase = |name: &str, body: &str| {
            std::fs::write(
                dir.join("phases").join(format!("{name}.py")),
                format!("from phases._common import _mark, _target\nimport time\n\n{body}\n"),
            )
            .unwrap();
        };
        phase("ok", "def ok(phase, run):\n    _mark('ok', run)\n");
        phase("ok2", "def ok2(phase, run):\n    _mark('ok2', run)\n");
        phase(
            "bad_in",
            "def bad_in(phase, run):\n    _mark('bad_in', run)\n    if run.slot_id == _target():\n        phase.fail('out of tolerance')\n",
        );
        phase(
            "stop_in",
            "def stop_in(phase, run):\n    _mark('stop_in', run)\n    if run.slot_id == _target():\n        phase.stop('operator said so')\n",
        );
        phase("bad", "def bad(phase, run):\n    _mark('bad', run)\n    phase.fail('rig not ready')\n");
        phase(
            "flaky",
            "def flaky(phase, run):\n    _mark('flaky', run)\n    if run.retry_count == 0:\n        phase.retry('try again')\n",
        );
        phase(
            "slow",
            "def slow(phase, run):\n    _mark('slow_start', run)\n    time.sleep(3)\n    _mark('slow', run)\n",
        );
        phase("cleanup", "def cleanup(phase, run):\n    _mark('cleanup', run)\n");
        phase("power_off", "def power_off(phase, run):\n    _mark('power_off', run)\n");
        std::fs::write(dir.join("phases").join("__init__.py"), "").unwrap();

        Self { dir }
    }

    fn target(&self, slot: &str) {
        std::fs::write(self.dir.join("target_slot"), slot).unwrap();
    }

    /// `(phase_key, slot_id, ns)` in execution order.
    fn marks(&self) -> Vec<(String, String, u128)> {
        std::fs::read_to_string(self.dir.join("marks"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let mut it = l.split_whitespace();
                (
                    it.next().unwrap().to_string(),
                    it.next().unwrap().to_string(),
                    it.next().unwrap().parse().unwrap(),
                )
            })
            .collect()
    }

    fn ran(&self, key: &str) -> Vec<String> {
        self.marks()
            .into_iter()
            .filter(|(k, _, _)| k == key)
            .map(|(_, s, _)| s)
            .collect()
    }

    fn ran_in(&self, key: &str, slot: &str) -> usize {
        self.marks()
            .iter()
            .filter(|(k, s, _)| k == key && s == slot)
            .count()
    }

    /// Position in the marks file of the first / last mark of `key`.
    fn first_index(&self, key: &str) -> Option<usize> {
        self.marks().iter().position(|(k, _, _)| k == key)
    }

    fn last_index(&self, key: &str) -> Option<usize> {
        self.marks().iter().rposition(|(k, _, _)| k == key)
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// Records every outcome a phase instance reported, so it can be read
/// off the wire and not only the aggregate. A phase that ran ends with a
/// JobComplete; one that was cancelled only ever gets a Skipped
/// JobProgress with `outcome: Some(Skip)`.
#[derive(Default)]
struct Capture(Mutex<Vec<(String, Option<String>, Outcome)>>);

impl EventSink for Capture {
    fn emit(&self, event: &ExecutionEvent) {
        let seen = match event {
            ExecutionEvent::JobComplete {
                phase_key,
                slot_id,
                outcome,
                ..
            } => Some((phase_key.clone(), slot_id.clone(), *outcome)),
            ExecutionEvent::JobProgress {
                phase_key,
                slot_id,
                outcome: Some(outcome),
                ..
            } => Some((phase_key.clone(), slot_id.clone(), *outcome)),
            _ => None,
        };
        if let Some(seen) = seen {
            self.0.lock().unwrap().push(seen);
        }
    }
}

impl Capture {
    fn outcome_of(&self, key: &str, slot: Option<&str>) -> Option<Outcome> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(k, s, _)| k == key && s.as_deref() == slot)
            .map(|(_, _, o)| *o)
    }
}

struct Run {
    stats: ExecutionStats,
    events: Arc<Capture>,
}

/// `stop_when` polls the bed every 50ms and raises an operator Stop the
/// first time it returns true. Deterministic where a fixed sleep is not:
/// the stop lands where the marks say the run is.
async fn run(
    bed: &Bed,
    python: &Path,
    tag: &str,
    slots: &[&str],
    workers: usize,
    strategy: ExecutionStrategy,
    stop_when: Option<Box<dyn Fn(&Bed) -> bool + Send>>,
) -> Run {
    let procedure_def =
        load_procedure_definition(&bed.dir.join("procedure.yaml")).expect("procedure loads");
    let mut orchestrator = Orchestrator::new_with_python(
        workers,
        bed.dir.clone(),
        Some(python.to_path_buf()),
        None,
        format!("exec-{tag}"),
        format!("run-{tag}"),
        procedure_def,
        None,
    );
    let events = Arc::new(Capture::default());
    let sink: Arc<dyn EventSink> = events.clone();
    orchestrator.set_event_sink(sink);
    orchestrator.initialize().await.expect("initialize");
    orchestrator
        .submit_procedure(
            slots.iter().map(|s| s.to_string()).collect(),
            strategy,
            std::collections::HashMap::new(),
            None,
        )
        .await
        .expect("submit_procedure");

    let stopper = stop_when.map(|pred| {
        let state = orchestrator.state.clone();
        let dir = bed.dir.clone();
        tokio::spawn(async move {
            let bed = Bed { dir };
            loop {
                if pred(&bed) {
                    state.write().await.request_shutdown(ShutdownCause::Operator);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            // Don't let this Bed's Drop remove the directory under the run.
            std::mem::forget(bed);
        })
    });

    let stats = orchestrator.execute_all().await.expect("execute_all");
    orchestrator.shutdown().await.expect("shutdown");
    if let Some(h) = stopper {
        h.abort();
    }
    Run { stats, events }
}

const FOUR: [&str; 4] = ["s1", "s2", "s3", "s4"];

fn four_slot_yaml(main: &str) -> String {
    format!(
        r#"
name: Slot stop
version: 1.0.0

execution:
  workers: 2

setup:
  - key: prep
    name: Prep
    python: phases.ok

main:
{main}

teardown:
  - key: cleanup
    name: Cleanup
    python: phases.cleanup
  - key: power_off
    name: Power off
    scope: execution
    python: phases.power_off
"#
    )
}

/// Slot 2 fails in main. Slot 2 reads FAIL with its TeardownEach run and
/// its later phase skipped; slots 1, 3, 4 run every phase and read PASS.
/// TeardownAll runs once, after the last TeardownEach. The run is FAIL.
#[tokio::test]
async fn failing_slot_stops_itself_and_its_neighbours_finish() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new(
        "fail-one",
        &four_slot_yaml(
            r#"
  - key: a
    name: A
    python: phases.ok
  - key: b
    name: B
    python: phases.bad_in
  - key: c
    name: C
    python: phases.ok2
"#,
        ),
    );
    bed.target("s2");

    let r = run(&bed, &python, "fail-one", &FOUR, 2, ExecutionStrategy::PhaseFirst, None).await;

    assert_eq!(r.stats.run_outcome, Some(Outcome::Fail));
    assert_eq!(r.stats.slot_outcomes["s2"], Outcome::Fail);
    for s in ["s1", "s3", "s4"] {
        assert_eq!(r.stats.slot_outcomes[s], Outcome::Pass, "slot {s} must not be dragged down");
        assert_eq!(bed.ran_in("ok2", s), 1, "phase C must run in slot {s}");
    }
    assert_eq!(bed.ran_in("ok2", "s2"), 0, "phase C is skipped in the failed slot");
    assert_eq!(r.events.outcome_of("c", Some("s2")), Some(Outcome::Skip));
    for s in FOUR {
        assert_eq!(bed.ran_in("cleanup", s), 1, "TeardownEach runs in slot {s}");
    }
    assert_eq!(bed.ran("power_off").len(), 1, "TeardownAll runs exactly once");
    assert!(
        bed.first_index("power_off").unwrap() > bed.last_index("cleanup").unwrap(),
        "TeardownAll runs after every TeardownEach; marks: {:?}",
        bed.marks()
    );
    assert_eq!(r.stats.completed_jobs, r.stats.total_jobs, "progress must close");
}

/// SetupAll fails: every slot FAIL, no main phase ran, TeardownAll ran.
/// Both strategies: under slot-first the slots not yet started must be
/// cancelled too, not queued after the first slot's TeardownEach.
#[tokio::test]
async fn shared_setup_failure_stops_every_slot() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    for (tag, strategy) in [
        ("setup-all-phase-first", ExecutionStrategy::PhaseFirst),
        ("setup-all-slot-first", ExecutionStrategy::SlotFirst),
    ] {
        let bed = Bed::new(
            tag,
            r#"
name: Shared setup fails
version: 1.0.0

execution:
  workers: 2

setup:
  - key: rig
    name: Rig
    scope: execution
    python: phases.bad
  - key: prep
    name: Prep
    python: phases.ok

main:
  - key: a
    name: A
    python: phases.ok2

teardown:
  - key: cleanup
    name: Cleanup
    python: phases.cleanup
  - key: power_off
    name: Power off
    scope: execution
    python: phases.power_off
"#,
        );

        let r = run(&bed, &python, tag, &FOUR, 2, strategy, None).await;

        assert_eq!(r.stats.run_outcome, Some(Outcome::Fail), "{tag}");
        assert_eq!(r.stats.slot_outcomes.len(), 4, "{tag}: every slot gets an outcome");
        for s in FOUR {
            assert_eq!(r.stats.slot_outcomes[s], Outcome::Fail, "{tag}: slot {s}");
        }
        assert!(bed.ran("ok2").is_empty(), "{tag}: no main phase may run");
        assert!(bed.ran("ok").is_empty(), "{tag}: no SetupEach may run");
        assert_eq!(bed.ran("power_off").len(), 1, "{tag}: TeardownAll runs once");
        assert!(
            r.stats.completed_jobs <= r.stats.total_jobs,
            "{tag}: progress must never overshoot ({} / {})",
            r.stats.completed_jobs,
            r.stats.total_jobs
        );
    }
}

/// `phase.stop()` in slot 3: slot 3 STOP, its later phase skipped, its
/// teardown run; the others PASS. The run is STOP (worst-of fold).
#[tokio::test]
async fn phase_stop_in_one_slot_stops_that_slot_only() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new(
        "stop-one",
        &four_slot_yaml(
            r#"
  - key: a
    name: A
    python: phases.ok
  - key: gate
    name: Gate
    python: phases.stop_in
  - key: c
    name: C
    python: phases.ok2
"#,
        ),
    );
    bed.target("s3");

    let r = run(&bed, &python, "stop-one", &FOUR, 2, ExecutionStrategy::PhaseFirst, None).await;

    assert_eq!(r.stats.slot_outcomes["s3"], Outcome::Stop);
    for s in ["s1", "s2", "s4"] {
        assert_eq!(r.stats.slot_outcomes[s], Outcome::Pass, "slot {s}");
        assert_eq!(bed.ran_in("ok2", s), 1);
    }
    assert_eq!(bed.ran_in("ok2", "s3"), 0);
    assert_eq!(bed.ran_in("cleanup", "s3"), 1, "the stopped slot still tears down");
    assert_eq!(r.stats.run_outcome, Some(Outcome::Stop));
}

/// A delayed retry pending in a slot that gets cancelled never executes;
/// the same retry in the neighbour slot does.
#[tokio::test]
async fn cancelled_slot_never_runs_its_pending_retry() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new(
        "retry",
        r#"
name: Pending retry
version: 1.0.0

execution:
  workers: 2

main:
  - key: flaky
    name: Flaky
    python: phases.flaky
    retry:
      limit: 2
      delay: 5s
  - key: b
    name: B
    python: phases.bad_in
"#,
    );
    bed.target("s1");

    let r = run(&bed, &python, "retry", &["s1", "s2"], 2, ExecutionStrategy::PhaseFirst, None).await;

    assert_eq!(bed.ran_in("flaky", "s1"), 1, "the cancelled slot's retry must not run");
    assert_eq!(bed.ran_in("flaky", "s2"), 2, "the neighbour's retry runs");
    assert_eq!(r.stats.slot_outcomes["s1"], Outcome::Fail);
    assert_eq!(r.stats.slot_outcomes["s2"], Outcome::Pass);
    assert_eq!(r.stats.run_outcome, Some(Outcome::Fail));
    assert_eq!(r.stats.completed_jobs, r.stats.total_jobs, "the aborted retry still counts");
}

/// Operator stop mid-run under slot-first: the slot that had finished
/// keeps PASS, the one in flight reads STOP, the run reads STOP.
#[tokio::test]
async fn operator_stop_keeps_finished_slot_outcomes() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new(
        "operator",
        r#"
name: Operator stop
version: 1.0.0

execution:
  workers: 1

main:
  - key: a
    name: A
    python: phases.ok
  - key: slow
    name: Slow
    python: phases.slow
"#,
    );

    let r = run(
        &bed,
        &python,
        "operator",
        &["s1", "s2"],
        1,
        ExecutionStrategy::SlotFirst,
        Some(Box::new(|bed: &Bed| bed.ran_in("slow_start", "s2") == 1)),
    )
    .await;

    assert_eq!(bed.ran_in("slow", "s1"), 1, "slot 1 finished before the stop");
    assert_eq!(r.stats.slot_outcomes["s1"], Outcome::Pass, "a finished slot keeps its outcome");
    assert_eq!(r.stats.slot_outcomes["s2"], Outcome::Stop);
    assert_eq!(r.stats.run_outcome, Some(Outcome::Stop));
}

/// Single slot: the last main phase may still retry when a teardown stage
/// follows it. The old "queue holds only teardown ⇒ shutting down" guess
/// denied that retry and the run read PASS off an Outcome::Retry.
#[tokio::test]
async fn last_main_phase_retries_with_a_teardown_queued() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new(
        "last-retry",
        r#"
name: Last phase retries
version: 1.0.0

execution:
  workers: 1

main:
  - key: flaky
    name: Flaky
    python: phases.flaky
    retry:
      limit: 2

teardown:
  - key: cleanup
    name: Cleanup
    python: phases.cleanup
"#,
    );

    let r = run(&bed, &python, "last-retry", &["default"], 1, ExecutionStrategy::PhaseFirst, None).await;

    assert_eq!(bed.ran_in("flaky", "default"), 2, "the retry must run");
    assert_eq!(r.stats.run_outcome, Some(Outcome::Pass));
    assert_eq!(r.events.outcome_of("flaky", Some("default")), Some(Outcome::Pass));
}
