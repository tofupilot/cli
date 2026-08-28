# StationCreateRequest


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `String` | :heavy_check_mark: | Name of the station |
| `procedure_id` | `Option<String>` | :heavy_minus_sign: | Optional procedure ID to link the station to |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata to attach to the station (max 50 keys per station). Plain object of key/value pairs; values can be string, number, or boolean. Type is detected from the value. Use it for descriptive fields such as location or asset tag — not for procedure configuration, which belongs to station config. |
