//! Auto-fix the labwc touch mouse-emulation default on Raspberry Pi OS.
//!
//! Pi OS Bookworm runs the `labwc` Wayland compositor with
//! mouse-emulation enabled by default, which makes labwc translate
//! every touch event into mouse-down + drag. Chromium then sees a
//! mouse drag and starts selecting text on labels instead of
//! scrolling the kiosk page; sliders and other Radix drag
//! affordances also break because they expect real touch events.
//!
//! Two forms exist in the wild and both must be patched:
//!
//!   1. Per-device attribute on a `<touch>` element (newer Pi OS):
//!      `<touch deviceName="…" mapToOutput="…" mouseEmulation="yes"/>`
//!   2. Global `<mouseEmulation>` child of `<libinput>` (the form
//!      stock Pi OS Bookworm `/etc/xdg/labwc/rc.xml` actually ships
//!      with on most installs we've seen):
//!      `<libinput><mouseEmulation>yes</mouseEmulation></libinput>`
//!
//! The fix is to flip every `mouseEmulation="yes"` on a `<touch>`
//! tag to `"no"`, AND to set `<mouseEmulation>no</mouseEmulation>`
//! inside `<libinput>` (splicing the child in if missing).
//!
//! Operators shouldn't have to know any of this — kiosk mode implies
//! "I want the touchscreen to behave like one." We patch in place
//! when:
//!   * the host is Linux,
//!   * kiosk UI is enabled (caller-gated),
//!   * the user runs labwc (rc.xml exists at the user or system
//!     path), and
//!   * at least one `<touch ...>` declares `mouseEmulation="yes"`.
//!
//! A `.bak.tofupilot` is written once before the first edit so the
//! operator can roll back. Subsequent runs are no-ops.

#![cfg(target_os = "linux")]

use std::path::PathBuf;

use crate::commands::db;
use crate::log;

/// Apply the fix if the host is running labwc and at least one
/// touch device has mouse-emulation enabled. Best-effort: every
/// failure path logs and returns silently — a quirky compositor
/// config must never block station startup.
pub fn apply_if_needed() {
    let Ok(home) = db::home_dir() else { return };
    let user_cfg: PathBuf = home.join(".config/labwc/rc.xml");

    // Prefer the user's copy. If they don't have one, seed from the
    // system default so our edit doesn't strip the operator's other
    // labwc preferences. Skip silently if neither exists — host
    // probably isn't running labwc at all.
    let (contents, seeded_from_system) = if let Ok(c) = std::fs::read_to_string(&user_cfg) {
        (c, false)
    } else {
        let system = PathBuf::from("/etc/xdg/labwc/rc.xml");
        let Ok(c) = std::fs::read_to_string(&system) else {
            return;
        };
        (c, true)
    };

    let Some(updated) = patch_rc_xml(&contents) else {
        return; // No `<touch ... mouseEmulation="yes">` to flip.
    };

    if let Some(parent) = user_cfg.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn(&format!(
                "labwc touch-fix: couldn't create {}: {e}",
                parent.display(),
            ));
            return;
        }
    }

    // One-time backup of the user file. The system-seeded branch
    // writes to a fresh user path, so there's nothing to back up
    // there.
    if !seeded_from_system {
        let backup = user_cfg.with_extension("xml.bak.tofupilot");
        if !backup.exists() {
            if let Err(e) = std::fs::copy(&user_cfg, &backup) {
                log::warn(&format!(
                    "labwc touch-fix: backup to {} failed: {e}",
                    backup.display(),
                ));
                return;
            }
        }
    }

    if let Err(e) = std::fs::write(&user_cfg, updated) {
        log::warn(&format!(
            "labwc touch-fix: writing {} failed: {e}",
            user_cfg.display(),
        ));
        return;
    }

    log::info(&format!(
        "Disabled labwc touch mouse-emulation in {} so touch-drag scrolls.",
        user_cfg.display(),
    ));

    // Best-effort reconfigure. Fails harmlessly if labwc isn't the
    // running compositor; the new value is picked up on next start
    // either way.
    let _ = std::process::Command::new("labwc")
        .arg("-r")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Returns the patched XML if any `mouseEmulation` setting was
/// flipped or spliced in, `None` if nothing matched. Runs both passes
/// (per-`<touch>` attribute + `<libinput>` child) so an rc.xml that
/// uses either form (or both) ends up fully patched.
fn patch_rc_xml(src: &str) -> Option<String> {
    let mut working = src.to_string();
    let mut changed = false;

    if let Some(next) = patch_touch_attrs(&working) {
        working = next;
        changed = true;
    }
    if let Some(next) = patch_libinput_child(&working) {
        working = next;
        changed = true;
    }

    if changed {
        Some(working)
    } else {
        None
    }
}

