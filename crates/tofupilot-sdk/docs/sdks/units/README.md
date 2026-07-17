# Units

## Overview

### Available Operations

* [create](#create) - Create unit
* [list](#list) - List and filter units
* [delete](#delete) - Delete units
* [get](#get) - Get unit
* [update](#update) - Update unit
* [add_child](#add_child) - Add sub-unit
* [remove_child](#remove_child) - Remove sub-unit
* [create_attachment](#create_attachment) - Attach file to unit
* [delete_attachment](#delete_attachment) - Delete unit attachments

## create

Create a unit with a serial number and link it to a part revision.

### Example Usage

```rust
use tofupilot::TofuPilot;
use tofupilot::types::*;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.units().create()
        .serial_number("SN-001234")
        .part_number("PCB-V1.2")
        .revision_number("REV-A")
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `serial_number` | `String` | :heavy_check_mark: | Unique serial number identifier for the unit. Must be unique within the organization. |
| `part_number` | `String` | :heavy_check_mark: | Component part number that defines what type of unit this is. If the part does not exist, it will be created. |
| `revision_number` | `String` | :heavy_check_mark: | Hardware revision identifier for the specific version of the part. If the revision does not exist, it will be created. |
| `sample` | `NullableField<Sample>` | :heavy_minus_sign: | Sample classification. 'golden' marks a known-good reference unit; 'failing' marks a known-faulty reference unit; 'ignored' marks a bench-check unit excluded from analytics and alerts. All are excluded from production analytics aggregates (FPY, Cpk, throughput) by default. Omit or null for regular production units. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata to attach to the unit (max 50 keys per unit). Plain object of key/value pairs; values can be string, number, or boolean. Type is detected from the value. |

### Response

**[`UnitCreateResponse`](../../models/unitcreateresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::Conflict` | 409 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## list

List units with filtering by serial number, part number, and batch. Cursor-paginated.

### Example Usage

```rust
use tofupilot::TofuPilot;
use tofupilot::types::*;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.units().list()
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
| `serial_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `part_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `revision_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `batch_numbers` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `procedure_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `outcomes` | `Option<Vec<LogGetOutcome>>` | :heavy_minus_sign: | N/A |
| `started_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `started_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `latest_only` | `Option<bool>` | :heavy_minus_sign: | N/A |
| `run_count_min` | `Option<i64>` | :heavy_minus_sign: | N/A |
| `run_count_max` | `Option<i64>` | :heavy_minus_sign: | N/A |
| `created_after` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `created_before` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `created_by_user_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `created_by_station_ids` | `Option<Vec<String>>` | :heavy_minus_sign: | N/A |
| `exclude_units_with_parent` | `Option<bool>` | :heavy_minus_sign: | N/A |
| `samples` | `Option<Vec<Sample>>` | :heavy_minus_sign: | N/A |
| `limit` | `Option<i64>` | :heavy_minus_sign: | Maximum number of units to return. |
| `cursor` | `Option<i64>` | :heavy_minus_sign: | N/A |
| `sort_by` | `Option<UnitListSortBy>` | :heavy_minus_sign: | Field to sort results by. last_run_at sorts by most recent test run date. last_run_procedure sorts by procedure name of the last run. |
| `sort_order` | `Option<ListSortOrder>` | :heavy_minus_sign: | Sort order direction. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Filter units by custom metadata. Supports up to 5 keys per request. Per-key operators: string `{in: [...]}`/`{contains: "..."}`, number `{gte, lte, gt, lt, eq}`, bool `{eq: true|false}`. |
| `include_metadata` | `Option<bool>` | :heavy_minus_sign: | When true, includes the unit metadata array in the response. Defaults to false to keep payloads small. |

### Response

**[`UnitListResponse`](../../models/unitlistresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## delete

Delete units by serial number. Sub-units are unlinked, not deleted. Irreversible.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.units().delete()
        .serial_numbers(vec!["SN-001234".into()])
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `serial_numbers` | `Vec<String>` | :heavy_check_mark: | Array of unit serial numbers to delete. |

### Response

**[`UnitDeleteResponse`](../../models/unitdeleteresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## get

Get a unit by serial number, with its part, parent/child links, and run history.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.units().get()
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
| `serial_number` | `String` | :heavy_check_mark: | Serial number of the unit to retrieve. |

### Response

**[`UnitGetResponse`](../../models/unitgetresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## update

Update a unit: serial number, part revision, batch, and file attachments.

### Example Usage

```rust
use tofupilot::TofuPilot;
use tofupilot::types::*;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.units().update()
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
| `serial_number` | `String` | :heavy_check_mark: | Serial number of the unit to update. |
| `new_serial_number` | `Option<String>` | :heavy_minus_sign: | New serial number for the unit. |
| `part_number` | `Option<String>` | :heavy_minus_sign: | New part number for the unit. |
| `revision_number` | `Option<String>` | :heavy_minus_sign: | New revision number for the unit. |
| `batch_number` | `NullableField<String>` | :heavy_minus_sign: | New batch number for the unit. Set to null to remove batch. |
| `attachments` | `Option<Vec<String>>` | :heavy_minus_sign: | Array of upload IDs to attach to the unit. |
| `sample` | `NullableField<Sample>` | :heavy_minus_sign: | Sample classification. 'golden' marks a known-good reference unit; 'failing' marks a known-faulty reference unit; 'ignored' marks a bench-check unit excluded from analytics and alerts. All are excluded from production analytics by default. Set to null to clear and treat as a production unit. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata to upsert on the unit. Plain object of key/value pairs. PATCH semantics: keys not present here are preserved. Pass `null` as a value to delete a key. |

### Response

**[`UnitUpdateResponse`](../../models/unitupdateresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::Conflict` | 409 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## add_child

Link a sub-unit to a parent unit to track assemblies.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.units().add_child()
        .serial_number("SN-001234")
        .child_serial_number("SN-001234")
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `serial_number` | `String` | :heavy_check_mark: | Serial number of the parent unit |
| `child_serial_number` | `String` | :heavy_check_mark: | Serial number of the sub-unit to add |

### Response

**[`UnitAddChildResponse`](../../models/unitaddchildresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## remove_child

Unlink a sub-unit from its parent. Neither unit is deleted.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.units().remove_child()
        .serial_number("SN-001234")
        .child_serial_number("SN-001234")
        .send()
        .await?;

    println!("{:?}", result);
    Ok(())
}
```

### Parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `serial_number` | `String` | :heavy_check_mark: | Serial number of the parent unit |
| `child_serial_number` | `String` | :heavy_check_mark: | Serial number of the sub-unit to remove |

### Response

**[`UnitRemoveChildResponse`](../../models/unitremovechildresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## create_attachment

Attach a file to a unit. Returns an upload ID and pre-signed URL; PUT the file to the URL, then call Finalize upload to commit.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.units().create_attachment()
        .serial_number("SN-001234")
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
| `serial_number` | `String` | :heavy_check_mark: | Serial number of the unit to attach the file to. Matched case-insensitively. |
| `name` | `String` | :heavy_check_mark: | File name including extension (e.g. "calibration.pdf"). Used to determine content type and display name. |

### Response

**[`UnitCreateAttachmentResponse`](../../models/unitcreateattachmentresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## delete_attachment

Delete attachments from a unit by ID. Removes the files from storage and unlinks them.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.units().delete_attachment()
        .serial_number("SN-001234")
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
| `serial_number` | `String` | :heavy_check_mark: | Serial number of the unit. Matched case-insensitively. |
| `ids` | `Vec<String>` | :heavy_check_mark: | Attachment IDs to delete |

### Response

**[`UnitDeleteAttachmentResponse`](../../models/unitdeleteattachmentresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

