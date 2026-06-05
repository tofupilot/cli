# RunCreateMeasurementsAggregations

Aggregation specification with computed value and optional validators.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `r#type` | `String` | :heavy_check_mark: | Aggregation function: "min", "max", "avg", "sum", "count", "std", "median", "percentile_95", etc. |
| `outcome` | `NullableField<Outcome>` | :heavy_minus_sign: | Computed result of aggregation validation. Server stores as-is. |
| `value` | `NullableField<serde_json::Value>` | :heavy_minus_sign: | Computed aggregation value. |
| `unit` | `NullableField<String>` | :heavy_minus_sign: | Unit for the aggregated value. |
| `validators` | `NullableField<Vec<RunCreateMeasurementsAggregationsValidators>>` | :heavy_minus_sign: | Validators applied to the aggregated value. |
