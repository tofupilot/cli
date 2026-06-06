# LogGetResponse

Log retrieved successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | N/A |
| `level` | `Level` | :heavy_check_mark: | N/A |
| `message` | `String` | :heavy_check_mark: | N/A |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | N/A |
| `source_file` | `Option<String>` | :heavy_minus_sign: | N/A |
| `line_number` | `Option<i64>` | :heavy_minus_sign: | N/A |
| `run` | `LogGetRun` | :heavy_check_mark: | N/A |
| `unit` | `LogGetUnit` | :heavy_check_mark: | N/A |
| `procedure` | `LogGetProcedure` | :heavy_check_mark: | N/A |
| `created_by_user` | `Option<LogGetCreatedByUser>` | :heavy_minus_sign: | N/A |
| `created_by_station` | `Option<LogGetCreatedByStation>` | :heavy_minus_sign: | N/A |
