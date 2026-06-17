//! Authentication: `login` (browser device flow or headless token redemption),
//! `whoami` (cache-first identity), and `logout`. Credentials persist to
//! `~/.tofupilot/credentials.json` (see [`credentials`]).

mod config;
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
}

#[derive(Deserialize)]
struct RedeemTokenResponse {
    api_key: String,
    organization_slug: String,
    installation_id: Option<String>,
    #[serde(default)]
    replaced_installations: u32,
}

fn load_whoami_cache() -> Option<db::WhoamiCache> {
    db::open().ok()?.get_whoami().ok()?
}

/// True when a cached identity is older than `WHOAMI_CACHE_TTL` (or the
/// timestamp is in the future from a clock step). A non-stale cache is
/// served without any network call. The future-side check mirrors the
/// update throttle: a backward clock jump shouldn't pin the cache as
/// "fresh" forever.
fn whoami_cache_is_stale(cache: &db::WhoamiCache) -> bool {
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
) -> Result<(), CliError> {
    let base = base_url.unwrap_or(DEFAULT_BASE_URL);
    let client = Client::builder().timeout(timeouts::AUTH_CLIENT).build()?;

    // Token path: redeem pre-approved setup token (headless station login)
    if let Some(token) = token {
        return redeem_token(&client, base, token).await;
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
    };
    let creds_for_save = creds.clone();
    tokio::task::spawn_blocking(move || credentials::save(&creds_for_save))
        .await
        .map_err(|e| CliError::msg(format!("save task panicked: {e}")))??;

    // Step 7: Fetch and cache whoami
    let whoami_client = Client::builder().timeout(timeouts::AUTH_PROBE).build()?;
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
    // station mode / service) for token logins; for a plain browser login
    // the machine is going back to development use, so tear down any boot
    // service a previous station login installed. This makes browser login
    // the symmetric "return to development" command — no separate disable
    // step. Best-effort: a failure just leaves a stale unit that the next
    // `tofupilot uninstall` removes.
    match creds.installation_id {
        Some(ref installation_id) => finalize_station_login(&creds, installation_id).await,
        None => teardown_boot_service().await,
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

    if let Some(cache) = load_whoami_cache() {
        display_whoami(&cache, json_mode);
        // Refresh only when the cache is stale, so the common case is
        // instant and offline never stalls. A stale-cache refresh is still
        // bounded by AUTH_PROBE and falls back silently — we already showed
        // the cached identity, so a failed refresh costs nothing but the
        // probe wait, and only once per TTL.
        if whoami_cache_is_stale(&cache) {
            if let Ok(client) = Client::builder().timeout(timeouts::AUTH_PROBE).build() {
                if let Ok(fresh) = fetch_whoami(&client, &creds).await {
                    save_whoami_cache(&fresh);
                }
            }
        }
        return Ok(());
    }

    // Cold cache: nothing local to show, so fetch from the server. Still
    // falls back to a minimal line if the network is unavailable.
    let client = Client::builder().timeout(timeouts::AUTH_PROBE).build()?;
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
    if let Some(creds) = credentials::load() {
        notify_server_logout(&creds, false).await;
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
    let Ok(client) = Client::builder().timeout(timeouts::AUTH_PROBE).build() else {
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

    Ok(db::WhoamiCache {
        fetched_at: chrono::Utc::now(),
        auth_type: info["auth_type"].as_str().unwrap_or("user").to_string(),
        user_id: info["user_id"].as_str().map(str::to_string),
        user_name: info["user_name"].as_str().map(str::to_string),
        user_email: info["user_email"].as_str().map(str::to_string),
        station_name: info["station_name"].as_str().map(str::to_string),
        station_id: info["station_id"].as_str().map(str::to_string),
        organization_name: info["organization_name"].as_str().unwrap_or("").to_string(),
        organization_slug: info["organization_slug"].as_str().unwrap_or("").to_string(),
    })
}

async fn redeem_token(client: &Client, base: &str, token: &str) -> Result<(), CliError> {
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
    };
    // See login fn above — icacls call inside `save` shells out on
    // Windows; offload off the tokio executor.
    let creds_for_save = creds.clone();
    tokio::task::spawn_blocking(move || credentials::save(&creds_for_save))
        .await
        .map_err(|e| CliError::msg(format!("save task panicked: {e}")))??;

    // Fetch and cache identity
    let whoami_client = Client::builder().timeout(timeouts::AUTH_PROBE).build()?;
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
