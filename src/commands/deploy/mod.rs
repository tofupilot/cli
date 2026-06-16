//! `tofupilot deploy` — push the linked procedure's local source tree to
//! TofuPilot and build it in the cloud, Vercel-style.
//!
//! Default is a preview deployment (build only, no station fan-out);
//! `--prod` targets production and pushes to every linked station once the
//! build is ready — unless the procedure is rolled back, in which case the
//! pin wins and the CLI warns (same contract as auto-push).
//!
//! Flow: pack (tar.zst, gitignore-aware) → presigned PUT upload → create
//! deployment → stream build logs by polling the `after_seq` cursor →
//! print the dashboard URL. Requires a user-scoped login (`tofupilot
//! login` in the browser); station keys are rejected server-side.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::commands::auth::credentials::{self, Credentials};
use crate::commands::link;
use crate::error::{CliError, CliResult};
use crate::http::{client, RequestBuilderExt};
use crate::log;

pub struct DeployArgs {
    pub path: Option<PathBuf>,
    pub prod: bool,
    pub target: Option<String>,
    pub yes: bool,
}

/// Directory names never shipped, regardless of .gitignore. Mirrors what
/// a git push would exclude plus Python/venv noise that is sometimes
/// committed but never useful to a cloud build.
/// Client-side mirror of the server's MAX_SOURCE_SIZE_BYTES. Checked after
/// packing, before the upload reads the whole tarball into memory, so a
/// runaway directory fails fast with a clear message instead of OOMing or
/// getting a 403 from the size-bound presigned URL.
const MAX_SOURCE_SIZE_BYTES: u64 = 100 * 1024 * 1024;

const ALWAYS_EXCLUDED_DIRS: &[&str] = &[
    ".git",
    "venv",
    ".venv",
    "__pycache__",
    "node_modules",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
];

#[derive(Deserialize)]
struct UploadResponse {
    #[serde(rename = "sourceKey")]
    source_key: String,
    #[serde(rename = "uploadUrl")]
    upload_url: String,
}

#[derive(Deserialize)]
struct CreateResponse {
    #[serde(rename = "deploymentId")]
    deployment_id: String,
    environment: String,
    #[serde(rename = "clearedRollback", default)]
    cleared_rollback: bool,
    #[serde(rename = "pushedStationIds")]
    pushed_station_ids: Vec<String>,
}

#[derive(Deserialize)]
struct LogsResponse {
    status: String,
    entries: Vec<LogEntry>,
}

#[derive(Deserialize)]
struct LogEntry {
    seq: i64,
    level: String,
    text: String,
}

pub async fn run_cmd(args: DeployArgs, json_mode: bool) -> i32 {
    match run(args, json_mode).await {
        Ok(code) => code,
        Err(e) => {
            log::error(&e.to_string());
            1
        }
    }
}

