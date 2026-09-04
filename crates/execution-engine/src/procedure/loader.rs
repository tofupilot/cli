use super::error::CommandError;
use crate::procedure::schema::{ProcedureDefinition, ProcedureYaml, RefList, RefLocation};
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
    load_procedure_definition_from_str_located(content).map_err(join_messages)
}

/// The one-string form of a refusal: every collected rule failure, one
/// per line, so a caller printing the error shows all of them.
fn join_messages(errors: Vec<LoadError>) -> String {
    errors
        .into_iter()
        .map(|e| e.message)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A refusal to load, with where it points when the loader knows: the
/// YAML position of a parse error, or the entry a post-parse rule refused
/// (a duplicate key, a bad `depends_on`). Rules that judge the whole file
/// carry neither. `message` alone is what `load_procedure_definition`
/// callers see; Studio uses the rest to underline the right line.
///
/// A parse or schema failure is returned alone — nothing after it can be
/// judged. The post-parse rules are all evaluated and returned together,
/// so fixing one does not reveal the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub message: String,
    /// 1-based, serde_yaml's own position of a parse error.
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub location: Option<RefLocation>,
}

impl LoadError {
    fn text(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            line: None,
            column: None,
            location: None,
        }
    }
    fn at(message: impl Into<String>, location: RefLocation) -> Self {
        Self {
            message: message.into(),
            line: None,
            column: None,
            location: Some(location),
        }
    }
}

impl From<String> for LoadError {
    fn from(message: String) -> Self {
        Self::text(message)
    }
}

/// `load_procedure_definition` over a file, keeping positions.
pub fn load_procedure_definition_located(
    file_path: &Path,
) -> Result<ProcedureDefinition, Vec<LoadError>> {
    let content = std::fs::read_to_string(file_path).map_err(|e| {
        vec![LoadError::text(format!(
            "Failed to read {}: {}",
            file_path.display(),
            e
        ))]
    })?;
    load_procedure_definition_from_str_located(&content)
}

/// `load_procedure_definition_from_str`, keeping positions. Refuses the
/// definition when any rule failed — the runner's contract.
pub fn load_procedure_definition_from_str_located(
    content: &str,
) -> Result<ProcedureDefinition, Vec<LoadError>> {
    let outcome = inspect_procedure_definition(content);
    match (outcome.definition, outcome.errors.is_empty()) {
        (Some(def), true) => Ok(def),
        (_, _) => Err(outcome.errors),
    }
}

/// Everything the loader can say about a procedure text: the definition
/// whenever the text PARSED, and every schema or rule failure found on it.
///
/// Loading is a refusal ("can the runner execute this?"); inspecting is
/// not. A duplicate phase key makes a procedure unrunnable, but the
/// struct behind it is fully built and safe to look at — so Studio lints
/// it and shows the duplicate next to the dangling `python:` three lines
/// below, instead of revealing one problem per fix. Only a parse failure
/// leaves nothing to inspect.
pub struct LoadOutcome {
    pub definition: Option<ProcedureDefinition>,
    pub errors: Vec<LoadError>,
}

/// `inspect_procedure_definition` over a file; an unreadable file is a
/// single error with no definition.
pub fn inspect_procedure_definition_file(file_path: &Path) -> LoadOutcome {
    match std::fs::read_to_string(file_path) {
        Ok(content) => inspect_procedure_definition(&content),
        Err(e) => LoadOutcome {
            definition: None,
            errors: vec![LoadError::text(format!(
                "Failed to read {}: {}",
                file_path.display(),
                e
            ))],
        },
    }
}

