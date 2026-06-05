mod common;
use common::*;
use tofupilot::types::*;

#[tokio::test]
async fn create_run_returns_id() {
    let result = create_test_run(&uid()).await;
    assert!(!result.id.is_empty());
}

#[tokio::test]
async fn create_run_with_procedure_version() {
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
        .procedure_version("1.2.3")
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    assert!(fetched.procedure.version.is_some());
    assert_eq!("1.2.3", fetched.procedure.version.unwrap().tag);
}

#[tokio::test]
async fn create_run_with_docstring() {
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
        .docstring("Test docstring for verification")
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    match &fetched.docstring {
        NullableField::Value(v) => assert_eq!("Test docstring for verification", v),
        other => panic!("expected Value, got {:?}", other),
    }
}

#[tokio::test]
async fn create_run_with_phases() {
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
        .phases(vec![RunCreatePhases::builder()
            .name("voltage_check")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("output_voltage")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!(3.3))
                .build()
                .unwrap()
            ])
            .build()
            .unwrap()
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
    assert!(!phases.is_empty());
    assert_eq!("voltage_check", phases[0].name);
    assert!(!phases[0].measurements.is_empty());
    assert_eq!("output_voltage", phases[0].measurements[0].name);
}

#[tokio::test]
async fn create_run_with_logs() {
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
                message: "Test started successfully".to_string(),
                source_file: "test_runner.rs".to_string(),
                line_number: 42,
            },
            RunCreateLogs {
                level: Level::Warning,
                timestamp: now - chrono::Duration::minutes(2),
                message: "Voltage slightly above nominal".to_string(),
                source_file: "test_runner.rs".to_string(),
                line_number: 88,
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
    assert!(logs.iter().any(|l| l.message == "Test started successfully"));
    assert!(logs.iter().any(|l| l.message == "Voltage slightly above nominal"));
}

#[tokio::test]
async fn create_run_empty_serial_number_fails() {
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let result = client().runs().create()
        .serial_number("")
        .procedure_id(proc_id)
        .part_number(format!("PART-{}", uid()))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await;

    assert!(matches!(result, Err(tofupilot::Error::BadRequest(_))));
}

#[tokio::test]
async fn create_run_invalid_procedure_id_fails() {
    let now = chrono::Utc::now();

    let result = client().runs().create()
        .serial_number(format!("SN-{}", uid()))
        .procedure_id(uuid::Uuid::new_v4().to_string())
        .part_number(format!("PART-{}", uid()))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await;

    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
#[allow(deprecated)]
async fn create_run_with_legacy_limits() {
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
        .phases(vec![RunCreatePhases::builder()
            .name("limit_phase")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("temperature")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!(25.0))
                .lower_limit(10.0)
                .upper_limit(40.0)
                .build()
                .unwrap()
            ])
            .build()
            .unwrap()
        ])
        .send()
        .await
        .unwrap();

    assert!(!created.id.is_empty());

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    let phases = fetched.phases.expect("phases should be present");
    assert_eq!(1, phases.len());
    assert!(!phases[0].measurements.is_empty());
}

#[tokio::test]
async fn create_run_with_aggregations() {
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
        .phases(vec![RunCreatePhases::builder()
            .name("agg_phase")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("signal_strength")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!(75.5))
                .aggregations(vec![RunCreateMeasurementsAggregations::builder()
                    .r#type("avg")
                    .value(serde_json::json!(72.3))
                    .build()
                    .unwrap()
                ])
                .build()
                .unwrap()
            ])
            .build()
            .unwrap()
        ])
        .send()
        .await
        .unwrap();

    assert!(!created.id.is_empty());
}

#[tokio::test]
async fn create_run_with_sub_units() {
    let uid_val = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let sub1 = format!("SUB1-{uid_val}");
    let sub2 = format!("SUB2-{uid_val}");

    for sub in [&sub1, &sub2] {
        client().runs().create()
            .serial_number(sub)
            .procedure_id(proc_id)
            .part_number(format!("SUB-PART-{uid_val}"))
            .started_at(now - chrono::Duration::minutes(10))
            .ended_at(now - chrono::Duration::minutes(8))
            .outcome(Outcome::Pass)
            .send()
            .await
            .unwrap();
    }

    let created = client().runs().create()
        .serial_number(format!("SN-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .sub_units(vec![sub1.clone(), sub2.clone()])
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    let sub_units = fetched.sub_units.expect("sub_units should be present");
    assert_eq!(2, sub_units.len());
    let sub_serials: Vec<String> = sub_units.iter()
        .map(|s| s.serial_number.to_lowercase())
        .collect();
    assert!(sub_serials.contains(&sub1.to_lowercase()));
    assert!(sub_serials.contains(&sub2.to_lowercase()));
}

#[tokio::test]
async fn create_run_with_retry_count() {
    let uid_val = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(10))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![
            RunCreatePhases::builder()
                .name("retried_phase")
                .outcome(PhasesOutcome::Fail)
                .started_at(now - chrono::Duration::minutes(10))
                .ended_at(now - chrono::Duration::minutes(8))
                .retry_count(0)
                .measurements(vec![RunCreateMeasurements::builder()
                    .name("voltage")
                    .outcome(ValidatorsOutcome::Fail)
                    .measured_value(serde_json::json!(2.0))
                    .build().unwrap()
                ])
                .build().unwrap(),
            RunCreatePhases::builder()
                .name("retried_phase")
                .outcome(PhasesOutcome::Pass)
                .started_at(now - chrono::Duration::minutes(7))
                .ended_at(now - chrono::Duration::minutes(5))
                .retry_count(1)
                .measurements(vec![RunCreateMeasurements::builder()
                    .name("voltage")
                    .outcome(ValidatorsOutcome::Pass)
                    .measured_value(serde_json::json!(3.3))
                    .build().unwrap()
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

    let phases = fetched.phases.unwrap();
    assert_eq!(2, phases.len());
    assert_eq!(0, phases[0].retry_count);
    assert_eq!(1, phases[1].retry_count);
}

#[tokio::test]
async fn create_run_retry_count_defaults_to_zero() {
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
        .phases(vec![RunCreatePhases::builder()
            .name("no_retry")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("test_val")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!(1.0))
                .build().unwrap()
            ])
            .build().unwrap()
        ])
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    assert_eq!(0, fetched.phases.unwrap()[0].retry_count);
}

#[tokio::test]
#[allow(deprecated)]
async fn create_run_legacy_limits_both_bounds() {
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
        .phases(vec![RunCreatePhases::builder()
            .name("both_limits")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("temp")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!(25.0))
                .lower_limit(10.0)
                .upper_limit(40.0)
                .build().unwrap()
            ])
            .build().unwrap()
        ])
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    let phases = fetched.phases.unwrap();
    let validators = phases[0].measurements[0].validators.as_ref().unwrap();
    assert_eq!(2, validators.len());
}

