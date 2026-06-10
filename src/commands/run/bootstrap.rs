//! Auto-provision a Python venv for local-path runs.
//!
//! Station-mode deployments ship a venv pre-built by the deployer's
//! installer (`pull/sync.rs`). Local-path runs (`tofupilot run ./proc`)
//! have no installer — historically the operator had to create the
//! venv by hand. When they didn't, the engine spawn failed with a bare
//! `os error 13` (or, post-0.22.7, "interpreter is a directory").
//!
//! This module closes that gap: on `tofupilot run` against a directory
//! with no venv, prompt the operator and provision one inline with the
//! same `uv venv` + deps-install steps the station installer runs.
//!
//! Cache invalidation: a stamp file at `<venv>/.tofupilot-stamp` stores
//! a hash of `(runtime_version, deps_file_contents)`. Mismatch rebuilds
//! the venv. Match short-circuits — second-run cost is one file read.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::commands::pull::sync::{create_venv, uv_path, venv_python};

/// Result of resolving the venv for a local-path run.
#[derive(Debug)]
pub struct EnsuredVenv {
    /// Absolute path to the interpreter inside the venv.
    pub python: PathBuf,
}

/// Deps source detected in the project. The detection order encodes
/// preference: a modern `pyproject.toml` project always wins, so a
/// `requirements.txt` shipped for legacy tooling is ignored when a
/// pyproject is also present.
///
/// For pyproject projects we record both the file path AND the
/// directory uv should resolve from (the *workspace root*). Multi-
/// procedure repos commit one pyproject at the repo root with
/// `[tool.uv.workspace] members = ["procedures/*"]` and individual
/// procedure dirs that don't carry their own pyproject. Walking up
/// from `<procedure_dir>` until we hit a pyproject means `uv sync`
/// runs against the workspace root and uv handles member linking.
enum DepsSource {
    Pyproject {
        /// Path to the resolved `pyproject.toml`. Equal to
        /// `<procedure_dir>/pyproject.toml` for single-procedure
        /// repos, equal to the ancestor workspace root's pyproject
        /// for multi-procedure repos.
        pyproject: PathBuf,
        /// Directory containing `pyproject`. uv invocations
        /// (`uv sync`) cd here so workspace resolution starts at the
        /// right place.
        root: PathBuf,
    },
    Requirements(PathBuf),
    None,
}

impl DepsSource {
    fn detect(project_dir: &Path) -> Self {
        // Walk up to 8 levels (matches the engine's bounded walk-up
        // for venv resolution) looking for a pyproject. Hitting the
        // filesystem root or running out of budget falls through to
        // requirements.txt detection in the original procedure dir.
        let mut current: Option<&Path> = Some(project_dir);
        for _ in 0..8 {
            let Some(dir) = current else { break };
            let pyproject = dir.join("pyproject.toml");
            if pyproject.exists() {
                return DepsSource::Pyproject {
                    pyproject,
                    root: dir.to_path_buf(),
                };
            }
            current = dir.parent();
        }
        let req = project_dir.join("requirements.txt");
        if req.exists() {
            return DepsSource::Requirements(req);
        }
        DepsSource::None
    }

    fn path(&self) -> Option<&Path> {
        match self {
            DepsSource::Pyproject { pyproject, .. } => Some(pyproject),
            DepsSource::Requirements(p) => Some(p),
            DepsSource::None => None,
        }
    }
}

/// Stamp = sha256 of `runtime_version || "\0" || deps_file_bytes`.
/// Stored at `<venv>/.tofupilot-stamp`. A mismatch (operator bumped
/// `requires-python`, added a dep, etc.) forces a rebuild on the next
/// run. Bytes-of-the-file is intentional — uv's resolver output isn't
/// reproducible enough to hash post-install, and a pyproject change
/// usually implies a sync regardless.
fn compute_stamp(runtime_version: &str, deps: &DepsSource) -> String {
    let mut hasher = Sha256::new();
    hasher.update(runtime_version.as_bytes());
    hasher.update(b"\0");
    if let Some(path) = deps.path() {
        if let Ok(bytes) = std::fs::read(path) {
            hasher.update(&bytes);
        }
    }
    format!("{:x}", hasher.finalize())
}

