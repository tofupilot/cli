//! `tofupilot link` / `tofupilot unlink` — bind a local procedure directory
//! to a remote dashboard procedure so `tofupilot run <path> --upload` syncs
//! the resulting run to that procedure.
//!
//! The link is a single `procedure.json` file at the procedure dir root,
//! framework-agnostic (yaml / openhtf / pytest / robot / plain). Modeled on
//! `vercel link` (`.vercel/project.json`), trimmed to what the upload path
//! needs: the procedure id. We keep the human name too so `unlink` and the
//! post-run hint can print something readable without an extra API round-trip.

use std::path::{Path, PathBuf};

use tofupilot_sdk::config::ClientConfig;
use tofupilot_sdk::TofuPilot;

use crate::commands::auth::credentials;

/// On-disk link record. Lives at `<procedure_dir>/procedure.json`.
///
/// Only `procedure_id` is load-bearing for upload — the api key already
/// scopes the org server-side, so (unlike vercel's orgId) we don't persist
/// an organization id. `procedure_name` is convenience metadata for CLI
/// output only.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProcedureLink {
    #[serde(rename = "procedureId")]
    pub procedure_id: String,
    #[serde(
        rename = "procedureName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub procedure_name: Option<String>,
}

/// Filename of the link record, written at the procedure dir root.
pub const LINK_FILE: &str = "procedure.json";

/// Read the link for a procedure dir, if any. Returns `None` when the file
/// is absent. A present-but-unparseable file logs a warning naming the
/// path and then returns `None` — without the warning a corrupt link would
/// silently report "not linked", which is actively misleading after the
/// user ran `tofupilot link`.
pub fn read_link(dir: &Path) -> Option<ProcedureLink> {
    let path = dir.join(LINK_FILE);
    let content = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&content) {
        Ok(link) => Some(link),
        Err(e) => {
            crate::log::warn(&format!(
                "Ignoring malformed {}: {e}. Re-run `tofupilot link` to repair it.",
                path.display()
            ));
            None
        }
    }
}

/// Resolve the directory a link command targets: an explicit path (file →
/// parent dir, dir → itself) or the current working directory.
/// Short, human-scannable form of a procedure id for picker labels —
/// enough to disambiguate same-named procedures without printing the
/// full UUID.
fn short_id(id: &str) -> &str {
    &id[..8.min(id.len())]
}

fn resolve_dir(path: Option<&Path>) -> Result<PathBuf, i32> {
    let raw = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().map_err(|e| {
            crate::log::error(&format!("Cannot resolve current directory: {e}"));
            1
        })?,
    };
    let canonical = std::fs::canonicalize(&raw).map_err(|e| {
        crate::log::error(&format!("Cannot resolve {}: {e}", raw.display()));
        1
    })?;
    if canonical.is_dir() {
        Ok(canonical)
    } else if canonical.is_file() {
        Ok(canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(canonical))
    } else {
        crate::log::error(&format!("Not a file or directory: {}", canonical.display()));
        Err(1)
    }
}

fn sdk() -> Result<TofuPilot, i32> {
    let creds = credentials::require().map_err(|e| {
        crate::log::error(&e.to_string());
        1
    })?;
    let config = ClientConfig::new(&creds.api_key).base_url(&creds.base_url);
    Ok(TofuPilot::with_config(config))
}

/// `tofupilot link [path] [--procedure <id|name>]`
///
/// Writes `<dir>/procedure.json` binding the local procedure to a remote
/// one. With `--procedure`, resolves that id/name non-interactively (CI).
/// Without it, fetches the org's procedures and shows a `dialoguer::Select`
/// picker — same pattern as `resolve_procedure_id` in the run path.
pub async fn link_cmd(path: Option<&Path>, procedure: Option<&str>, json_mode: bool) -> i32 {
    let dir = match resolve_dir(path) {
        Ok(d) => d,
        Err(code) => return code,
    };
    let client = match sdk() {
        Ok(c) => c,
        Err(code) => return code,
    };

    let chosen = match resolve_target(&client, procedure, json_mode).await {
        Ok(c) => c,
        Err(code) => return code,
    };

    let link = ProcedureLink {
        procedure_id: chosen.id.clone(),
        procedure_name: chosen.name.clone(),
    };
    let body = match serde_json::to_string_pretty(&link) {
        Ok(b) => b,
        Err(e) => {
            crate::log::error(&format!("Serialize link: {e}"));
            return 1;
        }
    };
    let file = dir.join(LINK_FILE);
    if let Err(e) = std::fs::write(&file, format!("{body}\n")) {
        crate::log::error(&format!("Write {}: {e}", file.display()));
        return 1;
    }

    let label = chosen
        .name
        .as_deref()
        .map(|n| format!("{n} ({})", chosen.id))
        .unwrap_or_else(|| chosen.id.clone());
    crate::log::success(&format!("Linked {} \u{2192} {label}", dir.display()));
    crate::log::info("Run with `tofupilot run <path> --upload` to sync runs to the dashboard.");
    0
}

