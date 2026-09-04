//! Build the operator-UI prompt for an `identify_unit` step.
//!
//! Component shape is canonical: `serial_number` is always present,
//! `part_number` always present, optional `revision_number` /
//! `batch_number` / `operated_by` only when configured, and one
//! `sub_unit:<key>` text input per configured sub-unit. The wire-side `IdentifyRequest`
//! event carries this shape verbatim — operator-UI consumes the
//! event type directly (no heuristic), but the canonical names are
//! still the contract every consumer relies on for field rendering.
//!
//! `min_length` / `max_length` / `pattern` are forwarded to the UI for
//! client-side feedback; the authoritative validation runs server-side
//! in `crate::unit::validate_unit_info` against the same `UnitConfig`
//! after the operator submits. `prefix` / `suffix` are forwarded too:
//! the UI renders them as adornments around the input, the operator
//! types only the middle, and `resolve` validates that typed input
//! against the length/pattern constraints before composing
//! `prefix + input + suffix` as the stored value.

use crate::procedure::{SubUnitsConfig, UnitConfig, UnitFieldConfig};
use crate::ui::{ComponentType, ComponentValue, UiComponent};

/// Build the canonical identify-unit component list for a `UnitConfig`.
///
/// Precondition: `cfg.serial_number` and `cfg.part_number` are `Some`.
/// `crate::procedure::loader` enforces this at procedure-load time;
/// callers reaching this function with a malformed `UnitConfig` get an
/// `Err` rather than a silently-incomplete prompt that operator-UI
/// would reject anyway.
pub fn build_components(
    cfg: &UnitConfig,
    // Procedure-root `operated_by:` — a RUN property prompted on the
    // same identification screen as the unit fields.
    operated_by: Option<&UnitFieldConfig>,
) -> Result<Vec<UiComponent>, String> {
    let serial_cfg = cfg
        .serial_number
        .as_ref()
        .ok_or_else(|| "unit config missing serial_number".to_string())?;
    let part_cfg = cfg
        .part_number
        .as_ref()
        .ok_or_else(|| "unit config missing part_number".to_string())?;

    let mut components = Vec::new();
    components.push(unit_identity_component(
        "serial_number",
        "Serial Number",
        serial_cfg,
        true,
    ));
    components.push(unit_identity_component(
        "part_number",
        "Part Number",
        part_cfg,
        true,
    ));
    if let Some(rev_cfg) = cfg.revision_number.as_ref() {
        components.push(unit_identity_component(
            "revision_number",
            "Revision Number",
            rev_cfg,
            false,
        ));
    }
    if let Some(batch_cfg) = cfg.batch_number.as_ref() {
        components.push(unit_identity_component(
            "batch_number",
            "Batch Number",
            batch_cfg,
            false,
        ));
    }
    if let Some(operated_cfg) = operated_by {
        // A RUN property, free text server-side (an operator email
        // needs its `@`) — never charset-defaulted.
        components.push(text_input_component(
            "operated_by",
            "Operated By",
            operated_cfg,
            false,
        ));
    }
    if let Some(sub_units) = cfg.sub_units.as_ref() {
        components.extend(build_sub_unit_components(sub_units));
    }
    // Metadata fields render after the built-in fields, alphabetically
    // (BTreeMap order), each with the `metadata:<key>` prefix convention
    // (mirrors `sub_unit:<key>`). Always optional — metadata is never
    // required.
    if let Some(md) = cfg.metadata.as_ref() {
        for (key, field_cfg) in md {
            components.push(text_input_component(
                &format!("metadata:{key}"),
                key,
                field_cfg,
                false,
            ));
        }
    }

    Ok(components)
}

fn build_sub_unit_components(cfg: &SubUnitsConfig) -> Vec<UiComponent> {
    cfg.0
        .iter()
        .map(|item| {
            let key = format!("sub_unit:{}", item.get_key());
            let label = item.label.clone();
            let field_cfg = item.serial_number.clone().unwrap_or_default();
            unit_identity_component(&key, &label, &field_cfg, true)
        })
        .collect()
}

/// A unit-identity field (serial / part / revision / batch /
/// sub-unit): like `text_input_component`, but a field the author left
/// pattern-less still carries the server's identity charset — so the
/// shared validator refuses a scan `runs.create` would reject, at
/// submit time, with the recital derived from the charset itself.
/// `operated_by` and `metadata:<key>` are free text server-side and
/// must NOT come through here.
fn unit_identity_component(
    key: &str,
    label: &str,
    field: &UnitFieldConfig,
    required: bool,
) -> UiComponent {
    let mut component = text_input_component(key, label, field, required);
    if component.pattern.is_none() {
        component.pattern =
            Some(station_protocol::pattern_messages::UNIT_FIELD_CHARSET_PATTERN.to_string());
    }
    component
}