/// Pass 1: flip every `<touch ... mouseEmulation="yes" ...>` to `"no"`.
fn patch_touch_attrs(src: &str) -> Option<String> {
    let mut out = String::with_capacity(src.len());
    let mut cursor = 0;
    let mut changed = false;

    while let Some(rel) = src[cursor..].find("<touch") {
        let tag_start = cursor + rel;
        let Some(rel_end) = src[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + rel_end + 1;

        let tag = &src[tag_start..tag_end];
        out.push_str(&src[cursor..tag_start]);
        let (rewritten, did_change) = flip_mouse_emulation(tag);
        out.push_str(&rewritten);
        if did_change {
            changed = true;
        }
        cursor = tag_end;
    }

    if !changed {
        return None;
    }
    out.push_str(&src[cursor..]);
    Some(out)
}

/// Pass 2: ensure `<libinput>` carries `<mouseEmulation>no</mouseEmulation>`.
/// Flips an existing `yes` child or splices a new `no` child in. No-op
/// when the libinput block is absent (host probably isn't running labwc
/// with libinput config) or the child is already `no`.
fn patch_libinput_child(src: &str) -> Option<String> {
    if let Some(start) = src.find("<mouseEmulation>") {
        let after = start + "<mouseEmulation>".len();
        let end_rel = src[after..].find("</mouseEmulation>")?;
        let end = after + end_rel;
        let value = src[after..end].trim();
        if value.eq_ignore_ascii_case("no") {
            return None;
        }
        let mut out = String::with_capacity(src.len());
        out.push_str(&src[..after]);
        out.push_str("no");
        out.push_str(&src[end..]);
        return Some(out);
    }

    let libinput_start = src.find("<libinput>")?;
    let insert_at = libinput_start + "<libinput>".len();
    let line_start = src[..libinput_start]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let indent: String = src[line_start..libinput_start]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    let mut out = String::with_capacity(src.len() + 64);
    out.push_str(&src[..insert_at]);
    out.push('\n');
    out.push_str(&indent);
    out.push_str("  <mouseEmulation>no</mouseEmulation>");
    out.push_str(&src[insert_at..]);
    Some(out)
}

/// Within a single `<touch ... />` tag, replace `mouseEmulation="yes"`
/// (case-insensitive on the value) with `mouseEmulation="no"`. Other
/// attributes are left exactly as-is. Returns the rewritten tag and a
/// flag indicating whether anything actually changed.
fn flip_mouse_emulation(tag: &str) -> (String, bool) {
    const ATTR: &str = "mouseEmulation=\"";
    let Some(attr_start) = tag.find(ATTR) else {
        return (tag.to_string(), false);
    };
    let value_start = attr_start + ATTR.len();
    let Some(rel_close) = tag[value_start..].find('"') else {
        return (tag.to_string(), false);
    };
    let value_end = value_start + rel_close;
    let value = &tag[value_start..value_end];
    if !value.eq_ignore_ascii_case("yes") {
        return (tag.to_string(), false);
    }
    let mut out = String::with_capacity(tag.len());
    out.push_str(&tag[..value_start]);
    out.push_str("no");
    out.push_str(&tag[value_end..]);
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::patch_rc_xml;

    #[test]
    fn flips_pi_default_self_closing() {
        let src = r#"<labwc_config>
  <touch deviceName="ADS7846 Touchscreen" mapToOutput="HDMI-A-1" mouseEmulation="yes"/>
</labwc_config>"#;
        let out = patch_rc_xml(src).unwrap();
        assert!(out.contains("mouseEmulation=\"no\""));
        assert!(!out.contains("mouseEmulation=\"yes\""));
    }

    #[test]
    fn flips_multiple_touch_tags() {
        let src = r#"<labwc_config>
  <touch deviceName="A" mouseEmulation="yes"/>
  <touch deviceName="B" mouseEmulation="yes"/>
</labwc_config>"#;
        let out = patch_rc_xml(src).unwrap();
        assert_eq!(out.matches("mouseEmulation=\"no\"").count(), 2);
    }

    #[test]
    fn preserves_other_attributes() {
        let src = r#"<touch deviceName="X" mapToOutput="DSI-1" mouseEmulation="yes"/>"#;
        let out = patch_rc_xml(src).unwrap();
        assert!(out.contains("deviceName=\"X\""));
        assert!(out.contains("mapToOutput=\"DSI-1\""));
        assert!(out.contains("mouseEmulation=\"no\""));
    }

    #[test]
    fn no_change_when_already_no() {
        let src = r#"<touch deviceName="X" mouseEmulation="no"/>"#;
        assert!(patch_rc_xml(src).is_none());
    }

    #[test]
    fn no_change_when_no_touch_tag() {
        let src = r#"<labwc_config>
  <theme name="default"/>
</labwc_config>"#;
        assert!(patch_rc_xml(src).is_none());
    }

    #[test]
    fn flips_libinput_mouse_emulation_child() {
        // The form stock Pi OS Bookworm rc.xml ships with.
        let src = r#"<labwc_config>
  <libinput>
    <mouseEmulation>yes</mouseEmulation>
  </libinput>
</labwc_config>"#;
        let out = patch_rc_xml(src).unwrap();
        assert!(out.contains("<mouseEmulation>no</mouseEmulation>"));
        assert!(!out.contains("<mouseEmulation>yes</mouseEmulation>"));
    }

    #[test]
    fn splices_libinput_child_when_missing() {
        // `<libinput>` block present but no `<mouseEmulation>` child —
        // labwc defaults to `yes`, so we splice `no` in.
        let src = r#"<labwc_config>
  <libinput>
    <other/>
  </libinput>
</labwc_config>"#;
        let out = patch_rc_xml(src).unwrap();
        assert!(out.contains("<mouseEmulation>no</mouseEmulation>"));
    }

    #[test]
    fn libinput_already_no_no_change() {
        let src = r#"<libinput>
  <mouseEmulation>no</mouseEmulation>
</libinput>"#;
        assert!(patch_rc_xml(src).is_none());
    }

    #[test]
    fn patches_both_forms_in_one_file() {
        let src = r#"<labwc_config>
  <libinput>
    <mouseEmulation>yes</mouseEmulation>
  </libinput>
  <touch deviceName="X" mouseEmulation="yes"/>
</labwc_config>"#;
        let out = patch_rc_xml(src).unwrap();
        assert!(out.contains("<mouseEmulation>no</mouseEmulation>"));
        assert!(out.contains("mouseEmulation=\"no\""));
        assert!(!out.contains("mouseEmulation=\"yes\""));
        assert!(!out.contains("<mouseEmulation>yes</mouseEmulation>"));
    }
}
