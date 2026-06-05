# PartListRequest


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of parts to return in a single page. |
| `cursor` | `Option<i64>` | :heavy_minus_sign: | N/A |
| `search_query` | `Option<String>` | :heavy_minus_sign: | N/A |
| `procedure_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `sort_by` | `Option<PartListSortBy>` | :heavy_minus_sign: | Field to sort results by. |
| `sort_order` | `Option<ListSortOrder>` | :heavy_minus_sign: | Sort order direction. |
