//! Sanitizes `log.source_file` before upload. The server enforces a
//! 1..=200 length, so paths are normalized and clamped to keep the UI
//! readable and the constraint satisfied.
//!
//! Steps (in order):
//! 1. Strip the Windows long-path prefix (`\\?\`).
//! 2. If the path is inside `procedure_dir`, make it relative — the
//!    deployment dir prefix (`C:\Users\...\.tofupilot\deployments\<uuid>`)
//!    is noise; the user wants to see `phases/binaries.py`.
//! 3. Trim, fall back to "unknown" on empty.
//! 4. Clamp to the trailing 200 chars (filename/line is the actionable
//!    part; leading directory segments drop first).

use std::path::Path;

const MAX_LEN: usize = 200;

pub fn sanitize_source_file(raw: &str, procedure_dir: &Path) -> String {
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(raw);

    // Try to relativize against procedure_dir. Both sides go through the
    // same prefix-strip first so a `\\?\C:\...` procedure_dir matches a
    // bare-prefix raw path. Fall back to the absolute path if the path
    // lives outside the deployment tree (3rd-party libs, traceback
    // entries from stdlib, etc.).
    let dir_str = procedure_dir.to_string_lossy();
    let dir_clean = dir_str
        .strip_prefix(r"\\?\")
        .unwrap_or(&dir_str)
        .trim_end_matches(['/', '\\']);
    let working = if !dir_clean.is_empty() && stripped.starts_with(dir_clean) {
        let rest = &stripped[dir_clean.len()..];
        rest.trim_start_matches(['/', '\\'])
    } else {
        stripped
    };

    let trimmed = working.trim();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }

    if trimmed.chars().count() <= MAX_LEN {
        return trimmed.to_string();
    }
    trimmed
        .chars()
        .rev()
        .take(MAX_LEN)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn pd(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn empty_becomes_unknown() {
        assert_eq!(sanitize_source_file("", &pd("/proc")), "unknown");
        assert_eq!(sanitize_source_file("   ", &pd("/proc")), "unknown");
    }

    #[test]
    fn strips_windows_long_path_prefix() {
        let s = sanitize_source_file(r"\\?\C:\Users\foo\bar.py", &pd("/unrelated"));
        assert_eq!(s, r"C:\Users\foo\bar.py");
    }

    #[test]
    fn relativizes_path_inside_procedure_dir_unix() {
        let s = sanitize_source_file(
            "/home/u/.tofupilot/deployments/abc/proc/phases/binaries.py",
            &pd("/home/u/.tofupilot/deployments/abc/proc"),
        );
        assert_eq!(s, "phases/binaries.py");
    }

    #[test]
    fn relativizes_path_inside_procedure_dir_windows() {
        let s = sanitize_source_file(
            r"\\?\C:\Users\u\.tofupilot\deployments\abc\proc\phases\binaries.py",
            &pd(r"C:\Users\u\.tofupilot\deployments\abc\proc"),
        );
        assert_eq!(s, r"phases\binaries.py");
    }

    #[test]
    fn keeps_absolute_path_outside_procedure_dir() {
        let s = sanitize_source_file(
            "/usr/lib/python3/site-packages/openhtf/core.py",
            &pd("/home/u/proc"),
        );
        assert_eq!(s, "/usr/lib/python3/site-packages/openhtf/core.py");
    }

    #[test]
    fn short_path_unchanged() {
        assert_eq!(
            sanitize_source_file("phases/binaries.py", &pd("/unrelated")),
            "phases/binaries.py"
        );
    }

    #[test]
    fn long_path_keeps_tail() {
        let long = "a".repeat(300);
        let out = sanitize_source_file(&long, &pd("/unrelated"));
        assert_eq!(out.chars().count(), MAX_LEN);
        assert!(out.chars().all(|c| c == 'a'));
    }

    #[test]
    fn long_absolute_path_outside_dir_preserves_filename() {
        let long = format!("/{}/binaries.py", "x".repeat(300));
        let out = sanitize_source_file(&long, &pd("/unrelated"));
        assert!(out.ends_with("binaries.py"));
        assert!(out.chars().count() <= MAX_LEN);
    }

    #[test]
    fn relativize_then_clamp() {
        let inside = format!("/home/u/proc/{}/binaries.py", "deep/nested".repeat(40));
        let out = sanitize_source_file(&inside, &pd("/home/u/proc"));
        assert!(out.ends_with("binaries.py"));
        assert!(out.chars().count() <= MAX_LEN);
        assert!(!out.starts_with("/home/u/proc"));
    }
}
