# Attachments

## Overview

### Available Operations

* [initialize](#initialize) - Initialize upload
* [finalize](#finalize) - Finalize upload

## initialize

Get a pre-signed URL to upload a file. Returns the upload ID and URL. PUT the file to the URL, then call Finalize upload.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.attachments().initialize()
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
| `name` | `String` | :heavy_check_mark: | File name including extension (e.g. "report.pdf") |

### Response

**[`AttachmentInitializeResponse`](../../models/attachmentinitializeresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::BadRequest` | 400 | application/json |
| `Error::Unauthorized` | 401 | application/json |
| `Error::Forbidden` | 403 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::BadGateway` | 502 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

## finalize

Finalize a file upload after uploading to the pre-signed URL. Validates the file and records its metadata.

### Example Usage

```rust
use tofupilot::TofuPilot;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    let result = client.attachments().finalize()
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
| `id` | `String` | :heavy_check_mark: | ID of the upload to finalize |

### Response

**[`AttachmentFinalizeResponse`](../../models/attachmentfinalizeresponse.md)**

### Errors

| Error Type | Status Code | Content Type |
| --- | --- | --- |
| `Error::Unauthorized` | 401 | application/json |
| `Error::NotFound` | 404 | application/json |
| `Error::InternalServerError` | 500 | application/json |
| `Error::UnexpectedStatus` | 4XX, 5XX | \*/\* |

