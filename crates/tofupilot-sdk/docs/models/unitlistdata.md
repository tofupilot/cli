# UnitListData


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the unit. |
| `serial_number` | `String` | :heavy_check_mark: | Human-readable serial number assigned to the unit. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the unit was created. |
| `sample` | `Option<Sample>` | :heavy_minus_sign: | Reference-sample classification. 'golden' = known-good reference, 'failing' = known-faulty reference, null = production unit. |
| `created_by_user` | `NullableField<UnitListCreatedByUser>` | :heavy_minus_sign: | User who created this unit. Null if created by a station or system. |
| `created_by_station` | `NullableField<UnitListCreatedByStation>` | :heavy_minus_sign: | Station that created this unit. Null if created by a user. |
| `batch` | `NullableField<UnitListBatch>` | :heavy_minus_sign: | Production batch this unit belongs to. Null if not part of a batch. |
| `parent` | `NullableField<UnitListParent>` | :heavy_minus_sign: | Parent unit in the assembly hierarchy. Null if this is a top-level unit. |
| `children` | `Vec<UnitListChildren>` | :heavy_check_mark: | Child units assembled into this unit. Limited to 10 results; use Get Unit endpoint for complete list. |
| `part` | `UnitListPart` | :heavy_check_mark: | Part information with the specific revision this unit is built from. |
| `last_run` | `NullableField<UnitListLastRun>` | :heavy_minus_sign: | Most recent test run performed on this unit. Null if no runs have been executed. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata key/value pairs on the unit. Only present when the request sets `include_metadata=true`. |
