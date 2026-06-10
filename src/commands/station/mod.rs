//! Station daemon mode: the long-lived, supervised process.
//!
//! Holds a persistent Centrifugo WebSocket to the dashboard, advertises
//! hardware info, and loops receiving `StationCommand`s — pulling deployments
//! and running them through the same `run::run_cmd` path as an interactive
//! run. Installed as a systemd user service or launchd agent.

pub(crate) mod bridge;
pub(crate) mod client;
#[cfg(target_os = "linux")]
mod labwc_touch;
mod pull_stage;

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::commands::auth::credentials::Credentials;
use crate::commands::config;
use crate::commands::pull::sync::StagedDeployment;
use crate::commands::update;
use crate::http::RequestBuilderExt;
use crate::log;
use station_protocol::{StationCommand, StationEvent};

pub(crate) struct HardwareInfo {
    pub hostname: String,
    pub os: String,
    pub platform: String,
    pub mac_address: Option<String>,
    pub cli_version: String,
}

pub(crate) fn collect_hardware() -> HardwareInfo {
    let hostname = sysinfo::System::host_name().unwrap_or_default();
    let os = format!(
        "{} {}",
        sysinfo::System::name().unwrap_or_default(),
        sysinfo::System::os_version().unwrap_or_default()
    );
    // Platform identifier (os_arch). Supported (has matching builder):
    // linux_x86_64, linux_aarch64, macos_arm64. Everything else falls through
    // as `{os}_{arch}` so the dashboard still displays a value.
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos_arm64".to_string(),
        (os, arch) => format!("{os}_{arch}"),
    };
    let mac_address = mac_address::get_mac_address()
        .ok()
        .flatten()
        .map(|m| m.to_string());
    let cli_version = env!("CARGO_PKG_VERSION").to_string();

    HardwareInfo {
        hostname,
        os,
        platform,
        mac_address,
        cli_version,
    }
}

