//! Validates operator-supplied UI response values against the component spec
//! before they are accepted.

use std::collections::HashMap;

use execution_engine::ui::{ComponentType, UiComponent};

use super::events::{CliEvent, UiErrorReason};

pub struct ValidationError {
    pub reason: UiErrorReason,
    pub field: Option<String>,
    pub got: Option<serde_json::Value>,
    pub expected: Option<String>,
}

impl ValidationError {
    pub fn into_event(self, request_id: &str) -> CliEvent {
        CliEvent::UiError {
            request_id: Some(request_id.to_string()),
            reason: self.reason,
            field: self.field,
            got: self.got,
            expected: self.expected,
        }
    }
}

/// Validate agent-submitted values against the component spec and coerce to
/// the engine's `HashMap<String, String>` shape. The executor stores all UI
/// responses as strings today (see `UI_RESPONSE_CHANNELS`), so numbers and
/// booleans are stringified.
pub fn validate_and_coerce(
    components: &[UiComponent],
    values: HashMap<String, serde_json::Value>,
) -> Result<HashMap<String, String>, ValidationError> {
    let known: HashMap<&str, &UiComponent> =
        components.iter().map(|c| (c.key.as_str(), c)).collect();

    for key in values.keys() {
        if !known.contains_key(key.as_str()) {
            return Err(ValidationError {
                reason: UiErrorReason::UnknownField,
                field: Some(key.clone()),
                got: None,
                expected: None,
            });
        }
    }

    let mut out = HashMap::new();
    for component in components {
        if !component.is_input {
            continue;
        }
        match values.get(&component.key) {
            Some(v) => {
                out.insert(component.key.clone(), coerce_value(component, v)?);
            }
            None if component.required => {
                return Err(ValidationError {
                    reason: UiErrorReason::MissingRequired,
                    field: Some(component.key.clone()),
                    got: None,
                    expected: Some(expected_summary(component)),
                });
            }
            None => {}
        }
    }

    Ok(out)
}