async fn run(args: DeployArgs, json_mode: bool) -> CliResult<i32> {
    let dir = match &args.path {
        Some(p) => p.clone(),
        None => std::env::current_dir().map_err(CliError::from)?,
    };
    let dir = dir
        .canonicalize()
        .map_err(|e| CliError::msg(format!("cannot resolve {}: {e}", dir.display())))?;
    if !dir.is_dir() {
        return Err(CliError::msg(format!(
            "{} is not a directory",
            dir.display()
        )));
    }

    let Some(link) = link::read_link(&dir) else {
        return Err(CliError::msg(format!(
            "{} is not linked to a procedure — run `tofupilot link` first",
            dir.display()
        )));
    };

    // `vercel deploy` parity: preview unless --prod / --target=production.
    let environment = match (&args.target, args.prod) {
        (Some(t), _) if t == "production" => "production",
        (Some(t), _) if t == "preview" => "preview",
        (Some(t), _) => {
            return Err(CliError::msg(format!(
                "unknown target {t:?} — expected \"production\" or \"preview\""
            )));
        }
        (None, true) => "production",
        (None, false) => "preview",
    };

    // The cloud build runs `uv` against this directory's pyproject — fail
    // fast locally rather than after a full upload. Only this directory is
    // packed, so the build runs at its root; a procedure that lives in a
    // repo subdirectory must be deployed from that subdirectory.
    let pyproject = dir.join("pyproject.toml");
    if !pyproject.is_file() {
        return Err(CliError::msg(format!(
            "no pyproject.toml in {} — run `tofupilot deploy` from the procedure's package directory",
            dir.display()
        )));
    }
    // Packing uses follow_symlinks(false), so a symlinked pyproject would
    // ship as a dangling link and the cloud build would fail with a
    // confusing "no pyproject" deep in uv. Reject it here with a clear
    // message instead.
    if pyproject
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(CliError::msg(
            "pyproject.toml is a symlink — the deploy upload ships symlinks as-is and the build would not resolve it. Deploy from the directory that holds the real file.",
        ));
    }
    // The tarball is only this directory. A uv workspace MEMBER resolves its
    // dependencies against a parent `[tool.uv.workspace]` that won't be in
    // the upload, so the cloud build would fail deep in `uv sync`. Warn now
    // with an actionable message instead.
    if let Ok(contents) = std::fs::read_to_string(&pyproject) {
        let is_workspace_root = contents.contains("[tool.uv.workspace]");
        let is_workspace_member =
            contents.contains("[tool.uv.sources]") && contents.contains("workspace = true");
        if is_workspace_member && !is_workspace_root && !json_mode {
            log::warn(
                "This looks like a uv workspace member. Only this directory is uploaded, so workspace dependencies in a parent pyproject.toml won't be available to the build. Deploy from the workspace root, or vendor the dependency.",
            );
        }
    }

    let creds = credentials::require()?;

    // Non-interactive production deploys must opt in explicitly: there is
    // no prompt to fall back on, so refuse rather than silently shipping
    // to every station.
    if environment == "production" && json_mode && !args.yes {
        return Err(CliError::msg(
            "production deploy in --json mode requires --yes (no interactive confirmation available)",
        ));
    }

    if environment == "production" && !args.yes && !json_mode {
        let name = link.procedure_name.as_deref().unwrap_or(&link.procedure_id);
        // Fail loud on a prompt I/O error (closed tty, etc.) rather than
        // silently treating it as "no" — a production deploy shouldn't be
        // silently cancelled by an unrelated terminal error.
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!(
                "Deploy to production? Every station linked to \"{name}\" ({}) will pick this up once built",
                creds.organization_slug
            ))
            .default(false)
            .interact()
            .map_err(|e| CliError::msg(format!("confirmation prompt failed: {e}")))?;
        if !confirmed {
            log::info("Cancelled.");
            return Ok(1);
        }
    }

    if !json_mode {
        log::info(&format!("Packing {}…", dir.display()));
    }
    let dir_for_pack = dir.clone();
    let packed = tokio::task::spawn_blocking(move || pack(&dir_for_pack))
        .await
        .map_err(|e| CliError::msg(format!("pack task panicked: {e}")))??;
    if packed.size_bytes > MAX_SOURCE_SIZE_BYTES {
        return Err(CliError::msg(format!(
            "packed source is {} — exceeds the {} limit. Add large files to .gitignore.",
            human_size(packed.size_bytes),
            human_size(MAX_SOURCE_SIZE_BYTES),
        )));
    }
    if !json_mode {
        log::info(&format!(
            "Source: {} ({} files, sha256 {})",
            human_size(packed.size_bytes),
            packed.file_count,
            &packed.sha256[..8],
        ));
    }

    let base = creds.base_url.trim_end_matches('/');

    // Step 1: presigned upload slot.
    let upload: UploadResponse = api_post(
        &creds,
        &format!("{base}/api/cli/deployments/upload"),
        &serde_json::json!({
            "procedureId": link.procedure_id,
            "sha256": packed.sha256,
            "sizeBytes": packed.size_bytes,
        }),
    )
    .await?;

    // Step 2: PUT the tarball straight to object storage. Content-Type and
    // Content-Length are part of the presigned signature.
    if !json_mode {
        log::info("Uploading…");
    }
    let bytes = tokio::fs::read(packed.tar.path())
        .await
        .map_err(CliError::from)?;
    let put = client()
        .put(&upload.upload_url)
        .header("Content-Type", "application/zstd")
        .body(bytes)
        .send()
        .await
        .map_err(|e| CliError::msg(format!("upload failed: {e}")))?;
    if !put.status().is_success() {
        let status = put.status();
        let body = put.text().await.unwrap_or_default();
        return Err(CliError::msg(format!(
            "upload failed ({status}): {}",
            body.chars().take(300).collect::<String>()
        )));
    }
    drop(packed.tar); // delete the temp file as soon as it's uploaded

    // Step 3: create the deployment (enqueues the cloud build).
    let created: CreateResponse = api_post(
        &creds,
        &format!("{base}/api/cli/deployments"),
        &serde_json::json!({
            "procedureId": link.procedure_id,
            "sourceKey": upload.source_key,
            "sha256": packed.sha256,
            "sizeBytes": packed.size_bytes,
            "environment": environment,
        }),
    )
    .await?;

    let url = format!(
        "{base}/{}/{}/deployments/{}",
        creds.organization_slug, link.procedure_id, created.deployment_id
    );

    if created.cleared_rollback && !json_mode {
        log::warn(
            "Production was rolled back — this deploy overrides the pin and resumes auto-push.",
        );
    }
    if !json_mode {
        log::info(&format!("Building… ({url})"));
    }

    // Step 4: stream build logs until the build reaches a terminal status.
    let status = stream_build(&creds, base, &created.deployment_id, json_mode).await?;

    if json_mode {
        println!(
            "{}",
            serde_json::json!({
                "deployment_id": created.deployment_id,
                "status": status,
                "environment": created.environment,
                "cleared_rollback": created.cleared_rollback,
                "pushed_station_ids": created.pushed_station_ids,
                "url": url,
            })
        );
    }

    if status == "ready" {
        if !json_mode {
            if created.pushed_station_ids.is_empty() {
                log::success(&format!("Preview deployment ready — {url}"));
            } else {
                log::success(&format!(
                    "Deployed to {} station{} — {url}",
                    created.pushed_station_ids.len(),
                    if created.pushed_station_ids.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                ));
            }
        }
        Ok(0)
    } else {
        if !json_mode {
            log::error(&format!("Build {status} — {url}"));
        }
        Ok(1)
    }
}

