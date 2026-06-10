//! Offline upload queue for runs that fail to reach the server.
//!
//! When an upload fails, the run and its attachments are persisted to the
//! local redb store and retried on a backoff (or parked for deterministic
//! 4xx-style rejections that won't fix themselves). The `queue` subcommand
//! lists, inspects, retries, removes, and exports entries.

use chrono::Utc;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use tofupilot_sdk::types::*;
use tokio::sync::broadcast;

use crate::commands::auth::credentials::Credentials;
use crate::commands::db;
use crate::display::{self, Column};
use crate::http::RequestBuilderExt;
use station_protocol::StationEvent;

/// Process-wide claim set for in-flight uploads. Both the continuous
/// `run_drain_loop` and operator-triggered `retry_one` race for the
/// same queue entries; without serialization they'd both call
/// `runs().create()` on the same row, minting a duplicate cloud run.
/// A claim is held for the duration of `upload_queued_run`. Drop is
/// the only way out — the guard handles panic-safety automatically.
fn upload_claims() -> &'static Mutex<HashSet<String>> {
    static CLAIMS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CLAIMS.get_or_init(|| Mutex::new(HashSet::new()))
}

struct UploadClaim(String);

impl UploadClaim {
    /// Returns `Some` if the queue id is now claimed by this caller,
    /// `None` if another task already holds the claim.
    fn try_acquire(queue_id: &str) -> Option<Self> {
        let mut set = upload_claims().lock().ok()?;
        if set.contains(queue_id) {
            return None;
        }
        set.insert(queue_id.to_string());
        Some(UploadClaim(queue_id.to_string()))
    }
}

