# Procedures

## Overview

### Available Operations

* [create](#create) - Create procedure
* [list](#list) - List and filter procedures
* [get](#get) - Get procedure
* [delete](#delete) - Delete procedure
* [update](#update) - Update procedure

## create

Create a procedure to group and track related runs.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.procedures().create()
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
| `name` | `String` | :heavy_check_mark: | Name of the procedure. Must be unique within the organization. |

### Response

**[`ProcedureCreateResponse`](../../models/procedurecreateresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## list

List procedures with filtering and search. Includes creator and linked repository. Cursor-paginated.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.procedures().list()
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of procedures to return per page. |
| `cursor` | `Option<f64>` | :heavy_minus_sign: | N/A |
| `search_query` | `Option<String>` | :heavy_minus_sign: | N/A |
| `created_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `created_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |

### Response

**[`ProcedureListResponse`](../../models/procedurelistresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## get

Get a procedure by ID, with recent runs, linked stations, and version history.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.procedures().get()
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
| `id` | `String` | :heavy_check_mark: | Unique identifier of the procedure to retrieve. |

### Response

**[`ProcedureGetResponse`](../../models/proceduregetresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## delete

Delete a procedure and all its runs, phases, measurements, and attachments. Irreversible.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.procedures().delete()
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
| `id` | `String` | :heavy_check_mark: | Unique identifier of the procedure to delete. |

### Response

**[`ProcedureDeleteResponse`](../../models/proceduredeleteresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## update

Update a procedure's name or configuration. Only provided fields are changed.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.procedures().update()
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
| `id` | `String` | :heavy_check_mark: | Unique identifier of the procedure to update. |
| `name` | `Option<String>` | :heavy_minus_sign: | New name for the procedure. |
| `production_branch` | `NullableField<String>` | :heavy_minus_sign: | Branch treated as production. Pushes to this branch deploy as production; every other branch deploys as preview. Null = no branch promoted to production. |
| `auto_push_enabled` | `Option<bool>` | :heavy_minus_sign: | Master switch for auto-pushing builds to linked stations. Build artifacts are always recorded; this only gates the station fan-out. |
| `excluded_branch_patterns` | `Option<Vec<String>>` | :heavy_minus_sign: | Branches matching any of these patterns (exact name or minimatch glob, e.g. "renovate/*") skip preview deployments. Empty array = no exclusions. |
| `root_directory` | `NullableField<String>` | :heavy_minus_sign: | Path within the linked repo to the directory holding this procedure's `pyproject.toml` (and `procedure.yaml` for framework procedures). Empty/null = repo root. |
| `entry_point` | `NullableField<String>` | :heavy_minus_sign: | Entry-point path inside the procedure's package dir, relative to it. Forwarded to the CLI through the deployment manifest. Empty/null = use the framework default (openhtf/plain → main.py, pytest → ".", yaml → procedure.yaml auto-discovery). |

### Response

**[`ProcedureUpdateResponse`](../../models/procedureupdateresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

