# RunGetMeasurements


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Measurement ID. |
| `name` | `String` | :heavy_check_mark: | Measurement name. |
| `outcome` | `Outcome` | :heavy_check_mark: | Measurement validation result. |
| `units` | `NullableField<String>` | :heavy_minus_sign: | Units of measurement. Not present for multi-dimensional measurements (units are per data series). |
| `validators` | `Option<Vec<RunGetValidators>>` | :heavy_minus_sign: | Structured validation rules with outcome and expected values. |
| `aggregations` | `NullableField<Vec<RunGetAggregations>>` | :heavy_minus_sign: | Aggregations computed over this measurement. |
| `measured_value` | `Option<serde_json::Value>` | :heavy_minus_sign: | The actual measured value. |
| `data_series` | `Option<Vec<RunGetDataSeries>>` | :heavy_minus_sign: | Multi-dimensional measurement data series. |
