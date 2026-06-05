# ProcedureGetResponse

Procedure retrieved successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the procedure. |
| `name` | `String` | :heavy_check_mark: | Procedure name. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the procedure was created. |
| `created_by_user` | `Option<ProcedureGetCreatedByUser>` | :heavy_minus_sign: | User who created this procedure. |
| `runs_count` | `f64` | :heavy_check_mark: | Total number of runs for this procedure. |
| `recent_runs` | `Vec<ProcedureGetRecentRuns>` | :heavy_check_mark: | List of recent runs for this procedure. |
| `stations` | `Vec<ProcedureGetStations>` | :heavy_check_mark: | Stations linked to this procedure. |
