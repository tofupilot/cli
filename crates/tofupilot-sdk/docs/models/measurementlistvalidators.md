# MeasurementListValidators


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `outcome` | `Outcome` | :heavy_check_mark: | Outcome of this validator against the measured value. |
| `operator` | `Option<String>` | :heavy_minus_sign: | Comparison operator (e.g. ">", "<=", "=="). Null for expression-only validators. |
| `expected_value` | `Option<serde_json::Value>` | :heavy_minus_sign: | Expected value the operator compares against. |
| `expression` | `String` | :heavy_check_mark: | Human-readable validator expression. |
| `is_decisive` | `Option<bool>` | :heavy_minus_sign: | Whether this validator drove the measurement outcome. Null if unknown. |
| `is_expression_only` | `bool` | :heavy_check_mark: | True when the validator is defined purely by a custom expression. |
| `analytics_expression` | `Option<String>` | :heavy_minus_sign: | Synthetic operator+value expression for analytics. Null if expression-only. |
| `has_custom_expression` | `bool` | :heavy_check_mark: | True when the user supplied a custom validator expression. |
