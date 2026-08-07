//! The AST-gate test, end to end through the REAL engine: two complete
//! Orchestrator executions sharing one StationPlugHost, real worker
//! pool, real plug process, real phase RPC.
//!
//! Proves the full chain the isolated host tests cannot:
//!   procedure load → station branch in ensure_plugs_created_for_job →
//!   host acquire → register in run's ResourceManager → phase calls a
//!   method on the HELD instance → run teardown leaves it alive →
//!   second orchestrator reuses it → host shutdown kills it.
//!
//! The plug appends one line to `init_log` per `__init__`. The phase
//! itself asserts `read_init_count() == 1` — so if the second run had
//! respawned the plug, run 2 would FAIL, not just miscount.

use std::path::PathBuf;
use std::sync::Arc;

use execution_engine::orchestrator::Orchestrator;
use execution_engine::plugs::station_host::StationPlugHost;
use execution_engine::procedure::loader::load_procedure_definition;
use execution_engine::job::Outcome;
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
    init_log: PathBuf,
}

impl Bed {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("tp-two-runs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("instruments")).unwrap();
        std::fs::create_dir_all(dir.join("phases")).unwrap();
        let init_log = dir.join("init_log");

        std::fs::write(
            dir.join("procedure.yaml"),
            r#"
name: Two Runs Station Plug
version: 1.0.0

plugs:
  - name: Counting PSU
    key: psu
    python: instruments.counting_psu:CountingPsu
    scope: station

main:
  - key: check
    name: Check
    python: phases.check
"#,
        )
        .unwrap();

        std::fs::write(
            dir.join("instruments").join("counting_psu.py"),
            format!(
                r#"
class CountingPsu:
    def __init__(self):
        with open({log:?}, "a") as f:
            f.write("init\n")

    def read_init_count(self):
        with open({log:?}) as f:
            return len(f.readlines())
"#,
                log = init_log.to_string_lossy()
            ),
        )
        .unwrap();

        // The phase writes its own marker so a SKIPPED phase can't
        // masquerade as a pass: a skipped phase yields Outcome::Pass
        // (stats.rs aggregate fall-through), so outcome alone doesn't
        // prove execution.
        std::fs::write(
            dir.join("phases").join("check.py"),
            format!(
                r#"
def check(psu):
    count = psu.read_init_count()
    with open({ran:?}, "a") as f:
        f.write("ran\n")
    assert count == 1, f"plug __init__ ran {{count}} times, expected exactly 1"
"#,
                ran = dir.join("phase_ran").to_string_lossy()
            ),
        )
        .unwrap();

        Self { dir, init_log }
    }

    fn init_count(&self) -> usize {
        std::fs::read_to_string(&self.init_log)
            .map(|s| s.lines().count())
            .unwrap_or(0)
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

async fn run_once(
    bed: &Bed,
    python: &PathBuf,
    host: &Arc<StationPlugHost>,
    run_tag: &str,
) -> Outcome {
    let procedure_def = load_procedure_definition(&bed.dir.join("procedure.yaml"))
        .expect("procedure loads");

    let mut orchestrator = Orchestrator::new_with_python(
        1,
        bed.dir.clone(),
        Some(python.clone()),
        None,
        format!("exec-{run_tag}"),
        format!("run-{run_tag}"),
        procedure_def,
        None,
    )
    .with_station_plug_host(Some(Arc::clone(host)));

    let sink: Arc<dyn EventSink> = Arc::new(NullSink);
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
    let stats = orchestrator.execute_all().await.expect("execute_all");
    orchestrator.shutdown().await.expect("shutdown");

    stats.run_outcome.expect("run outcome present")
}

#[tokio::test]
async fn two_runs_share_one_station_plug_instance() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new();
    let host = Arc::new(StationPlugHost::new());

    // Run 1: spawns the plug, phase asserts one __init__.
    let outcome_1 = run_once(&bed, &python, &host, "one").await;
    assert_eq!(outcome_1, Outcome::Pass, "run 1 must pass");
    assert_eq!(bed.phase_ran_count(), 1, "run 1's phase must actually execute");
    assert_eq!(bed.init_count(), 1);
    assert_eq!(
        host.held_count().await,
        1,
        "run teardown must leave the station plug held"
    );

    // Run 2: brand-new orchestrator, same host. The PHASE asserts the
    // count is still 1 — a respawn would fail the run itself, not just
    // this test's bookkeeping.
    let outcome_2 = run_once(&bed, &python, &host, "two").await;
    assert_eq!(outcome_2, Outcome::Pass, "run 2 must pass on the held instance");
    assert_eq!(bed.phase_ran_count(), 2, "run 2's phase must actually execute");
    assert_eq!(bed.init_count(), 1, "no second __init__ across runs");

    // Station stops: instance released.
    host.shutdown(None).await;
    assert_eq!(host.held_count().await, 0);
}

#[tokio::test]
async fn hostless_run_tears_station_plug_down() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = Bed::new();

