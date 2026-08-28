# PartUpdateRequestBody


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `new_number` | `Option<String>` | :heavy_minus_sign: | New unique identifier number for the part. |
| `name` | `Option<String>` | :heavy_minus_sign: | New human-readable name for the part. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata to upsert on the part. Plain object of key/value pairs. PATCH semantics: keys not present here are preserved. Pass `null` as a value to delete a key. |
