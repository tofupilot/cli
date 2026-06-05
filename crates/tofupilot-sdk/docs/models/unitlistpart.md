# UnitListPart

Part information with the specific revision this unit is built from.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the part. |
| `number` | `String` | :heavy_check_mark: | Part number. |
| `name` | `String` | :heavy_check_mark: | Human-readable part name. |
| `revision` | `UnitListRevision` | :heavy_check_mark: | Revision information for this part. Every unit must have a specific revision. |
