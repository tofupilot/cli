# MeasurementGetResponse

Measurement retrieved successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | N/A |
| `name` | `String` | :heavy_check_mark: | N/A |
| `outcome` | `Outcome` | :heavy_check_mark: | N/A |
| `units` | `NullableField<String>` | :heavy_minus_sign: | N/A |
| `validators` | `Option<Vec<MeasurementGetValidators>>` | :heavy_minus_sign: | N/A |
| `aggregations` | `NullableField<Vec<MeasurementGetAggregations>>` | :heavy_minus_sign: | N/A |
| `measured_value` | `Option<serde_json::Value>` | :heavy_minus_sign: | N/A |
| `data_series` | `Option<Vec<MeasurementGetDataSeries>>` | :heavy_minus_sign: | N/A |
| `docstring` | `NullableField<String>` | :heavy_minus_sign: | N/A |
| `phase` | `MeasurementGetPhase` | :heavy_check_mark: | N/A |
| `run` | `MeasurementGetRun` | :heavy_check_mark: | N/A |
| `procedure` | `MeasurementGetProcedure` | :heavy_check_mark: | N/A |