impl Drop for UploadClaim {
    fn drop(&mut self) {
        if let Ok(mut set) = upload_claims().lock() {
            set.remove(&self.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueuedAttachment {
    pub name: String,
    pub path: String,
    pub mimetype: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UploadFailureRecord {
    /// `http_4xx` / `http_5xx` / `network` / `unknown`. Mirrors the
    /// wire kind on `StationEvent::RunUploadFailed`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub error: String,
    /// ISO-8601. Set when the operator opened the panel and we want
    /// to render "5m ago".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueuedRun {
    pub request: RunCreateRequest,
    pub attachments: Vec<QueuedAttachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Number of upload attempts so far. Survives CLI restarts so
    /// backoff state is consistent. Old DB entries default to 0.
    #[serde(default)]
    pub attempt_count: u32,
    /// ISO-8601 wall-clock of the last attempt. Used to render
    /// "Last attempt: Xm ago" in the operator UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_at: Option<String>,
    /// ISO-8601 wall-clock when the next retry is allowed. None means
    /// "retry immediately on next drain". Set when `kind = http_5xx`
    /// or `network`. For `http_4xx` we explicitly set this to None
    /// AND set `parked` so the drain loop skips us.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<String>,
    /// True when the entry is parked (4xx error or operator-only
    /// retry). The drain loop skips parked entries until either
    /// `QueueRetry` arrives or `parked` is cleared.
    #[serde(default)]
    pub parked: bool,
    /// Last failure observed. Surfaces in the operator UI's
    /// expanded-row state and lets us re-emit the failure event on
    /// hydrate without losing the body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<UploadFailureRecord>,
    /// ISO-8601 wall-clock when the entry was first created. Lets the
    /// UI render "Queued 12m ago" for entries that pre-date the
    /// session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<String>,
}

/// Backoff schedule for transient failures. Capped at 1h; further
/// retries reuse the cap. The CLI doesn't drop entries on its own —
/// only the operator clicking Drop or `tofupilot queue rm` removes
/// them.
///
/// First-attempt backoff is deliberately > drain tick (5s) so a
/// transient 5xx doesn't immediately re-fire on the next tick before
/// the server has a chance to recover.
fn backoff_seconds(attempt: u32) -> i64 {
    match attempt {
        0 | 1 => 15,
        2 => 30,
        3 => 60 * 5,
        4 => 60 * 15,
        _ => 60 * 60,
    }
}

/// Truncate large response bodies so a verbose 422 doesn't fill the
/// UI / DB. 4 KiB is plenty for Zod error details and keeps the wire
/// payload reasonable.
const MAX_ERROR_BODY: usize = 4096;

fn truncate_error(s: String) -> String {
    if s.len() <= MAX_ERROR_BODY {
        s
    } else {
        let mut t = s[..MAX_ERROR_BODY].to_string();
        t.push_str("\n…(truncated)");
        t
    }
}

fn dashboard_url_for(base_url: &str, org_slug: &str, procedure_id: &str, run_id: &str) -> String {
    // base_url comes from `Credentials::base_url` which the CLI
    // normalises to e.g. `https://www.tofupilot.app`. The run page
    // lives at `/{org}/{procedure_id}/runs/{run_id}` (route file
    // `apps/web/app/[organization]/(dashboard)/[id]/runs/[run_id]/page.tsx`).
    // The page fetches by run_id + org; the procedure segment is just a
    // path slug, so any valid procedure_id works.
    format!(
        "{}/{}/{}/runs/{}",
        base_url.trim_end_matches('/'),
        org_slug,
        procedure_id,
        run_id
    )
}

/// Mint a queue id for a new run. Format `<procedure_id>_<unix_millis>`
/// matches the on-disk queue layout — both the YAML engine and the
/// OpenHTF connector mint these the same way, hence the helper instead
/// of two byte-identical inline expressions.
pub fn new_queue_id(procedure_id: &str) -> String {
    format!("{procedure_id}_{}", chrono::Utc::now().timestamp_millis())
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

/// Classification of an upload failure that determines retry policy.
#[derive(Debug, Clone)]
struct ClassifiedFailure {
    kind: &'static str,
    status: Option<u16>,
    error: String,
    /// 4xx errors don't auto-retry. The drain loop skips them; only
    /// `QueueRetry` (operator click / CLI command) un-parks the
    /// entry. 5xx / network do retry on a backoff.
    park: bool,
}

fn classify_sdk_error(err: &tofupilot_sdk::error::Error) -> ClassifiedFailure {
    use tofupilot_sdk::error::Error as E;
    match err {
        E::BadRequest(api) => mk_4xx(400, api.to_string()),
        E::Unauthorized(api) => mk_4xx(401, api.to_string()),
        E::Forbidden(api) => mk_4xx(403, api.to_string()),
        E::NotFound(api) => mk_4xx(404, api.to_string()),
        E::Conflict(api) => mk_4xx(409, api.to_string()),
        E::UnprocessableContent(api) => mk_4xx(422, api.to_string()),
        E::RateLimited(api) => mk_5xx(429, api.to_string()),
        E::InternalServerError(api) => mk_5xx(500, api.to_string()),
        E::BadGateway(api) => mk_5xx(502, api.to_string()),
        E::UnexpectedStatus { status, body } => {
            if (400..500).contains(status) {
                mk_4xx(*status, body.clone())
            } else if (500..600).contains(status) {
                mk_5xx(*status, body.clone())
            } else {
                // 1xx / 2xx-non-success / 3xx — the SDK didn't
                // recognise it as a known shape, retry won't help.
                // Park so the operator inspects.
                ClassifiedFailure {
                    kind: "unknown",
                    status: Some(*status),
                    error: truncate_error(body.clone()),
                    park: true,
                }
            }
        }
        E::Http(e) => ClassifiedFailure {
            kind: "network",
            status: None,
            error: truncate_error(format!("HTTP transport: {e}")),
            park: false,
        },
        E::Json(e) => ClassifiedFailure {
            // Server returned non-JSON or schema-incompatible JSON.
            // Retry can't fix a malformed response — park.
            kind: "unknown",
            status: None,
            error: truncate_error(format!("JSON: {e}")),
            park: true,
        },
        E::Io(e) => ClassifiedFailure {
            // Local I/O during upload (file handle, transient disk).
            // Often recoverable — file system hiccup, ephemeral
            // permission issue. Treat as transient.
            kind: "network",
            status: None,
            error: truncate_error(format!("I/O: {e}")),
            park: false,
        },
        E::Validation(msg) => mk_4xx(0, msg.clone()),
    }
}

fn mk_4xx(status: u16, msg: String) -> ClassifiedFailure {
    ClassifiedFailure {
        kind: "http_4xx",
        status: if status == 0 { None } else { Some(status) },
        error: truncate_error(msg),
        park: true,
    }
}

fn mk_5xx(status: u16, msg: String) -> ClassifiedFailure {
    ClassifiedFailure {
        kind: "http_5xx",
        status: Some(status),
        error: truncate_error(msg),
        park: false,
    }
}

fn classify_attachment_error(msg: String) -> ClassifiedFailure {
    // The attachment helper returns plain strings. We don't have a
    // status code so we can't distinguish 4xx from 5xx; classify by
    // matching keywords. Deterministic-error keywords (denied,
    // signature, expired) park even if a transient keyword also
    // appears — those rejections won't fix themselves on retry.
    let lower = msg.to_lowercase();
    let deterministic = lower.contains("denied")
        || lower.contains("forbidden")
        || lower.contains("expired")
        || lower.contains("signature")
        || lower.contains("invalid")
        || lower.contains("not found");
    let probably_transient = !deterministic
        && (lower.contains("connection")
            || lower.contains("timeout")
            || lower.contains("dns")
            || lower.contains("reset")
            || lower.contains("temporarily"));
    ClassifiedFailure {
        kind: if probably_transient {
            "network"
        } else {
            "unknown"
        },
        status: None,
        error: truncate_error(msg),
        park: !probably_transient,
    }
}

/// Bus for emitting upload-related events. Cloned cheaply; CLI hosts
/// pass the same bus they use for the run-event broadcast so consumer
/// UIs see upload events on the same wire.
pub type EventBus = broadcast::Sender<StationEvent>;

/// Upload a queued run. Emits `RunUploadStarted` / `Succeeded` /
/// `Failed` / `RunUploaded` to `bus` so operator UIs can render
/// progress. `silent` suppresses stderr — used by the background
/// drain loop where stderr would interleave with the live run.
///
/// Returns the server-issued run id on full success (run created and
/// every attachment uploaded; the entry is dequeued). Returns `None`
/// on any failure or when another task holds the upload claim.
/// Opens the state DB only for the brief entry-state mutations, never
/// across the awaited network upload — holding the redb lock through a
/// slow upload would starve every other tofupilot process on the host.
pub async fn upload_queued_run(
    http: &reqwest::Client,
    creds: &Credentials,
    queue_id: &str,
    queued: &QueuedRun,
    bus: Option<&EventBus>,
    silent: bool,
) -> Option<String> {
    // Claim the entry so concurrent callers (drain loop tick + an
    // operator-triggered retry) can't both upload the same run. The
    // guard is dropped at function exit (or panic), releasing the
    // claim. Loser silently no-ops; the holder will publish an
    // updated state event the loser can pick up.
    let _claim = match UploadClaim::try_acquire(queue_id) {
        Some(c) => c,
        None => return None,
    };
    let attempt = queued.attempt_count.saturating_add(1);
    if let Some(bus) = bus {
        let _ = bus.send(StationEvent::RunUploadStarted {
            queue_id: queue_id.to_string(),
            attempt,
        });
    }

    let sdk = tofupilot_sdk::TofuPilot::with_config(
        tofupilot_sdk::config::ClientConfig::new(&creds.api_key).base_url(&creds.base_url),
    );

    // `latest_state` is the source of truth across the run-create and
    // attachment-upload phases. Once `runs().create()` succeeds we
    // persist the server-issued run_id; if a later attachment fails,
    // we record the failure against this updated state so the next
    // retry skips the create call (otherwise we'd mint a duplicate
    // run on the API and the old run would dangle).
    let mut latest_state: QueuedRun = queued.clone();
    let run_id = if let Some(ref id) = latest_state.run_id {
        id.clone()
    } else {
        match sdk
            .runs()
            .create()
            .body(queued.request.clone())
            .send()
            .await
        {
            Ok(res) => {
                latest_state.run_id = Some(res.id.clone());
                latest_state.attempt_count = attempt;
                latest_state.last_attempt_at = Some(Utc::now().to_rfc3339());
                latest_state.last_error = None;
                latest_state.next_retry_at = None;
                latest_state.parked = false;
                if let Ok(db) = db::open() {
                    let _ = db.enqueue_run(queue_id, &latest_state);
                }
                res.id
            }
            Err(e) => {
                if !silent {
                    eprintln!("  Upload failed (queued for retry): {e}");
                }
                let cls = classify_sdk_error(&e);
                record_failure(queue_id, &latest_state, attempt, &cls, bus);
                return None;
            }
        }
    };

    let base = creds.base();
    let mut all_ok = true;
    let mut last_failure: Option<ClassifiedFailure> = None;
    for att in &queued.attachments {
        if !std::path::Path::new(&att.path).exists() {
            continue;
        }
        match upload_attachment(http, base, &creds.api_key, &run_id, att).await {
            Ok(()) => {
                let _ = std::fs::remove_file(&att.path);
            }
            Err(e) => {
                if !silent {
                    eprintln!("  Attachment failed ({}): {e}", att.name);
                }
                all_ok = false;
                last_failure = Some(classify_attachment_error(e.to_string()));
            }
        }
    }

    if all_ok {
        if !silent {
            eprintln!("  Uploaded: {run_id}");
        }
        if let Ok(db) = db::open() {
            let _ = db.dequeue_run(queue_id);
        }
        cleanup_attachments(&queued.attachments);
        if let Some(bus) = bus {
            let dashboard_url = Some(dashboard_url_for(
                &creds.base_url,
                &creds.organization_slug,
                &queued.request.procedure_id,
                &run_id,
            ));
            let _ = bus.send(StationEvent::RunUploadSucceeded {
                queue_id: queue_id.to_string(),
                run_id: run_id.clone(),
                dashboard_url: dashboard_url.clone(),
            });
            // Mirror to the back-compat `RunUploaded` event so existing
            // consumers keep working.
            let _ = bus.send(StationEvent::RunUploaded {
                procedure_id: queued.request.procedure_id.clone(),
                run_id: run_id.clone(),
                dashboard_url,
            });
        }
        Some(run_id)
    } else {
        if !silent {
            eprintln!("  Some attachments failed (queued for retry)");
        }
        if let Some(cls) = last_failure {
            // Pass `latest_state` so the persisted failure carries
            // the server-issued run_id; otherwise the next retry
            // would skip the run-create check and re-POST.
            record_failure(queue_id, &latest_state, attempt, &cls, bus);
        }
        None
    }
}

/// Persist a failure on the queue entry and emit the wire event.
fn record_failure(
    queue_id: &str,
    queued: &QueuedRun,
    attempt: u32,
    cls: &ClassifiedFailure,
    bus: Option<&EventBus>,
) {
    let now = Utc::now();
    let next_retry_at = if cls.park {
        None
    } else {
        Some(now + chrono::Duration::seconds(backoff_seconds(attempt)))
    };
    let mut updated = queued.clone();
    updated.attempt_count = attempt;
    updated.last_attempt_at = Some(now.to_rfc3339());
    updated.next_retry_at = next_retry_at.map(|t| t.to_rfc3339());
    updated.parked = cls.park;
    updated.last_error = Some(UploadFailureRecord {
        kind: cls.kind.to_string(),
        status: cls.status,
        error: cls.error.clone(),
        failed_at: Some(now.to_rfc3339()),
    });
    if let Ok(db) = db::open() {
        let _ = db.enqueue_run(queue_id, &updated);
    }

    if let Some(bus) = bus {
        let _ = bus.send(StationEvent::RunUploadFailed {
            queue_id: queue_id.to_string(),
            attempt,
            kind: cls.kind.to_string(),
            status: cls.status,
            error: cls.error.clone(),
            next_retry_at: next_retry_at.map(|t| t.to_rfc3339()),
        });
    }
}

fn cleanup_attachments(attachments: &[QueuedAttachment]) {
    if let Some(att) = attachments.first() {
        if let Some(dir) = std::path::Path::new(&att.path).parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

async fn upload_attachment(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    run_id: &str,
    att: &QueuedAttachment,
) -> crate::error::CliResult<()> {
    let res = http
        .post(format!("{base_url}/api/v2/runs/{run_id}/attachments"))
        .bearer(api_key)
        .json(&serde_json::json!({"name": att.name}))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let res = crate::commands::http::ok_or_describe(res)
        .await
        .map_err(|e| format!("attachment init: {}", e.body()))?;

    let url = res
        .json::<serde_json::Value>()
        .await
        .map_err(|e| e.to_string())?
        .get("upload_url")
        .and_then(|v| v.as_str())
        .ok_or("no upload_url")?
        .to_string();

    let data = std::fs::read(&att.path).map_err(|e| format!("read: {e}"))?;
    let put = http
        .put(&url)
        .header("Content-Type", &att.mimetype)
        .body(data)
        .send()
        .await
        .map_err(|e| format!("put: {e}"))?;
    crate::commands::http::ok_or_describe(put)
        .await
        .map_err(|e| format!("attachment upload: {}", e.body()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Enqueue
// ---------------------------------------------------------------------------

/// Persist a fresh `QueuedRun` and emit `RunUploadQueued`. This is
/// the canonical entrypoint — call it from every place that produces
/// a run (`run/mod.rs::spawn_upload`, OpenHTF connector, …) so the
/// emit + persist contract isn't subtly different per call site.
pub fn enqueue(
    db: &db::StateDb,
    queue_id: &str,
    queued: &mut QueuedRun,
    bus: Option<&EventBus>,
) -> crate::error::CliResult<()> {
    if queued.queued_at.is_none() {
        queued.queued_at = Some(Utc::now().to_rfc3339());
    }
    db.enqueue_run(queue_id, &*queued)?;
    if let Some(bus) = bus {
        let _ = bus.send(StationEvent::RunUploadQueued {
            queue_id: queue_id.to_string(),
            procedure_id: queued.request.procedure_id.clone(),
            outcome: queued.request.outcome.to_string(),
            serial_number: Some(queued.request.serial_number.clone()),
            attachment_count: queued.attachments.len() as u32,
            queued_at: queued
                .queued_at
                .clone()
                .unwrap_or_else(|| Utc::now().to_rfc3339()),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Drain (background-safe)
// ---------------------------------------------------------------------------

fn is_due(queued: &QueuedRun) -> bool {
    if queued.parked {
        return false;
    }
    match queued.next_retry_at.as_deref() {
        None => true,
        Some(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            // Strictly past the threshold — `>=` would let an entry
            // scheduled at exactly tick-time fire on this tick AND
            // on the previous tick whose timestamp matches.
            Ok(t) => Utc::now() > t,
            // Garbage timestamp — treat as due rather than parking
            // forever on a parse error.
            Err(_) => true,
        },
    }
}

/// Drain queue. If `silent`, suppress all output (for background use).
/// `bus` (optional) gets the upload-progress events.
pub async fn drain(creds: &Credentials, bus: Option<&EventBus>, silent: bool) {
    let db = match db::open() {
        Ok(db) => db,
        Err(e) => {
            if !silent {
                eprintln!("db: {e}");
            }
            return;
        }
    };
    let pending: Vec<(String, QueuedRun)> = match db.list_queued_runs() {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return,
        Err(e) => {
            if !silent {
                eprintln!("queue read: {e}");
            }
            return;
        }
    };
    let due: Vec<(String, QueuedRun)> = pending.into_iter().filter(|(_, q)| is_due(q)).collect();
    if due.is_empty() {
        return;
    }
    if !silent {
        eprintln!("Uploading {} queued run(s)...\n", due.len());
    }
    // Release the redb lock before the (slow) uploads; entry-state
    // mutations inside upload_queued_run reopen it briefly.
    drop(db);
    let http = crate::http::client();
    for (id, queued) in &due {
        upload_queued_run(http, creds, id, queued, bus, silent).await;
    }
}

/// Continuous background drain. Tick every 5s; pick up entries whose
/// `next_retry_at` has come due. Spawned once at station startup; runs
/// for the lifetime of the process. Cheap when the queue is empty —
/// just a directory listing.
pub async fn run_drain_loop(creds: Credentials, bus: EventBus) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        drain(&creds, Some(&bus), true).await;
    }
}

// ---------------------------------------------------------------------------
// Single-entry actions (operator-driven)
// ---------------------------------------------------------------------------

/// Operator clicked "Retry now" on a parked entry. Clears `parked`
/// and `next_retry_at` so the next drain tick picks it up. Also
/// triggers an immediate single-shot upload so the operator gets
/// feedback within the round-trip rather than the next 5s tick.
pub async fn retry_one(creds: &Credentials, bus: Option<&EventBus>, queue_id: &str) {
    let db = match db::open() {
        Ok(db) => db,
        Err(_) => return,
    };
    let entry = db
        .list_queued_runs::<QueuedRun>()
        .ok()
        .and_then(|v| v.into_iter().find(|(id, _)| id == queue_id));
    let Some((_, mut queued)) = entry else { return };
    queued.parked = false;
    queued.next_retry_at = None;
    let _ = db.enqueue_run(queue_id, &queued);
    drop(db);
    upload_queued_run(
        crate::http::client(),
        creds,
        queue_id,
        &queued,
        bus,
        true,
    )
    .await;
}

/// Operator clicked "Drop". Hard-delete the entry + attachments.
pub fn drop_one(bus: Option<&EventBus>, queue_id: &str) {
    let db = match db::open() {
        Ok(db) => db,
        Err(_) => return,
    };
    let pending: Vec<(String, QueuedRun)> = db.list_queued_runs().unwrap_or_default();
    if let Some((_, queued)) = pending.iter().find(|(id, _)| id == queue_id) {
        for att in &queued.attachments {
            let _ = std::fs::remove_file(&att.path);
        }
        cleanup_attachments(&queued.attachments);
        if let Ok(db) = db::open() {
            let _ = db.dequeue_run(queue_id);
        }
        if let Some(bus) = bus {
            let _ = bus.send(StationEvent::RunUploadDropped {
                queue_id: queue_id.to_string(),
                reason: "manual".to_string(),
            });
        }
    }
}

/// Snapshot of the current queue, formatted as wire events. Used at
/// hydration time so operator UIs that mount mid-session see what's
/// already queued without waiting for the next state change.
pub fn snapshot_events() -> Vec<StationEvent> {
    let db = match db::open() {
        Ok(db) => db,
        Err(_) => return Vec::new(),
    };
    let pending: Vec<(String, QueuedRun)> = db.list_queued_runs().unwrap_or_default();
    let mut out = Vec::with_capacity(pending.len() * 2);
    for (queue_id, q) in pending {
        // Replay the queue event so the UI seeds the row.
        out.push(StationEvent::RunUploadQueued {
            queue_id: queue_id.clone(),
            procedure_id: q.request.procedure_id.clone(),
            outcome: q.request.outcome.to_string(),
            serial_number: Some(q.request.serial_number.clone()),
            attachment_count: q.attachments.len() as u32,
            queued_at: q
                .queued_at
                .clone()
                .unwrap_or_else(|| Utc::now().to_rfc3339()),
        });
        // If we have prior failure state, replay it so the row
        // immediately shows "failed" with the captured body.
        if let Some(err) = q.last_error {
            out.push(StationEvent::RunUploadFailed {
                queue_id,
                attempt: q.attempt_count,
                kind: err.kind,
                status: err.status,
                error: err.error,
                next_retry_at: q.next_retry_at,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// CLI: tofupilot queue [ls|get|retry|rm|export]
// ---------------------------------------------------------------------------

/// Human status for a queue entry. Lifecycle first: a parked or
/// backoff entry is not done even when `run_id` is already set (run
/// created server-side, attachments still failing) — reporting those
/// as uploaded would read as terminal success in the `ls` table.
fn entry_status(q: &QueuedRun) -> &'static str {
    if q.parked {
        "parked"
    } else if q.next_retry_at.is_some() {
        "backoff"
    } else if q.run_id.is_some() {
        "attachments pending"
    } else {
        "pending"
    }
}

/// One queue entry as a flat JSON object. Shared by the `ls` table
/// and its `--json` output so both stay in sync.
fn entry_summary(id: &str, q: &QueuedRun) -> serde_json::Value {
    serde_json::json!({
        "queue_id": id,
        "procedure_id": q.request.procedure_id,
        "outcome": q.request.outcome.to_string(),
        "serial_number": q.request.serial_number,
        "run_id": q.run_id,
        "status": entry_status(q),
        "attempts": q.attempt_count,
        "attachments": q.attachments.len(),
        "parked": q.parked,
        "queued_at": q.queued_at,
        "last_attempt_at": q.last_attempt_at,
        "next_retry_at": q.next_retry_at,
        "last_error": q.last_error.as_ref().map(|e| e.kind.clone()),
    })
}

pub async fn list_cmd(json_mode: bool) -> i32 {
    let db = match db::open() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open database: {e}");
            return 1;
        }
    };

    let pending: Vec<(String, QueuedRun)> = match db.list_queued_runs() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to read queue: {e}");
            return 1;
        }
    };

    if json_mode {
        for (id, queued) in &pending {
            println!("{}", entry_summary(id, queued));
        }
        return 0;
    }

    if pending.is_empty() {
        eprintln!("Queue is empty.");
        return 0;
    }

    let columns = [
        Column {
            header: "QUEUE ID",
            path: "queue_id",
            format: "",
            width: 20,
            truncate: true,
        },
        Column {
            header: "PROCEDURE",
            path: "procedure_id",
            format: "",
            width: 20,
            truncate: true,
        },
        Column {
            header: "OUTCOME",
            path: "outcome",
            format: "",
            width: 8,
            truncate: false,
        },
        Column {
            header: "SERIAL",
            path: "serial_number",
            format: "",
            width: 20,
            truncate: true,
        },
        Column {
            header: "STATUS",
            path: "status",
            format: "",
            width: 20,
            truncate: false,
        },
        Column {
            header: "ATTEMPTS",
            path: "attempts",
            format: "",
            width: 8,
            truncate: false,
        },
        Column {
            header: "ATTACHMENTS",
            path: "attachments",
            format: "",
            width: 5,
            truncate: false,
        },
    ];

    let items: Vec<serde_json::Value> =
        pending.iter().map(|(id, q)| entry_summary(id, q)).collect();

    display::print_table(&items, &columns);
    0
}

pub async fn get_cmd(queue_id: &str, json_mode: bool) -> i32 {
    let db = match db::open() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open database: {e}");
            return 1;
        }
    };
    let pending: Vec<(String, QueuedRun)> = match db.list_queued_runs() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to read queue: {e}");
            return 1;
        }
    };
    let Some((id, q)) = pending.into_iter().find(|(qid, _)| qid == queue_id) else {
        eprintln!("Queue entry '{queue_id}' not found.");
        return 1;
    };

    if json_mode {
        let mut v = serde_json::to_value(&q).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("queue_id".to_string(), id.clone().into());
            obj.insert("status".to_string(), entry_status(&q).into());
        }
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return 0;
    }

    println!("Queue ID:     {id}");
    println!("Status:       {}", entry_status(&q));
    println!("Procedure:    {}", q.request.procedure_id);
    println!("Serial:       {}", q.request.serial_number);
    println!("Outcome:      {}", q.request.outcome);
    if let Some(run_id) = &q.run_id {
        println!("Run ID:       {run_id} (run created server-side)");
    }
    println!("Attempts:     {}", q.attempt_count);
    if let Some(t) = &q.queued_at {
        println!("Queued at:    {t}");
    }
    if let Some(t) = &q.last_attempt_at {
        println!("Last attempt: {t}");
    }
    if let Some(t) = &q.next_retry_at {
        println!("Next retry:   {t}");
    }
    if let Some(err) = &q.last_error {
        let status = err
            .status
            .map(|s| format!(" (HTTP {s})"))
            .unwrap_or_default();
        println!("Last error:   {}{status}: {}", err.kind, err.error);
    }
    if q.attachments.is_empty() {
        println!("Attachments:  none");
    } else {
        println!("Attachments:");
        for att in &q.attachments {
            let missing = if std::path::Path::new(&att.path).exists() {
                ""
            } else {
                " (missing)"
            };
            println!("  {} ({}) {}{missing}", att.name, att.mimetype, att.path);
        }
    }
    0
}

pub async fn export_cmd(queue_id: &str, out: Option<&std::path::Path>, json_mode: bool) -> i32 {
    let db = match db::open() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open database: {e}");
            return 1;
        }
    };
    let pending: Vec<(String, QueuedRun)> = match db.list_queued_runs() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to read queue: {e}");
            return 1;
        }
    };
    let Some((_, q)) = pending.into_iter().find(|(qid, _)| qid == queue_id) else {
        eprintln!("Queue entry '{queue_id}' not found.");
        return 1;
    };
    let payload = match serde_json::to_string_pretty(&q.request) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to serialize payload: {e}");
            return 1;
        }
    };
    match out {
        Some(path) => {
            if let Err(e) = std::fs::write(path, payload + "\n") {
                eprintln!("Failed to write {}: {e}", path.display());
                return 1;
            }
            if json_mode {
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "exported",
                        "queue_id": queue_id,
                        "path": path.display().to_string(),
                    })
                );
            } else {
                eprintln!("Exported {queue_id} to {}", path.display());
            }
        }
        // Without --out the payload itself goes to stdout and is
        // already JSON, so json_mode changes nothing.
        None => println!("{payload}"),
    }
    0
}

