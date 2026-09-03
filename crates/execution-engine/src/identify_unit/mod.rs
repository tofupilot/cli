//! Canonical identify-unit step.
//!
//! Every runner (CLI YAML, OpenHTF connector, future pytest connector)
//! resolves the unit being tested through this single entry point.
//! The host (typically the CLI) supplies an `IdentifyHost`
//! implementation that owns the actual UI dance — emitting the
//! dedicated `StationEvent::IdentifyRequest`, registering a oneshot in
//! `UI_RESPONSE_CHANNELS`, fanning out to TUI / kiosk / agent /
//! dashboard, and awaiting the operator response. The framework owns
//! the rest: component shape, `auto_identify` short-circuit, response
//! parsing, validation.
//!
//! Identity is run metadata, not a phase. Wire consumers detect the
//! prompt via the typed `IdentifyRequest` event (no component-shape
//! heuristic). The `phase_key` carried on `PromptRequest` is now
//! informational only — kept for log/grep stability across runners
//! and for the agent-protocol's lifecycle bookkeeping.
//!
//! `identify(...)` is per-slot. Multi-slot callers iterate (and may
//! `tokio::join!` if their host's `prompt` is concurrency-safe);
//! sequencing policy lives at the call site, not here.

pub mod components;
pub mod resolve;

use async_trait::async_trait;
use std::collections::HashMap;

use crate::procedure::UnitConfig;
use crate::ui::UiComponent;
use crate::unit::UnitInfo;

/// Sentinel `phase_key` for identify-unit prompts. Wire consumers
/// route via the dedicated `IdentifyRequest` event, but a stable key
/// here keeps agent-protocol replay logs and grep-friendly
/// diagnostics consistent across runners.
pub const IDENTIFY_PHASE_KEY: &str = "identify_unit";

/// Request handed to an `IdentifyHost` for emission to the operator.
#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub request_id: String,
    pub slot_id: Option<String>,
    pub phase_key: String,
    pub components: Vec<UiComponent>,
}

/// Reasons a host can't (or won't) deliver an operator response.
#[derive(Debug, Clone)]
pub enum IdentifyHostError {
    /// Operator dismissed the prompt or the UI channel closed before
    /// a response arrived (kiosk closed, agent stdin disconnected,
    /// etc.).
    Cancelled(String),
    /// Any other host-side failure (broadcast send error, etc.).
    Other(String),
}

impl std::fmt::Display for IdentifyHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled(reason) => write!(f, "identify-unit cancelled: {}", reason),
            Self::Other(reason) => write!(f, "identify-unit host error: {}", reason),
        }
    }
}

impl std::error::Error for IdentifyHostError {}

/// Outcome of `identify(...)`.
#[derive(Debug, Clone)]
pub enum IdentifyError {
    /// Operator did not provide a response (kiosk dismissed, agent
    /// disconnect, etc.). Callers typically treat this as a clean run
    /// abort rather than a hard error.
    Cancelled(String),
    /// `UnitConfig` was malformed (missing required fields, etc.).
    InvalidConfig(String),
    /// Operator response failed validation (regex / length / required).
    Validation(String),
    /// Host returned a non-cancellation error.
    Host(String),
    /// The procedure needs operator unit identification but no UI is
    /// available to deliver it (headless run: no TUI, no kiosk, no agent,
    /// not station mode). Without this the prompt would await a response
    /// that can never arrive and the run would hang forever.
    NoUi(String),
}

impl std::fmt::Display for IdentifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled(r) => write!(f, "identify-unit cancelled: {}", r),
            Self::InvalidConfig(r) => write!(f, "invalid unit config: {}", r),
            Self::Validation(r) => write!(f, "identify-unit validation failed: {}", r),
            Self::Host(r) => write!(f, "identify-unit host error: {}", r),
            Self::NoUi(r) => write!(f, "{}", r),
        }
    }
}

impl std::error::Error for IdentifyError {}

/// Host-side adapter for the `identify_unit` UI dance. Implementors
/// own the broadcast / oneshot / fanout plumbing; the framework owns
/// the prompt shape and response semantics.
#[async_trait]
pub trait IdentifyHost: Send + Sync {
    async fn prompt(
        &self,
        req: PromptRequest,
    ) -> Result<HashMap<String, String>, IdentifyHostError>;

    /// Whether this host has any channel that could ever deliver an
    /// operator response (TUI, kiosk, agent, station dashboard). When
    /// false, `identify` fails fast with `NoUi` instead of broadcasting a
    /// prompt nobody can answer and awaiting it forever. Defaults to
    /// `true` so existing hosts keep their behaviour.
    fn can_prompt(&self) -> bool {
        true
    }
}

