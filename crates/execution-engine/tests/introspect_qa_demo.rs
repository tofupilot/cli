//! Introspection end to end against the real qa-demo-iot fixture: a real
//! Python process imports the fixture's phase modules and the Rust side
//! intersects the reported parameters with the procedure's plug keys.
//!
//! The fixture is the reference case for partial-run plug narrowing:
//! two plugs, a setup phase that takes none (`all_setup(phase)`), a
//! phase with no callable at all (`phase_2`), and `check_voltage_plug`
//! that uses exactly one (`check_voltage_with_plug(measurements,
//! power_supply)`).

use std::path::PathBuf;

use execution_engine::procedure::introspect_procedure;
use execution_engine::procedure::load_procedure_definition;

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

#[tokio::test]
async fn introspects_qa_demo_iot() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not found");
        return;
    };

    let procedure_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/studio/procedures/qa-demo-iot")
        .canonicalize()
        .expect("qa-demo-iot fixture not found");

    let def = load_procedure_definition(&procedure_dir.join("procedure.yaml")).unwrap();

    let intr = introspect_procedure(&procedure_dir, &Some(python), &def)
        .await
        .expect("introspection failed");

    // check_voltage_with_plug(measurements, power_supply) → one plug of two.
    let check_voltage = def
        .main
        .iter()
        .find(|p| p.key == "check_voltage_plug")
        .unwrap();
    assert_eq!(
        def.plug_keys_for_phase(check_voltage, &intr),
        Some(vec!["power_supply".to_string()])
    );

    // all_setup(phase) → signature read, zero plugs.
    let setup = def.setup.iter().find(|p| p.key == "new_phase").unwrap();
    assert_eq!(def.plug_keys_for_phase(setup, &intr), Some(vec![]));

    // phase_2 has no `python:` key at all → provably zero plugs. This is
    // the assertion that catches a regression to `None`, which would make
    // play-on-phase_2 start every plug instead of neither.
    let phase_2 = def.main.iter().find(|p| p.key == "phase_2").unwrap();
    assert_eq!(def.plug_keys_for_phase(phase_2, &intr), Some(vec![]));
}
