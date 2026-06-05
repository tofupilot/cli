//! Deployment sync: fetch an artifact descriptor from the web API, download
//! the pre-built bundle from object storage, verify its sha256, extract the
//! zstd-compressed tarball, and run the station installer.
//!
//! Artifact format is produced by apps/build-worker + docker/build-python:
//!
//! ```text
//! deployment.tar.zst
//!   └── bundle/
//!       ├── wheels/*.whl      # transitive dep wheels (standalone only)
//!       ├── vendor/*.whl      # workspace shared-lib wheels
//!       ├── project/<tree>    # procedure source tree
//!       ├── pylock.toml       # PEP 751 lockfile (hashes + pins)
//!       └── manifest.json     # schema v1, kind=source
//! ```
//!
//! The CLI owns installation end-to-end in Rust — no shell script ships in
//! the bundle. Source-shipping is universal: the procedure tree is moved
//! to the deployment-dir root by the installer and run from there;
//! framework detection happens at run time, not install time. The install
//! shells out to `uv` (which the station's CLI installer puts on PATH);
//! standalone mode adds `--no-index --find-links wheels` to keep the
//! install offline.

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::commands::auth::credentials::Credentials;
use crate::commands::db::{PullState, StateDb};
use crate::http::RequestBuilderExt;

/// Path to the `uv` binary. Resolved lazily by `uv_path()` —
/// PATH first, then `~/.tofupilot/bin/uv`, then a fresh download
/// from astral-sh GitHub releases. Cached for the lifetime of the
/// process so we hit `ensure_uv` once even though 10 install
/// commands fan out from a single pull.
static UV_PATH: tokio::sync::OnceCell<std::path::PathBuf> = tokio::sync::OnceCell::const_new();

pub(crate) async fn uv_path() -> crate::error::CliResult<&'static std::path::Path> {
    UV_PATH
        .get_or_try_init(|| async { crate::commands::uv_bootstrap::ensure_uv().await })
        .await
        .map(|p| p.as_path())
}

pub struct PullInfo {
    /// Human-readable procedure name used for log output and stored in
    /// PullState.name. The authoritative git SHA lives on the artifact
    /// descriptor fetched per-procedure, not here.
    pub name: String,
    /// station_deployment row id from the /api/cli/pull list response.
    /// Persisted into PullState so the cleanup pass can stamp
    /// DeploymentRemoved events with the right id without re-querying.
    pub deployment_id: String,
}

pub struct PullResult {
    pub path: PathBuf,
    pub file_count: u32,
}

#[derive(serde::Deserialize)]
struct ArtifactDescriptor {
    /// Authoritative git SHA for this artifact. We prefer this over the SHA
    /// from the /api/cli/pull list response because the station_deployment
    /// row may have been rotated between the list fetch and this descriptor
    /// fetch; the descriptor reflects what the artifact actually contains.
    sha: String,
    #[serde(rename = "artifactUrl")]
    artifact_url: String,
    #[serde(rename = "artifactSha256")]
    artifact_sha256: String,
}

