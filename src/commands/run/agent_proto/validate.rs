//! Validates operator-supplied UI response values against the component spec
//! before they are accepted.

use std::collections::HashMap;

use execution_engine::ui::{ComponentType, ComponentValue, UiComponent};

use super::events::{CliEvent, UiErrorReason};

#[derive(Debug)]
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
            // An omitted optional input still records its declared
            // `default_value`, matching the web client which seeds every
            // component from `getComponentDefaultValue` before submit. A
            // bound component with a default would otherwise record no
            // measurement on the agent path (and a downstream phase
            // reading it would hit the AttributeError this fix targets),
            // while the kiosk records the default.
            None => {
                if let Some(default) = default_value_string(component) {
                    out.insert(component.key.clone(), default);
                }
            }
        }
    }

    // Pack `bind:` components into the `__bound_measurements__` sentinel
    // so an agent-driven run records the same measurements as the kiosk
    // and TUI. The engine reads only this sentinel; a bare coerced value
    // is ignored. Resolve each bind from the already-coerced `out` map so
    // numbers/switches reuse the validated string form. Shared with the
    // TUI via `execution_engine::ui::build_bound_measurements_payload`.
    if let Some(bound) =
        execution_engine::ui::build_bound_measurements_payload(components, |comp| {
            out.get(&comp.key).cloned()
        })
    {
        out.insert("__bound_measurements__".to_string(), bound);
    }

    Ok(out)
}

