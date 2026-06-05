# PartCreateRequest


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `number` | `String` | :heavy_check_mark: | Unique identifier number for the part. |
| `name` | `Option<String>` | :heavy_minus_sign: | Human-readable name for the part. If not provided, a default name will be used. |
| `revision_number` | `Option<String>` | :heavy_minus_sign: | Revision identifier for the part version. If not provided, default revision identifier will be used. |
