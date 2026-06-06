# DeploymentListMeta

Pagination metadata.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `has_more` | `bool` | :heavy_check_mark: | Whether more deployments are available beyond this page. |
| `next_cursor` | `Option<i64>` | :heavy_minus_sign: | Cursor to fetch the next page. Null when there are no more results. |
