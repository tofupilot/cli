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
    let mut metadata: HashMap<String, String> = HashMap::new();

    for (key, raw) in values {
        if let Some(sub_key) = key.strip_prefix("sub_unit:") {
            if let Some(val) = trim(raw) {
                sub_units.insert(sub_key.to_string(), val);
            }
        } else if let Some(md_key) = key.strip_prefix("metadata:") {
            // Only keys declared in the unit config are accepted —
            // undeclared ones are ignored like any unknown wire key.
            let declared = cfg
                .metadata
                .as_ref()
                .is_some_and(|md| md.contains_key(md_key));
            if declared {
                if let Some(val) = trim(raw) {
                    metadata.insert(md_key.to_string(), val);
                }
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
        metadata: if metadata.is_empty() {
            None
        } else {
            Some(metadata)
        },
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

    // Metadata fields with a default_value resolve like other fields;
    // default-less fields are simply absent (metadata is never required,
    // so auto_identify imposes no new requirements).
    let metadata = cfg.metadata.as_ref().and_then(|md| {
        let map: HashMap<String, String> = md
            .iter()
            .filter_map(|(key, f)| f.default_value.clone().map(|val| (key.clone(), val)))
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
        metadata,
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
            metadata: None,
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
            metadata: None,
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
            metadata: None,
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
            metadata: None,
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
            metadata: None,
        };
        let err = auto_identify_unit_info(&cfg).unwrap_err();
        assert!(
            err.contains("sub_units.motor.serial_number.default_value"),
            "expected motor-named error, got: {err}"
        );
    }

    fn cfg_with_metadata() -> UnitConfig {
        let mut cfg = cfg_minimal();
        let mut md = std::collections::BTreeMap::new();
        md.insert(
            "modification".to_string(),
            UnitFieldConfig {
                pattern: Some("^MOD-[0-9]+$".to_string()),
                ..Default::default()
            },
        );
        md.insert("amendment".to_string(), UnitFieldConfig::default());
        cfg.metadata = Some(md);
        cfg
    }

    #[test]
    fn resolve_response_routes_metadata_prefix() {
        let cfg = cfg_with_metadata();
        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "SN-1".to_string());
        values.insert("part_number".to_string(), "PCB".to_string());
        values.insert("metadata:modification".to_string(), " MOD-42 ".to_string());
        values.insert("metadata:amendment".to_string(), "".to_string()); // blank → absent
        values.insert("metadata:undeclared".to_string(), "x".to_string()); // ignored

        let info = resolve_response(&cfg, values).unwrap();
        let md = info.metadata.expect("metadata populated");
        assert_eq!(md.get("modification").map(String::as_str), Some("MOD-42"));
        assert!(!md.contains_key("amendment"));
        assert!(!md.contains_key("undeclared"));
    }

    #[test]
    fn resolve_response_metadata_pattern_violation_errors() {
        let cfg = cfg_with_metadata();
        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "SN-1".to_string());
        values.insert("part_number".to_string(), "PCB".to_string());
        values.insert("metadata:modification".to_string(), "BAD-1".to_string());

        let err = resolve_response(&cfg, values).unwrap_err();
        assert!(err.contains("modification"), "got: {err}");
    }

    #[test]
    fn resolve_response_all_metadata_blank_is_none() {
        let cfg = cfg_with_metadata();
        let mut values = HashMap::new();
        values.insert("serial_number".to_string(), "SN-1".to_string());
        values.insert("part_number".to_string(), "PCB".to_string());
        values.insert("metadata:modification".to_string(), "  ".to_string());

        let info = resolve_response(&cfg, values).unwrap();
        assert!(info.metadata.is_none());
    }

    #[test]
    fn auto_identify_resolves_metadata_defaults() {
        let mut cfg = UnitConfig {
            auto_identify: true,
            serial_number: Some(UnitFieldConfig {
                default_value: Some("SN-AUTO".to_string()),
                ..Default::default()
            }),
            part_number: Some(UnitFieldConfig {
                default_value: Some("PCB-AUTO".to_string()),
                ..Default::default()
            }),
            revision_number: None,
            batch_number: None,
            sub_units: None,
            metadata: None,
        };
        let mut md = std::collections::BTreeMap::new();
        md.insert(
            "modification".to_string(),
            UnitFieldConfig {
                default_value: Some("MOD-42".to_string()),
                ..Default::default()
            },
        );
        // No default — absent, not an error (metadata never required)
        md.insert("amendment".to_string(), UnitFieldConfig::default());
        cfg.metadata = Some(md);

        let info = auto_identify_unit_info(&cfg).unwrap();
        let resolved = info.metadata.expect("metadata from defaults");
        assert_eq!(
            resolved.get("modification").map(String::as_str),
            Some("MOD-42")
        );
        assert!(!resolved.contains_key("amendment"));
    }
}