    async fn hostless_run(bed: &Bed, python: &PathBuf, tag: &str) -> Outcome {
        let procedure_def =
            load_procedure_definition(&bed.dir.join("procedure.yaml")).expect("loads");
        let mut orchestrator = Orchestrator::new_with_python(
            1,
            bed.dir.clone(),
            Some(python.clone()),
            None,
            format!("exec-hostless-{tag}"),
            format!("run-hostless-{tag}"),
            procedure_def,
            None,
        );
        let sink: Arc<dyn EventSink> = Arc::new(NullSink);
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
        let stats = orchestrator.execute_all().await.expect("execute_all");
        orchestrator.shutdown().await.expect("shutdown");
        stats.run_outcome.expect("outcome present")
    }

    // Run 1, no host: station degrades to execution scope, single instance,
    // phase passes on count == 1.
    let outcome_1 = hostless_run(&bed, &python, "one").await;
    assert_eq!(outcome_1, Outcome::Pass);
    assert_eq!(bed.phase_ran_count(), 1, "phase must actually execute");
    assert_eq!(bed.init_count(), 1);

    // Run 2, no host: THE teardown proof. If run 1's degraded plug had
    // survived (leak), this run would reuse it and the count would stay
    // 1. A fresh __init__ (count 2) is only possible if run 1 tore the
    // instance down. The phase's `count == 1` assert now fails, which
    // is expected — run 2's outcome must be non-Pass AND the log must
    // show a second init.
    let outcome_2 = hostless_run(&bed, &python, "two").await;
    assert_ne!(
        outcome_2,
        Outcome::Pass,
        "run 2's phase must see a FRESH instance (count 2) and fail its ==1 assert"
    );
    assert_eq!(bed.phase_ran_count(), 2, "run 2's phase must actually execute");
    assert_eq!(
        bed.init_count(),
        2,
        "second hostless run must respawn — run 1's instance was torn down"
    );
}

