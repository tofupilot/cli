//! Authentication: `login` (browser device flow or headless token redemption),
//! `whoami` (cache-first identity), and `logout`. Credentials persist to
//! `~/.tofupilot/credentials.json` (see [`credentials`]).

pub(crate) mod config;
pub mod credentials;

use config::{CLIENT_ID, DEFAULT_BASE_URL, POLL_INTERVAL};
use credentials::Credentials;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

use super::db;
use crate::config::timeouts;
use crate::error::CliError;
use crate::http::RequestBuilderExt;

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
}

#[derive(Deserialize)]
struct Organization {
    id: String,
    name: String,
    slug: String,
}

#[derive(Deserialize)]
struct ApiKeyResponse {
    api_key: String,
    installation_id: Option<String>,
    /// Present since the idempotency work (TP-1012). `serde(default)` so the
    /// CLI keeps working against an older or self-hosted dashboard that does
    /// not send it — uploads then carry no reference, exactly as before.
    #[serde(default)]
    credential_id: Option<String>,
}

#[derive(Deserialize)]
struct RedeemTokenResponse {
    api_key: String,
    organization_slug: String,
    installation_id: Option<String>,
    #[serde(default)]
    credential_id: Option<String>,
    #[serde(default)]
    replaced_installations: u32,
}

/// True when a cached identity is older than `WHOAMI_CACHE_TTL` (or the
/// timestamp is in the future from a clock step). A non-stale cache is
/// served without any network call. The future-side check mirrors the
/// update throttle: a backward clock jump shouldn't pin the cache as
/// "fresh" forever.
pub(crate) fn whoami_cache_is_stale(cache: &db::WhoamiCache) -> bool {
    let age = chrono::Utc::now() - cache.fetched_at;
    match chrono::Duration::from_std(timeouts::WHOAMI_CACHE_TTL) {
        Ok(ttl) => age >= ttl || age < chrono::Duration::zero(),
        Err(_) => true,
    }
}

fn save_whoami_cache(cache: &db::WhoamiCache) {
    if let Ok(db) = db::open() {
        let _ = db.set_whoami(cache);
    }
}

/// Best-effort, silent whoami refresh for `whoami_cmd`'s stale-cache
/// path: bounded by `AUTH_PROBE`, saved to the slot matching the
/// response's `auth_type`, failures swallowed — the caller has already
/// displayed the cached identity.
async fn refresh_whoami(creds: &Credentials) {
    let Ok(client) = crate::http::client_builder()
        .timeout(timeouts::AUTH_PROBE)
        .build()
    else {
        return;
    };
    if let Ok(fresh) = fetch_whoami(&client, creds).await {
        save_whoami_cache(&fresh);
    }
}

fn display_whoami(cache: &db::WhoamiCache, json_mode: bool) {
    if json_mode {
        println!(
            "{}",
            serde_json::json!({
                "type": "whoami",
                "auth_type": cache.auth_type,
                "user_id": cache.user_id,
                "user_name": cache.user_name,
                "user_email": cache.user_email,
                "station_id": cache.station_id,
                "station_name": cache.station_name,
                "organization_name": cache.organization_name,
                "organization_slug": cache.organization_slug,
            })
        );
        return;
    }
    match cache.auth_type.as_str() {
        "station" => {
            crate::log::success(&format!(
                "Logged in as station \"{}\" in {}",
                cache.station_name.as_deref().unwrap_or("unknown"),
                cache.organization_slug,
            ));
        }
        _ => {
            crate::log::success(&format!(
                "Logged in as {} ({}) in {}",
                cache.user_name.as_deref().unwrap_or("unknown"),
                cache.user_email.as_deref().unwrap_or("unknown"),
                cache.organization_slug,
            ));
        }
    }
}