pub fn inspect_procedure_definition(content: &str) -> LoadOutcome {
    let raw: ProcedureYaml = match serde_yaml::from_str(content) {
        Ok(raw) => raw,
        Err(e) => {
            return LoadOutcome {
                definition: None,
                errors: vec![LoadError {
                    message: format!("Failed to parse YAML: {}", e),
                    line: e.location().map(|l| l.line() as u32),
                    column: e.location().map(|l| l.column() as u32),
                    location: None,
                }],
            }
        }
    };

    let procedure_def = ProcedureDefinition::from(raw);

    // Every check below is evaluated; the failures are returned together.
    let mut errors: Vec<LoadError> = Vec::new();

    if let Err(e) = procedure_def.validate() {
        errors.push(LoadError::text(format!("Validation failed: {}", e)));
    }

    if let Some(unit) = &procedure_def.unit {
        if let Err(e) = unit.validate_auto_identify() {
            errors.push(LoadError::text(format!("Validation failed: {}", e)));
        }

        if let Err(e) = unit.validate_affixes() {
            errors.push(LoadError::text(format!("Validation failed: {}", e)));
        }

        if let Some(md) = &unit.metadata {
            if let Err(e) = crate::procedure::schema::validate_metadata_keys(
                md.keys().map(|k| k.as_str()),
                "unit.metadata",
            ) {
                errors.push(LoadError::text(format!("Validation failed: {}", e)));
            }
        }
    }

    // `operated_by` is free text server-side (no charset), but its
    // affixes still carry the length bound.
    if let Some(operated_by) = &procedure_def.operated_by {
        if let Err(e) = crate::procedure::schema::validate_unit_field_affixes(
            "operated_by",
            operated_by,
            false,
        ) {
            errors.push(LoadError::text(format!("Validation failed: {}", e)));
        }
    }

    for (_, phase) in procedure_def.get_all_phases_with_stage_scope() {
        if let Err(e) = phase.validate_single_runtime() {
            errors.push(LoadError::text(e));
        }
        if let Some(ui) = &phase.ui {
            if let Some(components) = &ui.components {
                for comp in components {
                    for check in [
                        comp.validate_width(),
                        comp.validate_aspect(),
                        comp.validate_fit(),
                        comp.validate_options_count(),
                        comp.validate_identity_affixes(),
                    ] {
                        if let Err(e) = check {
                            errors.push(LoadError::text(e));
                        }
                    }

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
                            errors.push(LoadError::text(format!(
                                "UI component `{}` (type `{:?}`) requires a non-empty `options` list",
                                comp.key, comp.component_type,
                            )));
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
            errors.push(LoadError::text(format!(
                "Phase `{}` has `scope: station`, which is only valid on plugs. \
                 Use `scope: execution` (execution-wide) or `scope: slot` (per-slot) on phases",
                phase.key
            )));
        }
    }

    // A procedure with no main phases isn't a test — the runner would exit
    // PASS without doing anything, which is misleading.
    if procedure_def.main.is_empty() {
        errors.push(LoadError::text(
            "Procedure has no `main` phases — at least one is required",
        ));
    }

    // Phase keys must be unique across `main`. Duplicates corrupt the
    // dependency graph and produce phantom duplicate phase events at runtime
    // because the scheduler indexes jobs by key.
    let mut seen_keys: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (index, phase) in procedure_def.main.iter().enumerate() {
        if !seen_keys.insert(phase.key.as_str()) {
            errors.push(LoadError::at(
                format!(
                    "Duplicate phase key `{}` — every main phase must have a unique key",
                    phase.key
                ),
                RefLocation::key(RefList::Main, index, "key"),
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
    for (index, plug) in procedure_def.plugs.iter().enumerate() {
        if let Some(first) = seen_plug_keys.insert(plug.key.to_lowercase(), plug.name.as_str()) {
            errors.push(LoadError::at(
                format!(
                    "Duplicate plug key `{}` (plugs `{}` and `{}`) — every plug must have a \
                     unique key, because a phase receives a plug by naming that key as a \
                     parameter. Set an explicit `key:` on one of them",
                    plug.key, first, plug.name
                ),
                RefLocation::entry(RefList::Plugs, index),
            ));
        }
    }

    // `depends_on` must reference phase keys that exist in the procedure.
    // Silently ignoring unknown dependencies lets a typo mask broken ordering.
    let known_keys: std::collections::HashSet<&str> =
        procedure_def.main.iter().map(|p| p.key.as_str()).collect();
    for (index, phase) in procedure_def.main.iter().enumerate() {
        for dep in &phase.depends_on {
            if !known_keys.contains(dep.as_str()) {
                errors.push(LoadError::at(
                    format!(
                        "Phase `{}` depends on unknown phase `{}` (known phases: {})",
                        phase.key,
                        dep,
                        known_keys.iter().copied().collect::<Vec<_>>().join(", ")
                    ),
                    RefLocation::key(RefList::Main, index, "depends_on"),
                ));
            }
        }
    }

    // Belt: explicit self-reference pre-check. Three-color DFS below
    // catches this too, but the error text is clearer when handled
    // directly ("depends on itself" vs "a -> a").
    let mut self_dependent = false;
    for (index, phase) in procedure_def.main.iter().enumerate() {
        if phase.depends_on.iter().any(|d| d == &phase.key) {
            self_dependent = true;
            errors.push(LoadError::at(
                format!("Phase `{}` depends on itself", phase.key),
                RefLocation::key(RefList::Main, index, "depends_on"),
            ));
        }
    }

    // Braces: full cycle detection in the dependency graph. Skipped when a
    // self-reference was already reported, which the DFS would repeat as
    // `a -> a`.
    if !self_dependent {
        if let Some(cycle) = find_dependency_cycle(&procedure_def.main) {
            // Point at the first phase of the cycle as written in the file.
            let index = procedure_def
                .main
                .iter()
                .position(|p| cycle.contains(&p.key))
                .unwrap_or(0);
            errors.push(LoadError::at(
                format!(
                    "Circular dependency detected in `depends_on`: {}",
                    cycle.join(" -> ")
                ),
                RefLocation::key(RefList::Main, index, "depends_on"),
            ));
        }
    }

    LoadOutcome {
        definition: Some(procedure_def),
        errors,
    }
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
        assert!(
            plugs_with_explicit_scope("plugs:\n  - name: Bad\n    scope: nonsense\n")
                .contains("Bad")
        );
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
        let dir = std::env::temp_dir().join(format!("tp-loader-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("procedure.yaml");
        std::fs::write(&path, yaml).unwrap();
        let result = load_procedure_definition(&path);
        std::fs::remove_dir_all(&dir).ok();
        result
    }

    /// Public API on a shared crate: the CLI reads it to name a
    /// procedure, and Studio's project discovery labels every row in the
    /// switcher with it. A blank name has to read as "no name" so the
    /// caller falls back to the holding directory, rather than
    /// labelling the row with an empty string.
    #[test]
    fn procedure_name_from_str_reads_the_name_or_nothing() {
        assert_eq!(
            procedure_name_from_str("name: Board Test\nversion: 1.0.0\n").as_deref(),
            Some("Board Test")
        );
        // Trimmed, because the YAML carries whatever the author typed.
        assert_eq!(
            procedure_name_from_str("name: \"  Padded  \"\n").as_deref(),
            Some("Padded")
        );
        // Present but empty, and whitespace-only, are both "no name".
        assert_eq!(procedure_name_from_str("name: \"\"\n"), None);
        assert_eq!(procedure_name_from_str("name: \"   \"\n"), None);
        // No `name:` key at all, and not a mapping at all.
        assert_eq!(procedure_name_from_str("version: 1.0.0\n"), None);
        assert_eq!(procedure_name_from_str("- just\n- a list\n"), None);
        // Unparseable YAML answers None instead of panicking: this runs
        // over files the operator is in the middle of editing.
        assert_eq!(procedure_name_from_str("name: [unclosed\n"), None);
        assert_eq!(procedure_name_from_str(""), None);
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
        assert!(
            err.contains("Duplicate plug key"),
            "unexpected error: {err}"
        );
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
    fn an_empty_command_loads_and_is_reported_not_refused() {
        // The editor's "runtime chosen, command not yet" state. It has to
        // LOAD — Studio's Sequence view runs this same loader, so a
        // refusal would remove the panel the user needs to fix it — and
        // it has to be REPORTED, because `sh -c ""` exits 0: unreported,
        // it is a phase that silently passes without doing anything.
        let dir = std::env::temp_dir().join(format!("tp-exec-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("procedure.yaml");
        std::fs::write(
            &path,
            r#"
name: Empty Command
main:
  - key: flash
    name: Flash
    executable:
      command: ""
  - key: measure
    name: Measure
    executable:
      command: ./measure.sh
"#,
        )
        .unwrap();

        let def = load_procedure_definition(&path)
            .expect("an empty command must load, so the editor can show it");
        let problems = def.resolve_runtime_refs(&dir, None);
        assert_eq!(problems.len(), 1, "unexpected: {problems:?}");
        assert!(
            problems[0].message.contains("`flash`") && problems[0].message.contains("no command"),
            "got: {}",
            problems[0].message
        );

        // A partial run that does not include the phase is not blocked by
        // it — same rule as the python refs beside it.
        let filter: HashSet<String> = ["measure".to_string()].into_iter().collect();
        assert!(
            def.resolve_runtime_refs(&dir, Some(&filter))
                .iter()
                .all(|p| !p.is_error()),
            "a partial run must not be gated by an unrelated plug or phase"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_runtime_refs_reports_dangling_refs_loading_lets_through() {
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
        let problems = def.resolve_runtime_refs(&dir, None);
        assert_eq!(problems.len(), 2, "unexpected: {problems:?}");
        assert!(
            problems[0].message.starts_with("Plug `psu`"),
            "got: {}",
            problems[0]
        );
        assert!(
            problems[1].message.starts_with("Phase `p1`"),
            "got: {}",
            problems[1]
        );
        for p in &problems {
            assert!(p.message.contains("Python file not found"), "got: {p}");
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
        assert!(def.resolve_runtime_refs(&dir, None).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Scaffold for the lint tests: a procedure dir with one plug file and
    /// one phase file, and the YAML written by the caller.
    fn lint_fixture(
        plug_py: &str,
        phase_py: &str,
        yaml: &str,
    ) -> (std::path::PathBuf, ProcedureDefinition) {
        let dir = std::env::temp_dir().join(format!("tp-lint-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("phases")).unwrap();
        std::fs::create_dir_all(dir.join("plugs")).unwrap();
        std::fs::write(dir.join("plugs/psu.py"), plug_py).unwrap();
        std::fs::write(dir.join("phases/main.py"), phase_py).unwrap();
        let path = dir.join("procedure.yaml");
        std::fs::write(&path, yaml).unwrap();
        let def = load_procedure_definition(&path).expect("fixture must load");
        (dir, def)
    }

    #[test]
    fn resolve_runtime_refs_reports_a_symbol_the_file_does_not_define() {
        // The file exists, the identifier is valid — and the name is a
        // typo. Before this rule the first signal was an import failure
        // on the bench.
        let (dir, def) = lint_fixture(
            "class PowerSupply:\n    def __init__(self, address):\n        pass\n",
            "def check():\n    pass\n",
            r#"
name: Typos
plugs:
  - name: PSU
    key: psu
    python: plugs.psu:PowerSuply
main:
  - key: p1
    name: P1
    python: phases.main:chek
"#,
        );
        let problems = def.resolve_runtime_refs(&dir, None);
        assert_eq!(problems.len(), 2, "unexpected: {problems:?}");
        assert!(
            problems[0].message.contains("class `PowerSuply` not found"),
            "got: {}",
            problems[0]
        );
        assert_eq!(
            problems[0].location,
            RefLocation::key(RefList::Plugs, 0, "python")
        );
        assert!(
            problems[1].message.contains("`chek` not found"),
            "got: {}",
            problems[1]
        );
        assert_eq!(
            problems[1].location,
            RefLocation::key(RefList::Main, 0, "python")
        );
        assert!(problems.iter().all(|p| p.is_error()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_runtime_refs_accepts_re_exports_and_factories() {
        // A lint that blocks a run must not call a working reference
        // broken: `module:Name` resolves through a top-level import or
        // assignment just as well as through `class Name`.
        let (dir, def) = lint_fixture(
            "from .impl import PowerSupply\nMeter = make_plug('dmm')\n",
            "from .steps import check\n",
            r#"
name: Indirect
plugs:
  - name: PSU
    key: psu
    python: plugs.psu:PowerSupply
  - name: DMM
    key: dmm
    python: plugs.psu:Meter
main:
  - key: p1
    name: P1
    python: phases.main:check
"#,
        );
        let problems = def.resolve_runtime_refs(&dir, None);
        assert!(problems.is_empty(), "unexpected: {problems:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_runtime_refs_checks_plug_config_against_init() {
        // `config:` is splatted into the class as kwargs: an unknown key
        // and a missing required one are both a TypeError at setup. The
        // suggestion uses the same two-edit threshold as the Inspector.
        let (dir, def) = lint_fixture(
            "class PowerSupply:\n    def __init__(self, address: str, port: int = 5025, initial_voltage: float = 0.0):\n        pass\n",
            "def check():\n    pass\n",
            r#"
name: Config
plugs:
  - name: PSU
    key: psu
    python: plugs.psu:PowerSupply
    config:
      port: 1
      intial_voltage: 3.3
main:
  - key: p1
    name: P1
    python: phases.main:check
"#,
        );
        let problems = def.resolve_runtime_refs(&dir, None);
        assert_eq!(problems.len(), 2, "unexpected: {problems:?}");
        let unknown = problems
            .iter()
            .find(|p| p.message.contains("`intial_voltage` is not a parameter"))
            .expect("unknown key reported");
        assert!(
            unknown.message.contains("did you mean `initial_voltage`"),
            "got: {unknown}"
        );
        assert_eq!(
            unknown.location,
            RefLocation::key(RefList::Plugs, 0, "config.intial_voltage")
        );
        assert!(unknown.is_error(), "a def __init__ is authoritative");
        let missing = problems
            .iter()
            .find(|p| p.message.contains("requires `address`"))
            .expect("missing required reported");
        assert_eq!(
            missing.location,
            RefLocation::key(RefList::Plugs, 0, "config")
        );
        assert!(missing.is_error());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_runtime_refs_does_not_read_a_docstring_as_dataclass_fields() {
        // A Google-style docstring is the house style, and its
        // `Attributes:` / `Raises:` entries are indented `name: text` —
        // the shape of an annotated field. Read as fields on a
        // `@dataclass`, which is a certain constructor, every one becomes
        // an Error there is no `config:` the author can write to satisfy.
        let (dir, def) = lint_fixture(
            concat!(
                "from dataclasses import dataclass\n",
                "\n",
                "@dataclass\n",
                "class PowerSupply:\n",
                "    \"\"\"A PSU config.\n",
                "\n",
                "    Attributes:\n",
                "        address: the VISA address\n",
                "    Raises:\n",
                "        ValueError: if bad\n",
                "    \"\"\"\n",
                "\n",
                "    address: str = \"192.168.1.1\"\n",
                "    port: int = 5025\n",
            ),
            "def check():\n    pass\n",
            r#"
name: Docstring
plugs:
  - name: PSU
    key: psu
    python: plugs.psu:PowerSupply
    config:
      port: 1
main:
  - key: p1
    name: P1
    python: phases.main:check
"#,
        );
        let problems = def.resolve_runtime_refs(&dir, None);
        assert!(problems.is_empty(), "unexpected: {problems:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_runtime_refs_config_rules_are_warnings_off_attributes_and_silent_on_kwargs() {
        // No `def __init__`: the annotated attributes may be a dataclass —
        // or a plain class inheriting its constructor from another file.
        // The lint cannot tell, so it warns and never refuses.
        let (dir, def) = lint_fixture(
            "class PowerSupply(Base):\n    address: str\n    port: int = 1\n\nclass Meter:\n    def __init__(self, **kwargs):\n        pass\n\nclass Opaque(Base):\n    pass\n",
            "def check():\n    pass\n",
            r#"
name: Uncertain
plugs:
  - name: PSU
    key: psu
    python: plugs.psu:PowerSupply
    config:
      baud: 9600
  - name: DMM
    key: dmm
    python: plugs.psu:Meter
    config:
      anything: goes
  - name: Opaque
    key: opaque
    python: plugs.psu:Opaque
    config:
      whatever: 1
main:
  - key: p1
    name: P1
    python: phases.main:check
"#,
        );
        let problems = def.resolve_runtime_refs(&dir, None);
        // PSU: unknown `baud` + missing `address`, both warnings. DMM takes
        // **kwargs: nothing. Opaque: signature unknowable: nothing.
        assert_eq!(problems.len(), 2, "unexpected: {problems:?}");
        assert!(problems.iter().all(|p| !p.is_error()), "got: {problems:?}");
        assert!(
            problems.iter().all(|p| p.message.starts_with("Plug `psu`")),
            "got: {problems:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_runtime_refs_reports_duplicate_measurement_keys() {
        let (dir, def) = lint_fixture(
            "class PowerSupply:\n    pass\n",
            "def check():\n    pass\n",
            r#"
name: Dup Measurements
main:
  - key: p1
    name: P1
    python: phases.main:check
    measurements:
      - key: voltage
        name: Voltage
      - key: current
        name: Current
      - key: voltage
        name: Voltage again
"#,
        );
        let problems = def.resolve_runtime_refs(&dir, None);
        assert_eq!(problems.len(), 1, "unexpected: {problems:?}");
        assert!(
            problems[0]
                .message
                .contains("duplicate measurement key `voltage`"),
            "got: {}",
            problems[0]
        );
        assert_eq!(
            problems[0].location,
            RefLocation::key(RefList::Main, 0, "measurements")
        );
        assert!(problems[0].is_error());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn located_load_errors_point_at_the_entry_or_the_parse_position() {
        let dup = "name: A\nplugs:\n  - name: Power Supply\n    python: plugs.a:A\n  - name: Power-Supply\n    python: plugs.b:B\nmain:\n  - key: p\n    name: P\n";
        let errs = load_procedure_definition_from_str_located(dup).unwrap_err();
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        let err = &errs[0];
        assert!(
            err.message.contains("Duplicate plug key"),
            "got: {}",
            err.message
        );
        assert_eq!(err.location, Some(RefLocation::entry(RefList::Plugs, 1)));
        assert_eq!((err.line, err.column), (None, None));

        let cycle = "name: A\nmain:\n  - key: a\n    name: A\n    depends_on: [b]\n  - key: b\n    name: B\n    depends_on: [a]\n";
        let errs = load_procedure_definition_from_str_located(cycle).unwrap_err();
        assert!(
            errs[0].message.contains("Circular"),
            "got: {}",
            errs[0].message
        );
        assert_eq!(
            errs[0].location,
            Some(RefLocation::key(RefList::Main, 0, "depends_on"))
        );

        let bad_yaml = "name: A\nmain:\n  - key: [unclosed\n";
        let errs = load_procedure_definition_from_str_located(bad_yaml).unwrap_err();
        let err = &errs[0];
        assert!(
            err.message.starts_with("Failed to parse YAML"),
            "got: {}",
            err.message
        );
        assert!(err.line.is_some() && err.column.is_some(), "got: {err:?}");
        assert_eq!(err.location, None);

        // The string-returning entry point is unchanged for its callers.
        assert_eq!(
            load_procedure_definition_from_str(dup).unwrap_err(),
            load_procedure_definition_from_str_located(dup).unwrap_err()[0].message
        );
    }

    #[test]
    fn inspect_keeps_the_definition_when_only_rules_fail() {
        let dup = "name: A\nplugs:\n  - name: P\n    key: p\n    python: plugs.a:A\nmain:\n  - key: m\n    name: M\n  - key: m\n    name: M2\n";
        let outcome = inspect_procedure_definition(dup);
        assert_eq!(outcome.errors.len(), 1, "got: {:?}", outcome.errors);
        let def = outcome
            .definition
            .expect("a duplicate key is unrunnable, not uninspectable");
        assert_eq!(def.plugs.len(), 1);
        // The runner-facing loaders still refuse it.
        assert!(load_procedure_definition_from_str_located(dup).is_err());
        assert!(load_procedure_definition_from_str(dup).is_err());

        // A parse failure leaves nothing to inspect.
        let broken = inspect_procedure_definition("name: A\nmain:\n  - key: [x\n");
        assert!(broken.definition.is_none());
        assert_eq!(broken.errors.len(), 1);
    }

    #[test]
    fn bound_identity_affix_outside_the_server_charset_fails_at_load() {
        // The same guard `unit:` fields get, on the phase-prompt path
        // that composes the affix into the recorded serial: catch the
        // bad character here, not at upload after the test has run.
        let yaml = "name: A\nmain:\n  - key: id\n    name: Identify\n    ui:\n      components:\n        - key: sn\n          type: text_input\n          bind: unit.serial_number\n          prefix: \"SN#\"\n";
        let outcome = inspect_procedure_definition(yaml);
        assert_eq!(outcome.errors.len(), 1, "got: {:?}", outcome.errors);
        assert!(outcome.errors[0].message.contains("prefix 'SN#'"), "{:?}", outcome.errors);
        let ok = yaml.replace("\"SN#\"", "\"SN-\"");
        assert!(inspect_procedure_definition(&ok).errors.is_empty());
    }

    #[test]
    fn loader_reports_every_failed_rule_not_just_the_first() {
        // A duplicate plug key AND an unknown depends_on AND a duplicate
        // phase key: fixing one must not be how you discover the next.
        let yaml = "name: A\nplugs:\n  - name: P\n    key: p\n    python: plugs.a:A\n  - name: P2\n    key: p\n    python: plugs.b:B\nmain:\n  - key: a\n    name: A\n    depends_on: [zzz]\n  - key: a\n    name: A2\n";
        let errs = load_procedure_definition_from_str_located(yaml).unwrap_err();
        let msgs: Vec<&str> = errs.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(errs.len(), 3, "got: {msgs:?}");
        assert!(
            msgs.iter().any(|m| m.contains("Duplicate phase key `a`")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("Duplicate plug key `p`")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("unknown phase `zzz`")),
            "{msgs:?}"
        );
        // One line each in the string form.
        assert_eq!(
            load_procedure_definition_from_str(yaml)
                .unwrap_err()
                .lines()
                .count(),
            3
        );
    }

    #[test]
    fn resolve_runtime_refs_warns_on_a_phase_module_outside_the_tree() {
        // `phases2` is not a directory here, so the spec may resolve to an
        // installed package at run time: say so, do not block.
        let (dir, def) = lint_fixture(
            "class PowerSupply:\n    pass\n",
            "def check():\n    pass\n",
            "name: Outside\nmain:\n  - key: p1\n    name: P1\n    python: phases2.measure:check\n",
        );
        let problems = def.resolve_runtime_refs(&dir, None);
        assert_eq!(problems.len(), 1, "unexpected: {problems:?}");
        assert!(!problems[0].is_error());
        assert!(
            problems[0].message.contains("not found in the project"),
            "got: {}",
            problems[0]
        );
        // Inside the tree the same miss is an error, as before.
        std::fs::create_dir_all(dir.join("phases2")).unwrap();
        let problems = def.resolve_runtime_refs(&dir, None);
        assert!(problems[0].is_error(), "got: {problems:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn resolve_runtime_refs_suggests_the_colon_spelling() {
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
        let problems = def.resolve_runtime_refs(&dir, None);
        assert_eq!(problems.len(), 3, "unexpected: {problems:?}");
        // The dotted-class spelling whose ':' variant resolves gets the
        // did-you-mean; a spec that is broken either way does not.
        assert!(
            problems[0].message.contains("did you mean `plugs.psu:PSU`"),
            "got: {}",
            problems[0]
        );
        assert!(
            !problems[1].message.contains("did you mean"),
            "got: {}",
            problems[1]
        );
        assert!(
            problems[2]
                .message
                .contains("did you mean `phases.main:check`"),
            "got: {}",
            problems[2]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_runtime_refs_mirrors_the_runtime_not_stricter() {
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
        let all = def.resolve_runtime_refs(&dir, None);
        // The wheel-style spec is reported, but as a warning: it may
        // resolve through importlib at run time, so it must not gate.
        let warnings: Vec<_> = all.iter().filter(|p| !p.is_error()).collect();
        assert_eq!(warnings.len(), 1, "unexpected: {all:?}");
        assert!(
            warnings[0].message.starts_with("Phase `wheel`")
                && warnings[0].message.contains("not found in the project"),
            "got: {}",
            warnings[0]
        );
        let problems: Vec<_> = all.iter().filter(|p| p.is_error()).collect();
        assert_eq!(problems.len(), 2, "unexpected: {problems:?}");
        assert!(
            problems[0].message.starts_with("Plug `dmm`"),
            "got: {}",
            problems[0]
        );
        assert!(
            problems[1].message.starts_with("Phase `broken`"),
            "got: {}",
            problems[1]
        );

        // Partial run on `ok`: `broken` is outside the dependency closure,
        // and plugs don't gate at all (the runtime narrows the plug set by
        // signature introspection, so `dmm` would never be built) — the
        // same procedure starts.
        let filter: std::collections::HashSet<String> = ["ok".to_string()].into_iter().collect();
        assert!(def.resolve_runtime_refs(&dir, Some(&filter)).is_empty());

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