/// The declared `default_value` of an input component as a wire string,
/// or `None` when there is no default. Mirrors the string forms the TUI
/// (`default_string`) and web client produce so an omitted optional input
/// records the same value across surfaces. Array defaults join with `,`
/// (multiselect/checklist CSV shape).
fn default_value_string(component: &UiComponent) -> Option<String> {
    match component.default_value.as_ref()? {
        ComponentValue::String(s) if !s.is_empty() => Some(s.clone()),
        ComponentValue::String(_) => None,
        ComponentValue::Number(n) => Some(n.to_string()),
        ComponentValue::Boolean(b) => Some(b.to_string()),
        ComponentValue::Array(a) if !a.is_empty() => Some(a.join(",")),
        ComponentValue::Array(_) => None,
    }
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
        ComponentType::Multiselect | ComponentType::Checklist => {
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
                // Honor the `trim` flag (defaults to true) before any
                // length/pattern check or recording, matching the web
                // client (`run-state.ts` coerceComponentValueToWireString)
                // and the TUI (`to_response`). Without this a pasted value
                // with stray whitespace is recorded differently per surface
                // and slips past a `pattern` validator on the agent path.
                let trimmed = if component.trim { s.trim() } else { s.as_str() };

                // A required text field shouldn't silently accept "".
                // A pure-whitespace string is also rejected when required,
                // matching common UX expectations; non-required fields are
                // passed through unchanged.
                if component.required && trimmed.is_empty() {
                    return Err(ValidationError {
                        reason: UiErrorReason::InvalidValue,
                        field: Some(component.key.clone()),
                        got: Some(value.clone()),
                        expected: Some("non-empty text".into()),
                    });
                }
                // Length + pattern validators — enforced by the web
                // renderer and the TUI (`validate_one`); the agent path
                // previously skipped them, so an agent/`--ui-values` run
                // could store a value the interactive surfaces reject.
                // Empty optional values are exempt (matches the TUI, which
                // returns early when the field is blank).
                if !trimmed.is_empty() {
                    let len = trimmed.chars().count();
                    if let Some(min) = component.min_length {
                        if len < min as usize {
                            return Err(ValidationError {
                                reason: UiErrorReason::InvalidValue,
                                field: Some(component.key.clone()),
                                got: Some(value.clone()),
                                expected: Some(format!("at least {min} characters")),
                            });
                        }
                    }
                    if let Some(max) = component.max_length {
                        if len > max as usize {
                            return Err(ValidationError {
                                reason: UiErrorReason::InvalidValue,
                                field: Some(component.key.clone()),
                                got: Some(value.clone()),
                                expected: Some(format!("at most {max} characters")),
                            });
                        }
                    }
                    if let Some(ref pattern) = component.pattern {
                        match regex::Regex::new(pattern) {
                            Ok(re) if !re.is_match(trimmed) => {
                                return Err(ValidationError {
                                    reason: UiErrorReason::InvalidValue,
                                    field: Some(component.key.clone()),
                                    got: Some(value.clone()),
                                    expected: Some(format!("match pattern: {pattern}")),
                                });
                            }
                            // Unparseable pattern: skip rather than reject,
                            // matching the TUI (`validate_one` swallows a
                            // bad regex). A malformed YAML pattern shouldn't
                            // hard-block a run on one surface only.
                            _ => {}
                        }
                    }
                }
                trimmed.to_string()
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
            ComponentType::Radio | ComponentType::Select => {
                if !allowed.contains(&as_string.as_str()) {
                    return Err(ValidationError {
                        reason: UiErrorReason::InvalidValue,
                        field: Some(component.key.clone()),
                        got: Some(value.clone()),
                        expected: Some(format!("one of: {}", allowed.join(", "))),
                    });
                }
            }
            ComponentType::Multiselect | ComponentType::Checklist => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use execution_engine::ui::{ComponentType, UiComponent, UiOption};

    fn radio(key: &str, bind: &str) -> UiComponent {
        UiComponent {
            key: key.into(),
            bind: Some(bind.into()),
            options: Some(vec![
                UiOption { label: "A".into(), value: "A".into(), image: None },
                UiOption { label: "B".into(), value: "B".into(), image: None },
            ]),
            ..UiComponent::new(ComponentType::Radio)
        }
    }

    #[test]
    fn agent_path_packs_bound_measurements() {
        // An agent-driven run must record the operator answer as a
        // measurement, not just pass the raw value through. The engine
        // reads only `__bound_measurements__`.
        let comps = vec![radio("motor_type_measure", "measurements.motor_type_measure")];
        let mut values = HashMap::new();
        values.insert(
            "motor_type_measure".to_string(),
            serde_json::Value::String("A".to_string()),
        );
        let out = validate_and_coerce(&comps, values).expect("valid");
        let bound = out
            .get("__bound_measurements__")
            .expect("__bound_measurements__ packed");
        let v: serde_json::Value = serde_json::from_str(bound).unwrap();
        assert_eq!(v["motor_type_measure"], "A");
        // Raw coerced value still present alongside the sentinel.
        assert_eq!(out.get("motor_type_measure").map(String::as_str), Some("A"));
    }

    #[test]
    fn agent_path_no_sentinel_without_bind() {
        let comp = UiComponent {
            key: "free".into(),
            required: false,
            bind: None,
            ..UiComponent::new(ComponentType::TextInput)
        };
        let mut values = HashMap::new();
        values.insert("free".to_string(), serde_json::Value::String("x".into()));
        let out = validate_and_coerce(&[comp], values).expect("valid");
        assert!(!out.contains_key("__bound_measurements__"));
    }

    fn text(key: &str) -> UiComponent {
        UiComponent {
            key: key.into(),
            bind: Some(format!("measurements.{key}")),
            ..UiComponent::new(ComponentType::TextInput)
        }
    }

    #[test]
    fn agent_path_trims_text_like_other_surfaces() {
        // trim defaults to true; the recorded measurement must match the
        // kiosk/TUI (trimmed), not the raw padded input.
        let comps = vec![text("serial")];
        let mut values = HashMap::new();
        values.insert("serial".to_string(), serde_json::Value::String("  SN-1  ".into()));
        let out = validate_and_coerce(&comps, values).expect("valid");
        assert_eq!(out.get("serial").map(String::as_str), Some("SN-1"));
        let v: serde_json::Value =
            serde_json::from_str(out.get("__bound_measurements__").unwrap()).unwrap();
        assert_eq!(v["serial"], "SN-1");
    }

    #[test]
    fn agent_path_enforces_pattern() {
        let comp = UiComponent {
            pattern: Some("^[A-Z0-9-]+$".into()),
            ..text("serial")
        };
        let mut values = HashMap::new();
        // lowercase violates the pattern — must be rejected like the TUI.
        values.insert("serial".to_string(), serde_json::Value::String("sn 1".into()));
        let err = validate_and_coerce(&[comp], values).expect_err("pattern violation rejected");
        assert!(matches!(err.reason, UiErrorReason::InvalidValue));
    }

    #[test]
    fn agent_path_enforces_max_length() {
        let comp = UiComponent {
            max_length: Some(3),
            ..text("code")
        };
        let mut values = HashMap::new();
        values.insert("code".to_string(), serde_json::Value::String("ABCD".into()));
        let err = validate_and_coerce(&[comp], values).expect_err("too long rejected");
        assert!(matches!(err.reason, UiErrorReason::InvalidValue));
    }

    #[test]
    fn agent_path_seeds_omitted_optional_default() {
        // An optional bound input the agent omits still records its
        // declared default_value, matching the kiosk.
        let comp = UiComponent {
            required: false,
            default_value: Some(ComponentValue::String("DefaultMotor".into())),
            ..text("motor")
        };
        let out = validate_and_coerce(&[comp], HashMap::new()).expect("valid");
        assert_eq!(out.get("motor").map(String::as_str), Some("DefaultMotor"));
        let v: serde_json::Value =
            serde_json::from_str(out.get("__bound_measurements__").unwrap()).unwrap();
        assert_eq!(v["motor"], "DefaultMotor");
    }
}