/// Download + install the latest artifact for a procedure.
///
/// Three-phase atomic swap: the new bundle lands in `<proc>.tmp`, the current
/// directory is renamed to `<proc>.old`, the temp to `<proc>`, then `.old` is
/// removed. This guarantees that `<proc>/` is never partially replaced, even
/// on crash mid-install.
pub async fn pull_deployment(
    creds: &Credentials,
    db: &StateDb,
    procedure_id: &str,
    info: &PullInfo,
) -> crate::error::CliResult<PullResult> {
    let deployment_dir = deployment_path(procedure_id)?;

    let tmp_dir = deployment_dir.with_extension("tmp");
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir).map_err(|e| format!("Clean temp dir: {e}"))?;
    }
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("Create temp dir: {e}"))?;

    let descriptor = fetch_descriptor(&creds.base_url, &creds.api_key, procedure_id).await?;

    // Stream the artifact to disk rather than buffering it all in memory —
    // bundles can easily run to a few hundred MB between runtime + deps.
    let artifact_path = tmp_dir.join("artifact.tar.zst");
    download_artifact_to_file(
        &descriptor.artifact_url,
        creds,
        &artifact_path,
        &descriptor.artifact_sha256,
    )
    .await?;

    let file_count = extract_artifact_from_file(&artifact_path, &tmp_dir)?;
    // The artifact is no longer needed on disk once extracted.
    let _ = std::fs::remove_file(&artifact_path);

    run_installer(&tmp_dir).await?;

    // Three-phase swap: rename old to .old, rename tmp to final, delete .old
    let old_dir = deployment_dir.with_extension("old");
    if old_dir.exists() {
        std::fs::remove_dir_all(&old_dir).map_err(|e| format!("Clean stale .old dir: {e}"))?;
    }
    if deployment_dir.exists() {
        std::fs::rename(&deployment_dir, &old_dir)
            .map_err(|e| format!("Move old deployment: {e}"))?;
    }
    std::fs::rename(&tmp_dir, &deployment_dir).map_err(|e| format!("Move new deployment: {e}"))?;
    // fsync the parent directory so the rename itself is durable on
    // power loss. Without this, a crash between rename and dirent
    // commit can leave the deployment dir empty on reboot. The update
    // path at update/download.rs already does this; mirror it here.
    #[cfg(unix)]
    if let Some(parent) = deployment_dir.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    let _ = std::fs::remove_dir_all(&old_dir);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&deployment_dir, std::fs::Permissions::from_mode(0o700));
    }

    // descriptor.sha is the authoritative SHA — we use it (not the value
    // from the list response) because the deployment row could have been
    // superseded between the list fetch and this descriptor fetch.
    db.set_pull_state(
        procedure_id,
        &PullState {
            sha: descriptor.sha.clone(),
            pulled_at: chrono::Utc::now(),
            name: Some(info.name.clone()),
            deployment_id: info.deployment_id.clone(),
        },
    )?;

    Ok(PullResult {
        path: deployment_dir,
        file_count,
    })
}

/// Staged deployment — same mechanics as `pull_deployment` but stops before
/// the atomic swap. Used by the station loop to prepare a new bundle between
/// test cycles without disturbing the currently-executing procedure.
pub struct StagedDeployment {
    pub procedure_id: String,
    pub staging_path: PathBuf,
    pub active_path: PathBuf,
    pub sha: String,
    pub name: String,
    pub deployment_id: String,
}

impl StagedDeployment {
    pub async fn apply(self, db: &StateDb) -> crate::error::CliResult<PathBuf> {
        let old_dir = self.active_path.with_extension("old");
        if old_dir.exists() {
            let _ = std::fs::remove_dir_all(&old_dir);
        }
        if self.active_path.exists() {
            std::fs::rename(&self.active_path, &old_dir)
                .map_err(|e| format!("Move old deployment: {e}"))?;
        }
        std::fs::rename(&self.staging_path, &self.active_path)
            .map_err(|e| format!("Move staged deployment: {e}"))?;

        let old = old_dir.clone();
        tokio::spawn(async move {
            let _ = std::fs::remove_dir_all(&old);
        });

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.active_path, std::fs::Permissions::from_mode(0o700));
        }

        db.set_pull_state(
            &self.procedure_id,
            &PullState {
                sha: self.sha,
                pulled_at: chrono::Utc::now(),
                name: Some(self.name),
                deployment_id: self.deployment_id,
            },
        )?;

        Ok(self.active_path)
    }
}

