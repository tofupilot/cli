//! The procedure version recorded with a run. Each framework reads its
//! own manifest: a YAML procedure declares `version:` in its own file,
//! while the connectors (OpenHTF, pytest, robot, plain Python) have no
//! TofuPilot manifest and fall back to `pyproject.toml`. The two never
//! overlap — `Framework::detect` returns `Yaml` whenever a procedure
//! file exists, so a connector run has none to read.
//!
//! `pyproject.toml` used to serve both, which broke monorepos: it is
//! per-project, so a procedure in a subdirectory found nothing beside
//! it and recorded no version at all.

use std::path::Path;

/// Version a YAML procedure declares, read from the file being run.
///
/// The caller passes the path rather than letting this re-derive it: a
/// manifest `entry_point` can name any `.yaml`, so a deployed procedure
/// is not always called `procedure.yaml`. Re-deriving would miss those.
///
/// Goes through the engine's loader so it cannot drift from the schema
/// the run is validated against. A file the engine rejects yields no
/// version — the run fails on the same error, so a version here would
/// describe a procedure that never loaded.
pub fn read_yaml_version(procedure_yaml: &Path) -> Option<String> {
    let definition =
        execution_engine::procedure::loader::load_procedure_definition(procedure_yaml).ok()?;

    let version = definition.version.trim().to_string();
    Some(version).filter(|s| !s.is_empty())
}

