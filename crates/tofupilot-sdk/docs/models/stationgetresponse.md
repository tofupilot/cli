# StationGetResponse

Station retrieved successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier of the station |
| `name` | `String` | :heavy_check_mark: | Name of the station |
| `api_key` | `Option<String>` | :heavy_minus_sign: | API key prefix for the station (full key only shown on creation) |
| `procedures` | `Vec<StationGetProcedures>` | :heavy_check_mark: | Procedures linked to this station with recent run counts |
| `organization_slug` | `String` | :heavy_check_mark: | Slug of the organization this station belongs to |
| `team` | `Option<StationGetTeam>` | :heavy_minus_sign: | Team this station is assigned to |
