//! Cross-run lifecycle of the station plug host, against a real Python
//! plug process (`tp_plug.py` + system `python3`).
//!
//! The plug class appends one line to an `init_log` file every time its
//! `__init__` runs, so the number of lines is the number of times the
//! instrument "connected". The contract under test is the AST-style
//! requirement: connect once, hold across runs, reconnect only when the
//! definition changes or the process dies.

use std::path::PathBuf;
use std::sync::Arc;

use execution_engine::plugs::plug_service::probe_plug_health;
use execution_engine::plugs::station_host::StationPlugHost;
use execution_engine::{EventSink, NullSink};

/// True while some process is listening on the loopback port.
fn port_listening(port: u16) -> bool {
    let out = std::process::Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}")])
        .output()
        .expect("lsof available on dev machines");
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

fn python3() -> Option<PathBuf> {
    let out = std::process::Command::new("which")
        .arg("python3")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?;
    let path = PathBuf::from(path.trim());
    path.exists().then_some(path)
}

struct TestBed {
    dir: PathBuf,
    init_log: PathBuf,
}

impl TestBed {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tp-station-host-{}", name));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let init_log = dir.join("init_log");

        let plug_py = format!(
            r#"
class CountingPlug:
    def __init__(self, marker="default"):
        with open({log:?}, "a") as f:
            f.write(marker + "\n")

    def ping(self):
        return "pong"

    def tearDown(self):
        pass
"#,
            log = init_log.to_string_lossy()
        );
        std::fs::write(dir.join("counting_plug.py"), plug_py).unwrap();

        Self { dir, init_log }
    }

    fn config(&self, marker: &str) -> serde_json::Value {
        serde_json::json!({
            "file": self.dir.join("counting_plug.py").to_string_lossy(),
            "class": "CountingPlug",
            "config": { "marker": marker },
        })
    }

    fn init_count(&self) -> usize {
        std::fs::read_to_string(&self.init_log)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }
}

impl Drop for TestBed {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn sink() -> Arc<dyn EventSink> {
    Arc::new(NullSink)
}

#[tokio::test]
async fn station_plug_survives_across_acquires() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = TestBed::new("survives");
    let host = StationPlugHost::new();
    let python = Some(python);

    // Run 1: first acquire spawns → one __init__.
    let port_1 = host
        .acquire(&bed.dir, &python, "psu", "PSU", bed.config("v1"), &sink())
        .await
        .expect("first acquire spawns");
    assert_eq!(bed.init_count(), 1);
    assert_eq!(host.held_count().await, 1);

    // Run 2: same definition → reuse, same port, NO second __init__.
    let port_2 = host
        .acquire(&bed.dir, &python, "psu", "PSU", bed.config("v1"), &sink())
        .await
        .expect("second acquire reuses");
    assert_eq!(port_1, port_2, "held instance must keep its port");
    assert_eq!(bed.init_count(), 1, "reuse must not re-run __init__");

    host.shutdown(None).await;
    assert_eq!(host.held_count().await, 0);
}

#[tokio::test]
async fn changed_definition_respawns() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = TestBed::new("respawn");
    let host = StationPlugHost::new();
    let python = Some(python);

    host.acquire(&bed.dir, &python, "psu", "PSU", bed.config("v1"), &sink())
        .await
        .unwrap();
    assert_eq!(bed.init_count(), 1);

    // Changed __init__ kwargs → fingerprint mismatch → respawn.
    host.acquire(&bed.dir, &python, "psu", "PSU", bed.config("v2"), &sink())
        .await
        .unwrap();
    assert_eq!(bed.init_count(), 2, "changed definition must respawn");

    // And the new definition is now the held one — a third acquire with
    // it reuses again.
    host.acquire(&bed.dir, &python, "psu", "PSU", bed.config("v2"), &sink())
        .await
        .unwrap();
    assert_eq!(bed.init_count(), 2);

    host.shutdown(None).await;
}

