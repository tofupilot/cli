//! Regression for the plug service accept loop: many slots calling one
//! execution-scope plug at the same instant. Before the threaded accept
//! and the enlarged backlog (`tp_plug.py serve()`), a burst larger than
//! the kernel backlog was reset with ECONNRESET and the phases errored.

use std::path::PathBuf;
use std::sync::Arc;

use execution_engine::job::Outcome;
use execution_engine::orchestrator::Orchestrator;
use execution_engine::procedure::loader::load_procedure_definition;
use execution_engine::{EventSink, NullSink};

const SLOTS: usize = 32;

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
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("tp-plug-fanout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("instruments")).unwrap();
        std::fs::create_dir_all(dir.join("phases")).unwrap();

        let slots: String = (1..=SLOTS)
            .map(|i| format!("    - name: slot{i:02}\n"))
            .collect();
        std::fs::write(
            dir.join("procedure.yaml"),
            format!(
                r#"
name: Plug Fan-Out
version: 1.0.0

plugs:
  - name: Shared PSU
    key: psu
    python: instruments.shared_psu:SharedPsu
    scope: execution

main:
  - key: read
    name: Read
    python: phases.read

execution:
  strategy: phase_first
  workers: {SLOTS}
  slots:
{slots}"#
            ),
        )
        .unwrap();

        std::fs::write(
            dir.join("instruments").join("shared_psu.py"),
            r#"
import time

class SharedPsu:
    def __init__(self):
        self.reads = 0

    def read(self):
        time.sleep(0.02)
        self.reads += 1
        return self.reads
"#,
        )
        .unwrap();

        std::fs::write(
            dir.join("phases").join("read.py"),
            format!(
                r#"
def read(psu):
    value = psu.read()
    with open({ran:?}, "a") as f:
        f.write(f"{{value}}\n")
"#,
                ran = dir.join("phase_ran").to_string_lossy()
            ),
        )
        .unwrap();

        Self { dir }
    }

    fn phase_ran_count(&self) -> usize {
        std::fs::read_to_string(self.dir.join("phase_ran"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

#[tokio::test]
async fn burst_of_slots_on_one_shared_plug_all_pass() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new();

    let procedure_def =
        load_procedure_definition(&bed.dir.join("procedure.yaml")).expect("procedure loads");
    let slots: Vec<String> = procedure_def
        .execution
        .as_ref()
        .map(|e| e.slots.iter().map(|s| s.key.clone()).collect())
        .unwrap_or_default();
    assert_eq!(slots.len(), SLOTS);

    let mut orchestrator = Orchestrator::new_with_python(
        SLOTS,
        bed.dir.clone(),
        Some(python),
        None,
        "exec-fanout".to_string(),
        "run-fanout".to_string(),
        procedure_def,
        None,
    );
    let sink: Arc<dyn EventSink> = Arc::new(NullSink);
    orchestrator.set_event_sink(sink);

    orchestrator.initialize().await.expect("initialize");
    orchestrator
        .submit_procedure(
            slots,
            execution_engine::procedure::schema::ExecutionStrategy::PhaseFirst,
            std::collections::HashMap::new(),
            None,
        )
        .await
        .expect("submit_procedure");
    let stats = orchestrator.execute_all().await.expect("execute_all");
    orchestrator.shutdown().await.expect("shutdown");

    assert_eq!(
        stats.run_outcome.expect("run outcome present"),
        Outcome::Pass,
        "every slot must reach the shared plug without a connection reset"
    );
    assert_eq!(bed.phase_ran_count(), SLOTS, "every slot's phase must execute");
}