/// Poll the logs endpoint, printing new lines, until status is terminal.
/// Returns the terminal status string ("ready" | "failed").
async fn stream_build(
    creds: &Credentials,
    base: &str,
    deployment_id: &str,
    json_mode: bool,
) -> CliResult<String> {
    let mut after_seq: i64 = 0;
    loop {
        let url = format!("{base}/api/cli/deployments/{deployment_id}/logs?after_seq={after_seq}");
        let resp = client()
            .get(&url)
            .bearer(&creds.api_key)
            .send()
            .await
            .map_err(|e| CliError::msg(format!("log poll failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(CliError::msg(format!("log poll failed ({status})")));
        }
        let logs: LogsResponse = resp
            .json()
            .await
            .map_err(|e| CliError::msg(format!("log poll parse failed: {e}")))?;

        for entry in &logs.entries {
            after_seq = after_seq.max(entry.seq);
            if !json_mode {
                match entry.level.as_str() {
                    "ERROR" | "CRITICAL" => log::error(&entry.text),
                    "WARNING" => log::warn(&entry.text),
                    _ => println!("  {}", entry.text),
                }
            }
        }

        match logs.status.as_str() {
            "ready" | "failed" => return Ok(logs.status),
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

struct Packed {
    tar: tempfile::NamedTempFile,
    sha256: String,
    size_bytes: u64,
    file_count: usize,
}

/// Pack the procedure directory into a tar.zst, honoring .gitignore (even
/// outside a git repo) and always excluding VCS/venv noise. The archive
/// root is the package dir itself — the cloud build runs at its root.
fn pack(dir: &Path) -> CliResult<Packed> {
    let tmp = tempfile::Builder::new()
        .prefix("tofupilot-deploy-")
        .suffix(".tar.zst")
        .tempfile()
        .map_err(CliError::from)?;

    let encoder =
        zstd::Encoder::new(tmp.reopen().map_err(CliError::from)?, 9).map_err(CliError::from)?;
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);

    let mut file_count = 0usize;
    let walk = ignore::WalkBuilder::new(dir)
        .hidden(false) // ship dotfiles (.python-version etc.); .gitignore still applies
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false) // honor .gitignore files even outside a repo
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !(entry.file_type().is_some_and(|t| t.is_dir())
                && ALWAYS_EXCLUDED_DIRS.contains(&name.as_ref()))
        })
        .build();

    for entry in walk {
        let entry = entry.map_err(|e| CliError::msg(format!("walk failed: {e}")))?;
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = path
            .strip_prefix(dir)
            .map_err(|e| CliError::msg(format!("strip prefix: {e}")))?;
        builder
            .append_path_with_name(path, rel)
            .map_err(|e| CliError::msg(format!("pack {}: {e}", rel.display())))?;
        file_count += 1;
    }
    if file_count == 0 {
        return Err(CliError::msg(
            "nothing to pack — directory is empty after excludes",
        ));
    }

    let encoder = builder.into_inner().map_err(CliError::from)?;
    encoder.finish().map_err(CliError::from)?;

    // Hash + size of the finished archive.
    let mut file = tmp.reopen().map_err(CliError::from)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size_bytes = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(CliError::from)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size_bytes += n as u64;
    }

    Ok(Packed {
        tar: tmp,
        sha256: hex::encode(hasher.finalize()),
        size_bytes,
        file_count,
    })
}

async fn api_post<T: serde::de::DeserializeOwned>(
    creds: &Credentials,
    url: &str,
    body: &serde_json::Value,
) -> CliResult<T> {
    let resp = client()
        .post(url)
        .bearer(&creds.api_key)
        .json(body)
        .send()
        .await
        .map_err(|e| CliError::msg(format!("request failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        // Surface the server's `error` field when present — it carries
        // actionable messages ("run tofupilot login", size mismatch, …).
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| text.chars().take(300).collect());
        return Err(CliError::msg(format!("{msg} ({status})")));
    }
    resp.json()
        .await
        .map_err(|e| CliError::msg(format!("response parse failed: {e}")))
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
