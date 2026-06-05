# UnitGetParent

Parent unit information with part details and processed images.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unit ID. |
| `serial_number` | `String` | :heavy_check_mark: | Unit serial number. |
| `part` | `Option<UnitGetParentPart>` | :heavy_minus_sign: | Part information for the parent unit. |
