# ImportTabularRequest


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `upload_id` | `String` | :heavy_check_mark: | ID of a previously uploaded tabular file. |
| `procedure_id` | `String` | :heavy_check_mark: | Procedure to attach the imported run to. Always overrides any procedure referenced in the file. Create the procedure in the app first, then find the auto-generated ID on the procedure page. |
| `mapping` | `Option<ImportTabularMapping>` | :heavy_minus_sign: | Inline column mapping describing how source columns feed TofuPilot fields. Provide this OR template_id, not both. |
| `template_id` | `Option<String>` | :heavy_minus_sign: | ID of a saved mapping template to apply. Provide this OR mapping, not both. |
