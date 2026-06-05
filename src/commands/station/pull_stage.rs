//! Pull-and-stage orchestration for the station daemon: fetch a deployment and
//! extract it before a run.

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::commands::auth::credentials::Credentials;
use crate::commands::db;
use crate::commands::pull::sync::{stage_deployment, PullInfo, StagedDeployment};
use crate::http::RequestBuilderExt;

/// Download new deployments to staging dirs (background, no swap).
/// Called when a Pull command arrives while a test is running.
/// The staged deployment is swapped in between test cycles.
///
/// Logs every branch so journalctl tells the operator why a Pull
/// click "did nothing" (HTTP failure, parse error, all-up-to-date,
/// stage failure). Mirrors `update::background_check`'s visibility.
pub async fn stage_pull_to(creds: &Credentials, staged: &Arc<Mutex<Option<StagedDeployment>>>) {
    crate::log::info("Staging deployments in background (run in flight)...");
    let base = creds.base();
    let res = match crate::http::client()
        .get(format!("{base}/api/cli/pull"))
        .bearer(&creds.api_key)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            crate::log::warn(&format!("Pull staging: server returned {}", r.status(),));
            return;
        }
        Err(e) => {
            crate::log::warn(&format!("Pull staging: connection failed: {e}"));
            return;
        }
    };

    #[derive(serde::Deserialize)]
    struct Resp {
        procedures: Vec<Proc>,
    }
    #[derive(serde::Deserialize)]
    struct Proc {
        id: String,
        name: String,
        deployment: Option<Deploy>,
    }
    #[derive(serde::Deserialize)]
    struct Deploy {
        /// station_deployment row id, plumbed into PullState so removal
        /// events can stamp the right deployment_id without a re-fetch.
        id: String,
        sha: String,
    }

    let data: Resp = match res.json().await {
        Ok(d) => d,
        Err(e) => {
            crate::log::warn(&format!("Pull staging: parse failed: {e}"));
            return;
        }
    };

    let db = match db::open() {
        Ok(db) => db,
        Err(e) => {
            crate::log::warn(&format!("Pull staging: db open failed: {e}"));
            return;
        }
    };

    let mut staged_any = false;
    for proc in &data.procedures {
        let deployment = match &proc.deployment {
            Some(d) => d,
            None => continue,
        };

        // Skip if already at this SHA
        if let Ok(Some(ref current)) = db.get_pull_state(&proc.id) {
            if current.sha == deployment.sha {
                continue;
            }
        }

        let info = PullInfo {
            name: proc.name.clone(),
            deployment_id: deployment.id.clone(),
        };

        match stage_deployment(creds, &proc.id, &info).await {
            Ok(s) => {
                crate::log::info(&format!(
                    "Staged: {} ({})",
                    proc.name,
                    &deployment.sha[..7.min(deployment.sha.len())]
                ));
                *staged.lock().await = Some(s);
                staged_any = true;
            }
            Err(e) => {
                crate::log::warn(&format!("Stage failed for {}: {e}", proc.name));
            }
        }
    }

    if !staged_any {
        crate::log::info("Deployments up to date.");
    }
}
