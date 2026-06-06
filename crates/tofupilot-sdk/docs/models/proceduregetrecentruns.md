# ProcedureGetRecentRuns


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Run ID. |
| `started_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the run started. |
| `outcome` | `LogGetOutcome` | :heavy_check_mark: | Run outcome. |
| `unit` | `Option<ProcedureGetUnit>` | :heavy_minus_sign: | Unit information. |
