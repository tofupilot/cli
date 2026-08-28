# PartCreateRevisionRequestBody


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `number` | `String` | :heavy_check_mark: | Revision number (e.g., version number or code). |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata to attach to the revision (max 50 keys per revision). Plain object of key/value pairs; values can be string, number, or boolean. Type is detected from the value. |
