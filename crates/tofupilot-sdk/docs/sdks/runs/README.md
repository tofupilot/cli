# Runs

## Overview

### Available Operations

* [create](#create) - Create run
* [list](#list) - List and filter runs
* [delete](#delete) - Delete runs
* [get](#get) - Get run
* [update](#update) - Update run
* [create_attachment](#create_attachment) - Attach file to run
* [update_metadata](#update_metadata) - Update run metadata

## create

Create a run linked to a procedure and unit. Existing procedures and units are reused automatically.

### Example Usage

```rust
use tofupilot::TofuPilot;
use tofupilot::types::*;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.runs().create()
        .outcome(LogGetOutcome::default())
        .procedure_id("550e8400-e29b-41d4-a716-446655440000")
        .started_at(chrono::Utc::now())
        .ended_at(chrono::Utc::now())
        .serial_number("SN-001234")
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `outcome` | `LogGetOutcome` | :heavy_check_mark: | Overall test result. Use PASS when test succeeds, FAIL when test fails but script execution completed successfully, ERROR when script execution fails, TIMEOUT when test exceeds time limit, ABORTED for manual script interruption. |
| `procedure_id` | `String` | :heavy_check_mark: | Procedure ID. Create the procedure in the app first, then find the auto-generated ID on the procedure page. |
| `deployment_id` | `NullableField<String>` | :heavy_minus_sign: | Deployment ID this run was executed from. Set by the CLI when running a pulled deployment so the run is linked back to the exact build it ran. Validated against the procedure; left null for ad-hoc or local runs. |
| `procedure_version` | `NullableField<String>` | :heavy_minus_sign: | Specific version of the test procedure used for the run. Matched case-insensitively. If none exist, a procedure with this procedure version will be created. If no procedure version is specified, the run will not be linked to any specific version. |
| `operated_by` | `Option<String>` | :heavy_minus_sign: | Email address of the operator who executed the test run. Honored only for API-key callers (user keys and station keys); browser session callers are auto-stamped with the session user and this field is ignored. If the email does not match a member of the calling organization, it is silently dropped and the run is recorded with no operator. The run is linked to this user (when resolved) to track who performed the test. |
| `started_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the test run began execution. This timestamp will be used to track when the test execution started and for historical analysis of test runs. A separate created_at timestamp is stored internally server side to track upload date. |
| `ended_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the test run finished execution. |
| `serial_number` | `String` | :heavy_check_mark: | Unique serial number of the unit under test. Matched case-insensitively. If no unit with this serial number exists, one will be created. |
| `part_number` | `Option<String>` | :heavy_minus_sign: | Component part number for the unit. Matched case-insensitively. This field is required if the part number cannot be extracted from the serial number (as set in the settings). This field takes precedence over extraction from serial number. A component with the provided or extracted part number will be created if one does not exist. |
| `revision_number` | `Option<String>` | :heavy_minus_sign: | Hardware revision identifier for the unit. Matched case-insensitively. If none exist, a revision with this number will be created. If no revision is specified, the unit will be linked to the default revision of the part number. |
| `batch_number` | `Option<String>` | :heavy_minus_sign: | Production batch identifier for grouping units manufactured together. Matched case-insensitively. If none exist, a batch with this batch number will be created. If no batch number is specified, the unit will not be linked to any batch. |
| `sub_units` | `Option<Vec<String>>` | :heavy_minus_sign: | Array of sub-unit serial numbers that are part of this main unit. Matched case-insensitively. Each sub-unit must already exist and will be linked as a sub-component of the main unit under test. If no sub-units are specified, the unit will be created without sub-unit relationships. |
| `docstring` | `Option<String>` | :heavy_minus_sign: | Additional notes or documentation about this test run. |
| `phases` | `Option<Vec<RunCreatePhases>>` | :heavy_minus_sign: | Array of test phases with measurements and results. Each phase represents a distinct stage of the test execution with timing information, outcome status, and optional measurements. If no phases are specified, the run will be created without phase-level organization of test data. |
| `logs` | `Option<Vec<RunCreateLogs>>` | :heavy_minus_sign: | Array of log messages generated during the test execution. Each log entry captures events, errors, and diagnostic information with severity levels and source code references. If no logs are specified, the run will be created without log entries. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata to attach to the run (max 50 keys). Plain object of key/value pairs; values can be string, number, or boolean. Type is detected from the value. |
| `unit_metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata to upsert on the unit under test (max 50 keys per unit). PATCH semantics: keys not present here are preserved on the unit. |

### Response

**[`RunCreateResponse`](../../models/runcreateresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::Forbidden` | 403 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::UnprocessableContent` | 422 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## list

List runs with filtering by unit, procedure, date range, outcome, and station. Cursor-paginated.

### Example Usage

```rust
use tofupilot::TofuPilot;
use tofupilot::types::*;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.runs().list()
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `search_query` | `Option<String>` | :heavy_minus_sign: | N/A |
| `ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `outcomes` | `Option<Vec<LogGetOutcome>>` | :heavy_minus_sign: | N/A |
| `procedure_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `procedure_versions` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `deployment_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `environments` | `Option<Vec<Environment>>` | :heavy_minus_sign: | N/A |
| `serial_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `samples` | `Option<Vec<Sample>>` | :heavy_minus_sign: | N/A |
| `part_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `revision_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `batch_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `duration_min` | `Option<String>` | :heavy_minus_sign: | N/A |
| `duration_max` | `Option<String>` | :heavy_minus_sign: | N/A |
| `started_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `started_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `ended_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `ended_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `created_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `created_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `created_by_user_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `created_by_station_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `operated_by_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of runs to return per page. |
| `cursor` | `Option<i64>` | :heavy_minus_sign: | N/A |
| `sort_by` | `Option<RunListSortBy>` | :heavy_minus_sign: | Field to sort results by. |
| `sort_order` | `Option<ListSortOrder>` | :heavy_minus_sign: | Sort order direction. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Filter runs by custom metadata. Supports up to 5 keys per request. Per-key operators: string `{in: [...]}`/`{contains: "..."}`, number `{gte, lte, gt, lt, eq}`, bool `{eq: true|false}`. |
| `include_metadata` | `Option<bool>` | :heavy_minus_sign: | When true, includes the run metadata array in the response. Defaults to false to keep payloads small. |

### Response

**[`RunListResponse`](../../models/runlistresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## delete

Delete runs by ID. Also removes their phases, measurements, and attachments. Irreversible.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.runs().delete()
        .ids(vec!["550e8400-e29b-41d4-a716-446655440000".into()])
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `ids` | `Vec<String>` | :heavy_check_mark: | Run IDs to delete. |

### Response

**[`RunDeleteResponse`](../../models/rundeleteresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## get

Get a run by ID, with its metadata, phases, measurements, and logs.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.runs().get()
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
| `id` | `String` | :heavy_check_mark: | ID of the run to retrieve. |

### Response

**[`RunGetResponse`](../../models/rungetresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## update

Link uploaded files to a run. Upload files via Initialize and Finalize first, then call this to attach them.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.runs().update()
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
| `id` | `String` | :heavy_check_mark: | Unique identifier of the run to update. |
| `attachments` | `Option<Vec<String>>` | :heavy_minus_sign: | Array of upload IDs to attach to the run. |

### Response

**[`RunUpdateResponse`](../../models/runupdateresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## create_attachment

Attach a file to a run. Returns an upload ID and pre-signed URL; PUT the file to the URL, then call Finalize upload to commit.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.runs().create_attachment()
        .id("550e8400-e29b-41d4-a716-446655440000")
        .name("My Test Procedure")
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier of the run to attach the file to. |
| `name` | `String` | :heavy_check_mark: | File name including extension (e.g. "report.pdf"). Used to determine content type and display name. |

### Response

**[`RunCreateAttachmentResponse`](../../models/runcreateattachmentresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## update_metadata

Upsert custom metadata on a run as a key/value object. Omitted keys are preserved; pass `null` to delete a key.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.runs().update_metadata()
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
| `id` | `String` | :heavy_check_mark: | Unique identifier of the run to update. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata to upsert on the run. Plain object of key/value pairs. PATCH semantics: keys not present here are preserved. Pass `null` as a value to delete a key. |

### Response

**[`RunUpdateMetadataResponse`](../../models/runupdatemetadataresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

