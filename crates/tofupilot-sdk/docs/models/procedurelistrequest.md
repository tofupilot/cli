# ProcedureListRequest


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of procedures to return per page. |
| `cursor` | `Option<f64>` | :heavy_minus_sign: | N/A |
| `search_query` | `Option<String>` | :heavy_minus_sign: | N/A |
| `created_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `created_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
