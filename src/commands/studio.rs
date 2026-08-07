//! `tofupilot studio` — serve a local project to the dashboard Studio.
//!
//! Beyond the file RPC surface, the session hosts a run dispatcher:
//! `StationCommand::Run` from the studio page executes the project
//! through the same engine path as `tofupilot run`, with the browser
//! pane as the only UI (no TUI, no kiosk window, no upload).
//!
//! Binds the loopback server (same listener the kiosk uses), enables
//! the Studio RPC surface scoped to the project directory, and prints
//! the dashboard URL carrying the per-process session token in the URL
//! fragment (fragments never leave the browser, so the token is not
//! sent to the dashboard server). The dashboard Studio page then talks
//! directly to `127.0.0.1:<port>` — file RPC over HTTP, live events
//! over the existing `/ws` stream. Ctrl-C stops the session; the token
//! dies with the process.

use std::path::PathBuf;

pub async fn run_cmd(path: Option<PathBuf>, no_open: bool) -> i32 {
    let root = match resolve_root(path) {
        Ok(p) => p,
        Err(msg) => {
            crate::log::error(&msg);
            return 1;
        }
    };

    let whoami = crate::commands::db::open()
        .ok()
        .and_then(|db| db.get_whoami().ok().flatten());
    let identity = whoami
        .as_ref()
        .map(crate::local_ws::HelloIdentity::from)
        .unwrap_or_default();

    let project_name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();

    // `HostMode::Station` refuses a root bind: a root studio session
    // would hand root-privileged file writes to whoever holds the
    // token. The legitimate flows all run as a regular user; keep the
    // refusal. (It also matches the hello-frame mode the SPA expects
    // until a run attaches.)
    let server = match crate::local_ws::Server::start(
        format!("studio-{project_name}"),
        project_name.clone(),
        identity,
        crate::local_ws::HostMode::Station,
        // Ephemeral port: coexists with a running station daemon /
        // kiosk on the stable port, and allows several studio
        // sessions side by side. The pairing URL carries the port.
        crate::local_ws::PortChoice::Ephemeral,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            crate::log::error(&format!("could not start the studio server: {e}"));
            return 1;
        }
    };
    if let Err(e) = server.enable_studio(root.clone()).await {
        crate::log::error(&format!("could not enable the studio surface: {e}"));
        return 1;
    }
    let server = std::sync::Arc::new(server);

    // Advertise the project's procedure in the hello frame so the
    // Studio run pane's picker has something to run. The id is a
    // fixed local marker: the dispatcher always runs the project
    // root, whatever id the command carries.
    let proc_name = crate::commands::run::engine::find_procedure_yaml(&root)
        .and_then(|p| {
            execution_engine::procedure::loader::load_procedure_definition(&p)
                .ok()
                .map(|def| def.name)
        })
        .unwrap_or_else(|| project_name.clone());
    server
        .set_procedures(vec![crate::local_ws::ProcedureRef {
            id: STUDIO_PROCEDURE_ID.to_string(),
            name: proc_name,
        }])
        .await;

    // Run dispatcher: the studio page sends StationCommand::Run over
    // the loopback WS; without a sink those frames are dropped.
    // Stop/Kill/UiResponse route through the run attachment installed
    // by `run::start` → `attach_run`, so only Run (and ignorable
    // station-level commands) land here.
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<station_protocol::StationCommand>(16);
    server.set_station_cmd_sink(cmd_tx).await;
    // Explicit shutdown signal: the dispatcher holds an Arc of the
    // server, and the server holds the cmd sink, so neither channel
    // closes on its own at Ctrl-C — without this the only exit would
    // be an abort, which skips the run-teardown tail below.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let mut dispatcher = tokio::spawn(run_dispatcher(
        server.clone(),
        root.clone(),
        cmd_rx,
        shutdown_rx,
    ));

    let port = server.port();
    let token = server.session_token().to_string();

    // Dashboard URL when we know the org; otherwise a bare local
    // pairing hint. Token rides the fragment on purpose — it is
    // consumed by the Studio page's JS and never reaches any server.
    let base = crate::commands::auth::credentials::load()
        .map(|c| c.base().trim_end_matches('/').to_string())
        .unwrap_or_else(|| crate::commands::auth::config::DEFAULT_BASE_URL.to_string());
    let org_slug = whoami.as_ref().map(|w| w.organization_slug.clone());

    eprintln!();
    eprintln!("  Studio session for {}", root.display());
    match &org_slug {
        Some(slug) => {
            let url = format!("{base}/{slug}/studio#port={port}&token={token}");
            eprintln!("  Open: {url}");
            // Linux: never auto-open. xdg-open (and often the browser
            // itself) receives the URL in argv, and /proc/<pid>/cmdline
            // is world-readable — on a shared bench host any local user
            // could harvest the session token during the launch window.
            // The printed URL stays terminal-private; the operator
            // clicks or pastes it. macOS (LaunchServices) and Windows
            // (per-user cmdline visibility) don't leak argv to other
            // users the same way.
            #[cfg(target_os = "linux")]
            let no_open = {
                if !no_open {
                    eprintln!(
                        "  (auto-open is disabled on Linux so the token stays out of process argv)"
                    );
                }
                true
            };
            if !no_open {
                if let Err(e) = open::that_detached(&url) {
                    crate::log::warn(&format!("couldn't open browser ({e}); open the URL above"));
                }
            }
        }
        None => {
            eprintln!("  Not logged in. Dashboard pairing needs `tofupilot login` first.");
            eprintln!("  Local pairing: port={port} token={token}");
        }
    }
    eprintln!("  Press Ctrl-C to stop.");
    eprintln!();

    // Park until Ctrl-C. The server lives on this task's stack; drop
    // on return tears down the listener and any kiosk attachments.
    let exit_code = match tokio::signal::ctrl_c().await {
        Ok(()) => {
            eprintln!();
            crate::log::info("studio session ended");
            0
        }
        Err(e) => {
            crate::log::error(&format!("signal handler failed: {e}"));
            1
        }
    };

    // Orderly dispatcher shutdown BEFORE returning: main.rs turns this
    // return value into `std::process::exit`, which skips every Drop
    // handler — the engine child's kill_on_drop never fires, so the
    // cancel must reach the active run while we're still running. The
    // dispatcher's shutdown tail kills the active run and drains the
    // parked teardowns, each bounded (worst case ~15s), so this await
    // is bounded too; the outer timeout is a backstop against a wedged
    // dispatcher.
    let _ = shutdown_tx.send(());
    if tokio::time::timeout(std::time::Duration::from_secs(20), &mut dispatcher)
        .await
        .is_err()
    {
        crate::log::warn("studio: dispatcher shutdown timed out; aborting it");
        dispatcher.abort();
    }
    drop(server);
    exit_code
}

