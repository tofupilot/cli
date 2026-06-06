# Measurements

## Overview

### Available Operations

* [list](#list) - List and filter measurements
* [get](#get) - Get measurement

## list

List measurements for a procedure with filtering by phase, measurement, value, outcome, date range, unit, and run details. Cursor-paginated.

### Example Usage

```rust
use tofupilot::TofuPilot;
use tofupilot::types::*;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.measurements().list()
        .procedure_id("550e8400-e29b-41d4-a716-446655440000")
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `procedure_id` | `String` | :heavy_check_mark: | Procedure to list measurements for. Required: measurements are scoped to a single procedure. |
| `phase_name` | `Option<String>` | :heavy_minus_sign: | N/A |
| `measurement_name` | `Option<String>` | :heavy_minus_sign: | N/A |
| `phase_names` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `outcomes` | `Option<Vec<LogGetOutcome>>` | :heavy_minus_sign: | N/A |
| `ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `duration_min` | `Option<String>` | :heavy_minus_sign: | N/A |
| `duration_max` | `Option<String>` | :heavy_minus_sign: | N/A |
| `deployment_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `procedure_versions` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `environments` | `Option<Vec<Environment>>` | :heavy_minus_sign: | N/A |
| `serial_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `part_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `revision_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `batch_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `operated_by_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `created_by_station_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `created_by_user_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `started_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `started_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `exclude_retries` | `Option<bool>` | :heavy_minus_sign: | Exclude retried phase attempts, keeping only the final attempt. |
| `value_min` | `Option<f64>` | :heavy_minus_sign: | N/A |
| `value_max` | `Option<f64>` | :heavy_minus_sign: | N/A |
| `value_bool` | `Option<bool>` | :heavy_minus_sign: | N/A |
| `value_strings` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `measurement_outcomes` | `Option<Vec<Outcome>>` | :heavy_minus_sign: | N/A |
| `measurement_types` | `Option<Vec<MeasurementType>>` | :heavy_minus_sign: | N/A |
| `sort_by` | `Option<MeasurementListSortBy>` | :heavy_minus_sign: | Field to sort results by. |
| `sort_order` | `Option<ListSortOrder>` | :heavy_minus_sign: | Sort order direction. |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of measurements to return. |
| `cursor` | `Option<i64>` | :heavy_minus_sign: | Cursor for pagination. Use next_cursor from the previous response to fetch the next page. |

### Response

**[`MeasurementListResponse`](../../models/measurementlistresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## get

Get a single measurement by ID, with its value, validators, and run context.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.measurements().get()
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
| `id` | `String` | :heavy_check_mark: | ID of the measurement to retrieve. |

### Response

**[`MeasurementGetResponse`](../../models/measurementgetresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

