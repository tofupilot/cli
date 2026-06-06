# UnitListLastRun

Most recent test run performed on this unit. Null if no runs have been executed.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the run. |
| `outcome` | `LogGetOutcome` | :heavy_check_mark: | Final result of the test run execution. |
| `started_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the run execution started. |
| `ended_at` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | ISO 8601 timestamp when the run execution completed. Null if still running. |
| `procedure` | `Option<UnitListProcedure>` | :heavy_minus_sign: | Test procedure that was executed. Null if run had no associated procedure. |
