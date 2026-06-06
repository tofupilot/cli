# RunGetPhases


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Phase ID. |
| `name` | `String` | :heavy_check_mark: | Phase name. |
| `outcome` | `PhaseGetOutcome` | :heavy_check_mark: | Phase execution result. |
| `started_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the phase started. |
| `ended_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the phase ended. |
| `duration` | `String` | :heavy_check_mark: | ISO 8601 duration of the phase (computed from started_at and ended_at). |
| `retry_count` | `i64` | :heavy_check_mark: | Zero-based retry attempt index. 0 = first attempt, 1 = first retry, etc. |
| `docstring` | `NullableField<String>` | :heavy_minus_sign: | Phase documentation string. |
| `measurements` | `Vec<RunGetMeasurements>` | :heavy_check_mark: | Array of measurements taken during this phase. |
