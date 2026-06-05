# RunGetResponse

Single test run details.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the run. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the run was created. |
| `started_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the run execution started. |
| `ended_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the run execution ended. |
| `duration` | `String` | :heavy_check_mark: | ISO 8601 duration of the run (computed from started_at and ended_at). |
| `outcome` | `RunGetOutcome` | :heavy_check_mark: | Final result of the run execution. |
| `docstring` | `NullableField<String>` | :heavy_minus_sign: | Additional notes or documentation about this test run. |
| `created_by_user` | `NullableField<RunGetCreatedByUser>` | :heavy_minus_sign: | User whose API key was used to create this run. Only returned if `all` or `created_by` is included. |
| `created_by_station` | `NullableField<RunGetCreatedByStation>` | :heavy_minus_sign: | Station whose API key was used to create this run. Only returned if `all` or `created_by` is included. |
| `operated_by` | `NullableField<RunGetOperatedBy>` | :heavy_minus_sign: | User who operated this run. Only returned if `all` or `operated_by` is included. |
| `procedure` | `RunGetProcedure` | :heavy_check_mark: | Test procedure associated with this run. |
| `unit` | `RunGetUnit` | :heavy_check_mark: | Unit under test information. |
| `phases` | `Option<Vec<RunGetPhases>>` | :heavy_minus_sign: | Array of execution phases in this run. Only returned if `all` or `phases` is included. |
| `attachments` | `Option<Vec<RunGetAttachments>>` | :heavy_minus_sign: | Files attached to this run, including both regular attachments and test reports. Only returned if `all` or `attachments` is included. |
| `logs` | `Option<Vec<RunGetLogs>>` | :heavy_minus_sign: | N/A |
| `sub_units` | `Option<Vec<RunGetSubUnits>>` | :heavy_minus_sign: | Array of sub-units that had parent changes during this run. Only returned if `all` or `sub_units` is included. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata key/value pairs on the run. |
