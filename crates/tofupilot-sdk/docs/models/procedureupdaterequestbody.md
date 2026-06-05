# ProcedureUpdateRequestBody


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `Option<String>` | :heavy_minus_sign: | New name for the procedure. |
| `production_branch` | `NullableField<String>` | :heavy_minus_sign: | Branch treated as production. Pushes to this branch deploy as production; every other branch deploys as preview. Null = no branch promoted to production. |
| `auto_push_enabled` | `Option<bool>` | :heavy_minus_sign: | Master switch for auto-pushing builds to linked stations. Build artifacts are always recorded; this only gates the station fan-out. |
| `excluded_branch_patterns` | `Option<Vec<String>>` | :heavy_minus_sign: | Branches matching any of these patterns (exact name or minimatch glob, e.g. "renovate/*") skip preview deployments. Empty array = no exclusions. |
| `root_directory` | `NullableField<String>` | :heavy_minus_sign: | Path within the linked repo to the directory holding this procedure's `pyproject.toml` (and `procedure.yaml` for framework procedures). Empty/null = repo root. |
| `entry_point` | `NullableField<String>` | :heavy_minus_sign: | Entry-point path inside the procedure's package dir, relative to it. Forwarded to the CLI through the deployment manifest. Empty/null = use the framework default (openhtf/plain → main.py, pytest → ".", yaml → procedure.yaml auto-discovery). |
