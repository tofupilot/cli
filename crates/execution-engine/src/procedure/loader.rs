use super::error::CommandError;
use crate::procedure::schema::{ProcedureDefinition, ProcedureYaml};
use std::path::Path;
use validator::Validate;

fn validate_file_path(path: &Path) -> Result<(), CommandError> {
    if !path.exists() {
        return Err(CommandError::file_not_found(path.display()));
    }

    let extension = path.extension().and_then(|e| e.to_str()).ok_or_else(|| {
        CommandError::new(
            super::error::ErrorCode::InvalidFileExtension,
            "File has no extension",
        )
    })?;

    // Case-insensitive: the CLI's yaml-hint check accepts `.YML`/`.YAML`
    // and routes the file here, so the loader must agree.
    let extension = extension.to_ascii_lowercase();
    if extension != "yaml" && extension != "yml" {
        return Err(CommandError::new(
            super::error::ErrorCode::InvalidFileExtension,
            "File must be a YAML file (.yaml or .yml)",
        ));
    }

    Ok(())
}

#[must_use = "procedure definition should be checked for validation errors"]
pub fn load_procedure_definition(file_path: &Path) -> Result<ProcedureDefinition, String> {
    validate_file_path(file_path).map_err(|e| e.message)?;

    let content = std::fs::read_to_string(file_path)
        .map_err(|e| format!("Failed to read {}: {}", file_path.display(), e))?;

    load_procedure_definition_from_str(&content)
}

