//! Build a `UnitInfo` from an operator response or from configured
//! defaults.
//!
//! The two entry points are pure: no I/O, no UI. Components emit
//! string values today; if we add numeric fields later, the response
//! type will need to grow past `HashMap<String, String>`.

use std::collections::HashMap;

use crate::procedure::UnitConfig;
use crate::unit::{validate_unit_info, UnitInfo};

/// Parse the operator's response and validate it against the unit
/// config. Sub-unit values are routed via the `sub_unit:<key>` prefix
/// convention emitted on the `IdentifyRequest` wire event.
///
/// Empty / whitespace-only values are dropped — operator-UI sends back
/// every field, but a blank entry is "operator left it empty," not a
/// real value. `validate_unit_info` then rejects missing required
/// fields with a clear error.
pub fn resolve_response(
    cfg: &UnitConfig,
    values: HashMap<String, String>,
) -> Result<UnitInfo, String> {
    let trim = |s: String| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };

    let mut serial_number = None;
    let mut part_number = None;
    let mut revision_number = None;
    let mut batch_number = None;
    let mut sub_units: HashMap<String, String> = HashMap::new();

    for (key, raw) in values {
        if let Some(sub_key) = key.strip_prefix("sub_unit:") {
            if let Some(val) = trim(raw) {
                sub_units.insert(sub_key.to_string(), val);
            }
        } else {
            match key.as_str() {
                "serial_number" => serial_number = trim(raw),
                "part_number" => part_number = trim(raw),
                "revision_number" => revision_number = trim(raw),
                "batch_number" => batch_number = trim(raw),
                // Unknown keys ignored to keep the wire forward-compatible.
                _ => {}
            }
        }
    }

    let unit_info = UnitInfo {
        serial_number,
        part_number,
        revision_number,
        batch_number,
        sub_units: if sub_units.is_empty() {
            None
        } else {
            Some(sub_units)
        },
        status: "complete".to_string(),
    };

    validate_unit_info(&unit_info, &Some(cfg.clone()))?;
    Ok(unit_info)
}

