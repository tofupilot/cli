# UnitGetAttachments


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Attachment ID. |
| `name` | `String` | :heavy_check_mark: | File name. |
| `size` | `Option<f64>` | :heavy_minus_sign: | File size in bytes. |
| `content_type` | `Option<String>` | :heavy_minus_sign: | MIME type of the file. |
| `download_url` | `Option<String>` | :heavy_minus_sign: | Presigned URL for downloading the file. This URL is temporary and will expire. |
