//! Pre-baked UI response values loaded from `--ui-values <file>`.
//!
//! # Precedence vs. stdin responses
//!
//! When a UI request arrives and the prebaked file covers every required
//! input for that phase, the engine auto-resolves the oneshot synchronously
//! (see `handle_agent_ui_request` in `engine.rs`) and **never registers
//! the request with the stdin reader's `PendingRequests` map**. A late
//! `ui_response` from stdin for the same `request_id` therefore sees an
//! empty map and receives `UnknownRequest` — no race, no silent drop.
//!
//! Rationale: prebaked is an explicit operator override (the whole point of
//! the flag). Agents wanting interactive control should run without it.

use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Default)]
pub struct PreBakedValues {
    inner: HashMap<String, HashMap<String, serde_json::Value>>,
}

impl PreBakedValues {
    /// Load prebaked values, constraining the file to live under
    /// `procedure_dir`. Agents that pass `--ui-values` should scope
    /// their data to the procedure they're running — accepting an
    /// arbitrary absolute path (e.g. `/etc/passwd`) is a small but
    /// real surface. We resolve both paths via `canonicalize` so
    /// symlinks can't hop the boundary.
    pub fn load(path: &Path, procedure_dir: &Path) -> crate::error::CliResult<Self> {
        let canonical = execution_engine::path_utils::canonicalize_for_spawn(path)
            .map_err(|e| format!("Failed to resolve {}: {e}", path.display()))?;
        let root =
            execution_engine::path_utils::canonicalize_for_spawn(procedure_dir).map_err(|e| {
                format!(
                    "Failed to resolve procedure dir {}: {e}",
                    procedure_dir.display()
                )
            })?;
        if !canonical.starts_with(&root) {
            return Err(format!(
                "--ui-values path {} must be inside procedure directory {}",
                canonical.display(),
                root.display()
            )
            .into());
        }

        let raw = std::fs::read_to_string(&canonical)
            .map_err(|e| format!("Failed to read {}: {e}", canonical.display()))?;
        let inner: HashMap<String, HashMap<String, serde_json::Value>> = serde_json::from_str(&raw)
            .map_err(|e| format!("Failed to parse {}: {e}", canonical.display()))?;
        Ok(Self { inner })
    }

    /// Values pre-baked for a phase, keyed by component key.
    pub fn for_phase(&self, phase_key: &str) -> Option<&HashMap<String, serde_json::Value>> {
        self.inner.get(phase_key)
    }
}