/// Sorted list of locally-pulled deployments — what the operator-UI
/// idle screen offers as pickable rows. Returns empty on DB error so
/// the kiosk just shows "No procedures deployed" instead of crashing
/// the station loop. DB-error and table-iter-error paths log a warn
/// so an operator seeing the empty state can distinguish "really no
/// deployments" from "DB unavailable" via the station's stderr.
pub(crate) fn idle_procedures() -> Vec<crate::local_ws::ProcedureRef> {
    let db = match crate::commands::db::open() {
        Ok(d) => d,
        Err(e) => {
            crate::log::warn(&format!(
                "idle procedures: failed to open redb ({e}); kiosk will show empty list",
            ));
            return Vec::new();
        }
    };
    let rows = match db.list_pull_state() {
        Ok(r) => r,
        Err(e) => {
            crate::log::warn(&format!(
                "idle procedures: failed to list pull_state ({e}); kiosk will show empty list",
            ));
            return Vec::new();
        }
    };
    let mut out: Vec<crate::local_ws::ProcedureRef> = rows
        .into_iter()
        .map(|(id, state)| crate::local_ws::ProcedureRef {
            name: state.name.unwrap_or_else(|| id.clone()),
            id,
        })
        .collect();
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// Push the current idle procedure list into the local-ws hello frame
/// so a kiosk tab connecting (or hydrating) at idle sees the right
/// rows. No-op when the kiosk server isn't running.
pub(crate) async fn refresh_idle_procedures(
    local_ws_server: Option<&std::sync::Arc<crate::local_ws::Server>>,
) {
    if let Some(server) = local_ws_server {
        server.set_procedures(idle_procedures()).await;
    }
}

pub(crate) fn collect_hardware_event(installation_id: &str) -> StationEvent {
    let hw = collect_hardware();
    StationEvent::Hardware {
        installation_id: installation_id.to_string(),
        hostname: hw.hostname,
        os: hw.os,
        platform: hw.platform,
        mac_address: hw.mac_address,
        cli_version: hw.cli_version,
    }
}

pub(crate) fn collect_telemetry_event(installation_id: &str) -> StationEvent {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    sys.refresh_memory();

    let cpu_percent = sys.global_cpu_usage();
    let memory_mb = sys.used_memory() as f32 / 1_048_576.0;

    let disks = sysinfo::Disks::new_with_refreshed_list();
    let disk_free_mb = disks
        .list()
        .iter()
        .map(|d| d.available_space())
        .sum::<u64>() as f32
        / 1_048_576.0;

    let components = sysinfo::Components::new_with_refreshed_list();
    let temperature_c = components
        .list()
        .iter()
        .map(|c| c.temperature())
        .reduce(f32::max);

    StationEvent::Telemetry {
        installation_id: installation_id.to_string(),
        cpu_percent,
        memory_mb,
        disk_free_mb,
        temperature_c,
    }
}

fn show_banner(station_name: &str, org_slug: &str) {
    let blue = "\x1b[38;2;59;130;246m";
    let gold = "\x1b[38;2;234;179;8m";
    let nc = "\x1b[0m";
    let version = env!("CARGO_PKG_VERSION");

    // Plane glyph swaps to `*` on Windows: U+2708 isn't in conhost's
    // default font, renders as `?`.
    #[cfg(windows)]
    let plane = "*";
    #[cfg(not(windows))]
    let plane = "\u{2708}\u{fe0e}";

    eprintln!();
    eprintln!(" {blue}\u{256d}{nc} {gold}{plane}{nc} {blue}\u{256e}{nc}");
    eprintln!(" {nc}[\u{2022}\u{1d17}\u{2022}] TofuPilot CLI v{version}{nc}");
    eprintln!();
    log::success(&format!("{station_name} ({org_slug})"));
}

/// Exit codes for run. Picked from sysexits.h so a supervisor
/// can distinguish them from clap usage errors (~2) and normal CLI errors (1).
const EXIT_REVOKED: i32 = 75; // EX_TEMPFAIL -- credentials revoked, reauth needed
const EXIT_BROKER_LOST: i32 = 74; // EX_IOERR -- stream closed by broker

/// Station mode: single event loop, one stream connection.
///
/// When a test runs (Run command or station-loop restart), it's spawned non-blocking.
/// The loop keeps processing telemetry, config, pull, and UiResponse
/// throughout. No second connection, no drain window, no blocking.
pub async fn run_cmd(creds: &Credentials, json_mode: bool) -> i32 {
    let installation_id = match creds.installation_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        _ => {
            log::error("No installation ID. Generate a setup token from the station's page in the dashboard, then run `tofupilot login --token <setup-token>`.");
            return 1;
        }
    };

    // Single-instance gate is the loopback bind on 127.0.0.1:7321
    // performed by `local_ws::Server::start` below. A second daemon
    // hits EADDRINUSE and exits cleanly. No PID file, no suspend
    // marker, no takeover prompt.

    if !json_mode {
        let whoami = crate::commands::db::open()
            .ok()
            .and_then(|db| db.get_whoami().ok().flatten());
        let station_name = whoami
            .as_ref()
            .and_then(|w| w.station_name.as_deref())
            .unwrap_or("Station");
        let org_slug = whoami
            .as_ref()
            .map(|w| w.organization_slug.as_str())
            .unwrap_or(&creds.organization_slug);
        show_banner(station_name, org_slug);
    }

    // Upload-queue drain is spawned later, after the local-WS server
    // is built and the bridge fans drain events into both Centrifugo
    // and the loopback broadcast. Pre-broker enqueues are durable on
    // disk via `queue::enqueue`, so the drain doesn't have to be
    // running before broker handshake — the operator UI's queue panel
    // simply stays empty until then, same posture as every other
    // event surface. (Earlier code spawned an "early drain" here with
    // a broadcast whose receiver was dropped on the same line; every
    // bus.send returned NoReceivers so progress events were silently
    // swallowed.)

    // Retry the broker connect indefinitely instead of exiting. A Pi
    // boots before WiFi/DHCP fully settle, an upstream outage drops
    // the broker, etc. — the station should ride those out and
    // self-heal rather than dying. Backoff: 5s, doubling to 60s.
    // SIGINT bails out cleanly. Ok(None) (server doesn't support
    // streaming) is a config problem, not transient — exit.
    let mut client = {
        let mut delay = std::time::Duration::from_secs(5);
        let max_delay = std::time::Duration::from_secs(60);
        loop {
            match client::StreamClient::connect(creds).await {
                Ok(Some(c)) => break c,
                Ok(None) => {
                    log::error("Streaming not configured on this server.");
                    return 1;
                }
                Err(e) => {
                    if !json_mode {
                        log::warn(&format!(
                            "Broker connect failed ({e}); retrying in {}s...",
                            delay.as_secs()
                        ));
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = tokio::signal::ctrl_c() => {
                            if !json_mode { eprintln!(); log::info("Aborted."); }
                            return 130;
                        }
                    }
                    delay = std::cmp::min(delay * 2, max_delay);
                }
            }
        }
    };

    if let Err(e) = client
        .publish(&collect_hardware_event(installation_id))
        .await
    {
        log::warn(&format!("Failed to publish hardware event: {e}"));
    }

    // Cold-start auth probe. If the station was revoked while offline (e.g.
    // someone redeemed a setup token on another machine during the downtime)
    // we exit now instead of entering a long station-loop session with a dead key.
    if auth_probe(creds).await == AuthProbeOutcome::Unauthorized {
        if !json_mode {
            log::warn("Logged out: revoked");
        }
        clear_local_credentials();
        client.disconnect().await;
        return EXIT_REVOKED;
    }

    // Report the outcome of any update that preceded this process (we may have
    // been re-exec'd by apply_staged / run_update; the marker lets us publish
    // a matching UpdateApplied event now that the new binary is live).
    publish_pending_update_outcome(&client, installation_id).await;

    // Boot-time update check: synchronously fetch + stage + apply so a station
    // restart picks up the latest release immediately, instead of waiting up
    // to STATION_UPDATE_CHECK_INTERVAL (4h) for the first tick. If apply_staged
    // re-execs we never return; otherwise we fall through to the regular boot.
    // Kiosk isn't bound yet so no detach is needed -- pass `None`.
    if update::auto_update_enabled() {
        if let Err(e) = update::background_check().await {
            log::warn(&format!("Boot update check failed: {e}"));
        }
        try_apply_staged_update(&client, installation_id, json_mode, None).await;
    }

    let config_events = config::sync_config(creds, installation_id).await;
    for event in config_events {
        let _ = client.publish(&event).await;
    }

    // Boot pull. Use the station's existing StreamClient as the publisher
    // so we don't open a second WebSocket against the same station identity.
    // See `pull::run` doc — opening a second client steals Centrifugo's
    // server-side subs and disconnecting it leaves the station-mode WS
    // unsubscribed, dropping subsequent live `Pull` commands.
    {
        let publisher = client.clone_for_health();
        // Boot pull runs before the local-WS server is created (its
        // construction depends on `whoami` which we just fetched above).
        // No kiosk tab can be connected yet, so there's no loopback
        // consumer to miss; pass `None` for the local-WS bridge.
        crate::commands::pull::run_with(json_mode, Some(&publisher), None).await;
    }

    // Station mode is operator-driven. Each completed run leaves the
    // outcome screen up; the next cycle is gated on an explicit
    // `StationCommand::Run` (operator clicks "Run again" / "New run"
    // / picks a different procedure). We don't auto-restart because
    // the outcome screen carries information the operator needs to
    // see — flashing past it to the next identify prompt makes the
    // PASS/FAIL invisible.
    if !json_mode {
        log::info("Waiting for first procedure pick...");
    } else {
        println!("{}", serde_json::json!({"type": "connected"}));
    }

    let mut health_interval =
        tokio::time::interval(crate::config::timeouts::STATION_HEALTH_INTERVAL);
    health_interval.tick().await;

    // Cheap periodic auth probe. Catches the case where this installation was
    // replaced server-side while the Centrifugo signal was missed (station
    // offline, broker down, etc.). 5 min -> ~12 calls/hour per station.
    let mut auth_probe_interval =
        tokio::time::interval(crate::config::timeouts::STATION_AUTH_PROBE_INTERVAL);
    auth_probe_interval.tick().await;

    // Periodic background update check so always-on stations pick up new
    // releases without a restart. The actual swap still happens between
    // runs (see `apply_staged` after each run completes); this tick only
    // stages the new binary in the background.
    let mut update_check_interval =
        tokio::time::interval(crate::config::timeouts::STATION_UPDATE_CHECK_INTERVAL);
    // If the loop stalls (long teardown, blocking call), don't fire a
    // burst of catch-up ticks back-to-back — one is enough.
    update_check_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    update_check_interval.tick().await;

    let mut connected = true;

    // Long-lived local-WS server: bound at station startup if
    // `kiosk_ui` is on, kept alive for every run. Each run plugs
    // its own broadcast in via `attach_run`; the listener stays so
    // a browser tab opened pre-run survives the whole station
    // lifetime, and a tab opened mid-run hydrates the in-flight
    // state without needing to reconnect to a fresh port.
    let whoami = crate::commands::db::open()
        .ok()
        .and_then(|db| db.get_whoami().ok().flatten());

    if !json_mode {
        if let Some(station_id) = whoami.as_ref().and_then(|w| w.station_id.as_deref()) {
            let org = whoami
                .as_ref()
                .map(|w| w.organization_slug.as_str())
                .unwrap_or(&creds.organization_slug);
            log::info(&format!(
                "Web UI: {}/{}/operator/{}",
                creds.base(),
                org,
                station_id,
            ));
        }
    }

    let kiosk_enabled = crate::commands::db::open()
        .ok()
        .and_then(|db| db.get_config("kiosk_ui").ok().flatten())
        .is_some_and(|v| v == "on");

    // Patch labwc's mouseEmulation default so touch-drag scrolls
    // instead of selecting text on Pi OS Bookworm. No-op on macOS
    // (compiled out), no-op on Linux hosts not running labwc, no-op
    // when already set to `no`. See `labwc_touch` for the rationale.
    #[cfg(target_os = "linux")]
    if kiosk_enabled {
        labwc_touch::apply_if_needed();
    }

    // Local-WS station-level command channel. The kiosk tab forwards
    // station-level commands (Exit, Run, ...) here. Run-
    // scoped commands take other paths: `UiResponse` goes to the
    // active run's `ui_response_tx`, `Stop` / `Kill` go straight to
    // the run's cancel token (see `local_ws::handle_text`). Drained
    // alongside Centrifugo cmds in the select loop below.
    let (local_station_cmd_tx, mut local_station_cmd_rx) =
        tokio::sync::mpsc::channel::<StationCommand>(32);

    let local_ws_server: Option<std::sync::Arc<crate::local_ws::Server>> = if kiosk_enabled {
        let station_name = whoami
            .as_ref()
            .and_then(|w| w.station_name.clone())
            .unwrap_or_else(|| "Station".to_string());
        let identity = whoami
            .as_ref()
            .map(|w| crate::local_ws::HelloIdentity {
                auth_type: Some(w.auth_type.clone()),
                organization_slug: Some(w.organization_slug.clone()),
                organization_name: Some(w.organization_name.clone()),
                station_id: w.station_id.clone(),
                user_id: w.user_id.clone(),
                user_email: w.user_email.clone(),
                user_name: w.user_name.clone(),
            })
            .unwrap_or_default();
        match crate::local_ws::Server::start(installation_id.to_string(), station_name, identity)
            .await
        {
            Ok(s) => {
                if !json_mode {
                    log::info(&format!("Kiosk UI: {}", s.boot_url()));
                }
                // Wire the station-level cmd sink so the kiosk tab's
                // Exit button lands on the same `handle_command` path
                // the Centrifugo socket uses.
                s.set_station_cmd_sink(local_station_cmd_tx.clone()).await;
                // Seed the hello frame's procedure list BEFORE launching
                // the kiosk browser. The browser connects within hundreds
                // of ms on Windows (msedge `--app=URL` is direct);
                // `refresh_idle_procedures` doesn't run until ~line 471
                // which is after the boot update check + ConfigApplied
                // publish. If we attach first, the SPA's WS connect can
                // land before the procedure list is in `state.hello`,
                // and the empty hello frame is what the SPA renders into
                // its static `procedures` prop — `set_procedures` later
                // mutates state but doesn't broadcast, so the SPA stays
                // empty until full reload. Seeding here closes the race.
                s.set_procedures(idle_procedures()).await;
                // Attach kiosk window so the browser dies with the
                // station daemon. Logged but not fatal — station can
                // still serve the URL for a manually-opened tab.
                let _ = s.attach_kiosk().await;
                Some(std::sync::Arc::new(s))
            }
            Err(_) => {
                // Bind failure is fatal: EADDRINUSE means another
                // daemon already owns the redb lock (two would
                // corrupt state); other errors (permission denied, …)
                // leave the kiosk SPA with nothing to talk to. The
                // operator-facing message was already printed by
                // `Server::start`'s `map_err`.
                return 1;
            }
        }
    } else {
        None
    };

    // Continuous queue-drain loop. Wakes every 5s, picks up any
    // `next_retry_at`-due entries, emits progress events on a local
    // broadcast bridged into BOTH the centrifugo channel AND the
    // local-WS broadcast so operator UIs (web + Vite kiosk) see queue
    // activity even when no live run is happening.
    {
        let drain_creds = creds.clone();
        let publisher = client.clone_for_health();
        let local_ws_for_drain = local_ws_server.clone();
        tokio::spawn(async move {
            let (bus, _) = tokio::sync::broadcast::channel::<station_protocol::StationEvent>(64);
            let mut rx = bus.subscribe();
            let bridge = tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    let _ = publisher.publish(&ev).await;
                    if let Some(ref server) = local_ws_for_drain {
                        server.publish_event(ev).await;
                    }
                }
            });
            crate::commands::run::queue::run_drain_loop(drain_creds, bus).await;
            bridge.abort();
        });
    }

    // Foreground update AFTER the kiosk attach so the operator sees
    // the UI immediately instead of staring at a black screen while
    // the new binary downloads. The station is long-lived: don't lag
    // one restart behind a release. Failure publishes UpdateFailed;
    // success stages the new binary on disk and is applied between
    // runs by `try_apply_staged_update`.
    if update::auto_update_enabled() {
        let from = update::VERSION.to_string();
        let publisher = client.clone_for_health();
        match update::run_update_with_publisher(Some(&publisher)).await {
            Ok(_) => {}
            Err(e) if e.is_fetch() => {
                // Version endpoint unreachable: no upgrade was attempted.
                // Logging "Update failed: vX -> vX" here would be wrong
                // (we never picked a target) and would publish a spurious
                // UpdateFailed event. Treat as a transient connectivity
                // blip; next tick retries.
                log::warn(&format!("Update check failed: {e}"));
            }
            Err(e) => {
                let target = update::staged_version();
                let to_display = target.as_deref().unwrap_or("unknown");
                log::error(&format!("Update failed: v{from} -> v{to_display} ({e})"));
                let _ = client
                    .publish(&StationEvent::UpdateFailed {
                        installation_id: installation_id.to_string(),
                        from_version: from,
                        to_version: target,
                        error: e.to_string(),
                    })
                    .await;
            }
        }
    }

    // Queue drain is spawned later (after `local_ws_server` is built)
    // so the bridge can fan to both Centrifugo and the loopback
    // broadcast — without the loopback leg, a Vite kiosk never sees
    // upload-queue progress while idle.

    let exit_code: i32;
    // Staged deployment: downloaded in background during a test, swapped between tests.
    let staged: Arc<Mutex<Option<StagedDeployment>>> = Arc::new(Mutex::new(None));

    // Last procedure the operator launched. Memoised so an in-flight
    // `Run` aimed at the same procedure can be detected as a fresh
    // re-trigger versus a procedure switch. Updated on every explicit
    // `Run { procedure_id }`.
    let mut last_procedure_id: Option<String> = None;

    // Seed the kiosk hello frame with the current local deployments
    // so a tab opened before any run has a list to render. Without
    // this the idle screen sits on the empty `procedures: Vec::new()`
    // baked into `Server::start`, and shows "Select a procedure" with
    // zero rows even when the station has deployments on disk.
    refresh_idle_procedures(local_ws_server.as_ref()).await;

    // Cold-boot auto-pick on single-procedure stations: with only
    // one deployable procedure, there's no defensible "wait for
    // operator" behavior — the station should just resume after a
    // reboot (power blip, kernel update, watchdog kill). Multi-
    // procedure stations still wait for an explicit pick because the
    // choice is genuinely ambiguous.
    let mut active_run: Option<crate::commands::run::RunHandle> = None;
    // Set when a run ends naturally (`done_rx` resolves). Read by the
    // update-tick to defer `apply_staged_update` so the operator-UI's
    // outcome screen survives long enough for the operator to read it
    // before the process re-execs under them. Cleared on the next
    // Run command (operator moved on) or after the grace expires.
    let mut last_run_ended_at: Option<std::time::Instant> = None;
    // Detached prior-run awaiters. When the operator clicks Run while
    // a prior run is still in flight, the dispatcher escalates the
    // prior cancel and parks the JoinHandle here so the new
    // `try_start_run` doesn't block on Python teardown / publisher
    // drain. The set is reaped opportunistically (`try_join_next`)
    // before each new park so it doesn't grow unboundedly when an
    // operator hammers the button.
    let mut prior_run_teardowns: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    {
        let idle = idle_procedures();
        if idle.len() == 1 {
            let only = idle[0].id.clone();
            if !json_mode {
                log::info(&format!("Auto-starting only deployment: {}", idle[0].name,));
            }
            last_procedure_id = Some(only.clone());
            active_run = try_start_run(
                Some(&only),
                None,
                None,
                json_mode,
                creds,
                &client,
                local_ws_server.as_ref(),
            )
            .await;
        }
    }

    loop {
        // Poll run completion alongside stream events
        let run_done = async {
            match active_run.as_mut() {
                Some(handle) => (&mut handle.done_rx).await.ok(),
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            // Prefer run-completion over the update tick: if both fire
            // in the same poll, we want last_run_ended_at stamped
            // before the tick reads it, otherwise the tick can apply
            // a staged binary mid-outcome-screen.
            biased;
            _run_exit = run_done => {
                // Drop the run handle so the next select! tick stops
                // polling its `done_rx`. The outcome screen the
                // operator UI now displays stays put until they click
                // Run again / New run / Switch procedure, each of
                // which arrives as a `StationCommand::Run`.
                active_run = None;
                last_run_ended_at = Some(std::time::Instant::now());

                // Apply staged deployment swap, if any landed during
                // the run. Cheap if there's nothing staged.
                {
                    let mut s = staged.lock().await;
                    if let Some(swap) = s.take() {
                        let name = swap.name.clone();
                        if !json_mode {
                            log::info(&format!("Applying staged deployment: {name}"));
                        }
                        if let Ok(db) = crate::commands::db::open() {
                            match swap.apply(&db).await {
                                Ok(_) => {
                                    if !json_mode { log::success(&format!("Deployment updated: {name}")); }
                                }
                                Err(e) => { log::error(&format!("Deployment swap failed: {e}")); }
                            }
                        }
                    }
                }

                // CLI self-update is NOT applied here. The operator-UI
                // shows the outcome screen for as long as the operator
                // wants — re-execing now would yank that screen away
                // mid-read. The next update-check tick (or the one
                // after, etc.) calls `try_apply_staged_update` once
                // `outcome_grace_elapsed` is true.

                // Restore the kiosk hello frame to the idle procedure
                // list. `attach_run` overwrote it with the just-
                // finished run's single-procedure list; without this
                // a tab opening between runs would only see that one
                // procedure. Picks up any deployment swap from above.
                refresh_idle_procedures(local_ws_server.as_ref()).await;
            }
            local_cmd = local_station_cmd_rx.recv() => {
                if let Some(cmd) = local_cmd {
                    match handle_command(
                        cmd, &client, creds, installation_id, json_mode,
                        &mut active_run,
                        &mut prior_run_teardowns,
                        &mut last_procedure_id,
                        &mut last_run_ended_at,
                        &staged,
                        local_ws_server.as_ref(),
                    ).await {
                        LoopControl::Continue => {}
                        LoopControl::Exit(code) => {
                            exit_code = code;
                            break;
                        }
                    }
                }
            }
            msg = client.recv() => {
                match msg {
                    Some(client::StreamMsg::Command(cmd)) => {
                        match handle_command(
                            cmd, &client, creds, installation_id, json_mode,
                            &mut active_run,
                            &mut prior_run_teardowns,
                            &mut last_procedure_id,
                            &mut last_run_ended_at,
                            &staged,
                            local_ws_server.as_ref(),
                        ).await {
                            LoopControl::Continue => {}
                            LoopControl::Exit(code) => {
                                exit_code = code;
                                break;
                            }
                        }
                    }
                    Some(client::StreamMsg::Event(evt)) => {
                        // Events published by other status-channel
                        // subscribers (dashboard presence, future
                        // extensions). Forward into the active run's
                        // dedicated TUI-presence channel — NOT the
                        // run's internal broadcast bus, because the
                        // managed publisher task would re-publish them
                        // to Centrifugo and fanout would loop.
                        if let Some(ref handle) = active_run {
                            let _ = handle.tui_presence_tx.send(evt).await;
                        }
                    }
                    Some(client::StreamMsg::Connected) => {
                        if !connected {
                            connected = true;
                            if !json_mode { log::success("Reconnected."); }
                            let _ = client.publish(&collect_hardware_event(installation_id)).await;
                        }
                    }
                    Some(client::StreamMsg::Disconnected) => {
                        connected = false;
                        if !json_mode { log::warn("Connection lost. Reconnecting..."); }
                    }
                    None => {
                        if !json_mode { log::error("Stream closed."); }
                        exit_code = EXIT_BROKER_LOST;
                        break;
                    }
                }
            }
            _ = health_interval.tick() => {
                if connected {
                    // `collect_telemetry_event` does sync `sysinfo`
                    // refresh calls (CPU, memory, disks, components)
                    // which can take tens to hundreds of ms on
                    // Pi-class hosts with many mounts / sensors.
                    // Off-load to `spawn_blocking` so a slow refresh
                    // can't stall Run / Stop / Kill dispatch.
                    let inst_id = installation_id.to_string();
                    match tokio::task::spawn_blocking(move || {
                        collect_telemetry_event(&inst_id)
                    })
                    .await
                    {
                        Ok(event) => { let _ = client.publish(&event).await; }
                        Err(e) => log::warn(&format!("telemetry task panicked: {e}")),
                    }
                }
                // Reap any prior-run teardown tasks that finished while
                // the operator was on the outcome screen. The Run-arm
                // also reaps before parking, so this only matters when
                // the operator never clicks Run again — without it, a
                // hung Python child + a long-running idle station
                // would grow `prior_run_teardowns` until process exit.
                while prior_run_teardowns.try_join_next().is_some() {}
            }
            _ = update_check_interval.tick() => {
                // Background fetch + stage. Run inline (not detached)
                // so a slow download can't overlap the next tick's
                // download — both write the same staged_path and
                // would otherwise corrupt it. The dispatcher freeze
                // during the fetch is acceptable: we only run when
                // the loop is idle anyway.
                if update::auto_update_enabled() {
                    if let Err(e) = update::background_check().await {
                        log::warn(&format!("Background update check failed: {e}"));
                    }
                }
                // Apply staged binary if (a) no run is in flight and
                // (b) the operator isn't still reading the outcome
                // screen of the just-finished run. The grace gives
                // them time to read PASS/FAIL/ABORTED before the
                // process re-execs under their kiosk; after the
                // grace the operator-UI's auto-revalidating index.html
                // (Cache-Control no-cache) lets the SPA reload cleanly.
                if active_run.is_none() && outcome_grace_elapsed(last_run_ended_at) {
                    try_apply_staged_update(
                        &client,
                        installation_id,
                        json_mode,
                        local_ws_server.as_ref(),
                    ).await;
                }
            }
            _ = auth_probe_interval.tick() => {
                // Don't tear down mid-test: kills child processes with a
                // revoked key and orphans the upload. Skip this tick; the
                // select loop will probe again when the next tick arrives
                // (default Burst behavior fires any missed ticks back-to-back
                // once the test ends).
                if active_run.is_some() {
                    continue;
                }
                if auth_probe(creds).await == AuthProbeOutcome::Unauthorized {
                    if !json_mode { log::warn("Logged out: revoked"); }
                    clear_local_credentials();
                    exit_code = EXIT_REVOKED;
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                if !json_mode {
                    eprintln!();
                    log::info("Disconnecting...");
                }
                // Second ^C during cleanup hard-exits. Without this
                // a hung `disconnect().await` (unreachable WS, stalled
                // TLS close) leaves the operator stuck pressing ^C
                // forever — `tokio::signal::ctrl_c()` is one-shot and
                // nothing else listens past this break.
                tokio::spawn(async {
                    let _ = tokio::signal::ctrl_c().await;
                    eprintln!("\nForce exit.");
                    std::process::exit(130);
                });
                // Standard convention for SIGINT.
                exit_code = 130;
                break;
            }
        }
    }

    if let Some(handle) = active_run.take() {
        handle.abort().await;
    }
    // Detach the kiosk explicitly before dropping the Arc. `Server`'s
    // Drop only fires when the *last* Arc clone is gone, but the queue
    // drain task (and any other live tokio task) holds a clone — at
    // ctrl_c we have no shutdown signal for those, so the Server stays
    // alive past this scope and `KioskHandle::Drop` never runs. Result
    // on Windows: CLI exits but the msedge --kiosk window stays open.
    // `detach_kiosk` synchronously takes the KioskHandle out of the
    // Mutex, dropping it right there and killing the browser child
    // tree (taskkill /F /T on Windows, killpg on Unix).
    if let Some(ref server) = local_ws_server {
        server.detach_kiosk().await;
    }
    // Drop the kiosk server BEFORE the network disconnect: stops the
    // WS pump tasks, releases the Arc. Otherwise a hung
    // `disconnect().await` keeps WS state alive and the operator can't
    // tell the CLI is shutting down.
    drop(local_ws_server);
    // Bound the disconnect: centrifuge-client awaits a clean WS close
    // which can stall if the server is unreachable. 2s is generous for
    // a healthy close; past that the connection is gone anyway.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), client.disconnect()).await;
    exit_code
}

/// If a pending-update marker exists from a prior re-exec, publish the matching
/// UpdateApplied (version now matches) or UpdateFailed (version didn't advance)
/// event and clear the marker. Called once per run startup.
async fn publish_pending_update_outcome(client: &client::StreamClient, installation_id: &str) {
    let Ok(db) = crate::commands::db::open() else {
        return;
    };
    let Ok(Some(pending)) = db.get_pending_update() else {
        return;
    };

    let from = pending.from_version.clone();
    let to = pending.to_version.clone();

    // Treat any version >= `to` as success: a fresh install or manual upgrade
    // can land us on something newer than the staged target while the marker
    // is still in the db. Reporting that as a failure is misleading — the
    // staged update did get applied (or superseded by something better).
    let applied = update::version_at_least(&to);
    let event = if applied {
        let landed = update::VERSION;
        if landed == to {
            log::success(&format!("Update applied: v{from} -> v{to}"));
        } else {
            log::success(&format!(
                "Update applied: v{from} -> v{to} (running v{landed})"
            ));
        }
        // Wire `to_version` is the version that actually landed, not the
        // stale marker target. A fresh install can land on something newer
        // than the staged target — sending the marker value would lie to
        // server-side audit/dashboards about which version is now running.
        StationEvent::UpdateApplied {
            installation_id: installation_id.to_string(),
            from_version: from,
            to_version: landed.to_string(),
        }
    } else {
        let err = format!(
            "version after restart is {}, expected {}",
            update::VERSION,
            to,
        );
        log::error(&format!("Update failed: v{from} -> v{to} ({err})"));
        StationEvent::UpdateFailed {
            installation_id: installation_id.to_string(),
            from_version: from,
            to_version: Some(to),
            error: err,
        }
    };
    let _ = client.publish(&event).await;
    let _ = db.clear_pending_update();
}

pub(crate) enum LoopControl {
    Continue,
    /// Exit the station event loop with the given process exit code.
    Exit(i32),
}

/// Apply a staged CLI update if one is on disk and `auto_update` is
/// enabled. Fires from two sites:
///   * inter-run (the natural quiet window between cycles)
///   * idle-tick on the update-check interval (so an idle station —
///     no procedure picked, halted loop, parked outcome screen —
///     still applies updates without waiting for an operator click).
///
/// Both sites pre-check `active_run.is_none()` so the apply only fires
/// in safe windows; this fn does the publish + re-exec itself. On
/// success, `apply_staged` re-execs and never returns. On failure we
/// publish `UpdateFailed` and `process::exit(1)` so the supervisor
/// restarts a known-good build.
///
/// Outcome-screen grace: how long after a run ends we hold off on
/// re-execing so the operator can read PASS/FAIL/ABORTED before the
/// kiosk SPA reconnects to a fresh process. After the grace, we apply
/// regardless — the operator-UI's `Cache-Control: no-cache` index.html
/// reloads cleanly, but the outcome screen is gone.
const OUTCOME_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// True when enough time has passed since the last run ended that
/// re-execing won't disrupt an operator reading the outcome screen.
/// `None` (never had a run, or operator cleared it) is also "safe".
fn outcome_grace_elapsed(last_run_ended_at: Option<std::time::Instant>) -> bool {
    last_run_ended_at.is_none_or(|t| t.elapsed() >= OUTCOME_GRACE)
}
async fn try_apply_staged_update(
    client: &client::StreamClient,
    installation_id: &str,
    json_mode: bool,
    local_ws_server: Option<&std::sync::Arc<crate::local_ws::Server>>,
) {
    if !update::auto_update_enabled() || !update::has_staged() {
        return;
    }
    let from = update::VERSION.to_string();
    let to = update::staged_version().unwrap_or_else(|| "unknown".to_string());
    if !json_mode {
        log::info(&format!("Update: applying v{from} -> v{to}"));
    }
    let _ = client
        .publish(&StationEvent::UpdateStarted {
            installation_id: installation_id.to_string(),
            from_version: from.clone(),
            to_version: to.clone(),
        })
        .await;
    // Kill the kiosk window before reexec. `apply_staged` ends in
    // `execvp`, which wipes the heap in-place — `KioskHandle::drop`
    // never runs, so without this the orphaned browser piles up a
    // new window every update tick. The new process boots and re-
    // attaches a fresh kiosk via `attach_kiosk` at startup.
    if let Some(server) = local_ws_server {
        server.detach_kiosk().await;
    }
    if let Err(e) = update::apply_staged() {
        let _ = client
            .publish(&StationEvent::UpdateFailed {
                installation_id: installation_id.to_string(),
                from_version: from.clone(),
                to_version: Some(to.clone()),
                error: e.to_string(),
            })
            .await;
        log::error(&format!("Update failed: v{from} -> v{to} ({e})"));
        // Don't exit. A failed apply (current_exe missing, FS error,
        // permission denied) shouldn't kill a running station — operator
        // would lose their session. Drop the staged binary and poison
        // the target version so the next background-check tick doesn't
        // re-download and re-fail the same way every interval; a real
        // new release (different `latest`) clears the marker.
        update::discard_staged();
        // Belt-and-suspenders: `apply_staged` clears its own pending marker
        // on the failure paths that wrote one, but if any future path leaks
        // a marker it would surface as a spurious "version after restart is
        // X, expected Y" on the next boot — re-reporting a failure we just
        // published. Clear here too so this `UpdateFailed` is the only
        // record of this attempt.
        update::clear_pending_marker();
        // Skip the "unknown" sentinel: it never matches a server-advertised
        // `latest`, so cache::write would never clear it.
        if to != "unknown" {
            update::mark_poisoned(&to);
        }
    }
}

/// Handle one inbound StationCommand. Returns `Continue` unless the command
/// terminates the session (e.g. server-initiated Logout).
// Single caller (the station event loop); the args are independent
// state pieces it owns. Param bag would just dispatch through one more
// indirection.
#[allow(clippy::too_many_arguments)]
async fn handle_command(
    cmd: StationCommand,
    client: &client::StreamClient,
    creds: &Credentials,
    installation_id: &str,
    json_mode: bool,
    active_run: &mut Option<crate::commands::run::RunHandle>,
    prior_run_teardowns: &mut tokio::task::JoinSet<()>,
    // Memoised last picked procedure. Used to distinguish "operator
    // re-clicked Run on the same procedure mid-flight" (fresh-intent
    // signal — abort and restart) from "duplicate Run command for the
    // procedure already running" (drop).
    last_procedure_id: &mut Option<String>,
    // Reset on `Run` so the update-tick stops withholding apply once
    // the operator has moved past the outcome screen.
    last_run_ended_at: &mut Option<std::time::Instant>,
    staged: &Arc<Mutex<Option<StagedDeployment>>>,
    local_ws_server: Option<&std::sync::Arc<crate::local_ws::Server>>,
) -> LoopControl {
    match cmd {
        StationCommand::Logout {
            reason,
            installation_id: target,
        } => {
            // Ignore logouts addressed to a different installation on this channel.
            if let Some(ref t) = target {
                if t != installation_id {
                    return LoopControl::Continue;
                }
            }
            if !json_mode {
                let reason_str = reason.as_deref().unwrap_or("server-initiated");
                log::warn(&format!("Logged out: {reason_str}"));
            }
            // Signal cancel first (fires the RunComplete(aborted) publish
            // path), then race the wait against Ctrl-C. If Ctrl-C wins the
            // handle Drop kills the task, but the abort event was already
            // broadcast so the dashboard doesn't see a dangling run.
            if let Some(mut handle) = active_run.take() {
                handle.request_cancel();
                tokio::select! {
                    _ = handle.abort() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            clear_local_credentials();
            // Non-zero so launchd/systemd see the revocation rather than a
            // clean shutdown.
            return LoopControl::Exit(EXIT_REVOKED);
        }
        StationCommand::ConfigUpdate { key, value } => {
            // Apply BEFORE writing to DB so a failed apply doesn't
            // leave DB ahead of OS state. `apply_and_event` returns a
            // `ConfigApplied` carrying `success: bool` + optional
            // `error`; we only persist on success so a transient OS
            // failure (permissions, disk full) doesn't poison the
            // memo. Mirrors HR pattern of "act, then record".
            let quiet = active_run.is_some();
            // `apply_and_event` shells out to launchctl / systemctl /
            // KDE Plasma desktop tools for `launch_on_boot` and
            // `desktop_icon` keys. Sync `Command::output` blocks on
            // DBus / launchd handshake — if the supervisor is wedged
            // (DBus down, launchd backed up), the dispatcher freezes
            // until it returns. Wrap in `spawn_blocking` so Run /
            // Stop / Kill stay responsive.
            let key_for_apply = key.clone();
            let value_for_apply = value.clone();
            let installation_id_owned = installation_id.to_string();
            let event = tokio::task::spawn_blocking(move || {
                config::apply_and_event(
                    &key_for_apply,
                    &value_for_apply,
                    &installation_id_owned,
                    quiet,
                )
            })
            .await
            .unwrap_or_else(|_| StationEvent::ConfigApplied {
                installation_id: installation_id.to_string(),
                key: key.clone(),
                value: value.clone(),
                success: false,
                error: Some("apply task panicked".to_string()),
            });
            let applied_ok = matches!(event, StationEvent::ConfigApplied { success: true, .. },);
            if applied_ok {
                if let Ok(db) = crate::commands::db::open() {
                    let _ = db.set_config(&key, &value);
                }
            }
            let _ = client.publish(&event).await;
            if let Some(server) = local_ws_server {
                server.publish_event(event).await;
            }

            // `pull_stage` config is gone; station mode now loops by
            // default. Other config keys (kiosk_ui, launch_on_boot,
            // ...) flow through `apply_and_event` above with no
            // station-loop coupling. Note: `kiosk_ui` flips don't
            // restart the local-WS server — operator must reboot the
            // station for that key to take effect. Documented at
            // `apps/cli/src/commands/config.rs`.
        }
        StationCommand::Pull {} => {
            if active_run.is_some() {
                // Test running: stage in background, swap when test completes
                let creds = creds.clone();
                let staged = staged.clone();
                tokio::spawn(async move {
                    pull_stage::stage_pull_to(&creds, &staged).await;
                });
            } else {
                // Idle: normal blocking pull. Reuse the station's
                // existing StreamClient — see boot-pull comment above
                // for why a second client would break live Pull delivery.
                let publisher = client.clone_for_health();
                crate::commands::pull::run_with(json_mode, Some(&publisher), local_ws_server).await;
                // Pull may have added/removed deployments — push the
                // refreshed list to the kiosk hello frame so a tab
                // opening next sees the new options without a refresh.
                refresh_idle_procedures(local_ws_server).await;
            }
        }
        StationCommand::Run {
            procedure_id,
            reuse_unit,
            operated_by,
        } => {
            // Detach any in-flight run's teardown so the dispatcher
            // returns to its select! tick without blocking on Python
            // teardown / publisher drain (1-3s typical). The
            // operator-UI's `'pending'` RunState seed shows the
            // spinner immediately on click and the late
            // `RunComplete(ABORTED)` from the cancelled run gets
            // dropped by `isStaleForExecution` in the reducer.
            if let Some(mut handle) = active_run.take() {
                if !json_mode {
                    let why = match (&procedure_id, &*last_procedure_id, &reuse_unit) {
                        (Some(p), Some(last), _) if p != last => "operator switched procedure",
                        (_, _, Some(_)) => "operator clicked Run again",
                        _ => "operator requested fresh identify",
                    };
                    log::info(&format!("Aborting in-flight run: {why}"));
                }
                // Escalate immediately: operator already clicked again,
                // they want the prior run gone. Stop fires the graceful
                // path; Kill races the force path. Park the task on the
                // teardown JoinSet so the dispatcher returns to its
                // select! tick instead of waiting on Python teardown +
                // publisher drain.
                handle.request_cancel();
                handle.request_kill();
                // Reap any finished teardowns before parking a new one
                // so a hammering operator can't pile up unbounded
                // tasks. JoinSet's `try_join_next` is non-blocking.
                while prior_run_teardowns.try_join_next().is_some() {}
                if let Some(task) = handle.take_task() {
                    // Wrap in a future that aborts the inner JoinHandle
                    // if the wrapper itself is cancelled (JoinSet::Drop
                    // on station_loop exit). Without this, dropping the
                    // wrapper merely detaches the inner task — Python
                    // child process keeps running past CLI shutdown.
                    prior_run_teardowns.spawn(async move {
                        // RAII: aborts on Drop unless explicitly disarmed.
                        struct AbortOnDrop(Option<tokio::task::AbortHandle>);
                        impl Drop for AbortOnDrop {
                            fn drop(&mut self) {
                                if let Some(h) = self.0.take() {
                                    h.abort();
                                }
                            }
                        }
                        let mut guard = AbortOnDrop(Some(task.abort_handle()));
                        let _ = task.await;
                        // Natural completion — disarm so we don't abort
                        // a JoinHandle that's already finished (no-op
                        // anyway, but semantically cleaner).
                        guard.0 = None;
                    });
                }
            }
            // `procedure_id: None` means "rerun the last procedure".
            // The dashboard's `Run` button preserves the operator's
            // current selection by sending the id explicitly; the
            // None form is reserved for "Run again" affordances that
            // don't know the id (older clients, agent-driven flows).
            // Resolve against `last_procedure_id` so we don't fall
            // through `try_start_run`'s no-op path.
            let resolved_id = procedure_id
                .as_deref()
                .or(last_procedure_id.as_deref())
                .map(String::from);
            if let Some(ref id) = resolved_id {
                *last_procedure_id = Some(id.clone());
            }
            *active_run = try_start_run(
                resolved_id.as_deref(),
                reuse_unit,
                operated_by,
                json_mode,
                creds,
                client,
                local_ws_server,
            )
            .await;
            // Only clear the outcome grace if the new run actually
            // started. If `try_start_run` returns None (deployment
            // missing, kiosk-disabled, etc.) the operator stays on
            // the outcome screen — keep the grace timer armed so the
            // update-tick doesn't yank that screen out from under them.
            if active_run.is_some() {
                *last_run_ended_at = None;
            }
        }
        StationCommand::UiResponse { .. } => {
            if let Some(ref handle) = active_run {
                let _ = handle.ui_response_tx.send(cmd).await;
            }
        }
        StationCommand::Kill { ref reason } => {
            if let Some(handle) = active_run.as_mut() {
                if !json_mode {
                    log::warn(&format!(
                        "Force-kill requested by operator{}",
                        reason
                            .as_ref()
                            .map(|r| format!(" ({r})"))
                            .unwrap_or_default(),
                    ));
                }
                handle.request_kill();
            } else if !json_mode {
                log::warn("Kill command received with no active run; ignoring.");
            }
        }
        StationCommand::Stop { ref reason } => {
            if let Some(handle) = active_run.as_mut() {
                if !json_mode {
                    log::warn(&format!(
                        "Stop requested by operator{}",
                        reason
                            .as_ref()
                            .map(|r| format!(" ({r})"))
                            .unwrap_or_default(),
                    ));
                }
                handle.request_cancel();
            } else if !json_mode {
                log::warn("Stop command received with no active run; ignoring.");
            }
        }
        StationCommand::SkipPhase { .. } | StationCommand::RetryPhase { .. } => {
            // Engine doesn't expose phase-level skip / retry control
            // surfaces yet — protocol shipped first so UIs can wire
            // their buttons. Until the engine learns the verbs we
            // log and drop instead of silently swallowing.
            if !json_mode {
                log::warn("SkipPhase / RetryPhase received but engine doesn't support them yet.");
            }
        }
        StationCommand::QueueRetry { queue_id } => {
            // Operator pressed "Retry now" on a parked queue entry.
            // Run the single-shot upload off-task so the command-loop
            // stays responsive. Bridge a transient broadcast bus to
            // the centrifugo publisher so the queue's progress events
            // reach the same channel as live run events.
            let creds_clone = creds.clone();
            let publisher = client.clone_for_health();
            tokio::spawn(async move {
                let (bus, mut rx) = tokio::sync::broadcast::channel::<StationEvent>(16);
                let bridge = tokio::spawn(async move {
                    while let Ok(ev) = rx.recv().await {
                        let _ = publisher.publish(&ev).await;
                    }
                });
                crate::commands::run::queue::retry_one(&creds_clone, Some(&bus), &queue_id).await;
                drop(bus);
                let _ = bridge.await;
            });
        }
        StationCommand::Exit {} => {
            // Operator hit Exit. Tear down the active run and exit.
            // Supervisor units are configured `Restart=no` (Linux) /
            // no `KeepAlive` (macOS), so a clean exit stays clean —
            // no respawn, no in-flight coordination needed.
            //
            // Escalation ladder: graceful Stop (3s) → Kill (2s) → drop.
            // Process exit reaps any orphaned children via OS, so we
            // don't block forever waiting for a phase stuck on operator
            // input (e.g. identify-unit prompt with no response).
            if let Some(mut handle) = active_run.take() {
                if !json_mode {
                    log::info("Operator requested exit; aborting active run...");
                }
                handle.request_cancel();
                if let Some(task) = handle.take_task() {
                    tokio::pin!(task);
                    if tokio::time::timeout(std::time::Duration::from_secs(3), &mut task)
                        .await
                        .is_err()
                    {
                        if !json_mode {
                            log::warn("Graceful stop timed out; escalating to kill...");
                        }
                        handle.request_kill();
                        if tokio::time::timeout(std::time::Duration::from_secs(2), &mut task)
                            .await
                            .is_err()
                        {
                            if !json_mode {
                                log::warn("Kill timed out; aborting run task.");
                            }
                            task.abort();
                        }
                    }
                }
                // Handle drops here. OS reaps the Python child on
                // process exit below.
                drop(handle);
            }
            if !json_mode {
                log::info("Stopping CLI.");
            }
            return LoopControl::Exit(0);
        }
        StationCommand::QueueDrop { queue_id } => {
            // `drop_one` does blocking `std::fs::remove_file` /
            // `remove_dir` for each attachment; for a run with many
            // attachments that stalls the tokio worker. Bridge
            // through `spawn_blocking` so the runtime stays
            // responsive.
            let publisher = client.clone_for_health();
            tokio::spawn(async move {
                let (bus, mut rx) = tokio::sync::broadcast::channel::<StationEvent>(4);
                let bus_clone = bus.clone();
                let qid = queue_id.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::commands::run::queue::drop_one(Some(&bus_clone), &qid);
                })
                .await;
                drop(bus);
                while let Ok(ev) = rx.recv().await {
                    let _ = publisher.publish(&ev).await;
                }
            });
        }
    }
    LoopControl::Continue
}

/// Wipe the local auth bundle + deployments. Called on server-initiated
/// logout so the next `tofupilot` invocation goes through login again.
fn clear_local_credentials() {
    let _ = crate::commands::auth::credentials::clear();
    if let Ok(db) = crate::commands::db::open() {
        let _ = db.clear_whoami();
    }
    let _ = crate::commands::db::clear_deployments();
}

#[derive(PartialEq, Eq)]
enum AuthProbeOutcome {
    Ok,
    Unauthorized,
    /// Network error or 5xx; don't tear down on transient failures.
    Indeterminate,
}

/// Probe the server to verify our API key is still valid. 401/403 means this
/// installation has been revoked (typically replaced by a newer login).
async fn auth_probe(creds: &Credentials) -> AuthProbeOutcome {
    let base = creds.base();
    let Ok(client) = reqwest::Client::builder()
        .timeout(crate::config::timeouts::AUTH_PROBE)
        .build()
    else {
        return AuthProbeOutcome::Indeterminate;
    };
    match client
        .get(format!("{base}/api/cli/whoami"))
        .bearer(&creds.api_key)
        .send()
        .await
    {
        Ok(res) => {
            let status = res.status();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                AuthProbeOutcome::Unauthorized
            } else if status.is_success() {
                AuthProbeOutcome::Ok
            } else {
                // 404, 5xx, anything else: treat as transient, don't tear down.
                AuthProbeOutcome::Indeterminate
            }
        }
        Err(_) => AuthProbeOutcome::Indeterminate,
    }
}