pub async fn retry_cmd(creds: &Credentials, queue_id: Option<&str>, json_mode: bool) -> i32 {
    let Some(id) = queue_id else {
        // Manual `tofupilot queue retry` un-parks every entry so 4xx
        // failures get another shot — that's the whole point of the
        // command. Update flags on disk before draining; a swallowed
        // open error here would leave parked entries skipped by the
        // drain with no hint why nothing was attempted.
        match db::open() {
            Ok(db) => {
                let pending: Vec<(String, QueuedRun)> = db.list_queued_runs().unwrap_or_default();
                for (id, mut q) in pending {
                    if q.parked || q.next_retry_at.is_some() {
                        q.parked = false;
                        q.next_retry_at = None;
                        let _ = db.enqueue_run(&id, &q);
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to open database: {e}");
                return 1;
            }
        }
        drain(creds, None, json_mode).await;
        // Everything was un-parked above, so whatever is still queued
        // failed again on this pass.
        let remaining = match db::open().and_then(|db| db.list_queued_runs::<QueuedRun>()) {
            Ok(p) => p.len(),
            Err(e) => {
                eprintln!("Failed to read queue after retry: {e}");
                return 1;
            }
        };
        if json_mode {
            println!(
                "{}",
                serde_json::json!({"type": "retried", "remaining": remaining})
            );
        } else if remaining > 0 {
            eprintln!(
                "{remaining} entr{} still queued.",
                if remaining == 1 { "y" } else { "ies" }
            );
        }
        return if remaining > 0 { 1 } else { 0 };
    };

    let db = match db::open() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open database: {e}");
            return 1;
        }
    };
    let pending: Vec<(String, QueuedRun)> = match db.list_queued_runs() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to read queue: {e}");
            return 1;
        }
    };
    let Some((_, mut queued)) = pending.into_iter().find(|(qid, _)| qid == id) else {
        eprintln!("Queue entry '{id}' not found.");
        return 1;
    };
    queued.parked = false;
    queued.next_retry_at = None;
    let _ = db.enqueue_run(id, &queued);
    drop(db);
    let run_id = upload_queued_run(
        crate::http::client(),
        creds,
        id,
        &queued,
        None,
        json_mode,
    )
    .await;

    match run_id {
        Some(run_id) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::json!({"type": "uploaded", "queue_id": id, "run_id": run_id})
                );
            }
            0
        }
        None => {
            // In human mode the failure detail already went to stderr
            // (upload runs non-silent). In json mode surface the
            // persisted failure record — including a partial-success
            // run_id, so a script doesn't replay a payload whose run
            // already exists server-side.
            if json_mode {
                // Reopen: the pre-upload handle was dropped so the
                // redb lock wasn't held across the network call.
                let entry = db::open()
                    .and_then(|db| db.list_queued_runs::<QueuedRun>())
                    .unwrap_or_default()
                    .into_iter()
                    .find(|(qid, _)| qid == id)
                    .map(|(_, q)| q);
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "failed",
                        "queue_id": id,
                        "run_id": entry.as_ref().and_then(|q| q.run_id.clone()),
                        "error": entry.as_ref().and_then(|q| q.last_error.as_ref()).map(|e| {
                            serde_json::json!({
                                "kind": e.kind,
                                "status": e.status,
                                "error": e.error,
                            })
                        }),
                    })
                );
            }
            1
        }
    }
}

