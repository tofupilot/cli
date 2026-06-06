# DeploymentGetCommit

Git commit the deployment was built from. Null for non-git deployments.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | N/A |
| `sha` | `String` | :heavy_check_mark: | N/A |
| `message` | `String` | :heavy_check_mark: | N/A |
| `author_username` | `String` | :heavy_check_mark: | N/A |
| `author_avatar_url` | `Option<String>` | :heavy_minus_sign: | N/A |
| `committed_at` | `Option<chrono::DateTime<chrono::Utc>>` | :heavy_minus_sign: | N/A |
| `branch` | `Option<DeploymentGetBranch>` | :heavy_minus_sign: | N/A |
| `repository` | `Option<DeploymentGetRepository>` | :heavy_minus_sign: | N/A |
