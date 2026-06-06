# DeploymentListData


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the deployment. |
| `environment` | `Environment` | :heavy_check_mark: | Deployment environment. |
| `trigger` | `Trigger` | :heavy_check_mark: | How the deployment was triggered. |
| `status` | `DeploymentGetStatus` | :heavy_check_mark: | Build status of the deployment. |
| `skipped` | `bool` | :heavy_check_mark: | True when an auto-push deployment was recorded for audit but never enqueued to build. |
| `started_at` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | ISO 8601 timestamp when the build started. Null if not started. |
| `ended_at` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | ISO 8601 timestamp when the build ended. Null if not ended. |
| `deployed_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the deployment was created. |
| `procedure` | `Option<DeploymentListProcedure>` | :heavy_minus_sign: | Procedure this deployment targets. |
| `commit` | `Option<DeploymentListCommit>` | :heavy_minus_sign: | Git commit the deployment was built from. Null for non-git deployments. |
| `repository` | `Option<DeploymentListRepository>` | :heavy_minus_sign: | Source repository provider. Null for non-git deployments. |
| `stations` | `Vec<DeploymentListStations>` | :heavy_check_mark: | Stations this deployment has been pushed to. |