pub async fn stage_deployment(
    creds: &Credentials,
    procedure_id: &str,
    info: &PullInfo,
) -> crate::error::CliResult<StagedDeployment> {
    let active_path = deployment_path(procedure_id)?;
    let staging_path = active_path.with_extension("staging");

    if staging_path.exists() {
        std::fs::remove_dir_all(&staging_path).map_err(|e| format!("Clean staging dir: {e}"))?;
    }
    std::fs::create_dir_all(&staging_path).map_err(|e| format!("Create staging dir: {e}"))?;

    let descriptor = fetch_descriptor(&creds.base_url, &creds.api_key, procedure_id).await?;

    let artifact_path = staging_path.join("artifact.tar.zst");
    download_artifact_to_file(
        &descriptor.artifact_url,
        creds,
        &artifact_path,
        &descriptor.artifact_sha256,
    )
    .await?;
    extract_artifact_from_file(&artifact_path, &staging_path)?;
    let _ = std::fs::remove_file(&artifact_path);

    run_installer(&staging_path).await?;

    Ok(StagedDeployment {
        procedure_id: procedure_id.to_string(),
        staging_path,
        active_path,
        // See pull_deployment: descriptor.sha is the authoritative SHA.
        sha: descriptor.sha.clone(),
        name: info.name.clone(),
        deployment_id: info.deployment_id.clone(),
    })
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

fn deployment_path(procedure_id: &str) -> crate::error::CliResult<PathBuf> {
    if procedure_id.contains('/')
        || procedure_id.contains('\\')
        || procedure_id.contains("..")
        || procedure_id.is_empty()
    {
        return Err(format!("Invalid procedure ID: {procedure_id}").into());
    }
    let dir = crate::commands::db::deployments_dir()?.join(procedure_id);
    std::fs::create_dir_all(dir.parent().unwrap_or(&dir))
        .map_err(|e| format!("Create deployments dir: {e}"))?;
    Ok(dir)
}

/// Fetch artifact descriptor JSON. The server returns 409 when the
/// deployment exists but the build worker has not produced an artifact yet
/// (pending/building/failed). Surface that as a distinct error so callers
/// can back off rather than treat it as "no deployment".
async fn fetch_descriptor(
    base_url: &str,
    api_key: &str,
    procedure_id: &str,
) -> crate::error::CliResult<ArtifactDescriptor> {
    let base = base_url.strip_suffix('/').unwrap_or(base_url);
    let url = format!("{base}/api/cli/pull/{procedure_id}");

    let res = crate::http::client()
        .get(&url)
        .bearer(api_key)
        .header("User-Agent", "tofupilot-cli")
        .send()
        .await
        .map_err(|e| format!("Request artifact descriptor: {e}"))?;

    if res.status() == reqwest::StatusCode::CONFLICT {
        return Err("Artifact not ready: build is pending, running, or failed".into());
    }

    let res = crate::commands::http::ok_or_describe(res)
        .await
        .map_err(|e| format!("Fetch descriptor: {}", e.body()))?;

    res.json::<ArtifactDescriptor>()
        .await
        .map_err(|e| format!("Parse descriptor: {e}").into())
}

/// Stream the compressed bundle to disk, hashing as we go, and fsync at the
/// end. Bundles routinely reach 100–300 MB; buffering them entirely in RAM
/// would be wasteful on a station and has crashed us in past iterations.
///
/// The bearer token is only sent when the artifact URL host matches the
/// CLI's configured base URL — e.g. the self-hosted web app origin. For any
/// other host (R2, S3, CDN) we deliberately omit the token so the org's API
/// key can't leak into third-party access logs if the URL ever becomes
/// public.
async fn download_artifact_to_file(
    url: &str,
    creds: &Credentials,
    dest: &Path,
    expected_sha256: &str,
) -> crate::error::CliResult<()> {
    let mut req = crate::http::client()
        .get(url)
        .header("User-Agent", "tofupilot-cli");
    if url_host_matches(url, &creds.base_url) {
        req = req.bearer(&creds.api_key);
    }

    let res = req
        .send()
        .await
        .map_err(|e| format!("Download artifact: {e}"))?;
    let mut res = crate::commands::http::ok_or_describe(res)
        .await
        .map_err(|e| format!("Artifact download failed: {}", e.body()))?;

    let mut file = std::fs::File::create(dest).map_err(|e| format!("Create artifact file: {e}"))?;
    let mut hasher = Sha256::new();

    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|e| format!("Read artifact chunk: {e}"))?
    {
        hasher.update(&chunk);
        file.write_all(&chunk)
            .map_err(|e| format!("Write artifact chunk: {e}"))?;
    }
    file.flush().map_err(|e| format!("Flush artifact: {e}"))?;

    let actual = hex::encode(hasher.finalize());
    if actual != expected_sha256 {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "Artifact integrity check failed (expected {expected_sha256}, got {actual})",
        )
        .into());
    }
    Ok(())
}

