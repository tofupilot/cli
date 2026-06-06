# DeploymentGetResponse

Deployment retrieved successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the deployment. |
| `environment` | `Environment` | :heavy_check_mark: | Deployment environment. |
| `trigger` | `Trigger` | :heavy_check_mark: | How the deployment was triggered. |
| `status` | `DeploymentGetStatus` | :heavy_check_mark: | Build status of the deployment. |
| `deployed_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the deployment was created. |
| `started_at` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | ISO 8601 timestamp when the build started. Null if not started. |
| `ended_at` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | ISO 8601 timestamp when the build ended. Null if not ended. |
| `artifact_url` | `Option<String>` | :heavy_minus_sign: | URL of the built artifact. Null if not built. |
| `artifact_sha256` | `Option<String>` | :heavy_minus_sign: | SHA-256 checksum of the artifact. Null if not built. |
| `artifact_size_bytes` | `Option<f64>` | :heavy_minus_sign: | Size of the artifact in bytes. Null if not built. |
| `deployment_mode` | `DeploymentGetDeploymentMode` | :heavy_check_mark: | Deployment mode. |
| `platform` | `Option<String>` | :heavy_minus_sign: | Target platform. Null if unspecified. |
| `language` | `Option<String>` | :heavy_minus_sign: | Procedure language. Null if unspecified. |
| `runtime_version` | `Option<String>` | :heavy_minus_sign: | Runtime version. Null if unspecified. |
| `procedure` | `Option<DeploymentGetProcedure>` | :heavy_minus_sign: | Procedure this deployment targets. |
| `created_by_user` | `Option<DeploymentGetCreatedByUser>` | :heavy_minus_sign: | User who created the deployment. Null for system-created deployments. |
| `commit` | `Option<DeploymentGetCommit>` | :heavy_minus_sign: | Git commit the deployment was built from. Null for non-git deployments. |
| `stations` | `Vec<DeploymentGetStations>` | :heavy_check_mark: | Stations linked to the procedure, with per-station push state. pushed_at is null when this deployment has not been pushed to that station. |
| `build_logs` | `Vec<DeploymentGetBuildLogs>` | :heavy_check_mark: | Build log lines for this deployment, ordered by sequence. Empty if the build has not produced logs. |
