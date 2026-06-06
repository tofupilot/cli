# PhaseListRequest


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `procedure_id` | `String` | :heavy_check_mark: | Procedure to list phases for. Required: phases are scoped to a single procedure. |
| `names` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `outcomes` | `Option<Vec<PhaseGetOutcome>>` | :heavy_minus_sign: | N/A |
| `started_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `started_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `exclude_retries` | `Option<bool>` | :heavy_minus_sign: | N/A |
| `run_outcomes` | `Option<Vec<LogGetOutcome>>` | :heavy_minus_sign: | N/A |
| `ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `duration_min` | `Option<String>` | :heavy_minus_sign: | N/A |
| `duration_max` | `Option<String>` | :heavy_minus_sign: | N/A |
| `deployment_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `procedure_versions` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `environments` | `Option<Vec<Environment>>` | :heavy_minus_sign: | N/A |
| `operated_by_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `created_by_station_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `created_by_user_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `serial_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `part_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `revision_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `batch_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `samples` | `Option<Vec<PhaseListQueryParamSample>>` | :heavy_minus_sign: | N/A |
| `sort_by` | `Option<PhaseListSortBy>` | :heavy_minus_sign: | Field to sort results by. |
| `sort_order` | `Option<ListSortOrder>` | :heavy_minus_sign: | Sort order direction. |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of phases to return. |
| `cursor` | `Option<i64>` | :heavy_minus_sign: | Cursor for pagination. Use next_cursor from the previous response to fetch the next page. |
