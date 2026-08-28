# PartListData


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique database identifier of the part. |
| `number` | `String` | :heavy_check_mark: | Unique part number identifier. |
| `name` | `String` | :heavy_check_mark: | Human-readable name of the part. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | Time at which the part was created. |
| `revisions` | `Vec<PartListRevisions>` | :heavy_check_mark: | List of revisions for this part. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata key/value pairs on the part. Only present when the request sets `include_metadata=true`. |
