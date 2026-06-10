use crate::procedure::schema::{ProcedureDefinition, ProcedureYaml};
use super::error::CommandError;
use std::path::Path;
use validator::Validate;

fn validate_file_path(path: &Path) -> Result<(), CommandError> {
    if !path.exists() {
        return Err(CommandError::file_not_found(path.display()));
    }

    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| CommandError::new(
            super::error::ErrorCode::InvalidFileExtension,
            "File has no extension"
        ))?;

    if extension != "yaml" && extension != "yml" {
        return Err(CommandError::new(
            super::error::ErrorCode::InvalidFileExtension,
            "File must be a YAML file (.yaml or .yml)"
        ));
    }

    Ok(())
}

#[must_use = "procedure definition should be checked for validation errors"]
pub fn load_procedure_definition(file_path: &Path) -> Result<ProcedureDefinition, String> {
    validate_file_path(file_path).map_err(|e| e.message)?;

    let content = std::fs::read_to_string(file_path)
        .map_err(|e| format!("Failed to read {}: {}", file_path.display(), e))?;

    let raw: ProcedureYaml = serde_yaml::from_str(&content)
        .map_err(|e| format!("Failed to parse YAML: {}", e))?;

    let procedure_def = ProcedureDefinition::from(raw);

    procedure_def
        .validate()
        .map_err(|e| format!("Validation failed: {}", e))?;

    if let Some(unit) = &procedure_def.unit {
        unit.validate_auto_identify()
            .map_err(|e| format!("Validation failed: {}", e))?;
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
                        let empty = comp
                            .options
                            .as_ref()
                            .map(|o| o.is_empty())
                            .unwrap_or(true);
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
    let known_keys: std::collections::HashSet<&str> = procedure_def
        .main
        .iter()
        .map(|p| p.key.as_str())
        .collect();
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
            return Err(format!(
                "Phase `{}` depends on itself",
                phase.key
            ));
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
    let mut color: HashMap<&str, Color> = phases.iter().map(|p| (p.key.as_str(), Color::White)).collect();

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
        if color.get(phase.key.as_str()).copied().unwrap_or(Color::White) == Color::White {
            let mut stack = Vec::new();
            if let Some(cycle) = dfs(phase.key.as_str(), &by_key, &mut color, &mut stack) {
                return Some(cycle);
            }
        }
    }
    None
}
