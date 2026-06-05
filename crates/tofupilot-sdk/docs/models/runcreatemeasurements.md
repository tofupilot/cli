# RunCreateMeasurements


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `String` | :heavy_check_mark: | Name identifier for the measurement. Each measurement should have a descriptive name that identifies the specific data point being captured. Analytics at measurement level are computed using this name as unique identifier. |
| `outcome` | `Outcome` | :heavy_check_mark: | Result of the measurement validation. Use PASS when measurement meets all criteria, FAIL when measurement is outside acceptable limits or validation fails, UNSET when no validation was performed. |
| `x_axis` | `NullableField<RunCreateXAxis>` | :heavy_minus_sign: | Data series with numeric data, unit, and optional validators/aggregations. |
| `y_axis` | `NullableField<Vec<RunCreateYAxis>>` | :heavy_minus_sign: | Y-axis data series (one or more) for multi-dimensional measurements. Each series can have its own validators and aggregations. |
| `measured_value` | `NullableField<serde_json::Value>` | :heavy_minus_sign: | The actual value captured. [LEGACY for multi-dim] For multi-dimensional with per-axis validators/aggregations, use x_axis/y_axis instead. |
| `units` | `NullableField<serde_json::Value>` | :heavy_minus_sign: | [LEGACY for multi-dim] Units of measurement. For structured multi-dimensional, use units within x_axis/y_axis instead. |
| `lower_limit` | `Option<f64>` | :heavy_minus_sign: | Use validators with operator ">=" instead. Will be converted to a validator automatically. |
| `upper_limit` | `Option<f64>` | :heavy_minus_sign: | Use validators with operator "<=" instead. Will be converted to a validator automatically. |
| `validators` | `NullableField<Vec<RunCreateMeasurementsValidators>>` | :heavy_minus_sign: | Validators for this measurement. Use structured ValidatorSpec objects with operator and expected_value. |
| `aggregations` | `NullableField<Vec<RunCreateMeasurementsAggregations>>` | :heavy_minus_sign: | Aggregations computed over measurement values (min, max, avg, etc.). Each aggregation can have its own validators. |
| `docstring` | `NullableField<String>` | :heavy_minus_sign: | Additional notes or documentation about this measurement. |
