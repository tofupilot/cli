# BatchListData


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the batch. |
| `number` | `String` | :heavy_check_mark: | Batch number. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO timestamp when the batch was created. |
| `created_by_user` | `NullableField<BatchListCreatedByUser>` | :heavy_minus_sign: | User who created this batch. |
| `created_by_station` | `NullableField<BatchListCreatedByStation>` | :heavy_minus_sign: | Station that created this batch. |
| `unit_count` | `i64` | :heavy_check_mark: | Total number of units in this batch. |