/// `tofupilot unlink [path]` — remove `procedure.json`. Idempotent: a dir
/// that isn't linked is reported, not an error.
pub fn unlink_cmd(path: Option<&Path>) -> i32 {
    let dir = match resolve_dir(path) {
        Ok(d) => d,
        Err(code) => return code,
    };
    let file = dir.join(LINK_FILE);
    if !file.exists() {
        crate::log::info(&format!("{} is not linked.", dir.display()));
        return 0;
    }
    let name = read_link(&dir).and_then(|l| l.procedure_name);
    if let Err(e) = std::fs::remove_file(&file) {
        crate::log::error(&format!("Remove {}: {e}", file.display()));
        return 1;
    }
    match name {
        Some(n) => crate::log::success(&format!("Unlinked {} (was {n})", dir.display())),
        None => crate::log::success(&format!("Unlinked {}", dir.display())),
    }
    0
}

/// A resolved link target: the remote procedure's id and (best-effort) name.
struct Target {
    id: String,
    name: Option<String>,
}

/// Fetch every procedure across all pages, following `meta.next_cursor`.
///
/// The list endpoint paginates with a server-side default page size. A
/// single-page fetch would make `--procedure <name>` miss any procedure
/// past page 1 — exactly the non-interactive path that can't fall back to
/// a picker — so the selector must see the whole set.
async fn fetch_all_procedures(
    client: &TofuPilot,
) -> crate::error::CliResult<Vec<tofupilot_sdk::types::ProcedureListData>> {
    let mut all = Vec::new();
    let mut cursor: Option<f64> = None;
    // Bound the loop defensively so a server that always reports `has_more`
    // can't spin forever.
    for _ in 0..1000 {
        let mut builder = client.procedures().list();
        if let Some(c) = cursor {
            builder = builder.cursor(c);
        }
        let response = builder.send().await.map_err(|e| e.to_string())?;
        all.extend(response.data);
        if !response.meta.has_more {
            break;
        }
        match response.meta.next_cursor {
            Some(next) => cursor = Some(next as f64),
            // `has_more` but no cursor: nothing more we can ask for.
            None => break,
        }
    }
    Ok(all)
}

