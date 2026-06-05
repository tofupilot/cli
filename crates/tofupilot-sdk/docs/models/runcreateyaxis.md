# RunCreateYAxis

Data series with numeric data, unit, and optional validators/aggregations.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `data` | `Vec<f64>` | :heavy_check_mark: | Array of numeric data points for this axis. |
| `units` | `NullableField<String>` | :heavy_minus_sign: | Unit for this axis. |
| `name` | `NullableField<String>` | :heavy_minus_sign: | Name of this data series. |
| `description` | `NullableField<String>` | :heavy_minus_sign: | Description of this data series. |
| `validators` | `NullableField<Vec<RunCreateYAxisValidators>>` | :heavy_minus_sign: | Validators for this specific axis/series. |
| `aggregations` | `NullableField<Vec<RunCreateYAxisAggregations>>` | :heavy_minus_sign: | Aggregations computed over this axis data (min, max, avg, etc.). |
