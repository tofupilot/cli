# Parts

## Overview

### Available Operations

* [create](#create) - Create part
* [list](#list) - List and filter parts
* [get](#get) - Get part
* [update](#update) - Update part
* [delete](#delete) - Delete part

## create

Create a part, optionally with an initial revision. Part numbers match case-insensitively ("PART-001" == "part-001").

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.parts().create()
        .number("PART-001")
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `number` | `String` | :heavy_check_mark: | Unique identifier number for the part. |
| `name` | `Option<String>` | :heavy_minus_sign: | Human-readable name for the part. If not provided, a default name will be used. |
| `revision_number` | `Option<String>` | :heavy_minus_sign: | Revision identifier for the part version. If not provided, default revision identifier will be used. |

### Response

**[`PartCreateResponse`](../../models/partcreateresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::Conflict` | 409 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## list

List parts. Filter and search by name, number, or revision number. Cursor-paginated.

### Example Usage

```rust
use tofupilot::TofuPilot;
use tofupilot::types::*;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.parts().list()
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of parts to return in a single page. |
| `cursor` | `Option<i64>` | :heavy_minus_sign: | N/A |
| `search_query` | `Option<String>` | :heavy_minus_sign: | N/A |
| `procedure_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `sort_by` | `Option<PartListSortBy>` | :heavy_minus_sign: | Field to sort results by. |
| `sort_order` | `Option<ListSortOrder>` | :heavy_minus_sign: | Sort order direction. |

### Response

**[`PartListResponse`](../../models/partlistresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## get

Get a part by number, with its revisions, metadata, and linked units. Numbers match case-insensitively.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.parts().get()
        .number("PART-001")
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `number` | `String` | :heavy_check_mark: | Part number of the part to retrieve. |

### Response

**[`PartGetResponse`](../../models/partgetresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## update

Update a part's number or name. Numbers match case-insensitively.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.parts().update()
        .number("PART-001")
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `number` | `String` | :heavy_check_mark: | Part number of the part to update. |
| `new_number` | `Option<String>` | :heavy_minus_sign: | New unique identifier number for the part. |
| `name` | `Option<String>` | :heavy_minus_sign: | New human-readable name for the part. |

### Response

**[`PartUpdateResponse`](../../models/partupdateresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::Conflict` | 409 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## delete

Delete a part and all its revisions. Irreversible.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.parts().delete()
        .number("PART-001")
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `number` | `String` | :heavy_check_mark: | Part number to delete. |

### Response

**[`PartDeleteResponse`](../../models/partdeleteresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