/// Same parse + validation chain as `load_procedure_definition`, on
/// content that is not (or not yet) on disk. Validation is purely
/// structural — nothing after the file read touches the filesystem —
/// so proposed content can be checked before it is written.
#[must_use = "procedure definition should be checked for validation errors"]
pub fn load_procedure_definition_from_str(content: &str) -> Result<ProcedureDefinition, String> {
    let raw: ProcedureYaml =
        serde_yaml::from_str(content).map_err(|e| format!("Failed to parse YAML: {}", e))?;

    let procedure_def = ProcedureDefinition::from(raw);

    procedure_def
        .validate()
        .map_err(|e| format!("Validation failed: {}", e))?;

    if let Some(unit) = &procedure_def.unit {
        unit.validate_auto_identify()
            .map_err(|e| format!("Validation failed: {}", e))?;

        if let Some(md) = &unit.metadata {
            crate::procedure::schema::validate_metadata_keys(
                md.keys().map(|k| k.as_str()),
                "unit.metadata",
            )
            .map_err(|e| format!("Validation failed: {}", e))?;
        }
    }

    for (_, phase) in procedure_def.get_all_phases_with_stage_scope() {
        phase.validate_single_runtime()?;
        if let Some(ui) = &phase.ui {
            if let Some(components) = &ui.components {
                for comp in components {
                    comp.validate_width()?;
                    comp.validate_aspect()?;
                    comp.validate_fit()?;
                    comp.validate_options_count()?;

                    // Option-driven components become "choose nothing from
                    // nothing" at runtime if the options list is empty or
                    // missing. Catch it at load so the error points to the
                    // authoring bug, not to a silent pass 10 phases later.
                    use crate::procedure::schema::UIComponentType as T;
                    let needs_options = matches!(
                        comp.component_type,
                        T::Radio | T::Select | T::Multiselect | T::Checklist
                    );
                    if needs_options {
                        let empty = comp.options.as_ref().map(|o| o.is_empty()).unwrap_or(true);
                        if empty {
                            return Err(format!(
                                "UI component `{}` (type `{:?}`) requires a non-empty `options` list",
                                comp.key, comp.component_type,
                            ));
                        }
                    }
                }
            }
        }
    }

    // `scope: station` is a plug lifetime, not a phase grouping — a phase
    // can't run "across runs". Phases silently coerce non-`all` scopes to
    // per-slot (`iter_phases_with_stage`), so without this check a
    // station-scoped phase would quietly behave as `slot`. Catch it at
    // load with an error that points at the authoring bug.
    for phase in procedure_def
        .setup
        .iter()
        .chain(procedure_def.main.iter())
        .chain(procedure_def.teardown.iter())
    {
        if phase.scope == Some(crate::procedure::schema::Scope::Station) {
            return Err(format!(
                "Phase `{}` has `scope: station`, which is only valid on plugs. \
                 Use `scope: execution` (execution-wide) or `scope: slot` (per-slot) on phases",
                phase.key
            ));
        }
    }

    // A procedure with no main phases isn't a test — the runner would exit
    // PASS without doing anything, which is misleading.
    if procedure_def.main.is_empty() {
        return Err("Procedure has no `main` phases — at least one is required".into());
    }

    // Phase keys must be unique across `main`. Duplicates corrupt the
    // dependency graph and produce phantom duplicate phase events at runtime
    // because the scheduler indexes jobs by key.
    let mut seen_keys: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for phase in &procedure_def.main {
        if !seen_keys.insert(phase.key.as_str()) {
            return Err(format!(
                "Duplicate phase key `{}` — every main phase must have a unique key",
                phase.key
            ));
        }
    }

    // `depends_on` must reference phase keys that exist in the procedure.
    // Silently ignoring unknown dependencies lets a typo mask broken ordering.
    let known_keys: std::collections::HashSet<&str> =
        procedure_def.main.iter().map(|p| p.key.as_str()).collect();
    for phase in &procedure_def.main {
        for dep in &phase.depends_on {
            if !known_keys.contains(dep.as_str()) {
                return Err(format!(
                    "Phase `{}` depends on unknown phase `{}` (known phases: {})",
                    phase.key,
                    dep,
                    known_keys.iter().copied().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }

    // Belt: explicit self-reference pre-check. Three-color DFS below
    // catches this too, but the error text is clearer when handled
    // directly ("depends on itself" vs "a -> a").
    for phase in &procedure_def.main {
        if phase.depends_on.iter().any(|d| d == &phase.key) {
            return Err(format!("Phase `{}` depends on itself", phase.key));
        }
    }

    // Braces: full cycle detection in the dependency graph.
    if let Some(cycle) = find_dependency_cycle(&procedure_def.main) {
        return Err(format!(
            "Circular dependency detected in `depends_on`: {}",
            cycle.join(" -> ")
        ));
    }

    Ok(procedure_def)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_from_str(yaml: &str) -> Result<ProcedureDefinition, String> {
        let dir = std::env::temp_dir().join(format!(
            "tp-loader-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("procedure.yaml");
        std::fs::write(&path, yaml).unwrap();
        let result = load_procedure_definition(&path);
        std::fs::remove_dir_all(&dir).ok();
        result
    }

    #[test]
    fn version_may_be_omitted() {
        let result = load_from_str(
            r#"
name: No Version
main:
  - key: p1
    name: P1
    python: phases.p1
"#,
        );
        let def = result.expect("version is optional");
        assert_eq!(def.version, "");
    }

    #[test]
    fn blank_version_loads_as_empty() {
        let result = load_from_str(
            r#"
name: Blank Version
version: "   "
main:
  - key: p1
    name: P1
    python: phases.p1
"#,
        );
        let def = result.expect("a whitespace-only version trims to empty and still loads");
        assert_eq!(def.version, "");
    }

    #[test]
    fn version_over_50_chars_rejected() {
        let result = load_from_str(&format!(
            r#"
name: Long Version
version: "{}"
main:
  - key: p1
    name: P1
    python: phases.p1
"#,
            "1".repeat(51)
        ));
        let err = result.expect_err("a version over 50 chars must still be rejected");
        assert!(err.contains("Validation failed"), "unexpected error: {err}");
    }

    #[test]
    fn station_scope_on_plug_loads() {
        let result = load_from_str(
            r#"
name: Station Plug OK
version: "1.0"
plugs:
  - name: Power Supply
    python: instruments.psu:PowerSupply
    scope: station
main:
  - key: p1
    name: P1
    python: phases.p1
"#,
        );
        let def = result.expect("station scope on a plug is valid");
        assert!(def.plugs[0].scope_is_station());
    }

    #[test]
    fn station_scope_on_phase_rejected() {
        for stage in ["setup", "teardown"] {
            let result = load_from_str(&format!(
                r#"
name: Station Phase Bad
version: "1.0"
{stage}:
  - key: s1
    name: S1
    python: phases.s1
    scope: station
main:
  - key: p1
    name: P1
    python: phases.p1
"#
            ));
            let err = result.expect_err("station scope on a phase must be rejected");
            assert!(
                err.contains("only valid on plugs"),
                "unexpected error for {stage}: {err}"
            );
        }
    }

    #[test]
    fn resolve_python_refs_reports_dangling_refs_loading_lets_through() {
        let dir = std::env::temp_dir().join(format!("tp-loader-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("phases")).unwrap();
        std::fs::create_dir_all(dir.join("plugs")).unwrap();
        std::fs::write(dir.join("phases/main.py"), "def check():\n    pass\n").unwrap();
        std::fs::write(dir.join("plugs/psu.py"), "class PSU:\n    pass\n").unwrap();
        let path = dir.join("procedure.yaml");
        // Both refs use the dotted-callable spelling of the 2026-08-13
        // incident: every dot is a directory, so they resolve to
        // plugs/psu/PSU.py and phases/main/check.py — neither exists.
        std::fs::write(
            &path,
            r#"
name: Dangling Refs
plugs:
  - name: PSU
    key: psu
    python: plugs.psu.PSU
main:
  - key: p1
    name: P1
    python: phases.main.check
"#,
        )
        .unwrap();

        let def = load_procedure_definition(&path)
            .expect("structural loading must let dangling refs through");
        let problems = def.resolve_python_refs(&dir, None);
        assert_eq!(problems.len(), 2, "unexpected: {problems:?}");
        assert!(problems[0].starts_with("Plug `psu`"), "got: {}", problems[0]);
        assert!(problems[1].starts_with("Phase `p1`"), "got: {}", problems[1]);
        for p in &problems {
            assert!(p.contains("Python file not found"), "got: {p}");
        }

        // The ':' spelling resolves both against the same files.
        std::fs::write(
            &path,
            r#"
name: Resolving Refs
plugs:
  - name: PSU
    key: psu
    python: plugs.psu:PSU
main:
  - key: p1
    name: P1
    python: phases.main:check
"#,
        )
        .unwrap();
        let def = load_procedure_definition(&path).expect("valid procedure");
        assert!(def.resolve_python_refs(&dir, None).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_python_refs_mirrors_the_runtime_not_stricter() {
        let dir = std::env::temp_dir().join(format!("tp-loader-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("phases")).unwrap();
        std::fs::write(dir.join("phases/ok.py"), "def ok():\n    pass\n").unwrap();
        let path = dir.join("procedure.yaml");
        std::fs::write(
            &path,
            r#"
name: Runtime Parity
plugs:
  - name: DMM
    key: dmm
    python: plugs.dmm:DMM
main:
  - key: ok
    name: OK
    python: phases.ok
  - key: wheel
    name: Wheel
    python: shared.phases:check
  - key: off_
    name: Off
    python: phases.gone
    enabled: false
  - key: broken
    name: Broken
    python: phases.gone
"#,
        )
        .unwrap();
        let def = load_procedure_definition(&path).expect("valid procedure");

        // `shared.phases:check` has no `shared/` dir in the tree, so it may
        // resolve through tp_worker's importlib fallback (workspace wheel):
        // not the gate's call. `phases.gone` IS tree-bound (phases/ exists)
        // but its phase is disabled — no job, no gate. On a full run the
        // dangling `plugs/dmm.py` gates too: every declared plug is built.
        let problems = def.resolve_python_refs(&dir, None);
        assert_eq!(problems.len(), 2, "unexpected: {problems:?}");
        assert!(problems[0].starts_with("Plug `dmm`"), "got: {}", problems[0]);
        assert!(problems[1].starts_with("Phase `broken`"), "got: {}", problems[1]);

        // Partial run on `ok`: `broken` is outside the dependency closure,
        // and plugs don't gate at all (the runtime narrows the plug set by
        // signature introspection, so `dmm` would never be built) — the
        // same procedure starts.
        let filter: std::collections::HashSet<String> = ["ok".to_string()].into_iter().collect();
        assert!(def.resolve_python_refs(&dir, Some(&filter)).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_phase_scope_all_still_loads() {
        let result = load_from_str(
            r#"
name: Legacy Scope
version: "1.0"
setup:
  - key: s1
    name: S1
    python: phases.s1
    scope: all
main:
  - key: p1
    name: P1
    python: phases.p1
"#,
        );
        let def = result.expect("legacy `scope: all` on a setup phase still loads");
        use crate::procedure::schema::StageScope;
        let (stage, _) = def
            .get_all_phases_with_stage_scope()
            .into_iter()
            .next()
            .unwrap();
        assert!(matches!(stage, StageScope::SetupAll));
    }
}

/// DFS with three-color marking. Returns the cycle as an ordered list of
/// phase keys if one exists.
fn find_dependency_cycle(
    phases: &[crate::procedure::schema::PhaseDefinition],
) -> Option<Vec<String>> {
    use std::collections::HashMap;

    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    let by_key: HashMap<&str, &crate::procedure::schema::PhaseDefinition> =
        phases.iter().map(|p| (p.key.as_str(), p)).collect();
    let mut color: HashMap<&str, Color> = phases
        .iter()
        .map(|p| (p.key.as_str(), Color::White))
        .collect();

    fn dfs<'a>(
        node: &'a str,
        by_key: &HashMap<&'a str, &'a crate::procedure::schema::PhaseDefinition>,
        color: &mut HashMap<&'a str, Color>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        color.insert(node, Color::Gray);
        stack.push(node);
        if let Some(phase) = by_key.get(node) {
            for dep in &phase.depends_on {
                let dep_s: &str = dep.as_str();
                match color.get(dep_s).copied().unwrap_or(Color::White) {
                    Color::White => {
                        if let Some(c) = dfs(dep_s, by_key, color, stack) {
                            return Some(c);
                        }
                    }
                    Color::Gray => {
                        // Cycle: rewind the stack to where the cycle starts.
                        let start = stack.iter().position(|n| *n == dep_s).unwrap_or(0);
                        let mut cycle: Vec<String> =
                            stack[start..].iter().map(|s| s.to_string()).collect();
                        cycle.push(dep_s.to_string());
                        return Some(cycle);
                    }
                    Color::Black => {}
                }
            }
        }
        stack.pop();
        color.insert(node, Color::Black);
        None
    }

    for phase in phases {
        if color
            .get(phase.key.as_str())
            .copied()
            .unwrap_or(Color::White)
            == Color::White
        {
            let mut stack = Vec::new();
            if let Some(cycle) = dfs(phase.key.as_str(), &by_key, &mut color, &mut stack) {
                return Some(cycle);
            }
        }
    }
    None
}
