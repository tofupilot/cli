# RunCreateAttachmentResponse

Run attachment created successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier of the created attachment. Use this to reference the attachment in subsequent API calls. |
| `upload_url` | `String` | :heavy_check_mark: | Temporary pre-signed URL to upload the file directly to storage. Send a PUT request with the raw file bytes and appropriate Content-Type header. The URL expires after 60 seconds. |