/// Whether a URL points at the same host as the CLI's configured base URL.
/// Used to decide whether to attach the bearer token — we never send it to
/// third-party hosts (object storage, CDNs) where it could show up in logs.
fn url_host_matches(url: &str, base_url: &str) -> bool {
    let parse = |s: &str| -> Option<String> {
        reqwest::Url::parse(s)
            .ok()
            .and_then(|u| u.host_str().map(|s| s.to_ascii_lowercase()))
    };
    match (parse(url), parse(base_url)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Extract the zstd+tar bundle into `dest` by streaming from a file on
/// disk. Paths are validated against traversal and the top-level
/// `bundle/` prefix is stripped so `dest` becomes the bundle root
/// directly. Returns the number of regular files extracted, used for
/// the human-readable "pulled (N files)" log line.
fn extract_artifact_from_file(path: &Path, dest: &Path) -> crate::error::CliResult<u32> {
    let file = std::fs::File::open(path).map_err(|e| format!("Open artifact file: {e}"))?;
    let decoder =
        zstd::stream::read::Decoder::new(file).map_err(|e| format!("Open zstd stream: {e}"))?;
    let mut archive = tar::Archive::new(decoder);
    let mut file_count: u32 = 0;

    for entry in archive.entries().map_err(|e| format!("Read tar: {e}"))? {
        let mut entry = entry.map_err(|e| format!("Tar entry: {e}"))?;

        let raw_path = entry
            .path()
            .map_err(|e| format!("Entry path: {e}"))?
            .to_path_buf();
        let stripped = strip_bundle_prefix(&raw_path);
        if stripped.as_os_str().is_empty() {
            continue;
        }

        for component in stripped.components() {
            if matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir
            ) {
                return Err(format!("Path traversal blocked: {}", stripped.display()).into());
            }
        }

        let target = dest.join(&stripped);

        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| format!("Create dir: {e}"))?;
            continue;
        }

        if !entry.header().entry_type().is_file() {
            continue;
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Create parent dir: {e}"))?;
        }

        // Stream the file content directly to disk via tar's Read impl
        // — no in-memory copy. Mid-bundle wheels can be tens of MB each.
        let mut out = std::fs::File::create(&target).map_err(|e| format!("Create file: {e}"))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| format!("Write file: {e}"))?;
        file_count += 1;

        // Preserve executable bit — binaries under venv/bin must stay
        // runnable. The tar crate doesn't apply modes by default.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(mode) = entry.header().mode() {
                let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
            }
        }
    }

    Ok(file_count)
}

/// Drop the top-level `bundle/` directory that the build worker wraps the
/// artifact in, so extracted files land directly under the deployment dir.
/// Returns the original path unchanged if the first component isn't
/// literally `bundle` — a future build-worker change that ships a flat
/// layout (or a single top-level `manifest.json`) would otherwise have
/// real content silently discarded.
fn strip_bundle_prefix(path: &Path) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(std::path::Component::Normal(first)) if first == std::ffi::OsStr::new("bundle") => {
            components.as_path().to_path_buf()
        }
        _ => path.to_path_buf(),
    }
}

