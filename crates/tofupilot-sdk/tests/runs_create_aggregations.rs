mod common;
use common::*;
use tofupilot::types::*;

async fn create_with_aggregation(
    uid_val: &str,
    meas_name: &str,
    measured: f64,
    aggregations: Vec<RunCreateMeasurementsAggregations>,
) -> String {
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-A-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-A-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("agg_phase")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name(meas_name)
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!(measured))
                .aggregations(aggregations)
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
async fn aggregation_type_avg() {
    let u = uid();
    let id = create_with_aggregation(&u, "test_avg", 50.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("avg").value(serde_json::json!(42.0)).outcome("PASS")
            .build().unwrap()
    ]).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs { assert_eq!(1, a.len()); assert!(a[0].r#type.eq_ignore_ascii_case("avg")); }
    else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn aggregation_type_min() {
    let u = uid();
    let id = create_with_aggregation(&u, "test_min", 50.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("min").value(serde_json::json!(42.0)).outcome("PASS")
            .build().unwrap()
    ]).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs { assert!(a[0].r#type.eq_ignore_ascii_case("min")); }
    else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn aggregation_type_max() {
    let u = uid();
    let id = create_with_aggregation(&u, "test_max", 50.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("max").value(serde_json::json!(42.0)).outcome("PASS")
            .build().unwrap()
    ]).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs { assert!(a[0].r#type.eq_ignore_ascii_case("max")); }
    else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn aggregation_type_sum() {
    let u = uid();
    let id = create_with_aggregation(&u, "test_sum", 50.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("sum").value(serde_json::json!(42.0)).outcome("PASS")
            .build().unwrap()
    ]).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs { assert!(a[0].r#type.eq_ignore_ascii_case("sum")); }
    else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn aggregation_type_count() {
    let u = uid();
    let id = create_with_aggregation(&u, "test_count", 50.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("count").value(serde_json::json!(42.0)).outcome("PASS")
            .build().unwrap()
    ]).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs { assert!(a[0].r#type.eq_ignore_ascii_case("count")); }
    else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn aggregation_type_std() {
    let u = uid();
    let id = create_with_aggregation(&u, "test_std", 50.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("std").value(serde_json::json!(42.0)).outcome("PASS")
            .build().unwrap()
    ]).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs { assert!(a[0].r#type.eq_ignore_ascii_case("std")); }
    else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn aggregation_type_median() {
    let u = uid();
    let id = create_with_aggregation(&u, "test_median", 50.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("median").value(serde_json::json!(42.0)).outcome("PASS")
            .build().unwrap()
    ]).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs { assert!(a[0].r#type.eq_ignore_ascii_case("median")); }
    else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn multiple_aggregations_on_single_measurement() {
    let u = uid();
    let id = create_with_aggregation(&u, "multi_agg", 75.5, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("avg").value(serde_json::json!(72.3)).outcome("PASS")
            .build().unwrap(),
        RunCreateMeasurementsAggregations::builder()
            .r#type("max").value(serde_json::json!(80.1))
            .build().unwrap(),
        RunCreateMeasurementsAggregations::builder()
            .r#type("min").value(serde_json::json!(65.0))
            .build().unwrap(),
    ]).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs { assert_eq!(3, a.len()); }
    else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn aggregation_with_string_value() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-A-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-A-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("str_agg_phase")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("status_mode")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!("nominal"))
                .aggregations(vec![
                    RunCreateMeasurementsAggregations::builder()
                        .r#type("mode").value(serde_json::json!("nominal"))
                        .build().unwrap()
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
async fn aggregation_with_boolean_value() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-A-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-A-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("bool_agg_phase")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("all_pass")
                .outcome(ValidatorsOutcome::Pass)
                .measured_value(serde_json::json!(true))
                .aggregations(vec![
                    RunCreateMeasurementsAggregations::builder()
                        .r#type("all").value(serde_json::json!(true))
                        .build().unwrap()
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
async fn aggregation_with_validators() {
    let u = uid();
    let id = create_with_aggregation(&u, "agg_with_val", 75.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("avg")
            .value(serde_json::json!(72.3))
            .outcome("PASS")
            .validators(vec![
                RunCreateMeasurementsAggregationsValidators::builder()
                    .operator(">=").expected_value(serde_json::json!(60.0)).outcome("PASS")
                    .build().unwrap(),
                RunCreateMeasurementsAggregationsValidators::builder()
                    .operator("<=").expected_value(serde_json::json!(90.0)).outcome("PASS")
                    .build().unwrap(),
            ])
            .build().unwrap()
    ]).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs {
        if let NullableField::Value(v) = &a[0].validators { assert_eq!(2, v.len()); }
        else { panic!("expected validators on aggregation"); }
    } else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn aggregation_outcome_pass() {
    let u = uid();
    let id = create_with_aggregation(&u, "agg_pass", 50.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("avg").value(serde_json::json!(50.0)).outcome("PASS")
            .build().unwrap()
    ]).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs { assert_eq!(Some("PASS".to_string()), a[0].outcome); }
    else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn aggregation_outcome_fail() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-A-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-A-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Fail)
        .phases(vec![RunCreatePhases::builder()
            .name("fail_agg")
            .outcome(PhasesOutcome::Fail)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("agg_fail")
                .outcome(ValidatorsOutcome::Fail)
                .measured_value(serde_json::json!(50.0))
                .aggregations(vec![
                    RunCreateMeasurementsAggregations::builder()
                        .r#type("avg").value(serde_json::json!(50.0)).outcome("FAIL")
                        .build().unwrap()
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
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs { assert_eq!(Some("FAIL".to_string()), a[0].outcome); }
    else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn aggregation_with_special_char_type() {
    let u = uid();
    let id = create_with_aggregation(&u, "special_type", 90.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("percentile_95").value(serde_json::json!(95.0))
            .build().unwrap()
    ]).await;
    let fetched = client().runs().get().id(&id).send().await.unwrap();
    let aggs = fetched.phases.unwrap()[0].measurements[0].aggregations.clone();
    if let NullableField::Value(a) = aggs { assert!(a[0].r#type.eq_ignore_ascii_case("percentile_95")); }
    else { panic!("expected aggregations"); }
}

#[tokio::test]
async fn aggregation_with_negative_value() {
    let u = uid();
    let id = create_with_aggregation(&u, "neg_agg", -10.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("min").value(serde_json::json!(-15.5))
            .build().unwrap()
    ]).await;
    assert!(!id.is_empty());
}

#[tokio::test]
async fn aggregation_validator_with_is_decisive() {
    let u = uid();
    let id = create_with_aggregation(&u, "agg_decisive", 72.0, vec![
        RunCreateMeasurementsAggregations::builder()
            .r#type("avg")
            .value(serde_json::json!(72.0))
            .outcome("FAIL")
            .validators(vec![
                RunCreateMeasurementsAggregationsValidators::builder()
                    .operator(">=")
                    .expected_value(serde_json::json!(80.0))
                    .outcome("FAIL")
                    .is_decisive(false)
                    .build().unwrap(),
            ])
            .build().unwrap()
    ]).await;
    assert!(!id.is_empty());
}
