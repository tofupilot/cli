use super::error::CommandError;
use crate::procedure::schema::{ProcedureDefinition, ProcedureYaml};
use std::collections::HashSet;
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

/// Names of the plugs whose entry states an explicit `scope:` line.
///
/// The loaded definition cannot answer this, by design:
/// `From<PlugDefinitionYaml>` applies the `slot` default on the way in
/// and `PlugDefinition::to_yaml` erases an explicit `slot` on the way
/// out, so an author who wrote the default is indistinguishable from one
/// who omitted it — everywhere except the text. Studio needs the
/// difference to draw an inherited scope as inherited rather than as a
/// choice someone made.
///
/// Tolerant in the same way, and for the same reason, as
/// `procedure_name_from_str`: `scope` is read as an opaque value so a
/// legacy spelling (`each`/`all`/`run`) or an outright invalid one still
/// counts as stated, and every field is optional so a procedure being
/// edited still answers. Unparsable text yields an empty set, which
/// renders as "nothing stated" — the safe direction.
pub fn plugs_with_explicit_scope(content: &str) -> HashSet<String> {
    #[derive(serde::Deserialize)]
    struct JustPlugs {
        #[serde(default)]
        plugs: Vec<JustScope>,
    }
    #[derive(serde::Deserialize)]
    struct JustScope {
        #[serde(default)]
        name: String,
        #[serde(default)]
        scope: Option<serde_yaml::Value>,
    }

    serde_yaml::from_str::<JustPlugs>(content)
        .map(|parsed| {
            parsed
                .plugs
                .into_iter()
                .filter(|p| p.scope.is_some())
                // Trimmed to match `PlugDefinitionYaml.name`, which
                // deserializes through `serde_trim` — an untrimmed key
                // here would never match the projection's plug name.
                .map(|p| p.name.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The definition's own `name:`, read tolerantly: a partial parse that
/// only needs the `name` key, NOT the whole definition to be valid. A
/// procedure mid-edit (broken phases, bad refs) still needs a display
/// label so it can be listed, selected and fixed. `None` when the text
/// is unparsable or the name is empty. The one name-reading mechanism
/// for every consumer — Studio discovery and the hello frame both call
/// it, so "the procedure's display name" cannot drift between them.
pub fn procedure_name_from_str(content: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct JustTheName {
        name: String,
    }
    let parsed: JustTheName = serde_yaml::from_str(content).ok()?;
    let trimmed = parsed.name.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `procedure_name_from_str` over a file. Sync (small files, called
/// from startup paths); async callers read the file themselves and use
/// the `_from_str` form.
pub fn read_procedure_name(file_path: &Path) -> Option<String> {
    procedure_name_from_str(&std::fs::read_to_string(file_path).ok()?)
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

    // Plug keys must be unique too, and case-insensitively so: the worker
    // matches a phase parameter to a plug with `param_name.lower() ==
    // plug_key.lower()` (tp_worker.py), so `DMM` and `dmm` are one binding
    // as far as a phase is concerned. Duplicates have no failure mode a
    // user can read: the scope map keeps the LAST entry
    // (`orchestrator/initialization.rs`) while the config lookup finds the
    // FIRST (`orchestrator/plugs.rs`), so two declared instruments quietly
    // become one process built from one entry and torn down by the other's
    // lifetime. Keys are usually derived from `name`, which makes this
    // easy to hit without noticing — "Power Supply" and "Power-Supply" are
    // two distinct names that derive the same key.
    let mut seen_plug_keys: std::collections::HashMap<String, &str> =
        std::collections::HashMap::new();
    for plug in &procedure_def.plugs {
        if let Some(first) = seen_plug_keys.insert(plug.key.to_lowercase(), plug.name.as_str()) {
            return Err(format!(
                "Duplicate plug key `{}` (plugs `{}` and `{}`) — every plug must have a \
                 unique key, because a phase receives a plug by naming that key as a \
                 parameter. Set an explicit `key:` on one of them",
                plug.key, first, plug.name
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

    /// The distinction the projection cannot make for itself: an
    /// omitted `scope:` and a stated one that happens to name the
    /// default are the same `PlugDefinition` after loading.
    #[test]
    fn explicit_scope_separates_stated_from_inherited() {
        let yaml = r#"
name: P
version: 1.0.0
plugs:
  - name: Inherited
    python: plugs.a:A
  - name: Stated Default
    scope: slot
    python: plugs.b:B
  - name: Stated Other
    scope: station
    python: plugs.c:C
"#;
        let explicit = plugs_with_explicit_scope(yaml);
        assert!(!explicit.contains("Inherited"));
        assert!(explicit.contains("Stated Default"));
        assert!(explicit.contains("Stated Other"));
    }

    /// A legacy spelling is still a stated scope: the projection reports
    /// presence from here and the canonical spelling from the parse, so
    /// `all` must not read as inherited just because it is not the
    /// current word for it.
    #[test]
    fn explicit_scope_counts_legacy_spellings() {
        let yaml = "plugs:\n  - name: Legacy\n    scope: all\n";
        assert!(plugs_with_explicit_scope(yaml).contains("Legacy"));
    }

    /// Unparsable and plugless text answers "nothing stated" rather
    /// than failing: a procedure mid-edit still has to render.
    #[test]
    fn explicit_scope_tolerates_broken_and_plugless_text() {
        assert!(plugs_with_explicit_scope("{{{ not yaml").is_empty());
        assert!(plugs_with_explicit_scope("name: P\n").is_empty());
        // An invalid scope value is still a stated one — the page shows
        // the parse's spelling, and the parse is what rejects it.
        assert!(plugs_with_explicit_scope("plugs:\n  - name: Bad\n    scope: nonsense\n")
            .contains("Bad"));
    }

    /// Names are trimmed to match `PlugDefinitionYaml.name`, which
    /// deserializes through `serde_trim` — an untrimmed key would never
    /// match the plug it belongs to.
    #[test]
    fn explicit_scope_trims_names() {
        let yaml = "plugs:\n  - name: \"  Padded  \"\n    scope: execution\n";
        assert!(plugs_with_explicit_scope(yaml).contains("Padded"));
    }

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

    /// Two names that derive the same key are the realistic way to hit
    /// this — neither entry looks wrong on its own.
    #[test]
    fn duplicate_plug_key_rejected() {
        let err = load_from_str(
            r#"
name: Two Plugs One Key
version: "1.0"
plugs:
  - name: Power Supply
    python: instruments.psu:PowerSupply
  - name: Power-Supply
    python: instruments.psu2:PowerSupply
main:
  - key: p1
    name: P1
    python: phases.p1
"#,
        )
        .expect_err("two plugs deriving `power_supply` must be rejected");
        assert!(
            err.contains("Duplicate plug key `power_supply`")
                && err.contains("Power Supply")
                && err.contains("Power-Supply"),
            "unexpected error: {err}"
        );
    }

    /// The worker matches a parameter to a plug case-insensitively, so
    /// two keys differing only in case are one binding and must not load.
    #[test]
    fn plug_keys_differing_only_in_case_rejected() {
        let err = load_from_str(
            r#"
name: Case Clash
version: "1.0"
plugs:
  - name: Meter A
    key: DMM
    python: instruments.a:A
  - name: Meter B
    key: dmm
    python: instruments.b:B
main:
  - key: p1
    name: P1
    python: phases.p1
"#,
        )
        .expect_err("`DMM` and `dmm` are one binding for a phase");
        assert!(err.contains("Duplicate plug key"), "unexpected error: {err}");
    }

    /// The check must not fire on the ordinary case.
    #[test]
    fn distinct_plug_keys_load() {
        let def = load_from_str(
            r#"
name: Two Plugs
version: "1.0"
plugs:
  - name: Power Supply
    python: instruments.psu:PowerSupply
  - name: Multimeter
    python: instruments.dmm:Multimeter
main:
  - key: p1
    name: P1
    python: phases.p1
"#,
        )
        .expect("distinct keys load");
        assert_eq!(def.plugs.len(), 2);
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
    fn resolve_python_refs_suggests_the_colon_spelling() {
        let dir = std::env::temp_dir().join(format!("tp-loader-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("phases")).unwrap();
        std::fs::create_dir_all(dir.join("plugs")).unwrap();
        std::fs::write(dir.join("phases/main.py"), "def check():\n    pass\n").unwrap();
        std::fs::write(dir.join("plugs/psu.py"), "class PSU:\n    pass\n").unwrap();
        let path = dir.join("procedure.yaml");
        std::fs::write(
            &path,
            r#"
name: Hinted Refs
plugs:
  - name: PSU
    key: psu
    python: plugs.psu.PSU
  - name: Ghost
    key: ghost
    python: plugs.nowhere.Thing
main:
  - key: p1
    name: P1
    python: phases.main.check
"#,
        )
        .unwrap();

        let def = load_procedure_definition(&path).expect("valid structure");
        let problems = def.resolve_python_refs(&dir, None);
        assert_eq!(problems.len(), 3, "unexpected: {problems:?}");
        // The dotted-class spelling whose ':' variant resolves gets the
        // did-you-mean; a spec that is broken either way does not.
        assert!(
            problems[0].contains("did you mean `plugs.psu:PSU`"),
            "got: {}",
            problems[0]
        );
        assert!(
            !problems[1].contains("did you mean"),
            "got: {}",
            problems[1]
        );
        assert!(
            problems[2].contains("did you mean `phases.main:check`"),
            "got: {}",
            problems[2]
        );

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
