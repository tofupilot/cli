# PhaseGetMeasurements


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | N/A |
| `name` | `String` | :heavy_check_mark: | N/A |
| `outcome` | `Outcome` | :heavy_check_mark: | N/A |
| `units` | `NullableField<String>` | :heavy_minus_sign: | N/A |
| `validators` | `Option<Vec<PhaseGetValidators>>` | :heavy_minus_sign: | N/A |
| `aggregations` | `NullableField<Vec<PhaseGetAggregations>>` | :heavy_minus_sign: | N/A |
| `measured_value` | `Option<serde_json::Value>` | :heavy_minus_sign: | N/A |
| `data_series` | `Option<Vec<PhaseGetDataSeries>>` | :heavy_minus_sign: | N/A |
| `docstring` | `NullableField<String>` | :heavy_minus_sign: | N/A |
