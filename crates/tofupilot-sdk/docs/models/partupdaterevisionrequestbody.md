# PartUpdateRevisionRequestBody


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `number` | `Option<String>` | :heavy_minus_sign: | New revision number to set. |
| `image_id` | `Option<String>` | :heavy_minus_sign: | Upload ID for the revision image, or empty string to remove image |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata to upsert on the revision. Plain object of key/value pairs. PATCH semantics: keys not present here are preserved. Pass `null` as a value to delete a key. |