/// Build the shell command an operator can paste to provision the venv
/// by hand. Switches on `DepsSource` so pyproject projects don't get
/// pointed at a non-existent `requirements.txt`, and dep-less
/// procedures don't get an install step at all.
fn manual_bootstrap_hint(runtime_version: &str, deps: &DepsSource) -> String {
    match deps {
        DepsSource::Pyproject { root, .. } => {
            // `uv sync` walks up to the workspace root automatically,
            // but for the cd hint we point the operator at the
            // pyproject dir directly so the suggestion is copy-paste
            // ready regardless of where they are.
            format!(
                "cd {} && UV_PROJECT_ENVIRONMENT=venv uv sync --python {runtime_version}",
                root.display(),
            )
        }
        DepsSource::Requirements(_) => {
            format!("uv venv venv --python {runtime_version} && uv pip install -r requirements.txt",)
        }
        DepsSource::None => format!("uv venv venv --python {runtime_version}"),
    }
}

fn stamp_path(venv: &Path) -> PathBuf {
    venv.join(".tofupilot-stamp")
}

fn read_stamp(venv: &Path) -> Option<String> {
    std::fs::read_to_string(stamp_path(venv)).ok()
}

fn write_stamp(venv: &Path, stamp: &str) -> crate::error::CliResult<()> {
    std::fs::write(stamp_path(venv), stamp).map_err(|e| format!("Write venv stamp: {e}").into())
}

/// Resolve the Python runtime version for a local-path project.
///
/// Priority:
///   1. Resolved pyproject (workspace root for monorepos, procedure
///      dir for single-procedure repos) → `[project] requires-python`.
///      PEP 621 allows comma-separated specifiers (`>=3.12,<3.14`);
///      we take the first one because uv wants a single `--python`
///      arg. Leading comparison operators (`>=`, `~=`, `==`, `>`,
///      `^`, `!=`, `<`, `<=`) are stripped to leave just the version.
///   2. Default `3.11` — same default uv picks when given no hint.
fn resolve_runtime_version(deps: &DepsSource) -> String {
    const DEFAULT: &str = "3.11";
    let pyproject = match deps {
        DepsSource::Pyproject { pyproject, .. } => pyproject,
        DepsSource::Requirements(_) | DepsSource::None => return DEFAULT.to_string(),
    };
    let contents = match std::fs::read_to_string(pyproject) {
        Ok(c) => c,
        // Read error on a file we just listed — vanishingly rare.
        // Silently fall through to the default.
        Err(_) => return DEFAULT.to_string(),
    };
    match contents.parse::<toml::Value>() {
        Ok(value) => {
            if let Some(req) = value
                .get("project")
                .and_then(|p| p.get("requires-python"))
                .and_then(|v| v.as_str())
            {
                // First specifier in a comma-separated list. `>=3.12,<3.14`
                // → `>=3.12` → `3.12`. uv can't consume a multi-spec
                // string as `--python` input, so we pick the lower bound
                // and let uv resolve to the latest compatible install.
                if let Some(first) = req.split(',').next() {
                    let trimmed = first
                        .trim()
                        .trim_start_matches([' ', '>', '=', '<', '~', '^', '!']);
                    if !trimmed.is_empty() {
                        return trimmed.to_string();
                    }
                }
            }
        }
        Err(e) => {
            crate::log::warn(&format!(
                "Could not parse {}: {e}. Falling back to Python {DEFAULT} for venv bootstrap.",
                pyproject.display(),
            ));
        }
    }
    DEFAULT.to_string()
}

/// Ask the operator before provisioning. Returns `true` to proceed.
///
/// Non-interactive callers (kiosk, daemon, agent) auto-proceed: a
/// station running detached has no tty to prompt on. Interactive
/// terminal sessions get an actual prompt; default is `Y` so an
/// operator who hammers Enter still gets a working venv.
fn confirm_bootstrap(project_dir: &Path, runtime_version: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        crate::log::warn(&format!(
            "No venv at {} and stdin is not a tty; bootstrapping automatically.",
            project_dir.display()
        ));
        return true;
    }
    let prompt = format!(
        "No Python venv at {}. Bootstrap one (uv venv --python {})?",
        project_dir.display(),
        runtime_version,
    );
    dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(true)
        .interact()
        .unwrap_or(false)
}

