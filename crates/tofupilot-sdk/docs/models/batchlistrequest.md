# BatchListRequest


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `created_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `created_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of batches to return. Use `cursor` to fetch additional results. |
| `cursor` | `Option<i64>` | :heavy_minus_sign: | N/A |
| `search_query` | `Option<String>` | :heavy_minus_sign: | N/A |
| `part_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `revision_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `sort_by` | `Option<BatchListSortBy>` | :heavy_minus_sign: | Field to sort results by. |
| `sort_order` | `Option<ListSortOrder>` | :heavy_minus_sign: | Sort order direction. |
