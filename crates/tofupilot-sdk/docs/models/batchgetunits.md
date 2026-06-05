# BatchGetUnits


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unit ID. |
| `serial_number` | `String` | :heavy_check_mark: | Unit serial number. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the unit was created. |
| `part` | `BatchGetPart` | :heavy_check_mark: | Part information with revision details for this unit. |