/// Partial-run narrowing against a hosted station: two station plugs,
/// a phase that uses one, a phase that uses neither.
///
/// Pins two behaviours:
/// - a partial run only borrows the station plugs in its introspected
///   union (playing `check` acquires `psu`, never spawns `dmm`);
/// - progress accounting closes: reserved plug-scope event slots match
///   what actually emits, so completed_jobs reaches total_jobs even when
///   the union excludes every station plug (playing `no_plug` reserves
///   no acquire event for the hosted plugs it will never borrow).
#[tokio::test]
async fn partial_run_borrows_only_station_plugs_in_union() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };

    let dir = std::env::temp_dir().join(format!("tp-partial-station-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("instruments")).unwrap();
    std::fs::create_dir_all(dir.join("phases")).unwrap();
    let psu_log = dir.join("psu_init_log");
    let dmm_log = dir.join("dmm_init_log");

    std::fs::write(
        dir.join("procedure.yaml"),
        r#"
name: Partial Station Narrowing
version: 1.0.0

plugs:
  - name: PSU
    key: psu
    python: instruments.psu:Psu
    scope: station
  - name: DMM
    key: dmm
    python: instruments.dmm:Dmm
    scope: station

main:
  - key: check
    name: Check
    python: phases.check
  - key: no_plug
    name: No Plug
    python: phases.no_plug
"#,
    )
    .unwrap();

    for (module, class, log) in [("psu", "Psu", &psu_log), ("dmm", "Dmm", &dmm_log)] {
        std::fs::write(
            dir.join("instruments").join(format!("{module}.py")),
            format!(
                r#"
class {class}:
    def __init__(self):
        with open({log:?}, "a") as f:
            f.write("init\n")

    def ping(self):
        return "ok"
"#,
                log = log.to_string_lossy()
            ),
        )
        .unwrap();
    }

    std::fs::write(
        dir.join("phases").join("check.py"),
        r#"
def check(psu):
    assert psu.ping() == "ok"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("phases").join("no_plug.py"),
        r#"
def no_plug():
    pass
"#,
    )
    .unwrap();

    let init_count = |log: &PathBuf| {
        std::fs::read_to_string(log)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };

    async fn partial_run(
        dir: &PathBuf,
        python: &PathBuf,
        host: &Arc<StationPlugHost>,
        target: &str,
    ) -> execution_engine::orchestrator::ExecutionStats {
        let procedure_def =
            load_procedure_definition(&dir.join("procedure.yaml")).expect("procedure loads");
        let mut orchestrator = Orchestrator::new_with_python(
            1,
            dir.clone(),
            Some(python.clone()),
            None,
            format!("exec-partial-{target}"),
            format!("run-partial-{target}"),
            procedure_def,
            None,
        )
        .with_station_plug_host(Some(Arc::clone(host)));

        let sink: Arc<dyn EventSink> = Arc::new(NullSink);
        orchestrator.set_event_sink(sink);
        orchestrator.initialize().await.expect("initialize");
        orchestrator
            .submit_procedure(
                vec!["default".to_string()],
                execution_engine::procedure::schema::ExecutionStrategy::PhaseFirst,
                std::collections::HashMap::new(),
                Some(target),
            )
            .await
            .expect("submit_procedure");
        let stats = orchestrator.execute_all().await.expect("execute_all");
        orchestrator.shutdown().await.expect("shutdown");
        stats
    }

    // Play `check` (uses psu): only psu is borrowed, dmm never spawns.
    {
        let host = Arc::new(StationPlugHost::new());
        let stats = partial_run(&dir, &python, &host, "check").await;
        assert_eq!(stats.run_outcome, Some(Outcome::Pass), "check run must pass");
        assert_eq!(
            host.held_count().await,
            1,
            "only the union's station plug may be borrowed"
        );
        assert_eq!(init_count(&psu_log), 1, "psu must spawn exactly once");
        assert_eq!(init_count(&dmm_log), 0, "dmm is outside the union: never spawned");
        assert_eq!(
            stats.completed_jobs, stats.total_jobs,
            "every reserved progress slot must be consumed"
        );
        host.shutdown(None).await;
    }

    // Play `no_plug` (uses neither): nothing borrowed, and the progress
    // total must not reserve an acquire event that will never fire.
    {
        let host = Arc::new(StationPlugHost::new());
        let stats = partial_run(&dir, &python, &host, "no_plug").await;
        assert_eq!(stats.run_outcome, Some(Outcome::Pass), "no_plug run must pass");
        assert_eq!(host.held_count().await, 0, "no station plug in the union");
        assert_eq!(init_count(&psu_log), 1, "psu count unchanged from the first run");
        assert_eq!(init_count(&dmm_log), 0);
        assert_eq!(
            stats.completed_jobs, stats.total_jobs,
            "no reserved-but-never-emitted plug event may remain"
        );
        host.shutdown(None).await;
    }

    std::fs::remove_dir_all(&dir).ok();
}
