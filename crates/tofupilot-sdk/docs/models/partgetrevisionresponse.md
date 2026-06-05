# PartGetRevisionResponse

Revision retrieved successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier of the revision. |
| `number` | `String` | :heavy_check_mark: | Revision number. |
| `created_at` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | ISO 8601 timestamp when the revision was created. |
| `created_by_user` | `Option<PartGetRevisionCreatedByUser>` | :heavy_minus_sign: | User who created the revision. |
| `created_by_station` | `Option<PartGetRevisionCreatedByStation>` | :heavy_minus_sign: | Station that created the revision. |
| `part` | `PartGetRevisionPart` | :heavy_check_mark: | Part associated with this revision. |
| `units` | `Vec<PartGetRevisionUnits>` | :heavy_check_mark: | List of units created with this revision. |