#[tokio::test]
#[allow(deprecated)]
async fn create_run_legacy_limits_only_lower() {
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
        .phases(vec![RunCreatePhases::builder()
            .name("lower_only")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("current")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!(0.5))
                .lower_limit(0.0)
                .build().unwrap()
            ])
            .build().unwrap()
        ])
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    let phases = fetched.phases.unwrap();
    let validators = phases[0].measurements[0].validators.as_ref().unwrap();
    assert_eq!(1, validators.len());
}

#[tokio::test]
#[allow(deprecated)]
async fn create_run_legacy_limits_only_upper() {
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
        .phases(vec![RunCreatePhases::builder()
            .name("upper_only")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("power")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!(3.0))
                .upper_limit(5.0)
                .build().unwrap()
            ])
            .build().unwrap()
        ])
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    let phases = fetched.phases.unwrap();
    let validators = phases[0].measurements[0].validators.as_ref().unwrap();
    assert_eq!(1, validators.len());
}

#[tokio::test]
#[allow(deprecated)]
async fn create_run_legacy_limits_failure() {
    let uid_val = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Fail)
        .phases(vec![RunCreatePhases::builder()
            .name("fail_limit")
            .outcome(PhasesOutcome::Fail)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("voltage")
                .outcome(ValidatorsOutcome::Fail)
                .measured_value(serde_json::json!(50.0))
                .lower_limit(0.0)
                .upper_limit(10.0)
                .build().unwrap()
            ])
            .build().unwrap()
        ])
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    assert!(fetched.phases.unwrap()[0].measurements[0].validators.is_some());
}

#[tokio::test]
#[allow(deprecated)]
async fn create_run_legacy_limits_negative_values() {
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
        .phases(vec![RunCreatePhases::builder()
            .name("neg_limits")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("offset")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!(-5.0))
                .lower_limit(-10.0)
                .upper_limit(0.0)
                .build().unwrap()
            ])
            .build().unwrap()
        ])
        .send()
        .await
        .unwrap();

    assert!(!created.id.is_empty());
}

#[tokio::test]
async fn create_run_sub_units_without_sub_units() {
    let uid_val = uid();
    let created = create_test_run(&uid_val).await;

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    let sub_units = fetched.sub_units.unwrap_or_default();
    assert!(sub_units.is_empty());
}

#[tokio::test]
async fn create_run_sub_units_single() {
    let uid_val = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let sub_serial = format!("SUB-{uid_val}");
    client().runs().create()
        .serial_number(&sub_serial)
        .procedure_id(proc_id)
        .part_number(format!("SUB-PART-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(10))
        .ended_at(now - chrono::Duration::minutes(8))
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    let created = client().runs().create()
        .serial_number(format!("SN-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .sub_units(vec![sub_serial.clone()])
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    let sub_units = fetched.sub_units.expect("sub_units should be present");
    assert_eq!(1, sub_units.len());
    assert_eq!(sub_serial.to_lowercase(), sub_units[0].serial_number.to_lowercase());
}

#[tokio::test]
async fn create_run_sub_units_empty_list() {
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
        .sub_units(Vec::<String>::new())
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    let sub_units = fetched.sub_units.unwrap_or_default();
    assert!(sub_units.is_empty());
}

#[tokio::test]
async fn create_run_multiple_phases_measurements() {
    let uid_val = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(10))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![
            RunCreatePhases::builder()
                .name("phase_a")
                .outcome(PhasesOutcome::Pass)
                .started_at(now - chrono::Duration::minutes(10))
                .ended_at(now - chrono::Duration::minutes(7))
                .measurements(vec![
                    RunCreateMeasurements::builder()
                        .name("v1").outcome(ValidatorsOutcome::Pass)
                        .measured_value(serde_json::json!(3.3)).build().unwrap(),
                    RunCreateMeasurements::builder()
                        .name("v2").outcome(ValidatorsOutcome::Pass)
                        .measured_value(serde_json::json!(5.0)).build().unwrap(),
                ])
                .build().unwrap(),
            RunCreatePhases::builder()
                .name("phase_b")
                .outcome(PhasesOutcome::Pass)
                .started_at(now - chrono::Duration::minutes(6))
                .ended_at(now - chrono::Duration::minutes(3))
                .measurements(vec![
                    RunCreateMeasurements::builder()
                        .name("temp").outcome(ValidatorsOutcome::Pass)
                        .measured_value(serde_json::json!(25.0)).build().unwrap(),
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

    let phases = fetched.phases.unwrap();
    assert_eq!(2, phases.len());
    assert_eq!(2, phases[0].measurements.len());
    assert_eq!(1, phases[1].measurements.len());
}
