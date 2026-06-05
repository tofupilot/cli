# UnitListMeta


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `has_more` | `bool` | :heavy_check_mark: | Whether there are more results available for pagination. |
| `next_cursor` | `Option<i64>` | :heavy_minus_sign: | Cursor value to fetch the next page of results. Use this value as the cursor parameter in the next request. Null if no more results available. |
