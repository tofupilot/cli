# UnitUpdateRequestBody


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `new_serial_number` | `Option<String>` | :heavy_minus_sign: | New serial number for the unit. |
| `part_number` | `Option<String>` | :heavy_minus_sign: | New part number for the unit. |
| `revision_number` | `Option<String>` | :heavy_minus_sign: | New revision number for the unit. |
| `batch_number` | `NullableField<String>` | :heavy_minus_sign: | New batch number for the unit. Set to null to remove batch. |
| `attachments` | `Option<Vec<String>>` | :heavy_minus_sign: | Array of upload IDs to attach to the unit. |
| `sample` | `NullableField<Sample>` | :heavy_minus_sign: | Reference-sample classification. 'golden' marks a known-good reference unit; 'failing' marks a known-faulty reference unit. Both are excluded from production analytics by default. Set to null to clear and treat as a production unit. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata to upsert on the unit. Plain object of key/value pairs. PATCH semantics: keys not present here are preserved. Pass `null` as a value to delete a key. |