/// Login: device flow (interactive) or token redemption (headless).
pub async fn login_cmd(
    base_url: Option<&str>,
    org_slug: Option<&str>,
    token: Option<&str>,
    ca_cert: Option<&str>,
) -> Result<(), CliError> {
    let base = base_url.unwrap_or(DEFAULT_BASE_URL);
    // Re-logging in without --ca-cert keeps the CA already on file: writing
    // None would drop a self-hosted station back to the public trust store,
    // and every later command would fail with an opaque TLS error whose cause
    // is a flag the operator did not type this time.
    //
    // Only inherited for the SAME server, and looked up across BOTH credential
    // slots (station first) — see `stored_ca_for_base` for why the user-first
    // `load()` silently wiped a station's CA on token rotation. Passing
    // --ca-cert "" clears it.
    let explicit_ca = matches!(ca_cert, Some(p) if !p.is_empty());
    let stored_ca = credentials::stored_ca_for_base(base);
    let ca_cert = match ca_cert {
        Some(path) => Some(path.to_string()),
        None => stored_ca,
    }
    .filter(|p| !p.is_empty());
    // Install before the first request: a self-hosted login must trust the
    // private CA during login itself, not only afterwards. Precedence matches
    // the rest of the CLI and the self-hosting docs: an explicit --ca-cert
    // typed right now beats everything, but a merely INHERITED stored path
    // must not beat TOFUPILOT_CA_CERT — an operator who rotated the CA and
    // exported the new path would otherwise lose to a stale stored one
    // (main.rs applies the same env-wins rule to the startup install).
    if let Some(path) = ca_cert.as_deref() {
        if explicit_ca || std::env::var_os(crate::http::CA_CERT_ENV).is_none() {
            crate::http::set_ca_cert(path);
        }
    }
    let client = crate::http::client_builder()
        .timeout(timeouts::AUTH_CLIENT)
        .build()?;

    // Token path: redeem pre-approved setup token (headless station login)
    if let Some(token) = token {
        return redeem_token(&client, base, token, ca_cert.as_deref()).await;
    }

    // Device flow path: interactive browser login
    // Step 1: Request device code
    crate::log::info("Requesting device code...");
    let resp = client
        .post(format!("{base}/api/auth/device/code"))
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await?;
    let device: DeviceCodeResponse = super::http::ok_or_describe(resp)
        .await
        .map_err(|e| format!("Request device code: {}", e.body()))?
        .json()
        .await?;

    let formatted_code = if device.user_code.len() == 8 {
        format!("{}-{}", &device.user_code[..4], &device.user_code[4..])
    } else {
        device.user_code.clone()
    };

    eprintln!();
    eprintln!("  Your code: {formatted_code}");
    eprintln!();
    eprintln!("  Approve in your browser to continue.");
    eprintln!();

    // Step 2: Open browser. Use the dedup-aware launcher so a repeat
    // login doesn't spawn a duplicate tab on Chromium-family browsers.
    // On failure (no DE / headless / unsupported platform) fall back
    // to printing the URL so the operator can paste it manually
    // instead of being stuck waiting for a tab that never opens.
    let url = format!("{}?user_code={}", device.verification_uri, device.user_code);
    if let Err(e) = crate::browser_open::open_or_focus(&url) {
        crate::log::warn(&format!(
            "couldn't open browser ({e}); paste this URL: {url}"
        ));
    }

    // Step 3: Poll for approval (with timeout)
    let expires_in = device.expires_in.unwrap_or(1800);
    let token = poll_for_token(
        &client,
        base,
        &device.device_code,
        device.interval,
        expires_in,
    )
    .await?;

    // Step 4: Select organization
    let org = select_org(&client, base, &token, org_slug).await?;
    crate::log::success(&format!("Organization: {}", org.name));

    // Step 5: Create a user-scoped API key for the selected organization.
    let body = serde_json::json!({ "organization_id": org.id });

    let resp = client
        .post(format!("{base}/api/cli/login"))
        .bearer(&token)
        .json(&body)
        .send()
        .await?;
    let key: ApiKeyResponse = super::http::ok_or_describe(resp)
        .await
        .map_err(|e| format!("Create API key: {}", e.body()))?
        .json()
        .await?;

    // Step 6: Save credentials. On Windows `save` shells out to icacls
    // (50-300ms) to lock the ACL; on Unix it does `fs::set_permissions`
    // (microseconds). Wrap in `spawn_blocking` so the icacls subprocess
    // doesn't stall the tokio executor for the rest of the login flow.
    let creds = Credentials {
        api_key: key.api_key,
        base_url: base.to_string(),
        organization_slug: org.slug.clone(),
        installation_id: key.installation_id,
        credential_id: key.credential_id,
        ca_cert: ca_cert.clone(),
    };
    let creds_for_save = creds.clone();
    tokio::task::spawn_blocking(move || credentials::save(&creds_for_save))
        .await
        .map_err(|e| CliError::msg(format!("save task panicked: {e}")))??;

    // Step 7: Fetch and cache whoami
    let whoami_client = crate::http::client_builder()
        .timeout(timeouts::AUTH_PROBE)
        .build()?;
    if let Ok(cache) = fetch_whoami(&whoami_client, &creds).await {
        save_whoami_cache(&cache);
    }

    crate::log::success(&format!("Logged in to {}", org.name));

    // Fresh credentials usually mean the operator just fixed
    // whatever blocked uploads (4xx auth, wrong org, expired key).
    // Un-park parked entries and kick a drain so they get retried
    // with the new key instead of waiting for the next station-mode
    // tick (which may never come for a one-shot `login`).
    unpark_and_drain(&creds).await;

    // Step 8: Station login finalization (sync config, pull, hand off to
    // station mode / service) for token logins. A browser login on a
    // machine with NO station identity is a "return to development" — tear
    // down any boot service a previous station login left. But a browser
    // login on a machine that is STILL a station (the common case: an
    // operator logging in as a user only to run `tofupilot deploy`) must
    // leave the station's boot service intact, now that user and station
    // credentials live in separate slots. Best-effort: a failure just
    // leaves a stale unit that the next `tofupilot uninstall` removes.
    match creds.installation_id {
        Some(ref installation_id) => finalize_station_login(&creds, installation_id).await,
        None if credentials::load_station().is_none() => teardown_boot_service().await,
        None => {}
    }

    Ok(())
}

