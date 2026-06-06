# LogListData


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the log entry. |
| `level` | `Level` | :heavy_check_mark: | Log level indicating the severity. |
| `message` | `String` | :heavy_check_mark: | The log message content. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the log was created. |
| `source_file` | `Option<String>` | :heavy_minus_sign: | Source file where the log originated. |
| `line_number` | `Option<i64>` | :heavy_minus_sign: | Line number in the source file. |
| `run` | `LogListRun` | :heavy_check_mark: | Run information for this log entry. |
| `unit` | `LogListUnit` | :heavy_check_mark: | Unit information for this log entry. |
| `procedure` | `LogListProcedure` | :heavy_check_mark: | Procedure information for this log entry. |
| `created_by_user` | `NullableField<LogListCreatedByUser>` | :heavy_minus_sign: | User who created this log. Null if created by a station or system. |
| `created_by_station` | `NullableField<LogListCreatedByStation>` | :heavy_minus_sign: | Station that created this log. Null if created by a user. |
