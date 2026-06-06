# MeasurementGetAggregationsValidators

Validator result with outcome and comparison details.

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `outcome` | `Outcome` | :heavy_check_mark: | Validation result: PASS, FAIL, or UNSET. |
| `operator` | `Option<String>` | :heavy_minus_sign: | Comparison operator used for validation. |
| `expected_value` | `Option<serde_json::Value>` | :heavy_minus_sign: | Expected value for comparison. Type depends on measurement type. |
| `expression` | `String` | :heavy_check_mark: | Human-readable expression string for display. |
| `is_decisive` | `Option<bool>` | :heavy_minus_sign: | Whether this validator is decisive (if it fails, measurement fails). False for marginal/warning validators. |
| `is_expression_only` | `bool` | :heavy_check_mark: | True if validator only has expression (no structured operator/expected_value). |
| `analytics_expression` | `Option<String>` | :heavy_minus_sign: | Synthetic expression from operator+expected_value for analytics tooltip. Null if expression-only. |
| `has_custom_expression` | `bool` | :heavy_check_mark: | True if user provided a custom expression (shown in italic with analytics tooltip). |