/// Remove any station boot service left by a previous station login,
/// turning a plain login into the symmetric "return to development"
/// command. On a never-a-station machine the per-OS guards make this a
/// pure filesystem stat; on an actual station it shells out to
/// launchctl/systemctl/reg, so offload off the tokio executor like the
/// rest of this module's blocking work. Best-effort: a failure just
/// leaves a stale unit that the next `tofupilot uninstall` removes.
async fn teardown_boot_service() {
    let result = tokio::task::spawn_blocking(|| super::config::apply_launch_on_boot(false)).await;
    match result {
        Ok(Err(e)) => crate::log::warn(&format!(
            "couldn't remove the station boot service ({e}); run `tofupilot uninstall` if this machine was a station"
        )),
        Err(e) => crate::log::warn(&format!("boot-service teardown task panicked: {e}")),
        Ok(Ok(())) => {}
    }
}

/// Show current identity. Cache-first: when a cached identity exists we
/// display it immediately and refresh the cache in the background, so the
/// command is instant and never waits on the network — important offline
/// or on a flaky link where the probe would otherwise stall up to
/// `AUTH_PROBE`. The blocking server fetch only happens on a cold cache,
/// where there's nothing local to show.
pub async fn whoami_cmd(json_mode: bool) -> Result<(), CliError> {
    let creds = credentials::load().ok_or("not logged in, run `tofupilot login`")?;

    // Read the slot matching the credential record the refresh below will
    // query with, so the displayed identity and the fetch can never
    // belong to different logins.
    if let Some(cache) = db::cached_whoami(creds.whoami_slot()) {
        display_whoami(&cache, json_mode);
        // Refresh only when the cache is stale, so the common case is
        // instant and offline never stalls. A stale-cache refresh is still
        // bounded by AUTH_PROBE and falls back silently — we already showed
        // the cached identity, so a failed refresh costs nothing but the
        // probe wait, and only once per TTL.
        if whoami_cache_is_stale(&cache) {
            refresh_whoami(&creds).await;
        }
        return Ok(());
    }

    // Cold cache: nothing local to show, so fetch from the server. Still
    // falls back to a minimal line if the network is unavailable.
    let client = crate::http::client_builder()
        .timeout(timeouts::AUTH_PROBE)
        .build()?;
    match fetch_whoami(&client, &creds).await {
        Ok(cache) => {
            save_whoami_cache(&cache);
            display_whoami(&cache, json_mode);
        }
        Err(_) => {
            if json_mode {
                // Offline fallback: credentials exist but identity could
                // not be fetched. `partial` lets consumers distinguish
                // this from a full identity object.
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "whoami",
                        "partial": true,
                        "organization_slug": creds.organization_slug,
                        "base_url": creds.base_url,
                    })
                );
            } else {
                crate::log::success(&format!(
                    "Logged in to {} ({})",
                    creds.organization_slug, creds.base_url
                ));
            }
        }
    }
    Ok(())
}

