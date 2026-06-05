# PartGetResponse

Part retrieved successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the part. |
| `number` | `String` | :heavy_check_mark: | Part number. |
| `name` | `String` | :heavy_check_mark: | Part name. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the part was created. |
| `created_by_user` | `NullableField<PartGetCreatedByUser>` | :heavy_minus_sign: | User who created this part. |
| `created_by_station` | `NullableField<PartGetCreatedByStation>` | :heavy_minus_sign: | Station that created this part. |
| `revisions` | `Vec<PartGetRevisions>` | :heavy_check_mark: | List of revisions for this part. |
