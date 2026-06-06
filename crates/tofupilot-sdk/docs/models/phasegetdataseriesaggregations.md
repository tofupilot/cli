# PhaseGetDataSeriesAggregations

Aggregation result with computed value and optional validators.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the aggregation. |
| `r#type` | `String` | :heavy_check_mark: | Aggregation type (e.g., MIN, MAX, MEAN, RANGE, STD_DEV). |
| `outcome` | `Option<Outcome>` | :heavy_minus_sign: | Aggregation validation result: PASS, FAIL, UNSET, or null if no validators. |
| `value` | `Option<serde_json::Value>` | :heavy_minus_sign: | Computed aggregation value. Type depends on aggregation type. |
| `unit` | `NullableField<String>` | :heavy_minus_sign: | Unit of measurement for the aggregated value. |
| `validators` | `NullableField<Vec<PhaseGetDataSeriesAggregationsValidators>>` | :heavy_minus_sign: | Validators applied to the aggregated value. |