/// Fixed procedure id advertised in the hello frame for the studio
/// project. The dispatcher runs the project root regardless of the id
/// a Run command carries.
const STUDIO_PROCEDURE_ID: &str = "studio-local";

/// Receive station-level commands from the loopback WS and run the
/// project. Mirrors the station daemon's Run handling, reduced to one
/// local procedure: a new Run aborts any in-flight run (the operator
/// clicked Run again), failures are logged and never end the session.
///
/// `shutdown_rx` firing (Ctrl-C in `run_cmd`) breaks the loop into the
/// teardown tail: kill the active run, bound-await its task, drain the
/// parked teardowns — the studio counterpart of the station loop's
/// post-loop exit path.
async fn run_dispatcher(
    server: std::sync::Arc<crate::local_ws::Server>,
    root: std::path::PathBuf,
    mut cmd_rx: tokio::sync::mpsc::Receiver<station_protocol::StationCommand>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let mut active_run: Option<crate::commands::run::RunHandle> = None;
    let mut teardowns = tokio::task::JoinSet::new();

    // Most recent finished run, parked by `run_test` (upload is off for
    // studio runs) so an explicit UploadRun can publish it later.
    let retained: crate::commands::run::RetainedRun = Default::default();

    // Bridge the upload queue's progress events (`RunUploadQueued` /
    // `Started` / `Succeeded` / `Failed`) onto the loopback WS so the
    // studio page can render them. `spawn_upload` speaks broadcast; the
    // page listens on the server's event stream.
    let (upload_bus, mut upload_rx) =
        tokio::sync::broadcast::channel::<station_protocol::StationEvent>(64);
    let bridge_server = server.clone();
    let upload_bridge = tokio::spawn(async move {
        while let Ok(ev) = upload_rx.recv().await {
            bridge_server.publish_event(ev).await;
        }
    });

    loop {
        let cmd = tokio::select! {
            _ = &mut shutdown_rx => break,
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => cmd,
                None => break,
            },
        };
        match cmd {
            station_protocol::StationCommand::Run {
                reuse_unit,
                operated_by,
                only_phase,
                ..
            } => {
                // Always log the dispatch: "I clicked and nothing
                // happened" must be distinguishable between the frame
                // never arriving and the run failing after start.
                crate::log::info(&format!(
                    "studio: run requested ({})",
                    only_phase
                        .as_deref()
                        .map(|p| format!("phase '{p}' + deps/setup/teardown"))
                        .unwrap_or_else(|| "full procedure".to_string())
                ));
                if let Some(mut handle) = active_run.take() {
                    crate::log::info("studio: aborting in-flight run (Run again)");
                    handle.request_cancel();
                    handle.request_kill();
                    if let Some(task) = handle.take_task() {
                        // Shared parking: the wrapper aborts the inner
                        // run task if the wrapper itself is cancelled
                        // (JoinSet::Drop, drain deadline) — a bare
                        // spawn would detach the Python child on drop.
                        crate::commands::run::teardown::park_prior_run(&mut teardowns, task);
                    }
                }
                // The prior run's engine may still be draining teardown
                // phases against the project's instrument ports — wait
                // (bounded) before the replacement engine spawns, or two
                // processes can briefly drive the same serial/VISA ports.
                crate::commands::run::teardown::drain_prior_teardowns(&mut teardowns, 5, false)
                    .await;
                active_run = Some(
                    crate::commands::run::start(
                        STUDIO_PROCEDURE_ID,
                        root.clone(),
                        // No upload: studio runs are local iterations.
                        false,
                        false,
                        None,
                        None,
                        crate::commands::run::RunOptions {
                            only_phase,
                            retain_queued_run: Some(retained.clone()),
                            ..Default::default()
                        },
                        None,
                        // No TUI; kiosk_override=true is what routes
                        // events onto the loopback WS (attach_run is
                        // gated on it). No browser window opens: that
                        // only happens when run::start binds its own
                        // inline server, and we pass ours.
                        Some(false),
                        Some(true),
                        Some(server.clone()),
                        reuse_unit,
                        operated_by,
                        // Local project driven from the browser: provision
                        // the venv without a terminal prompt — the operator
                        // is not watching the terminal, and a tty Y/n would
                        // park the run invisibly.
                        crate::commands::run::bootstrap::BootstrapPolicy::Auto,
                        None,
                    )
                    .await,
                );
            }
            station_protocol::StationCommand::UploadRun {
                execution_id,
                procedure_id,
            } => {
                handle_upload_run(&server, &retained, &upload_bus, execution_id, procedure_id)
                    .await;
            }
            other => {
                crate::log::info(&format!(
                    "studio: ignoring station command {:?} (not supported in studio sessions)",
                    std::mem::discriminant(&other)
                ));
            }
        }
    }

    // Teardown tail. The session is exiting via `std::process::exit`
    // (which skips Drop handlers, so kill_on_drop on the engine child
    // never fires): the cancel must reach the run here or the Python
    // engine + plug processes are orphaned holding their instrument
    // connections. Escalate immediately — the operator hit Ctrl-C —
    // and stay bounded, mirroring the station Exit path's bounds.
    if let Some(mut handle) = active_run.take() {
        crate::log::info("studio: stopping active run...");
        handle.request_cancel();
        handle.request_kill();
        if let Some(task) = handle.take_task() {
            tokio::pin!(task);
            if tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
                .await
                .is_err()
            {
                crate::log::warn("studio: active run didn't stop in 5s; aborting its task");
                task.abort();
            }
        }
    }
    crate::commands::run::teardown::drain_prior_teardowns(&mut teardowns, 5, false).await;

    upload_bridge.abort();
}

