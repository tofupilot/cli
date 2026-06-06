# PhaseListData


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the phase. |
| `name` | `String` | :heavy_check_mark: | Name of the phase. |
| `outcome` | `PhaseGetOutcome` | :heavy_check_mark: | Outcome of the phase. |
| `started_at` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | ISO 8601 timestamp when the phase started. |
| `ended_at` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | ISO 8601 timestamp when the phase ended. |
| `duration_ms` | `Option<i64>` | :heavy_minus_sign: | Phase duration in milliseconds. Null when start or end is missing. |
| `retry_count` | `i64` | :heavy_check_mark: | Retry attempt number of the phase (0 for the first attempt). |
| `is_final_attempt` | `bool` | :heavy_check_mark: | True when this is the final attempt of the phase. |
| `run_id` | `String` | :heavy_check_mark: | ID of the run this phase belongs to. |
| `run_outcome` | `LogGetOutcome` | :heavy_check_mark: | Outcome of the run this phase belongs to. |
| `serial_number` | `Option<String>` | :heavy_minus_sign: | Serial number of the unit tested. |
| `sample` | `Option<Sample>` | :heavy_minus_sign: | Sample class of the unit (golden or failing). Null for regular units. |
