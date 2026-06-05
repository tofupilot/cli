# ProcedureListData


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the procedure. |
| `name` | `String` | :heavy_check_mark: | Name of the procedure. |
| `created_at` | `chrono::DateTime<chrono::Utc>` | :heavy_check_mark: | ISO 8601 timestamp when the procedure was created. |
| `created_by_user` | `ProcedureListCreatedByUser` | :heavy_check_mark: | User who created the procedure. |
| `linked_repository` | `Option<ProcedureListLinkedRepository>` | :heavy_minus_sign: | Linked repository for this procedure. |
