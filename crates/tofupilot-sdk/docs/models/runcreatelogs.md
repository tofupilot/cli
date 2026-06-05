# RunCreateLogs


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `level` | `Level` | :heavy_check_mark: | Severity level of the log message following standard system logging levels. Use DEBUG for detailed diagnostic information, INFO for general execution information, WARNING for unexpected events or potential issues, ERROR for serious problems that prevented function execution, CRITICAL for severe errors that may cause program termination. |
| `timestamp` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the log message was generated. |
| `message` | `String` | :heavy_check_mark: | Content of the log message. Contains the actual log text describing the event, error, or information being logged. Messages longer than 10,000 characters will be truncated; empty messages become "(empty)". |
| `source_file` | `String` | :heavy_check_mark: | Name or path of the source file where the log message originated. Helps identify the code location that generated the log entry. Paths longer than 200 characters keep the trailing 200 (leading characters are dropped); empty values become "unknown". |
| `line_number` | `i64` | :heavy_check_mark: | Line number in the source file where the log message was generated. Used for debugging and tracing log origins. |