/// Resolve which remote procedure to link to. With `--procedure`, match by
/// exact id first, then by exact name. Without it, show an interactive
/// picker over the org's procedures.
async fn resolve_target(
    client: &TofuPilot,
    procedure: Option<&str>,
    json_mode: bool,
) -> Result<Target, i32> {
    let procedures = match fetch_all_procedures(client).await {
        Ok(p) => p,
        Err(e) => {
            crate::log::error(&format!("Failed to list procedures: {e}"));
            return Err(1);
        }
    };

    if procedures.is_empty() {
        crate::log::error("No procedures found. Create one in the dashboard first.");
        return Err(1);
    }

    // Explicit selector: match id first (always unambiguous), then name.
    if let Some(sel) = procedure {
        if let Some(p) = procedures.iter().find(|p| p.id == sel) {
            return Ok(Target {
                id: p.id.clone(),
                name: Some(p.name.clone()),
            });
        }
        let by_name: Vec<_> = procedures
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(sel))
            .collect();
        match by_name.as_slice() {
            [p] => {
                return Ok(Target {
                    id: p.id.clone(),
                    name: Some(p.name.clone()),
                });
            }
            // Several procedures share this name — refuse to guess. List
            // the candidates so the caller can re-run with an unambiguous id.
            [_, ..] => {
                crate::log::error(&format!(
                    "Multiple procedures named '{sel}'. Re-run with one of these ids:"
                ));
                for p in by_name {
                    crate::log::info(&format!("  {} ({})", p.name, p.id));
                }
                return Err(1);
            }
            [] => {
                crate::log::error(&format!("No procedure matching '{sel}' (by id or name)."));
                return Err(1);
            }
        }
    }

    // Single procedure: nothing to pick.
    if procedures.len() == 1 {
        let p = &procedures[0];
        return Ok(Target {
            id: p.id.clone(),
            name: Some(p.name.clone()),
        });
    }

    // Non-interactive (CI / --json): can't show a picker; require --procedure.
    if json_mode {
        crate::log::error("Multiple procedures available. Use --procedure to select one.");
        for p in &procedures {
            println!(
                "{}",
                serde_json::json!({ "procedure_id": p.id, "name": p.name })
            );
        }
        return Err(1);
    }

    // Include a short id suffix so duplicate procedure names stay
    // distinguishable AND every label is unique. `dialoguer::FuzzySelect`
    // resolves the chosen label back to an index with `position(|i| i.eq(label))`
    // (first match), so identical labels would otherwise return the wrong
    // procedure.
    let labels: Vec<String> = procedures
        .iter()
        .map(|p| format!("{} ({})", p.name, short_id(&p.id)))
        .collect();
    let selection = dialoguer::FuzzySelect::new()
        .with_prompt("Select a procedure to link (type to filter)")
        .items(&labels)
        .default(0)
        .interact()
        .map_err(|_| {
            crate::log::info("Selection cancelled.");
            1
        })?;
    let p = &procedures[selection];
    Ok(Target {
        id: p.id.clone(),
        name: Some(p.name.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_link(dir: &Path, body: &str) {
        fs::write(dir.join(LINK_FILE), body).unwrap();
    }

    #[test]
    fn read_link_round_trips_full_record() {
        let d = tempdir().unwrap();
        write_link(
            d.path(),
            r#"{"procedureId":"proc_abc","procedureName":"PCB Flash"}"#,
        );
        let link = read_link(d.path()).expect("link should parse");
        assert_eq!(link.procedure_id, "proc_abc");
        assert_eq!(link.procedure_name.as_deref(), Some("PCB Flash"));
    }

    #[test]
    fn read_link_name_is_optional() {
        let d = tempdir().unwrap();
        write_link(d.path(), r#"{"procedureId":"proc_abc"}"#);
        let link = read_link(d.path()).expect("link should parse without a name");
        assert_eq!(link.procedure_id, "proc_abc");
        assert_eq!(link.procedure_name, None);
    }

    #[test]
    fn read_link_absent_returns_none() {
        let d = tempdir().unwrap();
        assert!(read_link(d.path()).is_none());
    }

    #[test]
    fn read_link_corrupt_returns_none() {
        // A present-but-unparseable file must degrade to `None` (the run
        // can still proceed locally). The warning side effect isn't
        // asserted here; what matters is it doesn't parse to a bogus link.
        let d = tempdir().unwrap();
        write_link(d.path(), "{ not valid json");
        assert!(read_link(d.path()).is_none());
    }

    #[test]
    fn read_link_missing_procedure_id_returns_none() {
        // `procedureId` is required; a record without it isn't a usable link.
        let d = tempdir().unwrap();
        write_link(d.path(), r#"{"procedureName":"orphan"}"#);
        assert!(read_link(d.path()).is_none());
    }

    #[test]
    fn link_serializes_with_camelcase_keys() {
        let link = ProcedureLink {
            procedure_id: "proc_abc".to_string(),
            procedure_name: Some("PCB Flash".to_string()),
        };
        let json = serde_json::to_string(&link).unwrap();
        assert!(json.contains("\"procedureId\":\"proc_abc\""), "got {json}");
        assert!(
            json.contains("\"procedureName\":\"PCB Flash\""),
            "got {json}"
        );
    }

    #[test]
    fn link_omits_absent_name_on_serialize() {
        let link = ProcedureLink {
            procedure_id: "proc_abc".to_string(),
            procedure_name: None,
        };
        let json = serde_json::to_string(&link).unwrap();
        assert!(!json.contains("procedureName"), "got {json}");
    }

    #[test]
    fn resolve_dir_accepts_directory() {
        let d = tempdir().unwrap();
        let resolved = resolve_dir(Some(d.path())).unwrap();
        assert_eq!(resolved, fs::canonicalize(d.path()).unwrap());
    }

    #[test]
    fn resolve_dir_maps_file_to_parent() {
        // `link ./entry.py` should bind the file's directory, so a later
        // `run ./entry.py --upload` finds the link in the same place.
        let d = tempdir().unwrap();
        let file = d.path().join("main.py");
        fs::write(&file, "x = 1\n").unwrap();
        let resolved = resolve_dir(Some(&file)).unwrap();
        assert_eq!(resolved, fs::canonicalize(d.path()).unwrap());
    }

    #[test]
    fn resolve_dir_rejects_missing_path() {
        let d = tempdir().unwrap();
        let missing = d.path().join("does-not-exist");
        assert!(resolve_dir(Some(&missing)).is_err());
    }
}