/// Read `[project].version` (PEP 621) from `<procedure_dir>/pyproject.toml`,
/// falling back to `[tool.poetry].version` for Poetry-style projects. Used
/// by the framework connectors, which have no procedure file of their own.
pub fn read_pyproject_version(procedure_dir: &Path) -> Option<String> {
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

    const MAIN: &str = "main:\n  - name: P\n    python: phases.p\n";

    fn write_pyproject(dir: &Path, content: &str) {
        fs::write(dir.join("pyproject.toml"), content).unwrap();
    }

    /// Writes a procedure file and returns its path, mirroring what
    /// `Framework::detect` hands the run.
    fn write_procedure(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("{body}{MAIN}")).unwrap();
        path
    }

    // --- YAML procedures -------------------------------------------------

    #[test]
    fn reads_yaml_version() {
        let d = tempdir().unwrap();
        let y = write_procedure(d.path(), "procedure.yaml", "name: Test\nversion: 3.1.4\n");
        assert_eq!(read_yaml_version(&y).as_deref(), Some("3.1.4"));
    }

    #[test]
    fn yaml_wins_over_pyproject() {
        let d = tempdir().unwrap();
        let y = write_procedure(d.path(), "procedure.yaml", "name: Test\nversion: 3.1.4\n");
        write_pyproject(d.path(), "[project]\nversion = \"1.2.3\"\n");
        assert_eq!(read_yaml_version(&y).as_deref(), Some("3.1.4"));
    }

    /// A manifest `entry_point` can name any `.yaml`. The version must
    /// come from that file, not from the pyproject.toml beside it.
    #[test]
    fn custom_named_yaml_is_read() {
        let d = tempdir().unwrap();
        let y = write_procedure(d.path(), "press.yaml", "name: Test\nversion: 4.2.0\n");
        write_pyproject(d.path(), "[project]\nversion = \"1.2.3\"\n");
        assert_eq!(read_yaml_version(&y).as_deref(), Some("4.2.0"));
    }

    /// A YAML procedure that omits the version records none, rather than
    /// silently inheriting the Python project's. The two describe
    /// different things.
    #[test]
    fn yaml_without_version_does_not_fall_back() {
        let d = tempdir().unwrap();
        let y = write_procedure(d.path(), "procedure.yaml", "name: Test\n");
        write_pyproject(d.path(), "[project]\nversion = \"1.2.3\"\n");
        assert!(read_yaml_version(&y).is_none());
    }

    #[test]
    fn yml_extension_is_read() {
        let d = tempdir().unwrap();
        let y = write_procedure(d.path(), "procedure.yml", "name: Test\nversion: 0.9\n");
        assert_eq!(read_yaml_version(&y).as_deref(), Some("0.9"));
    }

    /// A file the engine rejects yields no version: the run fails on the
    /// same parse error, so a version here would describe a procedure
    /// that never loaded.
    #[test]
    fn unparseable_yaml_yields_no_version() {
        let d = tempdir().unwrap();
        let y = d.path().join("procedure.yaml");
        fs::write(&y, "name: Test\nversion: 2.0\nnot_a_real_field: x\n").unwrap();
        write_pyproject(d.path(), "[project]\nversion = \"1.2.3\"\n");
        assert!(read_yaml_version(&y).is_none());
    }

    #[test]
    fn blank_yaml_version_does_not_fall_back() {
        let d = tempdir().unwrap();
        let y = write_procedure(d.path(), "procedure.yaml", "name: Test\nversion: \"   \"\n");
        write_pyproject(d.path(), "[project]\nversion = \"1.2.3\"\n");
        assert!(read_yaml_version(&y).is_none());
    }

    /// A manifest `entry_point` is joined without an existence check, so
    /// the path can point at nothing. The run fails on the missing file;
    /// reporting the Python project's version instead would attribute a
    /// version to a procedure that never ran.
    #[test]
    fn missing_yaml_yields_no_version() {
        let d = tempdir().unwrap();
        write_pyproject(d.path(), "[project]\nversion = \"1.2.3\"\n");
        let missing = d.path().join("procedure.yaml");
        assert!(read_yaml_version(&missing).is_none());
    }

    // --- Connector procedures --------------------------------------------

    /// Connectors read pyproject.toml and nothing else. A stray
    /// procedure.yaml must not change what they report — in practice
    /// `Framework::detect` makes this unreachable, so the test pins the
    /// contract rather than a live case.
    #[test]
    fn connector_ignores_a_stray_yaml() {
        let d = tempdir().unwrap();
        write_procedure(d.path(), "procedure.yaml", "name: Test\nversion: 9.9.9\n");
        write_pyproject(d.path(), "[project]\nversion = \"1.2.3\"\n");
        assert_eq!(read_pyproject_version(d.path()).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn reads_pep621_version() {
        let d = tempdir().unwrap();
        write_pyproject(d.path(), "[project]\nname = \"foo\"\nversion = \"1.2.3\"\n");
        assert_eq!(read_pyproject_version(d.path()).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn reads_poetry_version_fallback() {
        let d = tempdir().unwrap();
        write_pyproject(
            d.path(),
            "[tool.poetry]\nname = \"foo\"\nversion = \"0.4.0\"\n",
        );
        assert_eq!(read_pyproject_version(d.path()).as_deref(), Some("0.4.0"));
    }

    #[test]
    fn pep621_wins_over_poetry() {
        let d = tempdir().unwrap();
        write_pyproject(
            d.path(),
            "[project]\nversion = \"2.0.0\"\n[tool.poetry]\nversion = \"1.0.0\"\n",
        );
        assert_eq!(read_pyproject_version(d.path()).as_deref(), Some("2.0.0"));
    }

    #[test]
    fn missing_file_returns_none() {
        let d = tempdir().unwrap();
        assert!(read_pyproject_version(d.path()).is_none());
    }

    #[test]
    fn missing_version_field_returns_none() {
        let d = tempdir().unwrap();
        write_pyproject(d.path(), "[project]\nname = \"foo\"\n");
        assert!(read_pyproject_version(d.path()).is_none());
    }

    #[test]
    fn empty_version_returns_none() {
        let d = tempdir().unwrap();
        write_pyproject(d.path(), "[project]\nversion = \"   \"\n");
        assert!(read_pyproject_version(d.path()).is_none());
    }

    #[test]
    fn malformed_toml_returns_none() {
        let d = tempdir().unwrap();
        write_pyproject(d.path(), "this is not [valid toml");
        assert!(read_pyproject_version(d.path()).is_none());
    }
}
