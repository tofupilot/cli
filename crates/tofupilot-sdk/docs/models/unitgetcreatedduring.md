# UnitGetCreatedDuring

Run that created this unit.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Run ID. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the run was created. |
| `started_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the run started. |
| `ended_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the run ended. |
| `duration` | `String` | :heavy_check_mark: | ISO 8601 duration of the run (computed from started_at and ended_at). |
| `outcome` | `RunGetOutcome` | :heavy_check_mark: | Final result of the run execution. |
| `procedure` | `UnitGetProcedure` | :heavy_check_mark: | Procedure information. |
