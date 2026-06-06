# DeploymentListRequest


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `procedure_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `environments` | `Option<Vec<Environment>>` | :heavy_minus_sign: | N/A |
| `build_statuses` | `Option<Vec<DeploymentGetStatus>>` | :heavy_minus_sign: | N/A |
| `pushed` | `Option<bool>` | :heavy_minus_sign: | N/A |
| `branch_names` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `author_usernames` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `deployed_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `deployed_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `search_query` | `Option<String>` | :heavy_minus_sign: | N/A |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of deployments to return. |
| `cursor` | `Option<i64>` | :heavy_minus_sign: | Cursor for pagination. Use next_cursor from the previous response to fetch the next page. |
