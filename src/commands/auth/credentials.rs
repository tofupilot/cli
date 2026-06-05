//! Persisted credentials at `~/.tofupilot/credentials.json` (API key, base
//! URL, org, optional installation id) with restrictive file permissions.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub api_key: String,
    pub base_url: String,
    pub organization_slug: String,
    #[serde(default)]
    pub installation_id: Option<String>,
}

impl Credentials {
    /// `base_url` with all trailing `/` stripped, so a stray `//` in
    /// stored creds collapses cleanly when joined to a path.
    pub fn base(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }
}

/// Filesystem path to the persisted credentials JSON. Public so the
/// uninstaller can target it without re-deriving the path layout.
/// Falls back to `~/.tofupilot/credentials.json` when `tofupilot_dir`
/// can't be created (read-only home, no `$HOME`); the surrounding
/// load/save calls already tolerate IO failures, so a non-canonical
/// path here just means they no-op rather than panic.
pub fn credentials_path() -> PathBuf {
    super::super::db::tofupilot_dir()
        .unwrap_or_else(|_| PathBuf::from(".tofupilot"))
        .join("credentials.json")
}

pub fn save(creds: &Credentials) -> crate::error::CliResult<()> {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_string_pretty(creds)?)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    // Windows: file inherits the parent's ACL. On a default home dir
    // that's usually `Users:R` (read-only to peers), but a domain-
    // joined machine or an SMB-redirected profile can expose the
    // credentials file fleet-wide. Lock the ACL to the current user
    // only via icacls, mirroring chmod 0600 on Unix.
    #[cfg(windows)]
    {
        let _ = restrict_windows_acl(&path);
    }

    Ok(())
}

#[cfg(windows)]
fn restrict_windows_acl(path: &std::path::Path) -> crate::error::CliResult<()> {
    use std::process::Command;
    let user = std::env::var("USERNAME").map_err(|e| format!("USERNAME env not set: {e}"))?;
    // /inheritance:r drops inherited ACEs; /grant grants the current
    // user full control. Combined: only the current user can read.
    let output = Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant")
        .arg(format!("{user}:F"))
        .output()
        .map_err(|e| format!("Run icacls: {e}"))?;
    if !output.status.success() {
        return Err(format!("icacls failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(())
}

pub fn load() -> Option<Credentials> {
    let path = credentials_path();
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Canonical "not logged in" message. Centralized so a UX tweak lands
/// in one place across every command that requires auth.
pub const NOT_LOGGED_IN: &str = "Not logged in. Run `tofupilot login` first.";

/// `load()` + canonical error for commands that strictly require auth. The
/// error carries [`NOT_LOGGED_IN`]; callers typically log it and exit.
pub fn require() -> crate::error::CliResult<Credentials> {
    load().ok_or_else(|| crate::error::CliError::msg(NOT_LOGGED_IN))
}

pub fn clear() -> crate::error::CliResult<()> {
    let path = credentials_path();
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(base_url: &str) -> Credentials {
        Credentials {
            api_key: "k".into(),
            base_url: base_url.into(),
            organization_slug: "org".into(),
            installation_id: None,
        }
    }

    #[test]
    fn base_strips_single_trailing_slash() {
        assert_eq!(creds("https://x.app/").base(), "https://x.app");
    }

    #[test]
    fn base_collapses_repeated_trailing_slashes() {
        assert_eq!(creds("https://x.app///").base(), "https://x.app");
    }

    #[test]
    fn base_leaves_clean_url_untouched() {
        assert_eq!(creds("https://x.app").base(), "https://x.app");
    }

    #[test]
    fn installation_id_defaults_to_none_when_absent() {
        let json = r#"{"api_key":"k","base_url":"https://x.app","organization_slug":"org"}"#;
        let parsed: Credentials = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.installation_id, None);
    }

    #[test]
    fn credentials_round_trip_through_json() {
        let c = Credentials {
            installation_id: Some("inst_1".into()),
            ..creds("https://x.app")
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: Credentials = serde_json::from_str(&json).unwrap();
        assert_eq!(back.installation_id.as_deref(), Some("inst_1"));
        assert_eq!(back.base(), "https://x.app");
    }
}
