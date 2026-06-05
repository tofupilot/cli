# StationListData


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier of the station |
| `name` | `String` | :heavy_check_mark: | Name of the station |
| `procedures` | `Vec<StationListProcedures>` | :heavy_check_mark: | Procedures linked to this station |
| `procedures_count` | `f64` | :heavy_check_mark: | Total number of procedures linked to this station |
| `team` | `Option<StationListTeam>` | :heavy_minus_sign: | Team this station belongs to |