/// The slot a unit is identified for. `{slot}` and `{slot_name}` in any
/// `default_value` of the unit block (and of the root `operated_by:`)
/// expand to the slot key and its display name, so one `unit:` block
/// serves every slot of a fixture and `auto_identify: true` works
/// headless on a multi-slot station:
///
/// ```yaml
/// unit:
///   auto_identify: true
///   serial_number:
///     default_value: "BURNIN-{slot}"
/// ```
///
/// `{slot_name}` falls back to the key when the slot has no name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotRef<'a> {
    pub key: &'a str,
    pub name: Option<&'a str>,
}

impl<'a> SlotRef<'a> {
    pub fn new(key: &'a str, name: Option<&'a str>) -> Self {
        Self { key, name }
    }

    pub fn expand(&self, value: &str) -> String {
        value
            .replace("{slot}", self.key)
            .replace("{slot_name}", self.name.unwrap_or(self.key))
    }

    fn expand_field(&self, field: &mut crate::procedure::UnitFieldConfig) {
        if let Some(v) = field.default_value.as_mut() {
            *v = self.expand(v);
        }
    }

    /// The unit block with every `default_value` expanded for this slot.
    pub fn expand_config(&self, cfg: &UnitConfig) -> UnitConfig {
        let mut out = cfg.clone();
        for field in [
            &mut out.serial_number,
            &mut out.part_number,
            &mut out.revision_number,
            &mut out.batch_number,
        ]
        .into_iter()
        .flatten()
        {
            self.expand_field(field);
        }
        if let Some(sub) = out.sub_units.as_mut() {
            for item in sub.0.iter_mut().filter_map(|i| i.serial_number.as_mut()) {
                self.expand_field(item);
            }
        }
        if let Some(md) = out.metadata.as_mut() {
            for field in md.values_mut() {
                self.expand_field(field);
            }
        }
        out
    }
}