fn text_input_component(
    key: &str,
    label: &str,
    field: &UnitFieldConfig,
    required: bool,
) -> UiComponent {
    UiComponent {
        key: key.to_string(),
        label: Some(label.to_string()),
        required,
        description: field.description.clone(),
        placeholder: field.placeholder.clone(),
        default_value: field
            .default_value
            .as_ref()
            .map(|v| ComponentValue::String(v.clone())),
        min_length: field.min_length.map(|n| n as u32),
        max_length: field.max_length.map(|n| n as u32),
        pattern: field.pattern.clone(),
        pattern_message: field.pattern_message.clone(),
        prefix: field.prefix.clone(),
        suffix: field.suffix.clone(),
        ..UiComponent::new(ComponentType::TextInput)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procedure::{SubUnitItemConfig, SubUnitsConfig, UnitConfig, UnitFieldConfig};

    fn cfg_with_required_fields() -> UnitConfig {
        UnitConfig {
            auto_identify: false,
            serial_number: Some(UnitFieldConfig {
                default_value: Some("SN-DEFAULT".to_string()),
                placeholder: Some("SN-XXXX".to_string()),
                description: Some("Scan the barcode on the back panel".to_string()),
                min_length: Some(4),
                max_length: Some(24),
                pattern: Some("^SN-".to_string()),
                pattern_message: None,
                prefix: None,
                suffix: None,
            }),
            part_number: Some(UnitFieldConfig {
                default_value: Some("PCB-V2".to_string()),
                ..Default::default()
            }),
            revision_number: None,
            batch_number: None,
            sub_units: None,
            metadata: None,
        }
    }

    #[test]
    fn always_emits_serial_number_first() {
        let cfg = cfg_with_required_fields();
        let components = build_components(&cfg, None).unwrap();
        assert_eq!(components[0].key, "serial_number");
        assert_eq!(components[1].key, "part_number");
    }

    #[test]
    fn missing_serial_number_config_errors() {
        let mut cfg = cfg_with_required_fields();
        cfg.serial_number = None;
        let err = build_components(&cfg, None).unwrap_err();
        assert!(err.contains("serial_number"));
    }

    #[test]
    fn missing_part_number_config_errors() {
        let mut cfg = cfg_with_required_fields();
        cfg.part_number = None;
        let err = build_components(&cfg, None).unwrap_err();
        assert!(err.contains("part_number"));
    }

    #[test]
    fn forwards_default_placeholder_pattern_lengths() {
        let cfg = cfg_with_required_fields();
        let components = build_components(&cfg, None).unwrap();
        let serial = components
            .iter()
            .find(|c| c.key == "serial_number")
            .unwrap();
        assert!(matches!(
            serial.default_value,
            Some(ComponentValue::String(ref v)) if v == "SN-DEFAULT"
        ));
        assert_eq!(serial.placeholder.as_deref(), Some("SN-XXXX"));
        assert_eq!(
            serial.description.as_deref(),
            Some("Scan the barcode on the back panel")
        );
        assert_eq!(serial.min_length, Some(4));
        assert_eq!(serial.max_length, Some(24));
        assert_eq!(serial.pattern.as_deref(), Some("^SN-"));
        assert!(serial.required);
    }

    /// A pattern-less unit-identity field carries the server's identity
    /// charset so the shared submit validator refuses what `runs.create`
    /// would reject. `operated_by` stays free text (emails need `@`).
    #[test]
    fn pattern_less_identity_fields_default_to_the_unit_charset() {
        let mut cfg = cfg_with_required_fields();
        cfg.serial_number = Some(UnitFieldConfig::default());
        let components = build_components(&cfg, Some(&UnitFieldConfig::default())).unwrap();
        let serial = components.iter().find(|c| c.key == "serial_number").unwrap();
        assert_eq!(
            serial.pattern.as_deref(),
            Some(station_protocol::pattern_messages::UNIT_FIELD_CHARSET_PATTERN)
        );
        let operated = components.iter().find(|c| c.key == "operated_by").unwrap();
        assert_eq!(operated.pattern, None);
    }

    #[test]
    fn forwards_prefix_and_suffix() {
        let mut cfg = cfg_with_required_fields();
        cfg.serial_number = Some(UnitFieldConfig {
            prefix: Some("SN-".to_string()),
            suffix: Some("-EU".to_string()),
            ..Default::default()
        });
        let components = build_components(&cfg, None).unwrap();
        let serial = components
            .iter()
            .find(|c| c.key == "serial_number")
            .unwrap();
        assert_eq!(serial.prefix.as_deref(), Some("SN-"));
        assert_eq!(serial.suffix.as_deref(), Some("-EU"));
    }

    #[test]
    fn description_is_none_when_unset() {
        let cfg = cfg_with_required_fields();
        let components = build_components(&cfg, None).unwrap();
        // part_number config sets no description -> wire component carries None
        let part = components.iter().find(|c| c.key == "part_number").unwrap();
        assert_eq!(part.description, None);
    }

    #[test]
    fn forwards_sub_unit_description() {
        let mut cfg = cfg_with_required_fields();
        cfg.sub_units = Some(SubUnitsConfig(vec![SubUnitItemConfig {
            label: "Battery".to_string(),
            key: Some("battery".to_string()),
            serial_number: Some(UnitFieldConfig {
                description: Some("Located under the cover".to_string()),
                ..Default::default()
            }),
        }]));
        let components = build_components(&cfg, None).unwrap();
        let battery = components
            .iter()
            .find(|c| c.key == "sub_unit:battery")
            .expect("battery sub-unit component");
        assert_eq!(
            battery.description.as_deref(),
            Some("Located under the cover")
        );
    }

    #[test]
    fn revision_and_batch_are_optional() {
        let cfg = cfg_with_required_fields();
        let components = build_components(&cfg, None).unwrap();
        assert!(!components.iter().any(|c| c.key == "revision_number"));
        assert!(!components.iter().any(|c| c.key == "batch_number"));
        assert!(!components.iter().any(|c| c.key == "operated_by"));
    }

    #[test]
    fn operated_by_emitted_when_configured() {
        let cfg = cfg_with_required_fields();
        let operated = UnitFieldConfig {
            placeholder: Some("operator@acme.com".to_string()),
            ..Default::default()
        };
        let components = build_components(&cfg, Some(&operated)).unwrap();
        let operated = components
            .iter()
            .find(|c| c.key == "operated_by")
            .expect("operated_by component");
        assert_eq!(operated.label.as_deref(), Some("Operated By"));
        assert!(!operated.required);
        assert_eq!(operated.placeholder.as_deref(), Some("operator@acme.com"));
    }

    #[test]
    fn revision_and_batch_emitted_when_configured() {
        let mut cfg = cfg_with_required_fields();
        cfg.revision_number = Some(UnitFieldConfig::default());
        cfg.batch_number = Some(UnitFieldConfig::default());
        let components = build_components(&cfg, None).unwrap();
        assert!(components
            .iter()
            .any(|c| c.key == "revision_number" && !c.required));
        assert!(components
            .iter()
            .any(|c| c.key == "batch_number" && !c.required));
    }

    #[test]
    fn sub_units_emit_prefixed_keys() {
        let mut cfg = cfg_with_required_fields();
        cfg.sub_units = Some(SubUnitsConfig(vec![
            SubUnitItemConfig {
                label: "Battery".to_string(),
                key: Some("battery".to_string()),
                serial_number: None,
            },
            SubUnitItemConfig {
                label: "RF Module".to_string(),
                key: Some("rf_module".to_string()),
                serial_number: Some(UnitFieldConfig {
                    pattern: Some("^RF-".to_string()),
                    pattern_message: None,
                    ..Default::default()
                }),
            },
        ]));
        let components = build_components(&cfg, None).unwrap();
        let battery = components
            .iter()
            .find(|c| c.key == "sub_unit:battery")
            .expect("battery sub-unit component");
        assert_eq!(battery.label.as_deref(), Some("Battery"));
        assert!(battery.required);
        let rf = components
            .iter()
            .find(|c| c.key == "sub_unit:rf_module")
            .expect("rf_module sub-unit component");
        assert_eq!(rf.pattern.as_deref(), Some("^RF-"));
    }

    #[test]
    fn metadata_fields_emit_prefixed_optional_components() {
        let mut cfg = cfg_with_required_fields();
        let mut md = std::collections::BTreeMap::new();
        md.insert(
            "modification".to_string(),
            UnitFieldConfig {
                placeholder: Some("MOD-42".to_string()),
                pattern: Some("^MOD-[0-9]+$".to_string()),
                pattern_message: None,
                ..Default::default()
            },
        );
        md.insert("amendment".to_string(), UnitFieldConfig::default());
        cfg.metadata = Some(md);

        let components = build_components(&cfg, None).unwrap();
        let modification = components
            .iter()
            .find(|c| c.key == "metadata:modification")
            .expect("modification component");
        assert_eq!(modification.label.as_deref(), Some("modification"));
        assert!(!modification.required);
        assert_eq!(modification.placeholder.as_deref(), Some("MOD-42"));
        assert_eq!(modification.pattern.as_deref(), Some("^MOD-[0-9]+$"));

        // BTreeMap: alphabetical order — amendment before modification
        let idx_a = components
            .iter()
            .position(|c| c.key == "metadata:amendment")
            .unwrap();
        let idx_m = components
            .iter()
            .position(|c| c.key == "metadata:modification")
            .unwrap();
        assert!(idx_a < idx_m);
    }
}
