# PartListData


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique database identifier of the part. |
| `number` | `String` | :heavy_check_mark: | Unique part number identifier. |
| `name` | `String` | :heavy_check_mark: | Human-readable name of the part. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | Time at which the part was created. |
| `revisions` | `Vec<PartListRevisions>` | :heavy_check_mark: | List of revisions for this part. |
