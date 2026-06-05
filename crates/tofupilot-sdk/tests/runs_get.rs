mod common;
use common::*;
use tofupilot::types::*;

#[tokio::test]
async fn get_run_returns_matching_id() {
    let created = create_test_run(&uid()).await;
    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();
    assert_eq!(created.id, fetched.id);
}

#[tokio::test]
async fn get_run_nonexistent_returns_not_found() {
    let result = client().runs().get()
        .id(uuid::Uuid::new_v4().to_string())
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn get_run_includes_phases_and_measurements() {
    let uid_val = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![
            RunCreatePhases::builder()
                .name("init_phase")
                .outcome(PhasesOutcome::Pass)
                .started_at(now - chrono::Duration::minutes(5))
                .ended_at(now - chrono::Duration::minutes(3))
                .measurements(vec![
                    RunCreateMeasurements::builder()
                        .name("boot_time_ms")
                        .outcome(ValidatorsOutcome::Pass)
                        .measured_value(serde_json::json!(120.5))
                        .build().unwrap(),
                    RunCreateMeasurements::builder()
                        .name("memory_mb")
                        .outcome(ValidatorsOutcome::Pass)
                        .measured_value(serde_json::json!(256))
                        .build().unwrap(),
                ])
                .build().unwrap(),
            RunCreatePhases::builder()
                .name("stress_phase")
                .outcome(PhasesOutcome::Pass)
                .started_at(now - chrono::Duration::minutes(3))
                .ended_at(now)
                .measurements(vec![
                    RunCreateMeasurements::builder()
                        .name("cpu_temp")
                        .outcome(ValidatorsOutcome::Pass)
                        .measured_value(serde_json::json!(65.2))
                        .build().unwrap(),
                ])
                .build().unwrap(),
        ])
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    let phases = fetched.phases.expect("phases should be present");
    assert_eq!(2, phases.len());

    let init = phases.iter().find(|p| p.name == "init_phase").expect("init_phase");
    assert_eq!(2, init.measurements.len());

    let stress = phases.iter().find(|p| p.name == "stress_phase").expect("stress_phase");
    assert_eq!(1, stress.measurements.len());
    assert_eq!("cpu_temp", stress.measurements[0].name);
}

#[tokio::test]
async fn get_run_includes_logs() {
    let uid_val = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .logs(vec![
            RunCreateLogs {
                level: Level::Info,
                timestamp: now - chrono::Duration::minutes(4),
                message: "Initializing device".to_string(),
                source_file: "device.rs".to_string(),
                line_number: 10,
            },
            RunCreateLogs {
                level: Level::Error,
                timestamp: now - chrono::Duration::minutes(1),
                message: "Recovered from transient fault".to_string(),
                source_file: "fault_handler.rs".to_string(),
                line_number: 55,
            },
        ])
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    let logs = fetched.logs.expect("logs should be present");
    assert_eq!(2, logs.len());
    assert!(logs.iter().any(|l| l.message == "Initializing device" && l.source_file == "device.rs"));
    assert!(logs.iter().any(|l| l.message == "Recovered from transient fault" && l.line_number == 55));
}
