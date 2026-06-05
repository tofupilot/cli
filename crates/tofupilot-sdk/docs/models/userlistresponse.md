# UserListResponse


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier for the user. |
| `email` | `String` | :heavy_check_mark: | Email address of the user. |
| `name` | `Option<String>` | :heavy_minus_sign: | Display name of the user. |
| `image` | `Option<String>` | :heavy_minus_sign: | Profile image URL for the user. |
| `banned` | `bool` | :heavy_check_mark: | Whether the user is banned. |
