//! Shared JSON → SDK mapping macros for the framework connectors.
//!
//! The generated SDK declares one validator/aggregation struct per nesting
//! site (`RunCreateValidators`, `RunCreateYAxisValidators`,
//! `RunCreateMeasurementsAggregations`, …) even though they are
//! field-for-field identical. `serde_json::from_value` would collapse that,
//! but it is all-or-nothing: one unexpected `outcome` spelling from a
//! framework and the whole validator is dropped. These macros keep the
//! tolerant field-by-field mapping the connectors already use for
//! measurement-level validators, without writing it out six times.
//!
//! Declared with `#[macro_use]` ahead of `mod pytest;` in `connector/mod.rs`
//! so both connectors can use them.

/// Map a JSON array of validator objects onto `$ty`.
macro_rules! json_validators {
    ($ty:ident, $arr:expr) => {
        $arr.iter()
            .filter_map(|v| {
                let mut b = $ty::builder();
                if let Some(s) = json_str(v, "operator") {
                    b = b.operator(s);
                }
                if let Some(e) = v.get("expected_value").filter(|v| !v.is_null()) {
                    b = b.expected_value(e.clone());
                }
                if let Some(s) = json_str(v, "expression") {
                    b = b.expression(s);
                }
                if let Some(s) = json_str(v, "outcome") {
                    b = b.outcome(crate::commands::run::outcomes::validator_outcome_from_wire(
                        s,
                    ));
                }
                if let Some(d) = v.get("is_decisive").and_then(|v| v.as_bool()) {
                    b = b.is_decisive(d);
                }
                b.build().ok()
            })
            .collect::<Vec<_>>()
    };
}

/// Map a JSON array of aggregation objects onto `$agg_ty`, with their
/// nested validators onto `$val_ty`.
macro_rules! json_aggregations {
    ($agg_ty:ident, $val_ty:ident, $arr:expr) => {
        $arr.iter()
            .filter_map(|a| {
                // `type` is the only required field; an aggregation without
                // one cannot be stored, so skip it rather than fail the run.
                let mut b = $agg_ty::builder().r#type(json_str(a, "type")?);
                if let Some(v) = a.get("value").filter(|v| !v.is_null()) {
                    b = b.value(v.clone());
                }
                if let Some(u) = json_str(a, "unit") {
                    b = b.unit(u);
                }
                if let Some(s) = json_str(a, "outcome") {
                    b = b.outcome(crate::commands::run::outcomes::validator_outcome_from_wire(
                        s,
                    ));
                }
                if let Some(vs) = a.get("validators").and_then(|v| v.as_array()) {
                    let validators = json_validators!($val_ty, vs);
                    if !validators.is_empty() {
                        b = b.validators(validators);
                    }
                }
                b.build().ok()
            })
            .collect::<Vec<_>>()
    };
}
