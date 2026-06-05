# RunGetCreatedByUser

User whose API key was used to create this run. Only returned if `all` or `created_by` is included.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | User ID. |
| `name` | `Option<String>` | :heavy_minus_sign: | User display name. |
| `email` | `Option<String>` | :heavy_minus_sign: | User email address. |