/// Clear stored credentials, whoami cache, and local deployments.
/// Notifies server to mark installation as logged out.
pub async fn logout_cmd() -> Result<(), CliError> {
    // A machine can hold both a user and a station identity in separate
    // slots; logout clears both, so notify the server for each so neither
    // is left "active" server-side. Skip the station notify when it shares
    // the user's api_key (a pure-station machine or legacy single-file
    // install, where load() and load_station() resolve the same record).
    let user = credentials::load();
    let station = credentials::load_station();
    if let Some(creds) = &user {
        notify_server_logout(creds, false).await;
    }
    if let Some(creds) = &station {
        if user.as_ref().map(|u| &u.api_key) != Some(&creds.api_key) {
            notify_server_logout(creds, false).await;
        }
    }

    credentials::clear()?;
    if let Ok(db) = db::open() {
        let _ = db.clear_whoami();
    }
    let _ = db::clear_deployments();
    crate::log::success("Logged out.");
    Ok(())
}

/// Notify the server that this installation is logging out (or being
/// uninstalled). Best-effort: the local cleanup path always runs regardless
/// of the server outcome. Warns on non-2xx so lost audit events are visible.
pub async fn notify_server_logout(creds: &Credentials, uninstalled: bool) {
    let base = creds.base();
    let Ok(client) = crate::http::client_builder()
        .timeout(timeouts::AUTH_PROBE)
        .build()
    else {
        return;
    };
    let resp = client
        .post(format!("{base}/api/cli/logout"))
        .bearer(&creds.api_key)
        .json(&serde_json::json!({
            "installation_id": creds.installation_id,
            "uninstalled": uninstalled,
        }))
        .send()
        .await;
    if let Ok(r) = resp {
        if !r.status().is_success() {
            crate::log::warn(&format!(
                "Server logout returned {}; proceeding with local cleanup.",
                r.status(),
            ));
        }
    }
}

async fn fetch_whoami(client: &Client, creds: &Credentials) -> Result<db::WhoamiCache, CliError> {
    let resp = client
        .get(format!("{}/api/cli/whoami", creds.base_url))
        .bearer(&creds.api_key)
        .send()
        .await?;
    let info: serde_json::Value = super::http::ok_or_describe(resp)
        .await
        .map_err(|e| format!("whoami: {}", e.body()))?
        .json()
        .await?;

    Ok(whoami_cache_from_json(&info))
}

