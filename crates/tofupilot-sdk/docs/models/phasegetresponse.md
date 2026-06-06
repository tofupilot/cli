# PhaseGetResponse

Phase retrieved successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | N/A |
| `name` | `String` | :heavy_check_mark: | N/A |
| `outcome` | `PhaseGetOutcome` | :heavy_check_mark: | N/A |
| `started_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | N/A |
| `ended_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | N/A |
| `duration` | `String` | :heavy_check_mark: | N/A |
| `retry_count` | `i64` | :heavy_check_mark: | N/A |
| `docstring` | `NullableField<String>` | :heavy_minus_sign: | N/A |
| `measurements` | `Vec<PhaseGetMeasurements>` | :heavy_check_mark: | N/A |
| `run` | `PhaseGetRun` | :heavy_check_mark: | N/A |
| `procedure` | `PhaseGetProcedure` | :heavy_check_mark: | N/A |