/// Publish the retained Studio run to the procedure the user picked.
///
/// Pre-queue failures (not logged in, nothing retained, stale execution
/// id) surface as a synthetic `RunUploadFailed` on the WS — there is no
/// real queue entry yet at that point, so the queue_id is derived from
/// the execution id. Once `spawn_upload` takes over, the standard
/// queue events flow through `upload_bus`.
async fn handle_upload_run(
    server: &crate::local_ws::Server,
    retained: &crate::commands::run::RetainedRun,
    upload_bus: &tokio::sync::broadcast::Sender<station_protocol::StationEvent>,
    execution_id: String,
    procedure_id: String,
) {
    let fail = |error: String| station_protocol::StationEvent::RunUploadFailed {
        queue_id: format!("studio_{execution_id}"),
        attempt: 0,
        kind: "unknown".to_string(),
        status: None,
        error,
        next_retry_at: None,
    };

    // Fresh load: the studio session only borrows credentials at startup
    // to build the dashboard URL and never keeps them, and `run::start`
    // is called with no creds.
    let Some(creds) = crate::commands::auth::credentials::load() else {
        crate::log::warn("studio: cannot upload run — not logged in");
        server
            .publish_event(fail(
                "Not logged in on this machine. Run `tofupilot login`, then retry the upload."
                    .to_string(),
            ))
            .await;
        return;
    };

    let taken = {
        let mut slot = retained.lock().await;
        let matches = slot.as_ref().is_some_and(|(id, _)| *id == execution_id);
        if matches {
            slot.take()
        } else {
            None
        }
    };
    let Some((_, mut queued)) = taken else {
        crate::log::warn(&format!(
            "studio: no retained run for execution {execution_id}"
        ));
        server
            .publish_event(fail(
                "This run is no longer available to upload. Run the procedure again, then upload."
                    .to_string(),
            ))
            .await;
        return;
    };

    // Rewrite the local marker id ("studio-local") with the procedure
    // the user picked. Safe after the fact: `deployment_id` is the only
    // other procedure-derived field and a studio run has none, while
    // `procedure_version` came from the procedure directory and stays
    // the local YAML's version.
    queued.request.procedure_id = procedure_id.clone();

    crate::commands::run::spawn_upload(
        &creds,
        &procedure_id,
        queued,
        false,
        None,
        Some(upload_bus.clone()),
    );
}

fn resolve_root(path: Option<PathBuf>) -> Result<PathBuf, String> {
    let candidate = match path {
        Some(p) => p,
        None => std::env::current_dir().map_err(|e| format!("cannot resolve cwd: {e}"))?,
    };
    let canon = candidate
        .canonicalize()
        .map_err(|e| format!("project directory {} not found: {e}", candidate.display()))?;
    if !canon.is_dir() {
        return Err(format!("{} is not a directory", canon.display()));
    }
    Ok(canon)
}
