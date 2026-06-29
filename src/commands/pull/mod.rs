//! The `pull` command: sync deployments from the dashboard.
//!
//! Fetches procedures and deployments, opens a short-lived Centrifugo stream
//! for deployment events, and extracts the bundles into
//! `~/.tofupilot/deployments/` (see [`sync`]).

pub(crate) mod sync;

use crate::commands::auth::credentials;
use crate::commands::db;
use crate::commands::station;
use crate::http::RequestBuilderExt;
use station_protocol::StationEvent;
use sync::PullInfo;

#[derive(serde::Deserialize)]
struct PullResponse {
    station: PullStation,
    procedures: Vec<PullProcedure>,
}

#[derive(serde::Deserialize)]
struct PullStation {
    name: String,
}

#[derive(serde::Deserialize)]
struct PullProcedure {
    id: String,
    name: String,
    deployment: Option<PullDeployment>,
}

#[derive(serde::Deserialize)]
struct PullDeployment {
    /// station_deployment row id. Stamped on every event we publish for
    /// this deployment so the server can match started/completed/failed
    /// rows to the right deployment without sha-based reconciliation.
    id: String,
    sha: String,
    message: Option<String>,
}

/// Standalone entry: opens its own short-lived `StreamClient` to publish
/// deployment events, then disconnects. Use this from `tofupilot pull` and
/// the post-login finalize path where there is no surrounding station-mode
/// connection.
///
/// Do NOT call this from station mode — it would open a *second* WebSocket
/// against the same station identity, and Centrifugo's per-user subscription
/// state ends up bound to whichever connection lived last. When this
/// short-lived client disconnects, the station-mode WS is left without
/// server-side subs, and subsequent `Pull` commands silently miss. Station
/// mode must call `run_with(json_mode, Some(&publisher))` instead, threading
/// in a borrowed `PublishHandle` cloned off the long-lived StreamClient.
pub async fn run_cmd(json_mode: bool) -> i32 {
    // `pull` hits a station-only endpoint, so resolve the station identity
    // first — a stale user `credentials.json` must not shadow it (see
    // `credentials::load_station_first`).
    let creds = match credentials::require_station_first() {
        Ok(c) => c,
        Err(e) => {
            crate::log::error(&e.to_string());
            return 1;
        }
    };

    // Connect to stream for publishing deployment events (best-effort, 5s timeout)
    // Only if we have an installation_id (station-scoped login).
    let inst_id = creds.installation_id.clone().unwrap_or_default();
    let stream = if !inst_id.is_empty() {
        tokio::time::timeout(
            crate::config::timeouts::PULL_STREAM_CONNECT,
            station::client::StreamClient::connect(&creds),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten()
    } else {
        None
    };
    let publisher = stream.as_ref().map(|s| s.clone_for_health());

    let code = run_with(json_mode, publisher.as_ref(), None).await;

    // Disconnect the short-lived stream we just opened. The PublishHandle
    // we borrowed shares the same underlying client; dropping it without
    // calling disconnect on the StreamClient would leak the connection.
    if let Some(stream) = stream {
        stream.disconnect().await;
    }
    code
}

/// Body of the pull flow that does NOT manage its own stream connection.
/// Station mode passes `Some(&publisher)` cloned off its long-lived
/// `StreamClient`; standalone callers (`tofupilot pull`, post-login finalize)
/// go through `run`, which opens / disconnects a short-lived client around
/// this body.
pub async fn run_with(
    json_mode: bool,
    publisher: Option<&station::client::PublishHandle>,
    // Optional loopback bridge: when station mode owns a local-WS
    // server, every event we publish to Centrifugo also lands here so
    // a Vite kiosk SPA on the same machine sees deployment lifecycle
    // events without waiting for a fresh `hello` frame on reconnect.
    local_ws: Option<&std::sync::Arc<crate::local_ws::Server>>,
) -> i32 {
    // Station-only endpoint: prefer the station identity. See
    // `credentials::load_station_first`.
    let creds = match credentials::require_station_first() {
        Ok(c) => c,
        Err(e) => {
            crate::log::error(&e.to_string());
            return 1;
        }
    };

    // Self-heal visibility: if a stale user login is also on disk, it would
    // have shadowed the station key before the station-first switch. Tell the
    // operator we ignored it so a lingering `credentials.json` doesn't look
    // like the cause of a future problem.
    if creds.installation_id.is_some()
        && credentials::load().is_some_and(|u| u.installation_id.is_none())
    {
        crate::log::info(
            "Using station identity for pull; a stale user login in credentials.json was ignored.",
        );
    }

    let base = creds.base();

    // Fetch procedures with deployments
    let res = match crate::http::client()
        .get(format!("{base}/api/cli/pull"))
        .bearer(&creds.api_key)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            crate::log::error(&format!("Failed to connect: {e}"));
            return 1;
        }
    };

    let res = match crate::commands::http::ok_or_describe(res).await {
        Ok(r) => r,
        Err(e) => {
            crate::log::error(&format!("Failed to get deployments: {}", e.body()));
            if matches!(&e, crate::error::CliError::Status { status, .. } if *status == 403) {
                crate::log::info(
                    "Make sure you logged in as a station: generate a setup token from the station's page in the dashboard, then run `tofupilot login --token <setup-token>`.",
                );
            }
            return 1;
        }
    };

    let pull_data: PullResponse = match res.json().await {
        Ok(d) => d,
        Err(e) => {
            crate::log::error(&format!("Failed to parse response: {e}"));
            return 1;
        }
    };

    let db = match db::open() {
        Ok(db) => db,
        Err(e) => {
            crate::log::error(&format!("Failed to open database: {e}"));
            return 1;
        }
    };

    if !json_mode {
        crate::log::info(&format!(
            "Pulling deployments for {}...",
            pull_data.station.name
        ));
    }

    // The publisher is borrowed; do not open a second `StreamClient` here.
    // See `run` doc comment for the bug this avoids.
    let stream = publisher;
    let inst_id = creds.installation_id.clone().unwrap_or_default();

    let mut pulled = 0u32;
    let mut failed = 0u32;

    for proc in &pull_data.procedures {
        let deployment = match &proc.deployment {
            Some(d) => d,
            None => {
                // Procedure is linked to this station but the server returned
                // no deployment for it. Two ways to get here:
                //   * No commit has been deployed yet on the procedure.
                //   * The active deployment's `platform` filter excluded this
                //     station (e.g. station_installation.platform isn't synced
                //     yet — race between the Hardware event publish and the
                //     pull request). Surface it instead of silently skipping
                //     so operators can tell "nothing to install" from "the
                //     deployment got filtered out and the picker will fail".
                if !json_mode {
                    crate::log::warn(&format!(
                        "{} -- no matching deployment (skipped)",
                        proc.name
                    ));
                } else {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "no_deployment",
                            "procedure_id": proc.id,
                            "procedure_name": proc.name,
                        })
                    );
                }
                continue;
            }
        };

        let sha = &deployment.sha;

        // Track whether this is a brand-new deployment on the
        // station (no prior `pull_state` row). Drives the
        // `DeploymentAdded` event after the pull succeeds, so a
        // live-connected operator-ui kiosk can update its picker
        // without a reload. SHA-bumps on an already-known procedure
        // are NOT new deployments — the procedure was already in
        // the local list.
        let is_new_deployment = matches!(db.get_pull_state(&proc.id), Ok(None));

        // Check if already pulled to this SHA
        match db.get_pull_state(&proc.id) {
            Ok(Some(ref state)) if state.sha == *sha => {
                if !json_mode {
                    crate::log::success(&format!(
                        "{} -- up to date ({})",
                        proc.name,
                        &sha[..7.min(sha.len())]
                    ));
                } else {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "up_to_date",
                            "procedure_id": proc.id,
                            "procedure_name": proc.name,
                            "deployment_id": deployment.id,
                            "sha": sha,
                        })
                    );
                }
                continue;
            }
            Err(e) => {
                failed += 1;
                if !json_mode {
                    crate::log::error(&format!("{} -- failed to read pull state: {e}", proc.name));
                }
                continue;
            }
            _ => {}
        }

        let message = deployment.message.as_deref().unwrap_or("");

        if !json_mode {
            crate::log::info(&format!(
                "{} -- pulling {} ({})",
                proc.name,
                &sha[..7.min(sha.len())],
                message
            ));
        } else {
            println!(
                "{}",
                serde_json::json!({
                    "type": "pulling",
                    "procedure_id": proc.id,
                    "procedure_name": proc.name,
                    "deployment_id": deployment.id,
                    "sha": sha,
                    "message": message,
                })
            );
        }

        // The per-procedure endpoint now returns a JSON artifact descriptor
        // rather than streaming a tarball, so sync::pull_deployment handles
        // fetching + verification + install from the artifact URL itself.
        // The descriptor also carries the authoritative SHA, so we only pass
        // the human-readable name + station_deployment id here. The id flows
        // into PullState so DeploymentRemoved events can reference it later.
        let info = PullInfo {
            name: proc.name.clone(),
            deployment_id: deployment.id.clone(),
        };

        {
            let event = StationEvent::DeploymentPullStarted {
                installation_id: inst_id.clone(),
                procedure_id: proc.id.clone(),
                deployment_id: deployment.id.clone(),
            };
            if let Some(stream) = stream {
                let _ = stream.publish(&event).await;
            }
            if let Some(server) = local_ws {
                server.publish_event(event).await;
            }
        }

        match sync::pull_deployment(&creds, &db, &proc.id, &info).await {
            Ok(result) => {
                pulled += 1;
                {
                    let completed = StationEvent::DeploymentPullCompleted {
                        installation_id: inst_id.clone(),
                        procedure_id: proc.id.clone(),
                        deployment_id: deployment.id.clone(),
                        file_count: result.file_count,
                    };
                    if let Some(stream) = stream {
                        let _ = stream.publish(&completed).await;
                    }
                    if let Some(server) = local_ws {
                        server.publish_event(completed).await;
                    }
                    if is_new_deployment {
                        let added = StationEvent::DeploymentAdded {
                            installation_id: inst_id.clone(),
                            procedure_id: proc.id.clone(),
                            procedure_name: proc.name.clone(),
                            deployment_id: deployment.id.clone(),
                        };
                        if let Some(stream) = stream {
                            let _ = stream.publish(&added).await;
                        }
                        if let Some(server) = local_ws {
                            server.publish_event(added).await;
                        }
                    }
                }
                if !json_mode {
                    crate::log::success(&format!(
                        "{} -- pulled ({} files)",
                        proc.name, result.file_count
                    ));
                } else {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "pulled",
                            "procedure_id": proc.id,
                            "deployment_id": deployment.id,
                            "sha": sha,
                            "path": result.path.display().to_string(),
                        })
                    );
                }
            }
            Err(e) => {
                failed += 1;
                {
                    let event = StationEvent::DeploymentPullFailed {
                        installation_id: inst_id.clone(),
                        procedure_id: proc.id.clone(),
                        deployment_id: deployment.id.clone(),
                        error: e.to_string(),
                    };
                    if let Some(stream) = stream {
                        let _ = stream.publish(&event).await;
                    }
                    if let Some(server) = local_ws {
                        server.publish_event(event).await;
                    }
                }
                if !json_mode {
                    crate::log::error(&format!("{} -- failed: {e}", proc.name));
                } else {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "procedure_id": proc.id,
                            "deployment_id": deployment.id,
                            "error": e.to_string(),
                        })
                    );
                }
            }
        }
    }

    // Remove stale deployments: keep only procedures that both belong to
    // this station AND still have a deployment. Procedures unlinked from
    // the station, or procedures whose deployment was deleted/unpublished,
    // drop out of this set and their local copy is removed.
    let active_ids: std::collections::HashSet<&str> = pull_data
        .procedures
        .iter()
        .filter(|p| p.deployment.is_some())
        .map(|p| p.id.as_str())
        .collect();

    let deployments_dir = match db::deployments_dir() {
        Ok(d) => d,
        Err(_) => std::path::PathBuf::new(),
    };

    if deployments_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&deployments_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = match entry.file_name().into_string() {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                if name.ends_with(".tmp") || name.ends_with(".old") {
                    let _ = std::fs::remove_dir_all(&path);
                    continue;
                }
                if !active_ids.contains(name.as_str()) {
                    // Snapshot pull_state before deleting it — we need both
                    // the human-readable name (for log output) and the
                    // station_deployment id (for the published event) and
                    // the row is gone after `remove_pull_state` runs.
                    // Legacy rows that predate the deployment_id field
                    // deserialize as None (see db::get_pull_state) — for
                    // those we still clean the on-disk dir but skip the
                    // server publish since we have no deployment to
                    // reference.
                    let prior = db.get_pull_state(&name).ok().flatten();
                    let display_name = prior
                        .as_ref()
                        .and_then(|s| s.name.clone())
                        .unwrap_or_else(|| name[..8.min(name.len())].to_string());
                    let prior_deployment_id = prior.as_ref().map(|s| s.deployment_id.clone());
                    let _ = std::fs::remove_dir_all(&path);
                    let _ = db.remove_pull_state(&name);
                    if let Some(dep_id) = prior_deployment_id.as_ref() {
                        let event = StationEvent::DeploymentRemoved {
                            installation_id: inst_id.clone(),
                            procedure_id: name.clone(),
                            deployment_id: dep_id.clone(),
                        };
                        if let Some(stream) = stream {
                            let _ = stream.publish(&event).await;
                        }
                        if let Some(server) = local_ws {
                            server.publish_event(event).await;
                        }
                    }
                    if !json_mode {
                        crate::log::info(&format!("{display_name} -- removed (no longer linked)"));
                    } else {
                        println!(
                            "{}",
                            serde_json::json!({
                                "type": "removed",
                                "procedure_id": name,
                                "deployment_id": prior_deployment_id,
                            })
                        );
                    }
                }
            }
        }
    }

    // No disconnect: `stream` is a borrowed `PublishHandle`. The owning
    // `StreamClient` lives outside this function and outlives the call.

    if !json_mode {
        eprintln!();
        if pulled == 0 && failed == 0 {
            crate::log::info("No deployments to pull.");
        } else if failed == 0 {
            crate::log::success(&format!("Done. {pulled} pulled."));
        } else {
            crate::log::warn(&format!("Done. {pulled} pulled, {failed} failed."));
        }
    }

    if failed > 0 {
        1
    } else {
        0
    }
}