/// Install a v1 source bundle. One pass that covers all four cells of the
/// (mode × layout) matrix:
///
///   - parse the typed manifest and propagate validated fields
///     (runtime_version, mode) into the install steps;
///   - move `bundle/project/*` to the deployment-dir root so engine /
///     openhtf / plain-python all find their entry by file path;
///   - `uv venv --python <runtime_version>` provisions the interpreter
///     (uv downloads the matching python-build-standalone build on first
///     use and caches globally under `~/.local/share/uv/python`);
///   - `uv pip install -r pylock.toml` installs the dep closure. Mode
///     `standalone` adds `--no-index --find-links wheels/` so a missing
///     wheel surfaces as a hard error rather than a silent PyPI hit;
///   - workspace member wheels in `vendor/` install with `--no-deps`.
///
/// No procedure wheel, no runpy shim. Idempotent: tears down any
/// pre-existing venv. Requires `uv` on PATH (the station's CLI installer
/// puts it there).
async fn run_installer(dir: &Path) -> crate::error::CliResult<()> {
    use execution_engine::manifest::{Manifest, V1Kind, V1};
    let manifest_path = dir.join("manifest.json");
    let manifest = Manifest::parse(&manifest_path).map_err(|e| e.to_string())?;
    // Match (not irrefutable let) so adding a new Manifest variant or
    // V1Kind in the future surfaces a compile error here, not silently
    // routes through this install path.
    let source = match manifest {
        Manifest::V1(V1 {
            kind: V1Kind::Source(s),
            ..
        }) => s,
    };
    let python_version = source.runtime_version.clone();
    let mode = source.mode.clone();
    // Workspace-mode bundles surface the procedure source under a
    // subdirectory of the deployment root; single-package bundles
    // surface it at the root. Either way, the venv lives at the same
    // place as `main.py` / `procedure.yaml`, so the runtime side has
    // exactly one path to thread.
    let package_subdir: Option<PathBuf> = source.root_directory.as_deref().map(PathBuf::from);
    let dir = dir.to_path_buf();
    let uv = uv_path().await?.to_path_buf();

    tokio::task::spawn_blocking(move || {
        // Surface the project tree at the deployment-dir root. The build
        // packs source under `bundle/project/`; the extractor strips the
        // leading `bundle/`, leaving `<dir>/project/` alongside
        // `pylock.toml` and `manifest.json`. Move each entry up one level
        // so `<dir>/procedure.yaml` (or `main.py`) works for the engine /
        // openhtf detection.
        let project = dir.join("project");
        if project.exists() {
            for entry in std::fs::read_dir(&project)
                .map_err(|e| format!("Read project dir: {e}"))?
            {
                let entry = entry.map_err(|e| format!("Iterate project dir: {e}"))?;
                let target = dir.join(entry.file_name());
                // The bundle should never collide with sibling artifact
                // files (manifest.json, pylock.toml). Hard-error if it
                // does — silently overwriting either would brick the
                // installer's own metadata.
                if target.exists() {
                    return Err(format!(
                        "Source artifact collision: bundle file {} already exists at deployment root",
                        entry.file_name().to_string_lossy()
                    )
                    .into());
                }
                std::fs::rename(entry.path(), &target)
                    .map_err(|e| format!("Move {}: {e}", entry.file_name().to_string_lossy()))?;
            }
            std::fs::remove_dir(&project)
                .map_err(|e| format!("Remove empty project dir: {e}"))?;
        }

        // The venv goes inside the package dir. For single-package
        // bundles (`root_directory = null`) that's the deployment
        // root, identical to the prior layout. For workspace-mode
        // bundles it lives next to the procedure's `main.py` /
        // `procedure.yaml`, so the runtime side threads one path.
        let package_dir = match &package_subdir {
            Some(rel) => dir.join(rel),
            None => dir.clone(),
        };
        let venv = package_dir.join("venv");
        let lockfile = dir.join("pylock.toml");
        if !lockfile.exists() {
            return Err(format!(
                "Bundle is missing pylock.toml at {}",
                lockfile.display(),
            )
            .into());
        }

        create_venv(&uv, &venv, &python_version, &dir)?;

        let python = venv_python(&venv);
        let wheels = dir.join("wheels");
        let vendor = dir.join("vendor");
        let standalone = matches!(mode, execution_engine::manifest::Mode::Standalone);

        // Mode-aware deps install. Standalone adds `--no-index` + the
        // shipped wheelhouse; sync resolves online from the cache or
        // configured index. pylock.toml carries hashes; uv verifies
        // every artifact regardless of source. `--preview-features
        // pylock` silences uv's experimental-feature warning.
        let mut install_args: Vec<&std::ffi::OsStr> = vec![
            std::ffi::OsStr::new("--preview-features"),
            std::ffi::OsStr::new("pylock"),
            std::ffi::OsStr::new("pip"),
            std::ffi::OsStr::new("install"),
            std::ffi::OsStr::new("--python"),
            python.as_os_str(),
        ];
        if standalone {
            install_args.push(std::ffi::OsStr::new("--no-index"));
            install_args.push(std::ffi::OsStr::new("--find-links"));
            install_args.push(wheels.as_os_str());
        }
        if vendor.is_dir() {
            install_args.push(std::ffi::OsStr::new("--find-links"));
            install_args.push(vendor.as_os_str());
        }
        install_args.push(std::ffi::OsStr::new("-r"));
        install_args.push(lockfile.as_os_str());
        run_subprocess(&uv, &install_args, &dir, "uv pip install (deps)")?;

        // Workspace member wheels. uv-workspace builds emit one wheel
        // per non-procedure member into `vendor/` — shared packages
        // with plug classes / helpers referenced as
        // `python: shared.foo:Bar`. pylock was exported with
        // `--no-emit-workspace`, so they're not in the deps pass.
        // Source-shipped procedures have no procedure wheel of their
        // own, so every vendor wheel gets installed. Standalone adds
        // `--no-index` so a missing wheel can't accidentally hit PyPI.
        if vendor.is_dir() {
            install_vendor_wheels(&uv, &python, &vendor, &dir, standalone)?;
        }

        Ok::<_, crate::error::CliError>(())
    })
    .await
    .map_err(|e| format!("Installer task panicked: {e}"))?
}

