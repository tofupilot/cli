# StationGetCurrentProcedures


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Procedure ID |
| `name` | `String` | :heavy_check_mark: | Procedure name |
| `runs_count` | `f64` | :heavy_check_mark: | Number of runs created by this station in the last 7 days |
| `deployment` | `NullableField<StationGetCurrentDeployment>` | :heavy_minus_sign: | Deployment information for this procedure on this station |
