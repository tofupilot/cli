# RunGetUnit

Unit under test information.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unit ID. |
| `serial_number` | `String` | :heavy_check_mark: | Unit serial number. |
| `sample` | `Option<Sample>` | :heavy_minus_sign: | Reference-sample classification of the unit. 'golden' = known-good reference, 'failing' = known-faulty reference, null = production unit. |
| `part` | `RunGetPart` | :heavy_check_mark: | Part information with revision details. |
| `batch` | `NullableField<RunGetBatch>` | :heavy_minus_sign: | Batch information for this unit. |
