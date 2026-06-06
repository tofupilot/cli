# LogListRequest


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `procedure_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `search_query` | `Option<String>` | :heavy_minus_sign: | N/A |
| `levels` | `Option<Vec<Level>>` | :heavy_minus_sign: | N/A |
| `timestamp_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | Filter logs with timestamp after this date (inclusive). |
| `timestamp_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | Filter logs with timestamp before this date (inclusive). |
| `source_files` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `run_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `run_outcomes` | `Option<Vec<LogGetOutcome>>` | :heavy_minus_sign: | N/A |
| `procedure_versions` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `deployment_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `environments` | `Option<Vec<Environment>>` | :heavy_minus_sign: | N/A |
| `serial_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `part_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `revision_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `batch_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `samples` | `Option<Vec<Sample>>` | :heavy_minus_sign: | N/A |
| `created_by_station_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `created_by_user_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `sort_by` | `Option<LogListSortBy>` | :heavy_minus_sign: | Field to sort results by. |
| `sort_order` | `Option<ListSortOrder>` | :heavy_minus_sign: | Sort order direction. |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of logs to return. |
| `cursor` | `Option<i64>` | :heavy_minus_sign: | N/A |