/// Resolve a unit identity for one slot.
///
/// * `auto_identify: true` — short-circuits to `default_value` fields,
///   never calls `host.prompt`.
/// * otherwise — builds the canonical component list, asks the host,
///   parses + validates the response.
///
/// On success the returned `UnitInfo` has already passed
/// `validate_unit_info`. On any error the run should abort cleanly.
pub async fn identify(
    cfg: &UnitConfig,
    // Procedure-root `operated_by:` — run attribution prompted on the
    // same identification screen; None when the YAML omits it.
    operated_by: Option<&crate::procedure::UnitFieldConfig>,
    // The slot this unit is identified for: stamps `PromptRequest.slot_id`
    // and expands `{slot}` / `{slot_name}` in the defaults. None on a
    // single-slot run or a connector framework.
    slot: Option<SlotRef<'_>>,
    host: &dyn IdentifyHost,
) -> Result<UnitInfo, IdentifyError> {
    let expanded_cfg;
    let expanded_operated_by;
    let (cfg, operated_by) = match slot {
        Some(slot) => {
            expanded_cfg = slot.expand_config(cfg);
            expanded_operated_by = operated_by.cloned().map(|mut f| {
                slot.expand_field(&mut f);
                f
            });
            (&expanded_cfg, expanded_operated_by.as_ref())
        }
        None => (cfg, operated_by),
    };
    let slot_id = slot.map(|s| s.key);

    if cfg.auto_identify {
        return resolve::auto_identify_unit_info(cfg, operated_by)
            .map_err(IdentifyError::Validation);
    }

    // Fail fast when no UI can answer the prompt. A headless run (no TUI,
    // kiosk, agent, or station dashboard) would otherwise broadcast an
    // identify request, await a oneshot that never fires, and hang
    // forever. The message points at the ways to supply a unit identity
    // without an operator.
    if !host.can_prompt() {
        return Err(IdentifyError::NoUi(
            "this procedure requires operator unit identification, but the run is headless \
             with no UI to enter it. Set `auto_identify` with default values in the procedure \
             `.yaml` file, pass `--ui-values`, or run with `--tui` / `--kiosk`."
                .to_string(),
        ));
    }

    let components =
        components::build_components(cfg, operated_by).map_err(IdentifyError::InvalidConfig)?;
    let request = PromptRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        slot_id: slot_id.map(str::to_string),
        phase_key: IDENTIFY_PHASE_KEY.to_string(),
        components,
    };

    let values = match host.prompt(request).await {
        Ok(v) => v,
        Err(IdentifyHostError::Cancelled(r)) => return Err(IdentifyError::Cancelled(r)),
        Err(IdentifyHostError::Other(r)) => return Err(IdentifyError::Host(r)),
    };

    resolve::resolve_response(cfg, operated_by, values).map_err(IdentifyError::Validation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procedure::{UnitConfig, UnitFieldConfig};
    use std::sync::Mutex;

    /// Test host that records the request it received and replies with
    /// a canned response (or a canned error).
    struct FakeHost {
        captured: Mutex<Option<PromptRequest>>,
        reply: Result<HashMap<String, String>, IdentifyHostError>,
    }

    impl FakeHost {
        fn ok(reply: HashMap<String, String>) -> Self {
            Self {
                captured: Mutex::new(None),
                reply: Ok(reply),
            }
        }

        fn cancelled() -> Self {
            Self {
                captured: Mutex::new(None),
                reply: Err(IdentifyHostError::Cancelled(
                    "operator dismissed".to_string(),
                )),
            }
        }
    }

    #[async_trait]
    impl IdentifyHost for FakeHost {
        async fn prompt(
            &self,
            req: PromptRequest,
        ) -> Result<HashMap<String, String>, IdentifyHostError> {
            *self.captured.lock().unwrap() = Some(req);
            self.reply.clone()
        }
    }

    fn cfg_for_prompt() -> UnitConfig {
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

    #[tokio::test]
    async fn auto_identify_skips_host() {
        let cfg = UnitConfig {
            auto_identify: true,
            serial_number: Some(UnitFieldConfig {
                default_value: Some("SN-AUTO".to_string()),
                ..Default::default()
            }),
            part_number: Some(UnitFieldConfig {
                default_value: Some("PCB".to_string()),
                ..Default::default()
            }),
            revision_number: None,
            batch_number: None,
            sub_units: None,
            metadata: None,
        };
        let host = FakeHost::ok(HashMap::new());
        let info = identify(&cfg, None, Some(SlotRef::new("default", None)), &host)
            .await
            .unwrap();
        assert_eq!(info.serial_number.as_deref(), Some("SN-AUTO"));
        // Host was never called.
        assert!(host.captured.lock().unwrap().is_none());
    }

    /// Host with no responder surface: `can_prompt` is false, so
    /// `identify` must fail fast without ever calling `prompt`.
    struct NoUiHost {
        captured: Mutex<Option<PromptRequest>>,
    }

    #[async_trait]
    impl IdentifyHost for NoUiHost {
        fn can_prompt(&self) -> bool {
            false
        }
        async fn prompt(
            &self,
            req: PromptRequest,
        ) -> Result<HashMap<String, String>, IdentifyHostError> {
            *self.captured.lock().unwrap() = Some(req);
            Ok(HashMap::new())
        }
    }

    #[tokio::test]
    async fn no_ui_fails_fast_without_prompting() {
        let cfg = cfg_for_prompt();
        let host = NoUiHost {
            captured: Mutex::new(None),
        };
        let err = identify(&cfg, None, Some(SlotRef::new("default", None)), &host)
            .await
            .unwrap_err();
        assert!(matches!(err, IdentifyError::NoUi(_)));
        // The prompt was never broadcast — that's what stops the hang.
        assert!(host.captured.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn auto_identify_wins_over_no_ui() {
        // auto_identify short-circuits before the can_prompt check, so a
        // headless run with auto_identify still resolves without NoUi.
        let cfg = UnitConfig {
            auto_identify: true,
            serial_number: Some(UnitFieldConfig {
                default_value: Some("SN-AUTO".to_string()),
                ..Default::default()
            }),
            part_number: Some(UnitFieldConfig {
                default_value: Some("PCB".to_string()),
                ..Default::default()
            }),
            revision_number: None,
            batch_number: None,
            sub_units: None,
            metadata: None,
        };
        let host = NoUiHost {
            captured: Mutex::new(None),
        };
        let info = identify(&cfg, None, Some(SlotRef::new("default", None)), &host)
            .await
            .unwrap();
        assert_eq!(info.serial_number.as_deref(), Some("SN-AUTO"));
        assert!(host.captured.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn prompt_path_round_trips_response() {
        let cfg = cfg_for_prompt();
        let mut reply = HashMap::new();
        reply.insert("serial_number".to_string(), "SN-1".to_string());
        reply.insert("part_number".to_string(), "PN-1".to_string());
        let host = FakeHost::ok(reply);

        let info = identify(&cfg, None, Some(SlotRef::new("default", None)), &host)
            .await
            .unwrap();
        assert_eq!(info.serial_number.as_deref(), Some("SN-1"));
        assert_eq!(info.part_number.as_deref(), Some("PN-1"));

        let captured = host.captured.lock().unwrap();
        let req = captured.as_ref().expect("host received request");
        assert_eq!(req.phase_key, IDENTIFY_PHASE_KEY);
        assert_eq!(req.slot_id.as_deref(), Some("default"));
        assert!(req.components.iter().any(|c| c.key == "serial_number"));
        assert!(req.components.iter().any(|c| c.key == "part_number"));
    }

    #[tokio::test]
    async fn host_cancellation_maps_to_cancelled() {
        let cfg = cfg_for_prompt();
        let host = FakeHost::cancelled();
        let err = identify(&cfg, None, None, &host).await.unwrap_err();
        match err {
            IdentifyError::Cancelled(_) => {}
            other => panic!("expected Cancelled, got {other}"),
        }
    }

    #[tokio::test]
    async fn validation_error_surfaces() {
        let cfg = cfg_for_prompt();
        let host = FakeHost::ok(HashMap::new()); // operator submits nothing
        let err = identify(&cfg, None, None, &host).await.unwrap_err();
        match err {
            IdentifyError::Validation(_) => {}
            other => panic!("expected Validation, got {other}"),
        }
    }

    fn cfg_with_slot_defaults() -> UnitConfig {
        UnitConfig {
            auto_identify: true,
            serial_number: Some(UnitFieldConfig {
                default_value: Some("BURNIN-{slot}".to_string()),
                ..UnitFieldConfig::default()
            }),
            part_number: Some(UnitFieldConfig {
                default_value: Some("PCB".to_string()),
                ..UnitFieldConfig::default()
            }),
            batch_number: Some(UnitFieldConfig {
                default_value: Some("{slot_name}".to_string()),
                ..UnitFieldConfig::default()
            }),
            ..UnitConfig::default()
        }
    }

    /// `{slot}` / `{slot_name}` expand per slot on the auto-identify path,
    /// which is what lets an 80-slot burn-in rack start headless.
    #[tokio::test]
    async fn auto_identify_expands_slot_placeholders() {
        let cfg = cfg_with_slot_defaults();
        let host = FakeHost::ok(HashMap::new());
        let info = identify(&cfg, None, Some(SlotRef::new("s03", Some("Nest 3"))), &host)
            .await
            .unwrap();
        assert_eq!(info.serial_number.as_deref(), Some("BURNIN-s03"));
        assert_eq!(info.batch_number.as_deref(), Some("Nest 3"));
        assert!(
            host.captured.lock().unwrap().is_none(),
            "no prompt on auto_identify"
        );
    }

    /// Without a name `{slot_name}` reads the key; without a slot the
    /// placeholders are left alone (single-slot runs never see them).
    #[tokio::test]
    async fn slot_name_falls_back_to_key_and_no_slot_leaves_text() {
        let cfg = cfg_with_slot_defaults();
        let host = FakeHost::ok(HashMap::new());
        let info = identify(&cfg, None, Some(SlotRef::new("s03", None)), &host)
            .await
            .unwrap();
        assert_eq!(info.batch_number.as_deref(), Some("s03"));
        let info = identify(&cfg, None, None, &host).await.unwrap();
        assert_eq!(info.serial_number.as_deref(), Some("BURNIN-{slot}"));
    }

    /// The prompt path expands the defaults the operator sees too.
    #[tokio::test]
    async fn prompt_defaults_are_expanded_for_the_slot() {
        let mut cfg = cfg_with_slot_defaults();
        cfg.auto_identify = false;
        let mut reply = HashMap::new();
        reply.insert("serial_number".to_string(), "SN-1".to_string());
        reply.insert("part_number".to_string(), "PCB".to_string());
        let host = FakeHost::ok(reply);
        identify(&cfg, None, Some(SlotRef::new("s07", None)), &host)
            .await
            .unwrap();
        let req = host.captured.lock().unwrap().clone().unwrap();
        assert_eq!(req.slot_id.as_deref(), Some("s07"));
        let serial = req
            .components
            .iter()
            .find(|c| c.key == "serial_number")
            .unwrap();
        assert_eq!(
            format!("{:?}", serial.default_value),
            format!(
                "{:?}",
                Some(crate::ui::ComponentValue::String("BURNIN-s07".to_string()))
            )
        );
    }
}
