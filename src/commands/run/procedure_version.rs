//! Reads a procedure's Python runtime version from its `pyproject.toml`
//! (PEP 621 `requires-python` or Poetry), with a sensible default.

use std::path::Path;

/// Read `[project].version` (PEP 621) from `<procedure_dir>/pyproject.toml`.
/// Falls back to `[tool.poetry].version` for legacy Poetry-style projects.
/// Returns `None` if the file is missing, unparseable, or has no version.
pub fn read_procedure_version(procedure_dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(procedure_dir.join("pyproject.toml")).ok()?;
    let parsed: toml::Value = content.parse().ok()?;

    let pep621 = parsed
        .get("project")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str());

    let poetry = parsed
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str());

    pep621
        .or(poetry)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_pyproject(dir: &Path, content: &str) {
        fs::write(dir.join("pyproject.toml"), content).unwrap();
    }

    #[test]
    fn reads_pep621_version() {
        let d = tempdir().unwrap();
        write_pyproject(d.path(), "[project]\nname = \"foo\"\nversion = \"1.2.3\"\n");
        assert_eq!(read_procedure_version(d.path()).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn reads_poetry_version_fallback() {
        let d = tempdir().unwrap();
        write_pyproject(
            d.path(),
            "[tool.poetry]\nname = \"foo\"\nversion = \"0.4.0\"\n",
        );
        assert_eq!(read_procedure_version(d.path()).as_deref(), Some("0.4.0"));
    }

    #[test]
    fn pep621_wins_over_poetry() {
        let d = tempdir().unwrap();
        write_pyproject(
            d.path(),
            "[project]\nversion = \"2.0.0\"\n[tool.poetry]\nversion = \"1.0.0\"\n",
        );
        assert_eq!(read_procedure_version(d.path()).as_deref(), Some("2.0.0"));
    }

    #[test]
    fn missing_file_returns_none() {
        let d = tempdir().unwrap();
        assert!(read_procedure_version(d.path()).is_none());
    }

    #[test]
    fn missing_version_field_returns_none() {
        let d = tempdir().unwrap();
        write_pyproject(d.path(), "[project]\nname = \"foo\"\n");
        assert!(read_procedure_version(d.path()).is_none());
    }

    #[test]
    fn empty_version_returns_none() {
        let d = tempdir().unwrap();
        write_pyproject(d.path(), "[project]\nversion = \"   \"\n");
        assert!(read_procedure_version(d.path()).is_none());
    }

    #[test]
    fn malformed_toml_returns_none() {
        let d = tempdir().unwrap();
        write_pyproject(d.path(), "this is not [valid toml");
        assert!(read_procedure_version(d.path()).is_none());
    }
}
