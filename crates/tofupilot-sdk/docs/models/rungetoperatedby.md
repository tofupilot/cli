# RunGetOperatedBy

Operator of this run: a linked organization member (id/email set) or a declared free-text name (id/email null). Only returned if `all` or `operated_by` is included.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `Option<String>` | :heavy_minus_sign: | Operator user ID. Null when the operator is a declared name without a TofuPilot account. |
| `name` | `Option<String>` | :heavy_minus_sign: | Operator display name: the account name for linked operators, the declared free-text value otherwise. |
| `email` | `Option<String>` | :heavy_minus_sign: | Operator email address. Null for declared names (unverified operators have no account email). |
