# PartUpdateResponse

Part updated successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique database identifier of the updated part. |
| `number` | `String` | :heavy_check_mark: | Unique part number identifier. |
| `name` | `String` | :heavy_check_mark: | Human-readable name of the part. |
| `updated_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the part was updated. |
