mod common;
use common::*;
use tofupilot::types::*;

#[tokio::test]
async fn basic_x_axis_y_axis() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-M-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-M-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("mdm_basic")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("frequency_response")
                .outcome(ValidatorsOutcome::Pass)
                .x_axis(RunCreateXAxis::builder()
                    .data(vec![100.0, 1000.0, 10000.0])
                    .units("Hz")
                    .build().unwrap())
                .y_axis(vec![RunCreateYAxis::builder()
                    .data(vec![-3.0, 0.0, -6.0])
                    .units("dB")
                    .description("Gain")
                    .build().unwrap()])
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
    let ds = phases[0].measurements[0].data_series.as_ref().unwrap();
    assert!(!ds.is_empty());
}

#[tokio::test]
async fn multiple_y_axis_series() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-M-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-M-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("multi_y")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("iv_curve")
                .outcome(ValidatorsOutcome::Pass)
                .x_axis(RunCreateXAxis::builder()
                    .data(vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0])
                    .units("V")
                    .description("Voltage")
                    .build().unwrap())
                .y_axis(vec![
                    RunCreateYAxis::builder()
                        .data(vec![0.0, 0.1, 0.2, 0.3, 0.4, 0.5])
                        .units("A")
                        .description("Current")
                        .build().unwrap(),
                    RunCreateYAxis::builder()
                        .data(vec![0.0, 0.1, 0.4, 0.9, 1.6, 2.5])
                        .units("W")
                        .description("Power")
                        .build().unwrap(),
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
    let ds = phases[0].measurements[0].data_series.as_ref().unwrap();
    assert!(ds.len() >= 2);
}

#[tokio::test]
async fn y_axis_with_validators() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-M-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-M-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("y_validators")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("output_signal")
                .outcome(ValidatorsOutcome::Pass)
                .x_axis(RunCreateXAxis::builder()
                    .data(vec![1.0, 2.0, 3.0])
                    .units("s")
                    .build().unwrap())
                .y_axis(vec![RunCreateYAxis::builder()
                    .data(vec![3.0, 3.3, 3.1])
                    .units("V")
                    .description("Voltage")
                    .validators(vec![
                        RunCreateYAxisValidators::builder()
                            .operator(">=").expected_value(serde_json::json!(2.5)).outcome("PASS")
                            .build().unwrap(),
                        RunCreateYAxisValidators::builder()
                            .operator("<=").expected_value(serde_json::json!(4.0)).outcome("PASS")
                            .build().unwrap(),
                    ])
                    .build().unwrap()])
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
    assert!(fetched.phases.unwrap()[0].measurements[0].data_series.is_some());
}

#[tokio::test]
async fn y_axis_with_aggregations() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-M-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-M-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("y_aggs")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("temperature_sweep")
                .outcome(ValidatorsOutcome::Pass)
                .x_axis(RunCreateXAxis::builder()
                    .data(vec![0.0, 10.0, 20.0, 30.0, 40.0])
                    .units("min")
                    .build().unwrap())
                .y_axis(vec![RunCreateYAxis::builder()
                    .data(vec![22.0, 23.5, 24.0, 23.8, 23.2])
                    .units("C")
                    .description("Temperature")
                    .aggregations(vec![
                        RunCreateYAxisAggregations::builder()
                            .r#type("avg").value(serde_json::json!(23.3)).outcome("PASS")
                            .build().unwrap(),
                        RunCreateYAxisAggregations::builder()
                            .r#type("max").value(serde_json::json!(24.0))
                            .build().unwrap(),
                    ])
                    .build().unwrap()])
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
    assert!(fetched.phases.unwrap()[0].measurements[0].data_series.is_some());
}