/// Spawn a blocking uv invocation on the tokio threadpool. uv's
/// stdout is redirected to our stderr so operators still see its
/// progress output while the parent's stdout stays reserved for the
/// `--json` event stream (uv writing "Using CPython..." to stdout
/// would corrupt it). `env` lets callers inject things like
/// `UV_PROJECT_ENVIRONMENT` without leaking them into the parent
/// process. `label` flows into the error message; matches the shape
/// `sync::run_cmd` expects so call sites stay symmetrical.
async fn run_uv(
    uv: &Path,
    args: Vec<std::ffi::OsString>,
    cwd: &Path,
    env: &[(&'static str, std::ffi::OsString)],
    label: &'static str,
) -> crate::error::CliResult<()> {
    let uv = uv.to_path_buf();
    let cwd = cwd.to_path_buf();
    let env: Vec<(&'static str, std::ffi::OsString)> =
        env.iter().map(|(k, v)| (*k, v.clone())).collect();
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(&uv);
        command.args(&args).current_dir(&cwd);
        for (k, v) in &env {
            command.env(k, v);
        }
        command.stdout(std::process::Stdio::piped());
        let mut child = command.spawn().map_err(|e| format!("Spawn {label}: {e}"))?;
        let drain = child.stdout.take().map(|mut out| {
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut out, &mut std::io::stderr());
            })
        });
        let status = child.wait().map_err(|e| format!("Wait {label}: {e}"))?;
        // Join the drain so the tool's final output (the part that
        // explains a failure) lands before our error message.
        if let Some(handle) = drain {
            let _ = handle.join();
        }
        if !status.success() {
            return Err(
                format!("{label} exited with status {}", status.code().unwrap_or(-1),).into(),
            );
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("{label} task panicked: {e}"))?
}

/// Provision a venv with `uv` and install deps. The venv always
/// lands at `<venv_dir>/venv` so the runtime side has one path to
/// resolve. `venv_dir` equals the procedure dir for single-procedure
/// repos and the workspace root for monorepos — this matches the
/// station installer's `<package_dir>/venv` shape. The absolute
/// interpreter path flows back through `ensure_venv` →
/// `prepare_run`, which hands it to the engine as-is; no walk-up
/// happens at run time.
///
/// Pyproject path delegates to `uv sync` with
/// `UV_PROJECT_ENVIRONMENT=venv`, so uv handles workspace member
/// resolution (the studio app does the same — see
/// `apps/studio/src-tauri/src/python/venv.rs::sync_python`). uv
/// creates the venv as part of `sync`, so we skip the manual
/// `create_venv` call on this path.
///
/// Requirements path keeps the old `uv venv` + `uv pip install -r`
/// shape: no workspace concept, the venv lives next to the
/// requirements file.
async fn provision(
    venv_dir: &Path,
    runtime_version: &str,
    deps: &DepsSource,
) -> crate::error::CliResult<()> {
    let uv = uv_path().await?.to_path_buf();
    let venv = venv_dir.join("venv");

    match deps {
        DepsSource::Pyproject { root, .. } => {
            // Wipe any pre-existing `venv/` so uv sync starts clean.
            // sync would normally reuse the directory; explicit wipe
            // means a corrupted prior run can't taint the rebuild.
            let _ = std::fs::remove_dir_all(&venv);
            let args = vec!["sync".into(), "--python".into(), runtime_version.into()];
            run_uv(
                &uv,
                args,
                root,
                &[("UV_PROJECT_ENVIRONMENT", venv.as_os_str().to_owned())],
                "uv sync (pyproject)",
            )
            .await?;
        }
        DepsSource::Requirements(req) => {
            let venv_owned = venv.clone();
            let runtime = runtime_version.to_string();
            let venv_dir_owned = venv_dir.to_path_buf();
            let uv_for_venv = uv.clone();
            tokio::task::spawn_blocking(move || {
                create_venv(&uv_for_venv, &venv_owned, &runtime, &venv_dir_owned)
            })
            .await
            .map_err(|e| format!("uv venv task panicked: {e}"))??;

            let python = venv_python(&venv);
            let args = vec![
                "pip".into(),
                "install".into(),
                "--python".into(),
                python.as_os_str().to_owned(),
                "-r".into(),
                req.as_os_str().to_owned(),
            ];
            run_uv(
                &uv,
                args,
                venv_dir,
                &[],
                "uv pip install (requirements.txt)",
            )
            .await?;
        }
        DepsSource::None => {
            let venv_owned = venv.clone();
            let runtime = runtime_version.to_string();
            let venv_dir_owned = venv_dir.to_path_buf();
            let uv_for_venv = uv.clone();
            tokio::task::spawn_blocking(move || {
                create_venv(&uv_for_venv, &venv_owned, &runtime, &venv_dir_owned)
            })
            .await
            .map_err(|e| format!("uv venv task panicked: {e}"))??;
            crate::log::warn(
                "No pyproject.toml or requirements.txt found; venv created without deps. \
                 Procedure imports will fail unless the stdlib covers them.",
            );
        }
    }

    Ok(())
}