pub async fn drop_cmd(queue_id: Option<&str>, all: bool, json_mode: bool) -> i32 {
    let db = match db::open() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open database: {e}");
            return 1;
        }
    };

    if let Some(id) = queue_id {
        let pending: Vec<(String, QueuedRun)> = match db.list_queued_runs() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to read queue: {e}");
                return 1;
            }
        };
        match pending.iter().find(|(qid, _)| qid == id) {
            Some((_, queued)) => {
                drop_entry(&db, id, queued);
                if json_mode {
                    println!("{}", serde_json::json!({"type": "removed", "queue_id": id}));
                } else {
                    eprintln!("Removed: {id}");
                }
            }
            None => {
                eprintln!("Queue entry '{id}' not found.");
                return 1;
            }
        }
    } else if all {
        let pending: Vec<(String, QueuedRun)> = match db.list_queued_runs() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to read queue: {e}");
                return 1;
            }
        };
        if pending.is_empty() {
            if !json_mode {
                eprintln!("Queue is empty.");
            }
            return 0;
        }
        for (id, queued) in &pending {
            drop_entry(&db, id, queued);
            if json_mode {
                println!("{}", serde_json::json!({"type": "removed", "queue_id": id}));
            }
        }
        if !json_mode {
            eprintln!("Removed {} entries.", pending.len());
        }
    } else {
        eprintln!("Specify a queue ID or use --all to remove everything.");
        return 1;
    }
    0
}

