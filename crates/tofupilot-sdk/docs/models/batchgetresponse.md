# BatchGetResponse

Batch retrieved successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the batch. |
| `number` | `String` | :heavy_check_mark: | Batch number. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the batch was created. |
| `created_by_user` | `NullableField<BatchGetCreatedByUser>` | :heavy_minus_sign: | User who created this batch. |
| `created_by_station` | `NullableField<BatchGetCreatedByStation>` | :heavy_minus_sign: | Station that created this batch. |
| `units` | `Vec<BatchGetUnits>` | :heavy_check_mark: | Array of units in this batch. Empty array if no units. |
