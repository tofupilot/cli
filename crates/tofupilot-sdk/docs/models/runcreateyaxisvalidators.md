# RunCreateYAxisValidators

Structured validator specification with operator and expected value.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `outcome` | `NullableField<Outcome>` | :heavy_minus_sign: | Pre-computed validation result from test framework. Server stores as-is, does not re-evaluate. |
| `operator` | `NullableField<String>` | :heavy_minus_sign: | Comparison operator: ">", ">=", "<", "<=", "==", "!=", "matches", "in", "range" |
| `expected_value` | `NullableField<serde_json::Value>` | :heavy_minus_sign: | Expected value for comparison. Type depends on operator. |
| `expression` | `NullableField<String>` | :heavy_minus_sign: | Original expression string for display/audit purposes. |
| `is_decisive` | `NullableField<bool>` | :heavy_minus_sign: | Whether this validator is decisive (if it fails, measurement fails). False for marginal/warning validators. Defaults to true. |
