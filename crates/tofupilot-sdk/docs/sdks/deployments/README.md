# Deployments

## Overview

### Available Operations

* [list](#list) - List and filter deployments
* [get](#get) - Get deployment

## list

List deployments with filtering by procedure, environment, build status, branch, author, and date range. Cursor-paginated.

### Example Usage

```rust
use tofupilot::TofuPilot;
use tofupilot::types::*;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.deployments().list()
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
| `environments` | `Option<Vec<Environment>>` | :heavy_minus_sign: | N/A |
| `build_statuses` | `Option<Vec<DeploymentGetStatus>>` | :heavy_minus_sign: | N/A |
| `pushed` | `Option<bool>` | :heavy_minus_sign: | N/A |
| `branch_names` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `author_usernames` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `deployed_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `deployed_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `search_query` | `Option<String>` | :heavy_minus_sign: | N/A |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of deployments to return. |
| `cursor` | `Option<i64>` | :heavy_minus_sign: | Cursor for pagination. Use next_cursor from the previous response to fetch the next page. |

### Response

**[`DeploymentListResponse`](../../models/deploymentlistresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## get

Get a deployment by ID, with its artifact metadata, commit, creator, and per-station push state.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.deployments().get()
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
| `id` | `String` | :heavy_check_mark: | The deployment ID to retrieve. |

### Response

**[`DeploymentGetResponse`](../../models/deploymentgetresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