fn drop_entry(db: &db::StateDb, id: &str, queued: &QueuedRun) {
    for att in &queued.attachments {
        let _ = std::fs::remove_file(&att.path);
    }
    // Remove attachment dir if now empty
    cleanup_attachments(&queued.attachments);
    let _ = db.dequeue_run(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(run_id: Option<&str>, parked: bool, next_retry_at: Option<&str>) -> QueuedRun {
        serde_json::from_value(serde_json::json!({
            "request": {
                "outcome": "PASS",
                "procedure_id": "proc_1",
                "started_at": "2026-01-01T00:00:00Z",
                "ended_at": "2026-01-01T00:01:00Z",
                "serial_number": "SN1",
            },
            "attachments": [],
            "run_id": run_id,
            "parked": parked,
            "next_retry_at": next_retry_at,
        }))
        .expect("valid queued run")
    }

    #[test]
    fn entry_status_lifecycle_precedence() {
        // Fresh entry, nothing attempted yet.
        assert_eq!(entry_status(&test_entry(None, false, None)), "pending");
        // Run created server-side, attachments still to upload.
        assert_eq!(
            entry_status(&test_entry(Some("run_1"), false, None)),
            "attachments pending"
        );
        // Backoff wins over run_id: a created run with failing
        // attachments is not terminal success.
        assert_eq!(
            entry_status(&test_entry(
                Some("run_1"),
                false,
                Some("2026-01-01T00:00:00Z")
            )),
            "backoff"
        );
        // Parked wins over everything.
        assert_eq!(
            entry_status(&test_entry(Some("run_1"), true, None)),
            "parked"
        );
        assert_eq!(entry_status(&test_entry(None, true, None)), "parked");
    }

    #[test]
    fn new_queue_id_prefixes_procedure_then_underscore_millis() {
        let id = new_queue_id("proc_abc");
        let suffix = id
            .strip_prefix("proc_abc_")
            .expect("id must start with `<procedure_id>_`");
        assert!(
            suffix.parse::<i64>().is_ok(),
            "suffix must be epoch millis, got {suffix:?}"
        );
    }

    #[test]
    fn new_queue_id_namespaces_by_procedure() {
        // Two different procedures must never collide regardless of timing:
        // the procedure prefix alone guarantees distinct ids. (Asserting on
        // the timestamp suffix would be flaky — both calls can land in the
        // same millisecond.)
        let a = new_queue_id("proc_a");
        let b = new_queue_id("proc_b");
        assert!(a.starts_with("proc_a_"));
        assert!(b.starts_with("proc_b_"));
        assert_ne!(a, b);
    }

    #[test]
    fn backoff_follows_expected_progression_and_caps() {
        assert_eq!(backoff_seconds(0), 15);
        assert_eq!(backoff_seconds(1), 15);
        assert_eq!(backoff_seconds(2), 30);
        assert_eq!(backoff_seconds(3), 300);
        assert_eq!(backoff_seconds(4), 900);
        // Caps at one hour for all further attempts.
        assert_eq!(backoff_seconds(5), 3600);
        assert_eq!(backoff_seconds(99), 3600);
    }

    #[test]
    fn dashboard_url_joins_segments_and_trims_base_slash() {
        assert_eq!(
            dashboard_url_for("https://x.app", "acme", "proc_1", "run_9"),
            "https://x.app/acme/proc_1/runs/run_9"
        );
        // A trailing slash on the base must not double up.
        assert_eq!(
            dashboard_url_for("https://x.app/", "acme", "proc_1", "run_9"),
            "https://x.app/acme/proc_1/runs/run_9"
        );
    }

    #[test]
    fn attachment_error_transient_keywords_retry() {
        for msg in ["connection refused", "DNS failure", "request timeout"] {
            let c = classify_attachment_error(msg.to_string());
            assert_eq!(c.kind, "network", "{msg:?} should be network");
            assert!(!c.park, "{msg:?} should retry, not park");
        }
    }

    #[test]
    fn attachment_error_deterministic_keywords_park() {
        for msg in ["access denied", "signature expired", "not found"] {
            let c = classify_attachment_error(msg.to_string());
            assert_eq!(c.kind, "unknown", "{msg:?} should be unknown");
            assert!(c.park, "{msg:?} should park, not retry");
        }
    }

    #[test]
    fn attachment_error_deterministic_wins_over_transient() {
        // "denied" (deterministic) co-occurring with "connection"
        // (transient) must park — a 403 won't fix itself on retry.
        let c = classify_attachment_error("connection denied".to_string());
        assert!(c.park);
        assert_eq!(c.kind, "unknown");
    }

    #[test]
    fn queued_attachment_round_trips_through_json() {
        let att = QueuedAttachment {
            name: "log.txt".into(),
            path: "/tmp/log.txt".into(),
            mimetype: "text/plain".into(),
        };
        let json = serde_json::to_string(&att).unwrap();
        let back: QueuedAttachment = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "log.txt");
        assert_eq!(back.path, "/tmp/log.txt");
        assert_eq!(back.mimetype, "text/plain");
    }
}
