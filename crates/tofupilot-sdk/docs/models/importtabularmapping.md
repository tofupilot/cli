# ImportTabularMapping

Inline column mapping describing how source columns feed TofuPilot fields. Provide this OR template_id, not both.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `fields` | `ImportTabularFields` | :heavy_check_mark: | N/A |
| `measurements` | `serde_json::Value` | :heavy_check_mark: | N/A |
| `phases` | `Option<ImportTabularPhases>` | :heavy_minus_sign: | N/A |
| `metadata` | `Option<Vec<ImportTabularMetadata>>` | :heavy_minus_sign: | N/A |
| `unit_metadata` | `Option<Vec<ImportTabularUnitMetadata>>` | :heavy_minus_sign: | N/A |