#[tokio::test]
async fn context_change_releases_all_held_plugs() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed_a = TestBed::new("ctx-a");
    let bed_b = TestBed::new("ctx-b");
    let host = StationPlugHost::new();
    let python = Some(python);

    let old_port = host
        .acquire(&bed_a.dir, &python, "psu", "PSU", bed_a.config("a"), &sink())
        .await
        .unwrap();
    assert_eq!(host.held_count().await, 1);
    assert!(port_listening(old_port), "old context's plug is live");

    // New procedure dir = procedure switch: old instance must not be
    // reused even though the key matches.
    let new_port = host
        .acquire(&bed_b.dir, &python, "psu", "PSU", bed_b.config("a"), &sink())
        .await
        .unwrap();
    assert_eq!(host.held_count().await, 1, "old context's plug released");
    assert_eq!(bed_b.init_count(), 1, "fresh spawn in the new context");

    // "Released" must mean the old PROCESS died, not just the map
    // entry: stop_all_services errors are swallowed with .ok(), so
    // only the socket can prove it. Teardown is graceful (Cleanup →
    // Shutdown → kill) — give it a moment.
    for _ in 0..20 {
        if !port_listening(old_port) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(
        !port_listening(old_port),
        "old context's plug process must be dead, not orphaned"
    );
    assert!(port_listening(new_port), "new context's plug is live");

    host.shutdown(None).await;
}

/// A renamed key is the same file, class and config under a new name —
/// the fingerprint check cannot see it, so the run's declared set is
/// what has to release the old instance. The device the old process
/// holds is the one the new key is about to open.
#[tokio::test]
async fn renamed_key_releases_the_orphaned_instance() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = TestBed::new("renamed-key");
    let host = StationPlugHost::new();
    let python = Some(python);

    let old_port = host
        .acquire(&bed.dir, &python, "psu", "PSU", bed.config("a"), &sink())
        .await
        .unwrap();
    assert!(port_listening(old_port));

    // The next run declares the same plug under `power_supply`.
    let declared = std::collections::HashSet::from(["power_supply".to_string()]);
    host.release_absent(&bed.dir, &python, &declared, &sink())
        .await;
    assert_eq!(host.held_count().await, 0, "`psu` is no longer declared");

    for _ in 0..20 {
        if !port_listening(old_port) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(
        !port_listening(old_port),
        "the orphaned instance must be dead, not holding the instrument"
    );

    let new_port = host
        .acquire(
            &bed.dir,
            &python,
            "power_supply",
            "PSU",
            bed.config("a"),
            &sink(),
        )
        .await
        .unwrap();
    assert!(port_listening(new_port));
    assert_eq!(bed.init_count(), 2, "the new key connected on its own");

    host.shutdown(None).await;
}

/// A declared plug is untouched, and so is a plug held for a DIFFERENT
/// procedure — pruning against another procedure's list would tear down
/// instances that procedure is still using.
#[tokio::test]
async fn release_absent_spares_declared_and_other_contexts() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = TestBed::new("prune-spares");
    let other = TestBed::new("prune-other");
    let host = StationPlugHost::new();
    let python = Some(python);

    let port = host
        .acquire(&bed.dir, &python, "psu", "PSU", bed.config("a"), &sink())
        .await
        .unwrap();

    let declared = std::collections::HashSet::from(["psu".to_string()]);
    host.release_absent(&bed.dir, &python, &declared, &sink())
        .await;
    assert_eq!(host.held_count().await, 1, "still declared, still held");

    // Another procedure's (empty) plug list must not reach this context.
    host.release_absent(&other.dir, &python, &Default::default(), &sink())
        .await;
    assert_eq!(host.held_count().await, 1, "context mismatch is a no-op");
    assert!(port_listening(port));
    assert_eq!(bed.init_count(), 1, "no respawn happened");

    host.shutdown(None).await;
}

