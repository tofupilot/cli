//! TP-957 end to end through the REAL engine: procedure on disk, worker
//! pool, Python phases. The unit tests on `determine_aggregate_outcome`
//! pass the shutdown cause in by hand; this proves the plumbing that
//! produces it (`handle_stop` → `cancel_all_jobs` → `request_shutdown`)
//! yields the outcome the dashboard will store.
//!
//! Two shapes, both of which the first attempt at the fix got wrong:
//!   - a phase FAILS under the default `on_first_failure: stop`, the rest
//!     are cancelled → the run is FAIL (it was STOP, uploaded ABORTED);
//!   - a passing gate phase asks to end the run (`then: {pass: stop}`)
//!     with a teardown stage queued → the run is STOP (the flag is never
//!     raised in that shape, so only the results can say so).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::sync::Mutex;

use execution_engine::event_sink::ExecutionEvent;
use execution_engine::job::Outcome;
use execution_engine::orchestrator::Orchestrator;
use execution_engine::procedure::loader::load_procedure_definition;
use execution_engine::EventSink;

/// The outcome each phase reported on its JobComplete event.
#[derive(Default)]
struct Capture(Mutex<Vec<(String, Outcome)>>);

impl EventSink for Capture {
    fn emit(&self, event: &ExecutionEvent) {
        if let ExecutionEvent::JobComplete { phase_key, outcome, .. } = event {
            self.0.lock().unwrap().push((phase_key.clone(), *outcome));
        }
    }
}

impl Capture {
    fn outcome_of(&self, key: &str) -> Option<Outcome> {
        self.0.lock().unwrap().iter().find(|(k, _)| k == key).map(|(_, o)| *o)
    }
}

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
    fn new(tag: &str, procedure_yaml: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tp957-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("phases")).unwrap();
        std::fs::write(dir.join("procedure.yaml"), procedure_yaml).unwrap();
        std::fs::write(
            dir.join("phases").join("ok.py"),
            "def ok(phase):\n    pass\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("phases").join("bad.py"),
            "def bad(phase):\n    phase.fail('out of tolerance')\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("phases").join("slow.py"),
            "import time\n\ndef slow(phase):\n    time.sleep(3)\n",
        )
        .unwrap();
        Self { dir }
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

async fn run(bed: &Bed, python: &Path, tag: &str) -> Outcome {
    run_with(bed, python, tag, false, 1).await.0
}

/// `stop_before_execute` raises an operator Stop after `submit_procedure`
/// and before `execute_all` is polled once: the CLI's graceful arm does
/// exactly that when Stop (or Ctrl-C) lands during startup.
async fn run_with(
    bed: &Bed,
    python: &Path,
    tag: &str,
    stop_before_execute: bool,
    workers: usize,
) -> (Outcome, Arc<Capture>) {
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
            vec!["default".to_string()],
            execution_engine::procedure::schema::ExecutionStrategy::PhaseFirst,
            std::collections::HashMap::new(),
            None,
        )
        .await
        .expect("submit_procedure");
    if stop_before_execute {
        orchestrator
            .state
            .write()
            .await
            .request_shutdown(execution_engine::state::ShutdownCause::Operator);
    }
    let stats = orchestrator.execute_all().await.expect("execute_all");
    orchestrator.shutdown().await.expect("shutdown");
    (stats.run_outcome.expect("run outcome present"), events)
}

#[tokio::test]
async fn failing_phase_under_stop_on_first_failure_reports_fail() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new(
        "fail",
        r#"
name: TP-957 fail
version: 1.0.0

execution:
  workers: 1

main:
  - key: setup
    name: Setup
    python: phases.ok
  - key: calibrate
    name: Calibrate buttons
    python: phases.bad
  - key: register
    name: Register device
    python: phases.ok
"#,
    );
    assert_eq!(run(&bed, &python, "fail").await, Outcome::Fail);
}

#[tokio::test]
async fn then_pass_stop_with_teardown_reports_stop() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new(
        "gate",
        r#"
name: TP-957 gate
version: 1.0.0

execution:
  workers: 1

main:
  - key: gate
    name: Gate
    python: phases.ok
    then:
      pass: stop
  - key: never
    name: Never runs
    python: phases.ok

teardown:
  - key: cleanup
    name: Cleanup
    python: phases.ok
"#,
    );
    assert_eq!(run(&bed, &python, "gate").await, Outcome::Stop);
}

/// Fourth review blocker: an operator Stop that lands before any phase
/// has reported leaves `job_results` empty with the flag up. On main
/// this read STOP. It must never read PASS.
#[tokio::test]
async fn operator_stop_before_execute_all_reports_stop() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new(
        "early-stop",
        r#"
name: TP-957 early stop
version: 1.0.0

execution:
  workers: 1

main:
  - key: a
    name: A
    python: phases.ok
  - key: b
    name: B
    python: phases.ok
"#,
    );
    assert_eq!(run_with(&bed, &python, "early-stop", true, 1).await.0, Outcome::Stop);
}

/// Multi-worker: `Slow` is still running when `Bad` fails. The run
/// failed, and must read FAIL, not STOP. This is the shape a
/// multi-fixture station runs in; single-worker tests never produce it.
///
/// Since stop scope follows job scope, the failure stops the slot without
/// raising the execution flag, so `Slow` keeps its real outcome (PASS)
/// instead of a manufactured Stop.
#[tokio::test]
async fn failing_phase_with_a_sibling_in_flight_reports_fail() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new(
        "two-workers",
        r#"
name: TP-957 two workers
version: 1.0.0

execution:
  workers: 2

main:
  - key: slow
    name: Slow
    python: phases.slow
  - key: bad
    name: Bad
    python: phases.bad
"#,
    );
    let (outcome, events) = run_with(&bed, &python, "two-workers", false, 2).await;
    assert_eq!(outcome, Outcome::Fail);
    assert_eq!(events.outcome_of("bad"), Some(Outcome::Fail));
    assert_eq!(
        events.outcome_of("slow"),
        Some(Outcome::Pass),
        "a sibling finishing after the failure keeps its real outcome"
    );
}