/// Detect missing venv and provision one if the operator agrees.
///
/// Returns `Ok(EnsuredVenv)` when a usable venv is in place at
/// `<project>/venv` — either because it already existed and its stamp
/// matched, or because we just built it. Returns `Err` when the user
/// declined, the build failed, or bootstrap was disabled by flag.
///
/// `bootstrap_enabled = false` matches the operator passing
/// `--no-bootstrap`: we fall through with the original error so the
/// existing `env_error` path surfaces the diagnostic.
pub async fn ensure_venv(
    project_dir: &Path,
    bootstrap_enabled: bool,
) -> crate::error::CliResult<EnsuredVenv> {
    // Venv lives at `<venv_dir>/venv`. For single-procedure repos
    // `venv_dir == project_dir` and the layout matches station mode's
    // installer (`<package_dir>/venv`). For uv-workspace monorepos
    // `venv_dir == DepsSource::Pyproject.root`, the ancestor pyproject
    // dir — the venv is shared across every procedure in the
    // workspace, identical to what the station installer produces for
    // monorepo bundles. The interpreter path returned from this
    // function is absolute, so `prepare_run` doesn't need to know
    // which layout it got.
    let deps = DepsSource::detect(project_dir);
    let venv_dir: PathBuf = match &deps {
        DepsSource::Pyproject { root, .. } => root.clone(),
        DepsSource::Requirements(_) | DepsSource::None => project_dir.to_path_buf(),
    };
    let venv = venv_dir.join("venv");
    let python = venv_python(&venv);
    let runtime_version = resolve_runtime_version(&deps);
    let expected_stamp = compute_stamp(&runtime_version, &deps);

    // Three states for an existing `venv/`:
    //   * stamp matches expected → fast path, return interpreter.
    //   * stamp present but stale → rebuild (skip the missing-venv
    //     prompt because the venv exists; the operator already
    //     consented to bootstrap when they first ran).
    //   * no stamp at all → operator built this venv by hand. Don't
    //     touch it; stamp in place so future drift is detected.
    let rebuilding = if python.exists() {
        match read_stamp(&venv) {
            Some(actual) if actual == expected_stamp => return Ok(EnsuredVenv { python }),
            Some(_) => {
                if !bootstrap_enabled {
                    // Stamp drift but operator opted out of auto-rebuild;
                    // hand the (potentially stale) interpreter back and
                    // let the run proceed. They'll see import errors if
                    // deps actually changed, which is the contract of
                    // --no-bootstrap.
                    return Ok(EnsuredVenv { python });
                }
                true
            }
            None => {
                // Existing venv without a stamp — hand-built or
                // pre-bootstrap. Stamp it so future runs detect drift;
                // surface write failures so a RO mount doesn't
                // silently disable caching forever.
                if let Err(e) = write_stamp(&venv, &expected_stamp) {
                    crate::log::warn(&format!(
                        "Could not write venv stamp at {}: {e}. \
                         Future runs will not detect dependency drift.",
                        stamp_path(&venv).display(),
                    ));
                }
                return Ok(EnsuredVenv { python });
            }
        }
    } else {
        false
    };

    if !bootstrap_enabled {
        return Err(format!(
            "No Python venv at {} and --no-bootstrap was passed. \
             Rerun without --no-bootstrap to provision one automatically, \
             or create the venv manually: `{}`.",
            python.display(),
            manual_bootstrap_hint(&runtime_version, &deps),
        )
        .into());
    }

    // Prompt the operator only on a true first-time bootstrap. A
    // rebuild fires because deps changed under an already-consented
    // venv — re-asking would be noise.
    if !rebuilding && !confirm_bootstrap(&venv_dir, &runtime_version) {
        return Err(format!(
            "Operator declined to bootstrap venv at {}. \
             Rerun with --no-bootstrap to skip the prompt, \
             or create the venv manually before invoking `tofupilot run`.",
            venv.display(),
        )
        .into());
    }

    crate::log::info(&format!(
        "{} venv at {} (python {})",
        if rebuilding {
            "Rebuilding"
        } else {
            "Bootstrapping"
        },
        venv.display(),
        runtime_version,
    ));
    provision(&venv_dir, &runtime_version, &deps).await?;
    // Stamp-write failure is non-fatal — the venv is built and usable.
    // Losing the stamp just means future runs won't detect dependency
    // drift. Match the warn-and-continue posture of the unstamped-
    // existing-venv branch so identical end state (built venv, no
    // stamp) produces identical observable behavior.
    if let Err(e) = write_stamp(&venv, &expected_stamp) {
        crate::log::warn(&format!(
            "Could not write venv stamp at {}: {e}. \
             Future runs will not detect dependency drift.",
            stamp_path(&venv).display(),
        ));
    }
    Ok(EnsuredVenv { python })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn pyproject_at(dir: &Path, body: &[u8]) -> DepsSource {
        let pyproject = dir.join("pyproject.toml");
        fs::write(&pyproject, body).unwrap();
        DepsSource::Pyproject {
            pyproject,
            root: dir.to_path_buf(),
        }
    }

    #[test]
    fn stamp_changes_when_deps_change() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = pyproject_at(tmp.path(), b"[project]\nname='a'\ndeps=[]\n");
        let s1 = compute_stamp("3.11", &deps);
        fs::write(
            tmp.path().join("pyproject.toml"),
            b"[project]\nname='a'\ndeps=['x']\n",
        )
        .unwrap();
        let s2 = compute_stamp("3.11", &deps);
        assert_ne!(s1, s2);
    }

    #[test]
    fn stamp_changes_when_runtime_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let req = tmp.path().join("requirements.txt");
        fs::write(&req, b"openhtf\n").unwrap();
        let deps = DepsSource::Requirements(req);
        assert_ne!(compute_stamp("3.11", &deps), compute_stamp("3.12", &deps),);
    }

    #[test]
    fn deps_detect_prefers_pyproject_over_requirements() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("pyproject.toml"), b"[project]\nname='a'\n").unwrap();
        fs::write(tmp.path().join("requirements.txt"), b"openhtf\n").unwrap();
        match DepsSource::detect(tmp.path()) {
            DepsSource::Pyproject { root, .. } => assert_eq!(root, tmp.path()),
            _ => panic!("expected pyproject to win"),
        }
    }

    /// Nested workspaces: an inner pyproject (e.g. a member with its
    /// own pyproject) must win over an outer one. Otherwise we'd run
    /// `uv sync` from the outer workspace and the inner deps would
    /// be ignored. Walk-up returns the *nearest* ancestor.
    #[test]
    fn deps_detect_picks_nearest_ancestor_pyproject() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path();
        let inner = outer.join("nested").join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(outer.join("pyproject.toml"), b"[project]\nname='outer'\n").unwrap();
        fs::write(inner.join("pyproject.toml"), b"[project]\nname='inner'\n").unwrap();

        match DepsSource::detect(&inner) {
            DepsSource::Pyproject { root, .. } => assert_eq!(root, inner),
            _ => panic!("expected nearest ancestor (inner) to win"),
        }
    }

    /// Monorepo: procedure dir has no pyproject of its own, but an
    /// ancestor does. Detection must walk up and return the ancestor
    /// as the workspace root so `uv sync` runs against the right
    /// directory. This is the failure mode the user reported.
    #[test]
    fn deps_detect_walks_up_to_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("pyproject.toml"),
            b"[project]\nname='monorepo'\n[tool.uv.workspace]\nmembers=['procedures/*']\n",
        )
        .unwrap();
        let proc_dir = root.join("procedures").join("ft_smoke");
        fs::create_dir_all(&proc_dir).unwrap();

        match DepsSource::detect(&proc_dir) {
            DepsSource::Pyproject {
                root: detected_root,
                pyproject,
            } => {
                assert_eq!(detected_root, root);
                assert_eq!(pyproject, root.join("pyproject.toml"));
            }
            _ => panic!("expected pyproject discovered at workspace root"),
        }
    }

    #[test]
    fn deps_detect_falls_back_to_requirements() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("requirements.txt"), b"openhtf\n").unwrap();
        match DepsSource::detect(tmp.path()) {
            DepsSource::Requirements(_) => {}
            _ => panic!("expected requirements.txt fallback"),
        }
    }

    #[test]
    fn deps_detect_none_when_nothing_present() {
        let tmp = tempfile::tempdir().unwrap();
        match DepsSource::detect(tmp.path()) {
            DepsSource::None => {}
            _ => panic!("expected none"),
        }
    }

    #[test]
    fn runtime_version_reads_pyproject_requires_python() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = pyproject_at(
            tmp.path(),
            b"[project]\nname='a'\nrequires-python='>=3.12'\n",
        );
        assert_eq!(resolve_runtime_version(&deps), "3.12");
    }

    /// PEP 621 allows `>=3.12,<3.14`. uv's `--python` arg is a single
    /// spec, so we take the lower bound. Regression for a reviewer-
    /// caught bug where the full string was passed through and uv
    /// rejected it.
    #[test]
    fn runtime_version_handles_multi_spec_requires_python() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = pyproject_at(
            tmp.path(),
            b"[project]\nname='a'\nrequires-python='>=3.12,<3.14'\n",
        );
        assert_eq!(resolve_runtime_version(&deps), "3.12");
    }

    /// Manual bootstrap hint must match the actual deps source so an
    /// operator pasting the suggestion doesn't hit "requirements.txt
    /// does not exist" on a pyproject-only project, and so monorepo
    /// users get the `uv sync` workspace-aware command instead of the
    /// per-dir `uv pip install -e .` that fails outside workspace
    /// roots.
    #[test]
    fn manual_bootstrap_hint_switches_on_deps_source() {
        let tmp = tempfile::tempdir().unwrap();
        let pyp_deps = pyproject_at(tmp.path(), b"");
        let req = tmp.path().join("requirements.txt");
        fs::write(&req, b"").unwrap();

        let pyp_hint = manual_bootstrap_hint("3.12", &pyp_deps);
        assert!(pyp_hint.contains("uv sync"), "got: {pyp_hint}");
        assert!(
            pyp_hint.contains("UV_PROJECT_ENVIRONMENT=venv"),
            "got: {pyp_hint}",
        );
        assert!(
            pyp_hint.contains(&format!("cd {}", tmp.path().display())),
            "hint should cd to workspace root, got: {pyp_hint}",
        );

        let req_hint = manual_bootstrap_hint("3.12", &DepsSource::Requirements(req));
        assert!(
            req_hint.contains("uv pip install -r requirements.txt"),
            "got: {req_hint}",
        );

        let none_hint = manual_bootstrap_hint("3.12", &DepsSource::None);
        assert!(!none_hint.contains("uv pip install"), "got: {none_hint}");
        assert!(none_hint.contains("uv venv"), "got: {none_hint}");
    }

    #[test]
    fn runtime_version_defaults_when_no_pyproject() {
        assert_eq!(resolve_runtime_version(&DepsSource::None), "3.11");
    }

    /// `--no-bootstrap` with no existing venv must error out instead
    /// of silently proceeding. Mirrors the explicit-opt-out contract.
    #[tokio::test]
    async fn ensure_venv_errors_with_no_bootstrap_and_no_venv() {
        let tmp = tempfile::tempdir().unwrap();
        let err = ensure_venv(tmp.path(), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--no-bootstrap"), "got: {err}");
    }

    /// `--no-bootstrap` on a venv with a stale stamp: hand back the
    /// stale interpreter, don't rebuild. Operator opted out of
    /// automatic provisioning — they accept the risk of mismatched
    /// deps. Pins the documented contract for branch (d) in
    /// `ensure_venv`'s state machine.
    #[tokio::test]
    async fn ensure_venv_returns_stale_interpreter_when_bootstrap_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let venv = tmp.path().join("venv");
        let python = if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        };
        fs::create_dir_all(python.parent().unwrap()).unwrap();
        fs::write(&python, b"").unwrap();
        fs::write(stamp_path(&venv), b"stale-hash-from-an-old-run").unwrap();

        let ensured = ensure_venv(tmp.path(), false).await.unwrap();
        assert_eq!(ensured.python, python);
        // Stamp file untouched — bootstrap-disabled must not rewrite
        // it, or the next run (with bootstrap re-enabled) would skip
        // the rebuild it should now trigger.
        let stamp_after = fs::read_to_string(stamp_path(&venv)).unwrap();
        assert_eq!(stamp_after, "stale-hash-from-an-old-run");
    }

    /// Existing venv without a stamp must not trigger rebuild — that
    /// would clobber a hand-crafted venv on first run after upgrade.
    #[tokio::test]
    async fn ensure_venv_stamps_existing_unstamped_venv() {
        let tmp = tempfile::tempdir().unwrap();
        let python = if cfg!(windows) {
            tmp.path().join("venv").join("Scripts").join("python.exe")
        } else {
            tmp.path().join("venv").join("bin").join("python")
        };
        fs::create_dir_all(python.parent().unwrap()).unwrap();
        fs::write(&python, b"").unwrap();

        let ensured = ensure_venv(tmp.path(), true).await.unwrap();
        assert_eq!(ensured.python, python);
        assert!(stamp_path(&tmp.path().join("venv")).exists());
    }
}