fn coerce_value(
    component: &UiComponent,
    value: &serde_json::Value,
) -> Result<String, ValidationError> {
    let field = Some(component.key.clone());
    let expected = Some(expected_summary(component));

    let as_string = match &component.component_type {
        ComponentType::Switch => match value {
            serde_json::Value::Bool(b) => b.to_string(),
            // Strict lowercase string form. "True"/"TRUE"/"1"/"yes" rejected
            // — document explicitly in the protocol spec.
            serde_json::Value::String(s) if s == "true" || s == "false" => s.clone(),
            _ => {
                return Err(ValidationError {
                    reason: UiErrorReason::InvalidValue,
                    field,
                    got: Some(value.clone()),
                    expected: Some("switch: bool or \"true\"/\"false\" (lowercase)".into()),
                })
            }
        },
        ComponentType::NumberInput | ComponentType::Slider => {
            let parsed: f64 = match value {
                serde_json::Value::Number(n) => match n.as_f64() {
                    Some(f) => f,
                    None => {
                        return Err(ValidationError {
                            reason: UiErrorReason::InvalidValue,
                            field,
                            got: Some(value.clone()),
                            expected,
                        })
                    }
                },
                serde_json::Value::String(s) => match s.parse::<f64>() {
                    Ok(f) => f,
                    Err(_) => {
                        return Err(ValidationError {
                            reason: UiErrorReason::InvalidValue,
                            field,
                            got: Some(value.clone()),
                            expected,
                        })
                    }
                },
                _ => {
                    return Err(ValidationError {
                        reason: UiErrorReason::InvalidValue,
                        field,
                        got: Some(value.clone()),
                        expected,
                    })
                }
            };
            if !parsed.is_finite() {
                return Err(ValidationError {
                    reason: UiErrorReason::InvalidValue,
                    field: Some(component.key.clone()),
                    got: Some(value.clone()),
                    expected: Some("finite number (not NaN/Infinity)".to_string()),
                });
            }
            if let Some(min) = component.min {
                if parsed < min {
                    return Err(ValidationError {
                        reason: UiErrorReason::InvalidValue,
                        field: Some(component.key.clone()),
                        got: Some(value.clone()),
                        expected: Some(format!(">= {min}")),
                    });
                }
            }
            if let Some(max) = component.max {
                if parsed > max {
                    return Err(ValidationError {
                        reason: UiErrorReason::InvalidValue,
                        field: Some(component.key.clone()),
                        got: Some(value.clone()),
                        expected: Some(format!("<= {max}")),
                    });
                }
            }
            // Preserve original string form when provided so agents get the
            // same shape back (e.g. "3.14" not "3.14" vs "3.140000...").
            match value {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s.clone(),
                _ => parsed.to_string(),
            }
        }
        ComponentType::Multiselect | ComponentType::Checklist | ComponentType::ImageChecklist => {
            match value {
                serde_json::Value::Array(arr) => {
                    let parts: Result<Vec<String>, _> = arr
                        .iter()
                        .map(|v| match v {
                            serde_json::Value::String(s) => Ok(s.clone()),
                            _ => Err(()),
                        })
                        .collect();
                    match parts {
                        Ok(p) => {
                            // Ordered dedup — preserves the first occurrence
                            // order, drops later repeats. Phases don't
                            // expect `["a","a"]` to mean "a twice".
                            let mut seen = std::collections::HashSet::new();
                            let deduped: Vec<String> =
                                p.into_iter().filter(|x| seen.insert(x.clone())).collect();
                            deduped.join(",")
                        }
                        Err(_) => {
                            return Err(ValidationError {
                                reason: UiErrorReason::InvalidValue,
                                field,
                                got: Some(value.clone()),
                                expected,
                            })
                        }
                    }
                }
                serde_json::Value::String(s) => s.clone(),
                _ => {
                    return Err(ValidationError {
                        reason: UiErrorReason::InvalidValue,
                        field,
                        got: Some(value.clone()),
                        expected,
                    })
                }
            }
        }
        ComponentType::TextInput | ComponentType::Textarea => match value {
            serde_json::Value::String(s) => {
                // A required text field shouldn't silently accept "".
                // A pure-whitespace string is also rejected when required,
                // matching common UX expectations; non-required fields are
                // passed through unchanged.
                if component.required && s.trim().is_empty() {
                    return Err(ValidationError {
                        reason: UiErrorReason::InvalidValue,
                        field: Some(component.key.clone()),
                        got: Some(value.clone()),
                        expected: Some("non-empty text".into()),
                    });
                }
                s.clone()
            }
            _ => {
                return Err(ValidationError {
                    reason: UiErrorReason::InvalidValue,
                    field,
                    got: Some(value.clone()),
                    expected,
                })
            }
        },
        _ => match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => {
                return Err(ValidationError {
                    reason: UiErrorReason::InvalidValue,
                    field,
                    got: Some(value.clone()),
                    expected,
                })
            }
        },
    };

    if let Some(options) = component.options.as_ref() {
        let allowed: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
        match &component.component_type {
            ComponentType::Radio | ComponentType::Select | ComponentType::ImageChoice => {
                if !allowed.contains(&as_string.as_str()) {
                    return Err(ValidationError {
                        reason: UiErrorReason::InvalidValue,
                        field: Some(component.key.clone()),
                        got: Some(value.clone()),
                        expected: Some(format!("one of: {}", allowed.join(", "))),
                    });
                }
            }
            ComponentType::Multiselect
            | ComponentType::Checklist
            | ComponentType::ImageChecklist => {
                for part in as_string.split(',').filter(|p| !p.is_empty()) {
                    if !allowed.contains(&part) {
                        return Err(ValidationError {
                            reason: UiErrorReason::InvalidValue,
                            field: Some(component.key.clone()),
                            got: Some(value.clone()),
                            expected: Some(format!("subset of: {}", allowed.join(", "))),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    Ok(as_string)
}

fn expected_summary(component: &UiComponent) -> String {
    if let Some(options) = component.options.as_ref() {
        let vals: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
        return format!(
            "{} (one of: {})",
            component.component_type.as_str(),
            vals.join(", ")
        );
    }
    component.component_type.as_str().to_string()
}
