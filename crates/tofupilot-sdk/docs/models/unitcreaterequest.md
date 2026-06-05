# UnitCreateRequest


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `serial_number` | `String` | :heavy_check_mark: | Unique serial number identifier for the unit. Must be unique within the organization. |
| `part_number` | `String` | :heavy_check_mark: | Component part number that defines what type of unit this is. If the part does not exist, it will be created. |
| `revision_number` | `String` | :heavy_check_mark: | Hardware revision identifier for the specific version of the part. If the revision does not exist, it will be created. |
| `sample` | `NullableField<Sample>` | :heavy_minus_sign: | Reference-sample classification. 'golden' marks a known-good reference unit; 'failing' marks a known-faulty reference unit. Both are excluded from production analytics aggregates (FPY, Cpk, throughput) by default. Omit or null for regular production units. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata to attach to the unit (max 50 keys per unit). Plain object of key/value pairs; values can be string, number, or boolean. Type is detected from the value. |