#[tokio::test]
async fn dead_process_respawns_on_acquire() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = TestBed::new("dead");
    let host = StationPlugHost::new();
    let python = Some(python);

    let port = host
        .acquire(&bed.dir, &python, "psu", "PSU", bed.config("v1"), &sink())
        .await
        .unwrap();
    assert_eq!(bed.init_count(), 1);

    // Kill the plug process out from under the host. The Shutdown RPC
    // is only a courtesy notification (tp_plug.py replies and keeps
    // serving; the engine's graceful path force-kills afterwards), so a
    // real crash needs a real SIGKILL: find the listener's PID by port.
    {
        let out = std::process::Command::new("lsof")
            .args(["-ti", &format!("tcp:{port}")])
            .output()
            .expect("lsof available on dev machines");
        let pids = String::from_utf8_lossy(&out.stdout);
        assert!(!pids.trim().is_empty(), "plug process should be listening");
        for pid in pids.split_whitespace() {
            std::process::Command::new("kill")
                .args(["-9", pid])
                .status()
                .unwrap();
        }
        // Give the OS a moment to reap the listener.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    // Next acquire must detect the dead instance and respawn — and the
    // respawned instance must be USABLE, not just counted: a returned
    // port nobody listens on would pass an init-count-only assert.
    let new_port = host
        .acquire(&bed.dir, &python, "psu", "PSU", bed.config("v1"), &sink())
        .await
        .expect("acquire after process death must respawn");
    assert_eq!(bed.init_count(), 2, "death must trigger a fresh __init__");
    probe_plug_health(new_port)
        .await
        .expect("respawned plug must answer GetStatus");
    assert_eq!(
        host.held_count().await,
        1,
        "stale entry must be evicted, not accumulated"
    );

    host.shutdown(None).await;
}

/// The in-place redeploy scenario. A deployment update swaps the bundle
/// under the SAME directory, so neither the context check nor the
/// fingerprint (file/class/config — not code) can see it; the station
/// loop compensates by calling `shutdown` after applying a staged swap.
/// This test pins both halves: without the shutdown the old code is
/// silently reused (the gap), after it the next acquire respawns and
/// actually runs the new code (the fix).
#[tokio::test]
async fn in_place_redeploy_needs_shutdown_to_pick_up_new_code() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };
    let bed = TestBed::new("redeploy");
    let host = StationPlugHost::new();
    let python = Some(python);

    let old_port = host
        .acquire(&bed.dir, &python, "psu", "PSU", bed.config("v1"), &sink())
        .await
        .unwrap();
    assert_eq!(bed.init_count(), 1);

    // "Redeploy": rewrite the plug's code in place. Same path, same
    // class, same config — the fingerprint cannot tell the difference.
    let new_code = format!(
        r#"
class CountingPlug:
    def __init__(self, marker="default"):
        with open({log:?}, "a") as f:
            f.write(marker + "-new\n")

    def ping(self):
        return "pong"

    def tearDown(self):
        pass
"#,
        log = bed.init_log.to_string_lossy()
    );
    std::fs::write(bed.dir.join("counting_plug.py"), new_code).unwrap();

    // The gap: without intervention the held instance — built from the
    // replaced code — is reused as if nothing happened.
    let reused_port = host
        .acquire(&bed.dir, &python, "psu", "PSU", bed.config("v1"), &sink())
        .await
        .unwrap();
    assert_eq!(reused_port, old_port, "identical fingerprint must reuse");
    assert_eq!(bed.init_count(), 1, "reuse means no fresh __init__");

    // The fix: the station loop shuts the host down right after
    // applying the swap...
    host.shutdown(None).await;
    assert_eq!(host.held_count().await, 0);
    assert!(
        !port_listening(old_port),
        "shutdown must terminate the old-code process"
    );

    // ...so the next run's acquire spawns from the new bundle. The
    // marker proves the NEW code ran, not just that a respawn happened.
    let new_port = host
        .acquire(&bed.dir, &python, "psu", "PSU", bed.config("v1"), &sink())
        .await
        .expect("acquire after shutdown must respawn");
    assert_eq!(bed.init_count(), 2, "respawn must re-run __init__");
    let log = std::fs::read_to_string(&bed.init_log).unwrap();
    assert_eq!(
        log.lines().last(),
        Some("v1-new"),
        "respawned instance must run the redeployed code"
    );
    probe_plug_health(new_port)
        .await
        .expect("respawned plug must answer GetStatus");

    host.shutdown(None).await;
}