/// Build a `UnitInfo` from `default_value` fields when
/// `auto_identify: true`. Re-runs `validate_auto_identify` as a defense-
/// in-depth check; the procedure loader already enforces this at load
/// time, but a stray code path that constructs a `UnitConfig`
/// programmatically deserves the same guarantee.
pub fn auto_identify_unit_info(cfg: &UnitConfig) -> Result<UnitInfo, String> {
    cfg.validate_auto_identify()?;

    let sub_units = cfg.sub_units.as_ref().and_then(|sub| {
        let map: HashMap<String, String> = sub
            .0
            .iter()
            .filter_map(|item| {
                item.serial_number
                    .as_ref()
                    .and_then(|f| f.default_value.clone())
                    .map(|val| (item.get_key(), val))
            })
            .collect();
        if map.is_empty() {
            None
        } else {
            Some(map)
        }
    });

    let unit_info = UnitInfo {
        serial_number: cfg
            .serial_number
            .as_ref()
            .and_then(|f| f.default_value.clone()),
        part_number: cfg
            .part_number
            .as_ref()
            .and_then(|f| f.default_value.clone()),
        revision_number: cfg
            .revision_number
            .as_ref()
            .and_then(|f| f.default_value.clone()),
        batch_number: cfg
            .batch_number
            .as_ref()
            .and_then(|f| f.default_value.clone()),
        sub_units,
        status: "complete".to_string(),
    };

    validate_unit_info(&unit_info, &Some(cfg.clone()))?;
    Ok(unit_info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procedure::{SubUnitItemConfig, SubUnitsConfig, UnitConfig, UnitFieldConfig};

    fn cfg_minimal() -> UnitConfig {
        UnitConfig {
            auto_identify: false,
            serial_number: Some(UnitFieldConfig::default()),
            part_number: Some(UnitFieldConfig::default()),
            revision_number: None,
            batch_number: None,
            sub_units: None,
        }
    }

    fn cfg_with_optional_and_sub_units() -> UnitConfig {
        UnitConfig {
            auto_identify: false,
            serial_number: Some(UnitFieldConfig::default()),
            part_number: Some(UnitFieldConfig::default()),
            revision_number: Some(UnitFieldConfig::default()),
            batch_number: Some(UnitFieldConfig::default()),
            sub_units: Some(SubUnitsConfig(vec![
                SubUnitItemConfig {
                    label: "Battery".to_string(),
                    key: Some("battery".to_string()),
                    serial_number: None,
                },
                SubUnitItemConfig {
                    label: "Motor".to_string(),
                    key: Some("motor".to_string()),
                    serial_number: None,
                },
            ])),
        }
    }

    #[test]
    fn resolve_response_required_fields() {
        let cfg = cfg_minimal();
        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "SN-1".to_string());
        values.insert("part_number".to_string(), "PCB".to_string());

        let info = resolve_response(&cfg, values).unwrap();
        assert_eq!(info.serial_number.as_deref(), Some("SN-1"));
        assert_eq!(info.part_number.as_deref(), Some("PCB"));
        assert!(info.revision_number.is_none());
        assert!(info.batch_number.is_none());
        assert!(info.sub_units.is_none());
    }

    #[test]
    fn resolve_response_trims_and_drops_blanks() {
        let cfg = cfg_minimal();
        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "  SN-2  ".to_string());
        values.insert("part_number".to_string(), "PCB".to_string());
        values.insert("revision_number".to_string(), "   ".to_string());

        let info = resolve_response(&cfg, values).unwrap();
        assert_eq!(info.serial_number.as_deref(), Some("SN-2"));
        assert!(info.revision_number.is_none());
    }

    #[test]
    fn resolve_response_routes_sub_unit_prefix() {
        let cfg = cfg_with_optional_and_sub_units();
        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "SN".to_string());
        values.insert("part_number".to_string(), "PN".to_string());
        values.insert("sub_unit:battery".to_string(), "BAT-001".to_string());
        values.insert("sub_unit:motor".to_string(), "MOT-002".to_string());

        let info = resolve_response(&cfg, values).unwrap();
        let sub = info.sub_units.expect("sub_units populated");
        assert_eq!(sub.get("battery").map(String::as_str), Some("BAT-001"));
        assert_eq!(sub.get("motor").map(String::as_str), Some("MOT-002"));
    }

    #[test]
    fn resolve_response_ignores_unknown_keys() {
        let cfg = cfg_minimal();
        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "SN".to_string());
        values.insert("part_number".to_string(), "PN".to_string());
        values.insert("future_field".to_string(), "ignored".to_string());

        assert!(resolve_response(&cfg, values).is_ok());
    }

    #[test]
    fn resolve_response_validation_error_on_missing_required() {
        let cfg = cfg_minimal();
        let values = HashMap::new();
        let err = resolve_response(&cfg, values).unwrap_err();
        assert!(err.to_lowercase().contains("required"));
    }

    #[test]
    fn resolve_response_rejects_whitespace_only_serial() {
        // Operator submits a blank serial; we trim → drop → validation
        // rejects with the snake_case "required" message. Closes the
        // gap left by the prior `validate_unit_info` bug where display
        // labels skipped the required-field guard.
        let cfg = cfg_minimal();
        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "   ".to_string());
        values.insert("part_number".to_string(), "PCB".to_string());
        let err = resolve_response(&cfg, values).unwrap_err();
        assert!(
            err.contains("serial_number") && err.to_lowercase().contains("required"),
            "expected required-serial error, got: {err}"
        );
    }

    #[test]
    fn auto_identify_builds_from_defaults() {
        let cfg = UnitConfig {
            auto_identify: true,
            serial_number: Some(UnitFieldConfig {
                default_value: Some("SN-AUTO".to_string()),
                ..Default::default()
            }),
            part_number: Some(UnitFieldConfig {
                default_value: Some("PCB-AUTO".to_string()),
                ..Default::default()
            }),
            revision_number: Some(UnitFieldConfig {
                default_value: Some("A".to_string()),
                ..Default::default()
            }),
            batch_number: None,
            sub_units: Some(SubUnitsConfig(vec![SubUnitItemConfig {
                label: "Battery".to_string(),
                key: Some("battery".to_string()),
                serial_number: Some(UnitFieldConfig {
                    default_value: Some("BAT-AUTO".to_string()),
                    ..Default::default()
                }),
            }])),
        };
        let info = auto_identify_unit_info(&cfg).unwrap();
        assert_eq!(info.serial_number.as_deref(), Some("SN-AUTO"));
        assert_eq!(info.part_number.as_deref(), Some("PCB-AUTO"));
        assert_eq!(info.revision_number.as_deref(), Some("A"));
        let sub = info.sub_units.expect("sub_units populated");
        assert_eq!(sub.get("battery").map(String::as_str), Some("BAT-AUTO"));
    }

    #[test]
    fn auto_identify_rejects_missing_default() {
        let cfg = UnitConfig {
            auto_identify: true,
            serial_number: Some(UnitFieldConfig::default()),
            part_number: Some(UnitFieldConfig {
                default_value: Some("PCB-AUTO".to_string()),
                ..Default::default()
            }),
            revision_number: None,
            batch_number: None,
            sub_units: None,
        };
        let err = auto_identify_unit_info(&cfg).unwrap_err();
        assert!(err.contains("serial_number.default_value"));
    }

    #[test]
    fn auto_identify_rejects_sub_unit_missing_default() {
        let cfg = UnitConfig {
            auto_identify: true,
            serial_number: Some(UnitFieldConfig {
                default_value: Some("SN".to_string()),
                ..Default::default()
            }),
            part_number: Some(UnitFieldConfig {
                default_value: Some("PCB".to_string()),
                ..Default::default()
            }),
            revision_number: None,
            batch_number: None,
            sub_units: Some(SubUnitsConfig(vec![
                SubUnitItemConfig {
                    label: "Battery".to_string(),
                    key: Some("battery".to_string()),
                    serial_number: Some(UnitFieldConfig {
                        default_value: Some("BAT-001".to_string()),
                        ..Default::default()
                    }),
                },
                SubUnitItemConfig {
                    label: "Motor".to_string(),
                    key: Some("motor".to_string()),
                    // No default — should be rejected upfront with a
                    // message naming "motor", not the opaque
                    // "expected 2 sub-units got 1" from
                    // validate_unit_info.
                    serial_number: None,
                },
            ])),
        };
        let err = auto_identify_unit_info(&cfg).unwrap_err();
        assert!(
            err.contains("sub_units.motor.serial_number.default_value"),
            "expected motor-named error, got: {err}"
        );
    }
}