async fn try_start_run(
    procedure_id: Option<&str>,
    reuse_unit: Option<station_protocol::UnitInfo>,
    operated_by: Option<String>,
    json_mode: bool,
    creds: &Credentials,
    client: &client::StreamClient,
    local_ws_server: Option<&std::sync::Arc<crate::local_ws::Server>>,
) -> Option<crate::commands::run::RunHandle> {
    // No procedure to run on — every caller must pick one explicitly.
    // A None (or empty string, which we coerce here) is a bug at the
    // call site: boot or station-loop restart with no memoised
    // procedure shouldn't reach this fn, and an empty `procedure_id`
    // on the wire would silently resolve to the deployments root.
    let proc_id = match procedure_id {
        Some(id) if !id.is_empty() => id,
        _ => return None,
    };

    let publisher = crate::commands::run::EventPublisher::Managed {
        publish: client.clone_for_health(),
    };

    let proc_dir = match crate::commands::run::resolve_procedure_dir(proc_id) {
        Ok(d) => d,
        Err(code) => {
            if !json_mode {
                log::error(&format!("Failed to resolve deployment dir (exit {code})."));
            }
            return None;
        }
    };

    // Station mode answers UI requests via the dashboard WebSocket, not
    // stdin, so the agent-protocol flags (--ui-timeout, --ui-values) don't
    // apply here. Pass defaults. If station mode ever needs per-run agent
    // options, thread them through from the station-side config.
    // TUI-originated presence uses the station's installation id as
    // its wire identity. "Station" is shown as the display name so
    // dashboard viewers distinguish the TUI operator from themselves
    // even without a per-operator login. Color is a fixed teal so
    // the badge is recognizable across runs without a user record to
    // hash.
    let tui_presence_identity =
        creds
            .installation_id
            .clone()
            .map(|id| crate::commands::run::TuiPresenceIdentity {
                user_id: id,
                display_name: "Station".to_string(),
                color: "#14B8A6".to_string(),
            });
    Some(
        crate::commands::run::start(
            proc_id,
            proc_dir,
            true,
            json_mode,
            Some(creds),
            Some(publisher),
            crate::commands::run::AgentProtoOptions::default(),
            tui_presence_identity,
            // Station mode honours station config, never overrides.
            None,
            None,
            local_ws_server.cloned(),
            reuse_unit,
            operated_by,
            // Station mode runs only manifested deployments; bootstrap
            // is a no-op for those (the path is gated on
            // `manifest_present == false`). Pass `true` to keep the
            // signature uniform with standalone runs.
            true,
            // Deployments carry their entry point in the manifest.
            None,
        )
        .await,
    )
}
