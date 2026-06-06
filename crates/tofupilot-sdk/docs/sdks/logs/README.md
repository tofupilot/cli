# Logs

## Overview

### Available Operations

* [list](#list) - List and filter logs
* [get](#get) - Get log

## list

List logs with filtering by run, procedure, level, date range, unit, and source. Cursor-paginated.

### Example Usage

```rust
use tofupilot::TofuPilot;
use tofupilot::types::*;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.logs().list()
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `procedure_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `search_query` | `Option<String>` | :heavy_minus_sign: | N/A |
| `levels` | `Option<Vec<Level>>` | :heavy_minus_sign: | N/A |
| `timestamp_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | Filter logs with timestamp after this date (inclusive). |
| `timestamp_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | Filter logs with timestamp before this date (inclusive). |
| `source_files` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `run_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `run_outcomes` | `Option<Vec<LogGetOutcome>>` | :heavy_minus_sign: | N/A |
| `procedure_versions` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `deployment_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `environments` | `Option<Vec<Environment>>` | :heavy_minus_sign: | N/A |
| `serial_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `part_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `revision_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `batch_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `samples` | `Option<Vec<Sample>>` | :heavy_minus_sign: | N/A |
| `created_by_station_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `created_by_user_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `sort_by` | `Option<LogListSortBy>` | :heavy_minus_sign: | Field to sort results by. |
| `sort_order` | `Option<ListSortOrder>` | :heavy_minus_sign: | Sort order direction. |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of logs to return. |
| `cursor` | `Option<i64>` | :heavy_minus_sign: | N/A |

### Response

**[`LogListResponse`](../../models/loglistresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## get

Get a single log entry by ID, with its run and unit context.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.logs().get()
        .id("550e8400-e29b-41d4-a716-446655440000")
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the log entry. |

### Response

**[`LogGetResponse`](../../models/loggetresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

