# RunGetDataSeries


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `data` | `Vec<f64>` | :heavy_check_mark: | Array of numeric data points for this series. |
| `units` | `Option<String>` | :heavy_minus_sign: | Unit for this data series. |
| `name` | `NullableField<String>` | :heavy_minus_sign: | Name of this data series. |
| `description` | `NullableField<String>` | :heavy_minus_sign: | Description of this data series. |
| `validators` | `NullableField<Vec<RunGetDataSeriesValidators>>` | :heavy_minus_sign: | Validators for this data series. |
| `aggregations` | `NullableField<Vec<RunGetDataSeriesAggregations>>` | :heavy_minus_sign: | Aggregations computed over this data series. |
