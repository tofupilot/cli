# LogListRun

Run information for this log entry.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | ID of the run this log belongs to. |
| `outcome` | `LogGetOutcome` | :heavy_check_mark: | Outcome of the run. |
| `started_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | When the run started. |
