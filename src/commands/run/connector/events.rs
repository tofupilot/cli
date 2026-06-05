//! Typed schema for the Python-connector ↔ Rust NDJSON protocol.
//!
//! Mirror image of the `Event` class in `connector/openhtf.py`. A rename
//! or new variant on one side without the other surfaces here as a
//! deserialization error at the boundary — no silent drift.
//!
//! We parse every line into both a strongly-typed `PythonEvent` (for
//! dispatch and common-field access) AND keep the original `serde_json::
//! Value` alongside it (for downstream code that does ad-hoc field
//! extraction — `extract_run_measurements`, `build_request`, etc.).
//! Typing the headers catches rename drift without forcing us to type
//! every nested payload field.

use serde::Deserialize;
use std::collections::HashMap;

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PythonEvent {
    BridgeReady,

    TestStart {
        #[serde(default)]
        test_name: String,
        #[serde(default)]
        phases: Vec<String>,
        /// Connector-side opt-out (`htf.Test(..., identify=False)`).
        /// When false, the Rust side skips the identify handshake and
        /// no `set_unit_resolved` line is written back. Defaults to
        /// `true` when the field is absent.
        #[serde(default = "default_true")]
        identify: bool,
        /// Forwarded `auto_identify` kwarg from `htf.Test(...)`. Drives
        /// `auto_identify` in the framework `UnitConfig` we build.
        #[serde(default)]
        auto_identify: bool,
        /// `htf.Test(...)` unit kwargs (serial_number / part_number /
        /// revision_number / batch_number). Empty string means absent;
        /// we map empty → `None` when constructing the `UnitConfig`.
        #[serde(default)]
        unit_kwargs: HashMap<String, String>,
    },

    TestEnd {
        #[serde(default)]
        outcome: String,
    },

    PhaseBegin {
        #[serde(default)]
        name: String,
    },

    PhaseEnd {
        #[serde(default)]
        name: String,
        #[serde(default)]
        outcome: String,
        #[serde(default)]
        retry_count: u64,
        #[serde(default)]
        start_time_millis: Option<i64>,
        #[serde(default)]
        end_time_millis: Option<i64>,
        #[serde(default)]
        error: Option<String>,
    },

    /// Native OpenHTF `prompts.prompt(...)` from user code. Identity is
    /// owned by the `set_unit_resolved` handshake on `TestStart`, so a
    /// user test that calls `prompts.prompt("SN?")` after the framework
    /// prompt surfaces as a plain operator prompt (intentional).
    Prompt {
        #[serde(default)]
        prompt_id: String,
        #[serde(default)]
        phase_name: Option<String>,
        #[serde(default)]
        message: String,
        #[serde(default)]
        text_input: bool,
        #[serde(default)]
        image_url: Option<String>,
        /// Per-prompt timeout in seconds (Python's `prompt(timeout_s=...)`).
        /// Overrides `--ui-timeout` for this single prompt; the flag still
        /// applies as a default to prompts that didn't supply one.
        #[serde(default)]
        timeout_s: Option<f64>,
    },

    Attachment {
        #[serde(default)]
        name: String,
        #[serde(default)]
        path: String,
        #[serde(default)]
        mimetype: String,
        /// `true` when emitted from the live `PhaseState.attach` patch;
        /// the post-hoc output_callback does NOT set this. Lets the Rust
        /// pump fire `attachment_added` once at record time without
        /// double-firing on the post-phase batch.
        #[serde(default)]
        live: bool,
        #[serde(default)]
        phase_name: Option<String>,
    },

    /// Live measurement record. Emitted from the patched `MeasuredValue.set`
    /// each time user code writes a measurement value (OpenHTF) or, for
    /// pytest, when an AST-extracted measurement spec captures the value
    /// of the matched local at assert time. The post-hoc
    /// `phase_end.measurements` payload still carries the final validated
    /// values; this is the streaming preview.
    Measurement {
        #[serde(default)]
        name: String,
        #[serde(default)]
        value: serde_json::Value,
        #[serde(default)]
        phase_name: Option<String>,
        /// Optional unit (e.g. "V", "ms"). Set by the pytest connector
        /// from the AST-parsed `"description [unit]"` assert message;
        /// OpenHTF doesn't surface unit on the live event because its
        /// measurements declare units at definition time and the unit
        /// lives on the post-hoc payload.
        #[serde(default)]
        unit: Option<String>,
    },

    /// Live log line from a phase Python body. Emitted in real time via
    /// the OpenHTF logger handler. The phase_end record still carries
    /// the full log batch for completeness.
    PhaseLog {
        #[serde(default)]
        level: String,
        #[serde(default)]
        message: String,
        #[serde(default)]
        timestamp: String,
        #[serde(default)]
        phase_name: Option<String>,
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        line: Option<u32>,
    },

    Warning {
        #[serde(default)]
        message: String,
    },

    /// Swallow unknown types rather than aborting the stream. Python may
    /// ship a new event before Rust has a matching variant; the Rust side
    /// should log and move on, not crash.
    #[serde(other)]
    Unknown,
}

impl PythonEvent {
    /// Parse a raw JSON value into the typed event shape. `None` on
    /// deserialization failure (malformed event, unknown required fields);
    /// caller should log the raw value and continue.
    pub fn from_value(v: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(v.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_start_round_trips_unit_kwargs() {
        // Mirror exactly what `connector/openhtf.py::patched_init`
        // emits. Drift here surfaces as a deserialization failure
        // rather than a silent identify-unit regression.
        let raw = serde_json::json!({
            "type": "test_start",
            "test_name": "",
            "phases": ["hello"],
            "identify": true,
            "auto_identify": true,
            "unit_kwargs": {
                "serial_number": "SN-1",
                "part_number": "PCB",
                "revision_number": "",
                "batch_number": "",
            },
        });
        match PythonEvent::from_value(&raw).unwrap() {
            PythonEvent::TestStart {
                identify,
                auto_identify,
                unit_kwargs,
                ..
            } => {
                assert!(identify);
                assert!(auto_identify);
                assert_eq!(
                    unit_kwargs.get("serial_number").map(String::as_str),
                    Some("SN-1")
                );
                assert_eq!(
                    unit_kwargs.get("part_number").map(String::as_str),
                    Some("PCB")
                );
            }
            other => panic!("expected TestStart, got {:?}", other),
        }
    }

    #[test]
    fn test_start_minimal_payload_defaults_identify_true() {
        // Minimal emit (no identify / auto_identify / unit_kwargs) still
        // parses. `identify` defaults to true, `auto_identify` false,
        // `unit_kwargs` empty.
        let raw = serde_json::json!({
            "type": "test_start",
            "test_name": "",
            "phases": [],
        });
        match PythonEvent::from_value(&raw).unwrap() {
            PythonEvent::TestStart {
                identify,
                auto_identify,
                unit_kwargs,
                ..
            } => {
                assert!(identify);
                assert!(!auto_identify);
                assert!(unit_kwargs.is_empty());
            }
            other => panic!("expected TestStart, got {:?}", other),
        }
    }

    #[test]
    fn prompt_ignores_unknown_fields() {
        // Unknown JSON keys aren't strict; a `Prompt` payload carrying
        // extra fields parses as a plain operator prompt.
        let raw = serde_json::json!({
            "type": "prompt",
            "prompt_id": "p1",
            "message": "hi",
            "text_input": true,
        });
        assert!(matches!(
            PythonEvent::from_value(&raw).unwrap(),
            PythonEvent::Prompt { .. }
        ));
    }
}
