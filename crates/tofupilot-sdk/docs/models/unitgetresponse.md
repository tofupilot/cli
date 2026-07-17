# UnitGetResponse

Units retrieved successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the unit. |
| `serial_number` | `String` | :heavy_check_mark: | Unit serial number. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the unit was created. |
| `sample` | `Option<Sample>` | :heavy_minus_sign: | Reference-sample classification. 'golden' = known-good reference, 'failing' = known-faulty reference, 'ignored' = bench-check unit excluded from analytics and alerts, null = production unit. |
| `created_by_user` | `NullableField<UnitGetCreatedByUser>` | :heavy_minus_sign: | User who created this unit. |
| `created_by_station` | `NullableField<UnitGetCreatedByStation>` | :heavy_minus_sign: | Station that created this unit. |
| `part` | `UnitGetPart` | :heavy_check_mark: | Part information with revision details for this unit. Every unit must have a part and revision. |
| `batch` | `NullableField<UnitGetBatch>` | :heavy_minus_sign: | Batch information for this unit. |
| `parent` | `Option<UnitGetParent>` | :heavy_minus_sign: | Parent unit information with part details and processed images. |
| `children` | `Option<Vec<UnitGetChildren>>` | :heavy_minus_sign: | Child units with part details. |
| `created_during` | `NullableField<UnitGetCreatedDuring>` | :heavy_minus_sign: | Run that created this unit. |
| `attachments` | `Option<Vec<UnitGetAttachments>>` | :heavy_minus_sign: | Files attached to this unit. |
| `metadata` | `Option<std::collections::HashMap<String, serde_json::Value>>` | :heavy_minus_sign: | Custom metadata key/value pairs on the unit. |