/// `credential_id` from a `/api/cli/whoami` body: the api_key row's primary
/// key, absent from dashboards that predate it. Kept apart from
/// [`whoami_cache_from_json`] because it is not identity — it belongs in the
/// credential file, next to the key it describes, not in the whoami cache.
pub(crate) fn credential_id_from_whoami(info: &serde_json::Value) -> Option<String> {
    info["credential_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// How long a failed `credential_id` probe is remembered. The probe sits on
/// the startup path of every run, so a bench that is offline, or a self-hosted
/// dashboard that predates the field, must cost one AUTH_PROBE timeout an
/// hour, not one per run.
const CREDENTIAL_ID_PROBE_RETRY_SECS: i64 = 3600;

/// One marker per credential slot: a station and a user login on the same
/// machine talk to the same dashboard but are two different files.
fn credential_slot(creds: &Credentials) -> &'static str {
    if creds.installation_id.is_some() {
        "station"
    } else {
        "user"
    }
}

/// Whether a probe that failed at `failed_at` (unix seconds) is still inside
/// its retry window at `now`. Pure so it can be tested; a clock that went
/// backwards counts as elapsed rather than pinning the marker forever.
fn probe_window_holds(failed_at: i64, now: i64) -> bool {
    let elapsed = now - failed_at;
    (0..CREDENTIAL_ID_PROBE_RETRY_SECS).contains(&elapsed)
}

fn probe_recently_failed(slot: &str) -> bool {
    let Ok(db) = db::open() else { return false };
    let Ok(Some(failed_at)) = db.credential_probe_failed_at(slot) else {
        return false;
    };
    probe_window_holds(failed_at, chrono::Utc::now().timestamp())
}

fn remember_probe_failure(slot: &str) {
    if let Ok(db) = db::open() {
        let _ = db.set_credential_probe_failed_at(slot, chrono::Utc::now().timestamp());
    }
}

/// GET `/api/cli/whoami` and read `credential_id`. `None` covers every miss:
/// offline, non-2xx, unparseable body, or a dashboard that predates the field.
async fn fetch_credential_id(creds: &Credentials) -> Option<String> {
    let client = crate::http::client_builder()
        .timeout(timeouts::AUTH_PROBE)
        .build()
        .ok()?;
    let resp = client
        .get(format!("{}/api/cli/whoami", creds.base_url))
        .bearer(&creds.api_key)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let info = resp.json::<serde_json::Value>().await.ok()?;
    credential_id_from_whoami(&info)
}

/// Fill in `creds.credential_id` from the server when the credential file
/// predates that field, and persist it. Returns whether the file was updated.
///
/// This is what makes the run-upload idempotency reference reach the
/// installed base: a station enrolled before the field existed never re-runs
/// `login --token`, and a developer rarely re-runs `login`, so without this
/// both would keep uploading without a reference indefinitely. One round-trip
/// (AUTH_PROBE timeout), only while the field is missing — once written it
/// never runs again for that file. A miss is remembered for
/// [`CREDENTIAL_ID_PROBE_RETRY_SECS`] so the startup path is not taxed on
/// every run while the answer cannot change.
///
/// `save` routes by identity (station slot when `installation_id` is set), so
/// this cannot cross-write the other slot on a dual-login machine.
pub(crate) async fn backfill_credential_id(creds: &mut Credentials) -> bool {
    if creds.credential_id.is_some() {
        return false;
    }
    let slot = credential_slot(creds);
    if probe_recently_failed(slot) {
        return false;
    }
    let Some(id) = fetch_credential_id(creds).await else {
        remember_probe_failure(slot);
        return false;
    };
    persist_credential_id(creds, id).await
}

/// Set `credential_id` on `creds` and write the credential file. Shared by
/// [`backfill_credential_id`] and the station daemon's probe tick, which
/// already holds a whoami body and must not pay a second round-trip.
/// `spawn_blocking` for the same reason as the login flow: on Windows `save`
/// shells out to icacls.
pub(crate) async fn persist_credential_id(creds: &mut Credentials, id: String) -> bool {
    creds.credential_id = Some(id);
    let for_save = creds.clone();
    match tokio::task::spawn_blocking(move || credentials::save(&for_save)).await {
        Ok(Ok(())) => {
            crate::log::info("Enabled idempotent run uploads for this credential.");
            true
        }
        Ok(Err(e)) => {
            crate::log::warn(&format!(
                "Could not save the credential id ({e}); uploads keep going without an idempotency reference."
            ));
            false
        }
        Err(e) => {
            crate::log::warn(&format!("save task panicked: {e}"));
            false
        }
    }
}

/// Parse a `/api/cli/whoami` response body into a cache row, stamped now.
/// Shared with the station daemon's auth probe, which hits the same
/// endpoint and feeds the identity to the station slot instead of
/// discarding the body (one round-trip serves both purposes).
pub(crate) fn whoami_cache_from_json(info: &serde_json::Value) -> db::WhoamiCache {
    db::WhoamiCache {
        fetched_at: chrono::Utc::now(),
        auth_type: info["auth_type"].as_str().unwrap_or("user").to_string(),
        user_id: info["user_id"].as_str().map(str::to_string),
        user_name: info["user_name"].as_str().map(str::to_string),
        user_email: info["user_email"].as_str().map(str::to_string),
        station_name: info["station_name"].as_str().map(str::to_string),
        station_id: info["station_id"].as_str().map(str::to_string),
        organization_name: info["organization_name"].as_str().unwrap_or("").to_string(),
        organization_slug: info["organization_slug"].as_str().unwrap_or("").to_string(),
    }
}

async fn redeem_token(
    client: &Client,
    base: &str,
    token: &str,
    ca_cert: Option<&str>,
) -> Result<(), CliError> {
    crate::log::info("Redeeming setup token...");

    // Hardware fields required up-front — installation row is inserted
    // with NOT NULL columns before the first Hardware event lands.
    let hw = crate::commands::station::collect_hardware();

    let raw = client
        .post(format!("{base}/api/cli/login/redeem"))
        .json(&serde_json::json!({
            "token": token,
            "hostname": hw.hostname,
            "os": hw.os,
            "platform": hw.platform,
            "mac_address": hw.mac_address,
            "cli_version": hw.cli_version,
        }))
        .send()
        .await?;

    let raw = match super::http::ok_or_describe(raw).await {
        Ok(ok) => ok,
        Err(e) => {
            // Redeem-specific hints: setup tokens are single-use and time-boxed.
            // Most common failure is "installer ran the curl command twice" or
            // "token expired after an hour."
            let msg = e.body();
            let lower = msg.to_ascii_lowercase();
            if lower.contains("invalid") || lower.contains("consumed") || lower.contains("already")
            {
                return Err(format!(
                    "{msg}. Setup tokens are single-use -- generate a new one from the station's Setup page and re-run the install command.",
                ).into());
            }
            if lower.contains("expire") {
                return Err(format!(
                    "{msg}. Generate a fresh token (they expire after 1h) and re-run the install command.",
                ).into());
            }
            return Err(msg.into());
        }
    };

    let resp: RedeemTokenResponse = raw.json().await?;

    if resp.replaced_installations > 0 {
        let n = resp.replaced_installations;
        let noun = if n == 1 {
            "installation"
        } else {
            "installations"
        };
        crate::log::warn(&format!("Replaced {n} existing {noun} on this station."));
    }

    let creds = Credentials {
        api_key: resp.api_key,
        base_url: base.to_string(),
        organization_slug: resp.organization_slug,
        installation_id: resp.installation_id,
        credential_id: resp.credential_id,
        ca_cert: ca_cert.map(str::to_string),
    };
    // See login fn above — icacls call inside `save` shells out on
    // Windows; offload off the tokio executor.
    let creds_for_save = creds.clone();
    tokio::task::spawn_blocking(move || credentials::save(&creds_for_save))
        .await
        .map_err(|e| CliError::msg(format!("save task panicked: {e}")))??;

    // Fetch and cache identity
    let whoami_client = crate::http::client_builder()
        .timeout(timeouts::AUTH_PROBE)
        .build()?;
    if let Ok(cache) = fetch_whoami(&whoami_client, &creds).await {
        save_whoami_cache(&cache);
        display_whoami(&cache, false);
    } else {
        crate::log::success(&format!("Logged in to {}", creds.organization_slug));
    }

    // Same un-park + kick as the device-flow path. See the comment
    // there for rationale.
    unpark_and_drain(&creds).await;

    // Unlike the device flow, a token redemption is always a station
    // login: the server only issues these for `station:<id>`-scoped setup
    // tokens, so `installation_id` is expected to be present. A missing id
    // means a server-side anomaly during station setup, NOT a return-to-
    // development login — so warn and leave any existing boot service
    // alone rather than tearing down the service the operator is trying
    // to install.
    match creds.installation_id {
        Some(ref installation_id) => finalize_station_login(&creds, installation_id).await,
        None => crate::log::warn(
            "station login returned no installation id; the boot service was not set up. Retry `tofupilot login --token <token>`.",
        ),
    }

    Ok(())
}

/// Finalize a station login: sync server config (which also installs
/// the supervisor unit when the server pushes `launch_on_boot=on`),
/// pull deployments, then run the daemon in the foreground so the
/// operator sees live output for this session.
async fn finalize_station_login(creds: &Credentials, installation_id: &str) {
    let _ = super::config::sync_config(creds, installation_id).await;
    super::pull::run_cmd(false).await;
    // A station should survive a reboot without a second command, so a
    // successful token login is the point where we install the boot
    // service. (This used to be the separate `tofupilot install` step.)
    // Best-effort: a failure here still leaves a working foreground
    // daemon below; it just won't auto-start after a reboot.
    match super::config::apply_launch_on_boot(true) {
        Ok(()) => super::config::print_launch_on_boot_status(creds),
        Err(e) => crate::log::warn(&format!(
            "couldn't enable the station service on boot ({e}); the station runs now but won't restart after a reboot"
        )),
    }
    let code = crate::commands::station::run_cmd(creds, false).await;
    std::process::exit(code);
}

async fn poll_for_token(
    client: &Client,
    base: &str,
    device_code: &str,
    interval_secs: Option<u64>,
    expires_in: u64,
) -> Result<String, CliError> {
    let mut interval = Duration::from_secs(interval_secs.unwrap_or(POLL_INTERVAL));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(expires_in);

    loop {
        tokio::time::sleep(interval).await;

        if tokio::time::Instant::now() > deadline {
            return Err("code expired, run `tofupilot login` again".into());
        }

        let res = client
            .post(format!("{base}/api/auth/device/token"))
            .json(&serde_json::json!({
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                "device_code": device_code,
                "client_id": CLIENT_ID,
            }))
            .send()
            .await?;

        if res.status().is_success() {
            return Ok(res.json::<TokenResponse>().await?.access_token);
        }

        let err: TokenError = res.json().await?;
        match err.error.as_str() {
            "authorization_pending" => continue,
            "slow_down" => {
                interval += Duration::from_secs(5); // RFC 8628: permanently increase
                continue;
            }
            "access_denied" => return Err("authorization denied".into()),
            "expired_token" => return Err("code expired, run `tofupilot login` again".into()),
            other => return Err(format!("auth error: {other}").into()),
        }
    }
}

async fn select_org(
    client: &Client,
    base: &str,
    token: &str,
    slug: Option<&str>,
) -> Result<Organization, CliError> {
    let resp = client
        .get(format!("{base}/api/cli/login"))
        .bearer(token)
        .send()
        .await?;
    let orgs: Vec<Organization> = super::http::ok_or_describe(resp)
        .await
        .map_err(|e| format!("List organizations: {}", e.body()))?
        .json()
        .await?;

    if orgs.is_empty() {
        return Err("no organizations found for this account".into());
    }

    if let Some(slug) = slug {
        return orgs
            .into_iter()
            .find(|o| o.slug == slug)
            .ok_or_else(|| format!("organization '{slug}' not found").into());
    }

    if orgs.len() == 1 {
        return Ok(orgs.into_iter().next().expect("len checked == 1"));
    }

    eprintln!("Multiple organizations found:");
    for (i, o) in orgs.iter().enumerate() {
        eprintln!("  {}: {} ({})", i + 1, o.name, o.slug);
    }
    eprintln!();
    eprintln!("Use --org <slug> to select one.");
    Err("multiple organizations, use --org to select".into())
}

/// Clear the `parked` / `next_retry_at` flags on every queue entry
/// and run a single drain. Called after a successful `login`. The
/// usual reason an entry is parked is a 4xx (auth, wrong org, schema
/// mismatch); a fresh login most often means the operator just fixed
/// it. Pushing the entries through the drain immediately gives them
/// instant feedback rather than waiting for the next station-mode
/// tick (which never happens for a one-shot `login` invocation).
async fn unpark_and_drain(creds: &Credentials) {
    use crate::commands::run::queue;
    let Ok(db) = db::open() else { return };
    let pending: Vec<(String, queue::QueuedRun)> = db.list_queued_runs().unwrap_or_default();
    if pending.is_empty() {
        return;
    }
    for (id, mut q) in pending {
        if q.parked || q.next_retry_at.is_some() {
            q.parked = false;
            q.next_retry_at = None;
            let _ = db.enqueue_run(&id, &q);
        }
    }
    queue::drain(creds, None, true).await;
}

#[cfg(test)]
mod credential_id_tests {
    use super::{credential_id_from_whoami, probe_window_holds, CREDENTIAL_ID_PROBE_RETRY_SECS};

    #[test]
    fn a_failed_probe_is_remembered_for_the_window_and_no_longer() {
        let failed_at = 1_700_000_000;
        assert!(probe_window_holds(failed_at, failed_at));
        assert!(probe_window_holds(
            failed_at,
            failed_at + CREDENTIAL_ID_PROBE_RETRY_SECS - 1
        ));
        assert!(!probe_window_holds(
            failed_at,
            failed_at + CREDENTIAL_ID_PROBE_RETRY_SECS
        ));
        // Clock went backwards: retry rather than hold the marker forever.
        assert!(!probe_window_holds(failed_at, failed_at - 1));
    }

    #[test]
    fn reads_the_id_from_a_whoami_body() {
        let body = serde_json::json!({
            "auth_type": "station",
            "credential_id": "FZmQ8pKx3vN7",
            "station_id": "st_1"
        });
        assert_eq!(
            credential_id_from_whoami(&body).as_deref(),
            Some("FZmQ8pKx3vN7")
        );
    }

    #[test]
    fn absent_or_empty_means_none() {
        // A dashboard that predates the field, and a defensive empty string:
        // both must leave the credential file untouched rather than write a
        // reference namespace of "".
        let older = serde_json::json!({ "auth_type": "station", "station_id": "st_1" });
        assert_eq!(credential_id_from_whoami(&older), None);
        let empty = serde_json::json!({ "credential_id": "" });
        assert_eq!(credential_id_from_whoami(&empty), None);
        let wrong_type = serde_json::json!({ "credential_id": 42 });
        assert_eq!(credential_id_from_whoami(&wrong_type), None);
    }
}