#[tokio::test]
async fn y_axis_aggregations_with_validators() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-M-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-M-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("y_agg_val")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("signal_quality")
                .outcome(ValidatorsOutcome::Pass)
                .x_axis(RunCreateXAxis::builder()
                    .data(vec![1.0, 2.0, 3.0])
                    .units("s")
                    .build().unwrap())
                .y_axis(vec![RunCreateYAxis::builder()
                    .data(vec![95.0, 96.0, 94.5])
                    .units("%")
                    .description("Quality")
                    .aggregations(vec![
                        RunCreateYAxisAggregations::builder()
                            .r#type("avg")
                            .value(serde_json::json!(95.17))
                            .outcome("PASS")
                            .validators(vec![
                                RunCreateYAxisAggregationsValidators::builder()
                                    .operator(">=").expected_value(serde_json::json!(90.0)).outcome("PASS")
                                    .build().unwrap(),
                            ])
                            .build().unwrap(),
                    ])
                    .build().unwrap()])
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
async fn x_axis_with_validators() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-M-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-M-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("x_validators")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("time_series")
                .outcome(ValidatorsOutcome::Pass)
                .x_axis(RunCreateXAxis::builder()
                    .data(vec![0.0, 1.0, 2.0, 3.0])
                    .units("s")
                    .validators(vec![
                        RunCreateValidators::builder()
                            .operator(">=").expected_value(serde_json::json!(0)).outcome("PASS")
                            .build().unwrap(),
                    ])
                    .build().unwrap())
                .y_axis(vec![RunCreateYAxis::builder()
                    .data(vec![10.0, 20.0, 30.0, 40.0])
                    .units("mV")
                    .build().unwrap()])
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
async fn x_axis_with_aggregations() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-M-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-M-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("x_aggs")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("sampling")
                .outcome(ValidatorsOutcome::Pass)
                .x_axis(RunCreateXAxis::builder()
                    .data(vec![0.0, 0.5, 1.0, 1.5, 2.0])
                    .units("s")
                    .aggregations(vec![
                        RunCreateAggregations::builder()
                            .r#type("max").value(serde_json::json!(2.0))
                            .build().unwrap(),
                    ])
                    .build().unwrap())
                .y_axis(vec![RunCreateYAxis::builder()
                    .data(vec![1.0, 2.0, 3.0, 4.0, 5.0])
                    .units("V")
                    .build().unwrap()])
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
async fn comprehensive_mdm() {
    let u = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    let created = client().runs().create()
        .serial_number(format!("SN-M-{u}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-M-{u}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .phases(vec![RunCreatePhases::builder()
            .name("comprehensive_mdm")
            .outcome(PhasesOutcome::Pass)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now - chrono::Duration::minutes(3))
            .measurements(vec![RunCreateMeasurements::builder()
                .name("full_sweep")
                .outcome(ValidatorsOutcome::Pass)
                .x_axis(RunCreateXAxis::builder()
                    .data(vec![100.0, 1000.0, 10000.0])
                    .units("Hz")
                    .description("Frequency")
                    .build().unwrap())
                .y_axis(vec![
                    RunCreateYAxis::builder()
                        .data(vec![-1.0, 0.0, -3.0])
                        .units("dB")
                        .description("Gain")
                        .validators(vec![
                            RunCreateYAxisValidators::builder()
                                .operator(">=").expected_value(serde_json::json!(-6.0)).outcome("PASS")
                                .build().unwrap(),
                        ])
                        .aggregations(vec![
                            RunCreateYAxisAggregations::builder()
                                .r#type("min")
                                .value(serde_json::json!(-3.0))
                                .outcome("PASS")
                                .validators(vec![
                                    RunCreateYAxisAggregationsValidators::builder()
                                        .operator(">=").expected_value(serde_json::json!(-6.0)).outcome("PASS")
                                        .build().unwrap(),
                                ])
                                .build().unwrap(),
                        ])
                        .build().unwrap(),
                    RunCreateYAxis::builder()
                        .data(vec![-5.0, -10.0, -45.0])
                        .units("deg")
                        .description("Phase")
                        .build().unwrap(),
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
    let ds = phases[0].measurements[0].data_series.as_ref().unwrap();
    assert!(ds.len() >= 2);
}
