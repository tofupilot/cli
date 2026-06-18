//! Single source of truth for assembling the `__bound_measurements__`
//! sentinel from `bind:` UI components. The engine reads this sentinel
//! out of a UI response's `values` map (`worker::extract_bound_measurements`)
//! and turns it into phase measurements / unit fields — a bare
//! `values[key]` is ignored. Every operator-UI surface (TUI, agent
//! protocol, and the React/web client's `buildBoundMeasurementsPayload`)
//! must produce the identical payload so a procedure records the same
//! measurement regardless of which UI ran it.
//!
//! The web reference is `packages/operator-ui/src/run-state.ts`.

use super::types::{ComponentType, UiComponent};

/// Build the `__bound_measurements__` JSON payload from components that
/// carry a `bind` directive. `resolve` returns the operator's answer for
/// a component as a string (already trimmed/joined by the caller), or
/// `None` to skip it. Returns `None` when nothing binds.
///
/// Routing mirrors the web client:
/// - `measurements.X` / `measurement.X` → typed scalar under `X`
/// - `unit.X` → string under `__unit__.X`
/// - `unit.sub_units.X` → string under `__unit__.sub_units.X`
///
/// Type coercion mirrors the web client: number/slider → JSON number
/// (empty or unparseable skipped), switch → bool, everything else → the
/// string as-is. Empty bind names (`bind: measurements.`) are skipped to
/// match the web regex `^measurements?\.(.+)$`, which requires a name.
pub fn build_bound_measurements_payload<F>(
    components: &[UiComponent],
    resolve: F,
) -> Option<String>
where
    F: Fn(&UiComponent) -> Option<String>,
{
    let mut measurements = serde_json::Map::new();
    let mut unit_fields = serde_json::Map::new();
    let mut sub_units = serde_json::Map::new();

    for comp in components {
        let Some(bind) = comp.bind.as_deref() else {
            continue;
        };
        let Some(raw) = resolve(comp) else {
            continue;
        };

        if let Some(name) = bind
            .strip_prefix("measurements.")
            .or_else(|| bind.strip_prefix("measurement."))
        {
            if name.is_empty() {
                continue;
            }
            let typed = match comp.component_type {
                ComponentType::NumberInput | ComponentType::Slider => {
                    if raw.is_empty() {
                        continue;
                    }
                    match raw.parse::<f64>() {
                        Ok(n) => serde_json::json!(n),
                        Err(_) => continue,
                    }
                }
                ComponentType::Switch => serde_json::json!(raw == "true"),
                _ => serde_json::Value::String(raw),
            };
            measurements.insert(name.to_string(), typed);
        } else if let Some(field) = bind.strip_prefix("unit.") {
            // Unit fields are always strings on the wire (serial/part/etc.).
            if let Some(sub) = field.strip_prefix("sub_units.") {
                if !sub.is_empty() {
                    sub_units.insert(sub.to_string(), serde_json::Value::String(raw));
                }
            } else if !field.is_empty() {
                unit_fields.insert(field.to_string(), serde_json::Value::String(raw));
            }
        }
    }

    if measurements.is_empty() && unit_fields.is_empty() && sub_units.is_empty() {
        return None;
    }

    let mut out = measurements;
    if !unit_fields.is_empty() || !sub_units.is_empty() {
        let mut unit_obj = unit_fields;
        if !sub_units.is_empty() {
            unit_obj.insert("sub_units".to_string(), serde_json::Value::Object(sub_units));
        }
        // Ship `__unit__` as a JSON string, byte-identical to the web
        // client (`run-state.ts`: `out.__unit__ = JSON.stringify(unitObj)`).
        // The engine's `extract_bound_measurements` accepts either a nested
        // object or a string, but emitting the same form on every surface
        // keeps the wire payload identical and avoids a future consumer
        // that string-matches `__unit__` diverging by launch method.
        if let Ok(unit_str) = serde_json::to_string(&serde_json::Value::Object(unit_obj)) {
            out.insert("__unit__".to_string(), serde_json::Value::String(unit_str));
        }
    }
    serde_json::to_string(&serde_json::Value::Object(out)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{UiComponent, UiOption};

    fn radio(key: &str, bind: &str, image: bool) -> UiComponent {
        UiComponent {
            key: key.into(),
            bind: Some(bind.into()),
            options: Some(vec![UiOption {
                label: "A".into(),
                value: "A".into(),
                image: if image { Some("a.png".into()) } else { None },
            }]),
            ..UiComponent::new(ComponentType::Radio)
        }
    }

    #[test]
    fn packs_measurement_bind() {
        let comps = vec![radio("m", "measurements.m", false)];
        let json =
            build_bound_measurements_payload(&comps, |_| Some("A".into())).expect("present");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["m"], "A");
    }

    #[test]
    fn coerces_number_and_switch() {
        let num = UiComponent {
            key: "n".into(),
            bind: Some("measurements.n".into()),
            ..UiComponent::new(ComponentType::NumberInput)
        };
        let sw = UiComponent {
            key: "s".into(),
            bind: Some("measurements.s".into()),
            ..UiComponent::new(ComponentType::Switch)
        };
        let json = build_bound_measurements_payload(&[num, sw], |c| {
            Some(if c.key == "n" { "42".into() } else { "true".into() })
        })
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["n"], 42.0);
        assert_eq!(v["s"], true);
    }

    #[test]
    fn routes_unit_and_sub_units() {
        let serial = UiComponent {
            key: "sn".into(),
            bind: Some("unit.serial_number".into()),
            ..UiComponent::new(ComponentType::TextInput)
        };
        let battery = UiComponent {
            key: "bat".into(),
            bind: Some("unit.sub_units.Battery".into()),
            ..UiComponent::new(ComponentType::TextInput)
        };
        let json = build_bound_measurements_payload(&[serial, battery], |c| {
            Some(if c.key == "sn" { "SN-1".into() } else { "BAT-9".into() })
        })
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // `__unit__` ships as a JSON string (matches the web client).
        let unit_str = v["__unit__"].as_str().expect("__unit__ is a JSON string");
        let unit: serde_json::Value = serde_json::from_str(unit_str).unwrap();
        assert_eq!(unit["serial_number"], "SN-1");
        assert_eq!(unit["sub_units"]["Battery"], "BAT-9");
    }

    #[test]
    fn empty_name_skipped() {
        let comps = vec![radio("m", "measurements.", false)];
        assert!(build_bound_measurements_payload(&comps, |_| Some("A".into())).is_none());
    }

    #[test]
    fn no_bind_returns_none() {
        let comp = UiComponent {
            key: "x".into(),
            bind: None,
            ..UiComponent::new(ComponentType::TextInput)
        };
        assert!(build_bound_measurements_payload(&[comp], |_| Some("v".into())).is_none());
    }

    #[test]
    fn empty_number_skipped() {
        let num = UiComponent {
            key: "n".into(),
            bind: Some("measurements.n".into()),
            ..UiComponent::new(ComponentType::NumberInput)
        };
        assert!(build_bound_measurements_payload(&[num], |_| Some(String::new())).is_none());
    }
}