/// Wipe (if present) and re-create a venv at `venv` using uv. Mirrors
/// the station installer's invocation so local-path bootstrap and
/// pulled deployments converge on the same layout (`bin/python` on
/// unix, `Scripts/python.exe` on Windows). Called synchronously from
/// `spawn_blocking` contexts — the inner `uv` exec inherits stdio and
/// blocks the worker thread until done.
pub(crate) fn create_venv(
    uv: &Path,
    venv: &Path,
    runtime_version: &str,
    cwd: &Path,
) -> crate::error::CliResult<()> {
    let _ = std::fs::remove_dir_all(venv);
    run_subprocess(
        uv,
        &[
            std::ffi::OsStr::new("venv"),
            std::ffi::OsStr::new("--python"),
            std::ffi::OsStr::new(runtime_version),
            venv.as_os_str(),
        ],
        cwd,
        "uv venv",
    )
}

/// Venv interpreter path. uv writes the same layout as `python -m venv`:
/// `bin/python` on unix, `Scripts\python.exe` on Windows.
pub(crate) fn venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

/// Spawn a subprocess, inheriting stdio, and surface a descriptive error when
/// it fails. `label` is the human-readable step name used in error messages
/// (e.g. `"pip install"`).
pub(crate) fn run_subprocess(
    program: &Path,
    args: &[&std::ffi::OsStr],
    cwd: &Path,
    label: &str,
) -> crate::error::CliResult<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("Spawn {label}: {e}"))?;
    if !status.success() {
        return Err(format!("{label} exited with status {}", status.code().unwrap_or(-1),).into());
    }
    Ok(())
}

/// Install every `.whl` in `vendor/`. `--no-deps` because the pylock
/// pass already pinned the closure; we just need these specific wheels
/// in site-packages. `no_index` blocks any PyPI fallback for standalone
/// mode so a missing wheel surfaces as a hard error. No-op when
/// `vendor/` is empty.
fn install_vendor_wheels(
    uv: &Path,
    python: &Path,
    vendor: &Path,
    cwd: &Path,
    no_index: bool,
) -> crate::error::CliResult<()> {
    let entries = match std::fs::read_dir(vendor) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    let mut wheels: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("whl") {
            wheels.push(path);
        }
    }
    if wheels.is_empty() {
        return Ok(());
    }
    let mut args: Vec<&std::ffi::OsStr> = vec![
        std::ffi::OsStr::new("pip"),
        std::ffi::OsStr::new("install"),
        std::ffi::OsStr::new("--python"),
        python.as_os_str(),
        std::ffi::OsStr::new("--no-deps"),
    ];
    if no_index {
        args.push(std::ffi::OsStr::new("--no-index"));
    }
    for whl in &wheels {
        args.push(whl.as_os_str());
    }
    run_subprocess(uv, &args, cwd, "uv pip install (workspace members)")
}
