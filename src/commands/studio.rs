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
    let dispatcher = tokio::spawn(run_dispatcher(server.clone(), root.clone(), cmd_rx));

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
                    eprintln!("  (auto-open is disabled on Linux so the token stays out of process argv)");
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
    if let Err(e) = tokio::signal::ctrl_c().await {
        crate::log::error(&format!("signal handler failed: {e}"));
        return 1;
    }
    eprintln!();
    crate::log::info("studio session ended");
    dispatcher.abort();
    drop(server);
    0
}

/// Fixed procedure id advertised in the hello frame for the studio
/// project. The dispatcher runs the project root regardless of the id
/// a Run command carries.
const STUDIO_PROCEDURE_ID: &str = "studio-local";

/// Receive station-level commands from the loopback WS and run the
/// project. Mirrors the station daemon's Run handling, reduced to one
/// local procedure: a new Run aborts any in-flight run (the operator
/// clicked Run again), failures are logged and never end the session.
async fn run_dispatcher(
    server: std::sync::Arc<crate::local_ws::Server>,
    root: std::path::PathBuf,
    mut cmd_rx: tokio::sync::mpsc::Receiver<station_protocol::StationCommand>,
) {
    let mut active_run: Option<crate::commands::run::RunHandle> = None;
    let mut teardowns = tokio::task::JoinSet::new();
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            station_protocol::StationCommand::Run {
                reuse_unit,
                operated_by,
                ..
            } => {
                if let Some(mut handle) = active_run.take() {
                    crate::log::info("studio: aborting in-flight run (Run again)");
                    handle.request_cancel();
                    handle.request_kill();
                    while teardowns.try_join_next().is_some() {}
                    if let Some(task) = handle.take_task() {
                        teardowns.spawn(async move {
                            let _ = task.await;
                        });
                    }
                }
                active_run = Some(
                    crate::commands::run::start(
                        STUDIO_PROCEDURE_ID,
                        root.clone(),
                        // No upload: studio runs are local iterations.
                        false,
                        false,
                        None,
                        None,
                        crate::commands::run::RunOptions::default(),
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
                        // Local project: auto-provision the venv like a
                        // standalone `tofupilot run`.
                        true,
                        None,
                    )
                    .await,
                );
            }
            other => {
                crate::log::info(&format!(
                    "studio: ignoring station command {:?} (not supported in studio sessions)",
                    std::mem::discriminant(&other)
                ));
            }
        }
    }
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
