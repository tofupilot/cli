mod common;
use common::*;
use tofupilot::types::*;

fn make_validator(op: &str, expected: serde_json::Value, outcome: &str) -> RunCreateMeasurementsValidators {
    RunCreateMeasurementsValidators::builder()
        .operator(op)
        .expected_value(expected)
        .outcome(outcome)
        .build()
        .unwrap()
}

async fn create_with_validators(
    uid_val: &str,
    meas_name: &str,
    measured: serde_json::Value,
    meas_outcome: ValidatorsOutcome,
    validators: Vec<RunCreateMeasurementsValidators>,
) -> String {
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-V-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-V-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("validation_phase")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name(meas_name)
                .outcome(meas_outcome)
                .measured_value(measured)
                .validators(validators)
                .build()
                .unwrap()
            ])
            .build()
            .unwrap()
        ])
        .send()
        .await
        .unwrap();

    created.id
}

#[tokio::test]
async fn validator_operator_gte() {
    let u = uid();
    let id = create_with_validators(
        &u, "test_gte", serde_json::json!(10.0), ValidatorsOutcome::Pass,
        vec![make_validator(">=", serde_json::json!(5.0), "PASS")],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = phases[0].measurements[0].validators.as_ref().unwrap();
    assert_eq!(1, v.len());
    assert_eq!(">=", v[0].operator.as_deref().unwrap());
}

#[tokio::test]
async fn validator_operator_lte() {
    let u = uid();
    let id = create_with_validators(
        &u, "test_lte", serde_json::json!(10.0), ValidatorsOutcome::Pass,
        vec![make_validator("<=", serde_json::json!(15.0), "PASS")],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = phases[0].measurements[0].validators.as_ref().unwrap();
    assert_eq!("<=", v[0].operator.as_deref().unwrap());
}

#[tokio::test]
async fn validator_operator_gt() {
    let u = uid();
    let id = create_with_validators(
        &u, "test_gt", serde_json::json!(10.0), ValidatorsOutcome::Pass,
        vec![make_validator(">", serde_json::json!(5.0), "PASS")],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = phases[0].measurements[0].validators.as_ref().unwrap();
    assert_eq!(">", v[0].operator.as_deref().unwrap());
}

#[tokio::test]
async fn validator_operator_lt() {
    let u = uid();
    let id = create_with_validators(
        &u, "test_lt", serde_json::json!(10.0), ValidatorsOutcome::Pass,
        vec![make_validator("<", serde_json::json!(15.0), "PASS")],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = phases[0].measurements[0].validators.as_ref().unwrap();
    assert_eq!("<", v[0].operator.as_deref().unwrap());
}

#[tokio::test]
async fn validator_operator_eq() {
    let u = uid();
    let id = create_with_validators(
        &u, "test_eq", serde_json::json!(10.0), ValidatorsOutcome::Pass,
        vec![make_validator("==", serde_json::json!(10.0), "PASS")],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = phases[0].measurements[0].validators.as_ref().unwrap();
    assert_eq!("==", v[0].operator.as_deref().unwrap());
}

#[tokio::test]
async fn validator_operator_ne() {
    let u = uid();
    let id = create_with_validators(
        &u, "test_ne", serde_json::json!(10.0), ValidatorsOutcome::Pass,
        vec![make_validator("!=", serde_json::json!(5.0), "PASS")],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = phases[0].measurements[0].validators.as_ref().unwrap();
    assert_eq!("!=", v[0].operator.as_deref().unwrap());
}

#[tokio::test]
async fn validator_with_string_expected_value() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-V-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-V-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("string_check")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("status")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!("PASS"))
                .validators(vec![
                    RunCreateMeasurementsValidators::builder()
                        .operator("==")
                        .expected_value(serde_json::json!("PASS"))
                        .outcome("PASS")
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

    let fetched = client().runs().get().id(&created.id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    assert!(phases[0].measurements[0].validators.is_some());
}

#[tokio::test]
async fn validator_with_boolean_expected_value() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-V-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-V-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("bool_check")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("is_calibrated")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!(true))
                .validators(vec![
                    RunCreateMeasurementsValidators::builder()
                        .operator("==")
                        .expected_value(serde_json::json!(true))
                        .outcome("PASS")
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
async fn multiple_validators_range_check() {
    let u = uid();
    let id = create_with_validators(
        &u, "range_value", serde_json::json!(50.0), ValidatorsOutcome::Pass,
        vec![
            make_validator(">=", serde_json::json!(0), "PASS"),
            make_validator("<=", serde_json::json!(100), "PASS"),
        ],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = phases[0].measurements[0].validators.as_ref().unwrap();
    assert_eq!(2, v.len());
}

#[tokio::test]
async fn validator_with_is_decisive_false() {
    let u = uid();
    let id = create_with_validators(
        &u, "marginal_check", serde_json::json!(85.0), ValidatorsOutcome::Pass,
        vec![
            RunCreateMeasurementsValidators::builder()
                .operator(">=")
                .expected_value(serde_json::json!(90))
                .outcome("FAIL")
                .is_decisive(false)
                .build()
                .unwrap()
        ],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = &phases[0].measurements[0].validators.as_ref().unwrap()[0];
    assert_eq!(Some(false), v.is_decisive);
}

#[tokio::test]
async fn validator_with_is_decisive_true() {
    let u = uid();
    let id = create_with_validators(
        &u, "decisive_check", serde_json::json!(50.0), ValidatorsOutcome::Pass,
        vec![
            RunCreateMeasurementsValidators::builder()
                .operator(">=")
                .expected_value(serde_json::json!(0))
                .outcome("PASS")
                .is_decisive(true)
                .build()
                .unwrap()
        ],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = &phases[0].measurements[0].validators.as_ref().unwrap()[0];
    assert_eq!(Some(true), v.is_decisive);
}

#[tokio::test]
async fn expression_only_validator() {
    let u = uid();
    let id = create_with_validators(
        &u, "expr_check", serde_json::json!(50.0), ValidatorsOutcome::Pass,
        vec![
            RunCreateMeasurementsValidators::builder()
                .expression("value > threshold && value < max_threshold")
                .outcome("PASS")
                .build()
                .unwrap()
        ],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = &phases[0].measurements[0].validators.as_ref().unwrap()[0];
    assert!(v.is_expression_only);
    assert!(v.expression.contains("threshold"));
}

#[tokio::test]
async fn validator_with_custom_expression() {
    let u = uid();
    let id = create_with_validators(
        &u, "custom_expr", serde_json::json!(3.3), ValidatorsOutcome::Pass,
        vec![
            RunCreateMeasurementsValidators::builder()
                .operator(">=")
                .expected_value(serde_json::json!(0))
                .expression("voltage within safe range")
                .outcome("PASS")
                .build()
                .unwrap()
        ],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = &phases[0].measurements[0].validators.as_ref().unwrap()[0];
    assert!(v.has_custom_expression);
    assert_eq!("voltage within safe range", v.expression);
}

#[tokio::test]
async fn validator_fail_outcome() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-V-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-V-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Fail)
        .phases(vec![RunCreatePhases::builder()
            .name("fail_phase")
            .outcome(PhasesOutcome::Fail)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("over_limit")
                .outcome(ValidatorsOutcome::Fail)
                .measured_value(serde_json::json!(10.0))
                .validators(vec![
                    RunCreateMeasurementsValidators::builder()
                        .operator("<=")
                        .expected_value(serde_json::json!(5))
                        .outcome("FAIL")
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

    let fetched = client().runs().get().id(&created.id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = &phases[0].measurements[0].validators.as_ref().unwrap()[0];
    assert_eq!(ValidatorsOutcome::Fail, v.outcome);
}

#[tokio::test]
async fn validator_in_operator_with_string_list() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-V-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-V-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("in_check")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("grade")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!("A"))
                .validators(vec![
                    RunCreateMeasurementsValidators::builder()
                        .operator("in")
                        .expected_value(serde_json::json!(["A", "B", "C"]))
                        .outcome("PASS")
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

    let fetched = client().runs().get().id(&created.id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    let v = &phases[0].measurements[0].validators.as_ref().unwrap()[0];
    assert_eq!("in", v.operator.as_deref().unwrap());
}

#[tokio::test]
async fn validator_range_operator() {
    let u = uid();
    let id = create_with_validators(
        &u, "range_check", serde_json::json!(25.0), ValidatorsOutcome::Pass,
        vec![
            RunCreateMeasurementsValidators::builder()
                .operator("range")
                .expected_value(serde_json::json!([10.0, 50.0]))
                .outcome("PASS")
                .build()
                .unwrap()
        ],
    ).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    assert!(!phases[0].measurements[0].validators.as_ref().unwrap().is_empty());
}

#[tokio::test]
async fn multiple_measurements_with_validators() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-V-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-V-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("multi_meas")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![
                RunCreateMeasurements::builder()
                    .name("voltage")
                    .outcome(ValidatorsOutcome::Pass)
                    .measured_value(serde_json::json!(3.3))
                    .validators(vec![
                        make_validator(">=", serde_json::json!(3.0), "PASS"),
                        make_validator("<=", serde_json::json!(3.6), "PASS"),
                    ])
                    .build()
                    .unwrap(),
                RunCreateMeasurements::builder()
                    .name("current")
                    .outcome(ValidatorsOutcome::Pass)
                    .measured_value(serde_json::json!(0.5))
                    .validators(vec![
                        make_validator("<", serde_json::json!(1.0), "PASS"),
                    ])
                    .build()
                    .unwrap(),
            ])
            .build()
            .unwrap()
        ])
        .send()
        .await
        .unwrap();

    let fetched = client().runs().get().id(&created.id).send().await.unwrap();
    let phases = fetched.phases.unwrap();
    assert_eq!(2, phases[0].measurements.len());
    assert!(!phases[0].measurements[0].validators.as_ref().unwrap().is_empty());
    assert!(!phases[0].measurements[1].validators.as_ref().unwrap().is_empty());
}
