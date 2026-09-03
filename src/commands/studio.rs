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

    // Remember it before anything can fail below: a session that got
    // as far as resolving a root is a project the operator meant to
    // open, whether or not the server then starts.
    crate::commands::studio_recents::record(&root);

    // The credential record resolved here drives every URL part below and
    // is the same identity the run upload later re-resolves — see the
    // pairing-URL comment before `base` for why nothing URL-shaped may
    // come from anywhere else.
    let creds = crate::commands::auth::credentials::load();

    // Displayed identity only, never URL parts. Read from the slot
    // matching the credential record so a row written by the other login
    // (a station on a dev laptop, a user on a bench host) can't leak into
    // this session's hello frame.
    let whoami = crate::commands::db::cached_whoami(
        creds
            .as_ref()
            .map(|c| c.whoami_slot())
            .unwrap_or(crate::commands::db::WhoamiSlot::User),
    );
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
    // Tolerant name-only read, same mechanism Studio discovery uses: a
    // procedure mid-edit still advertises its name instead of falling
    // back to the folder because one phase failed to parse.
    let proc_name = crate::commands::run::engine::find_procedure_yaml(&root)
        .and_then(|p| execution_engine::procedure::loader::read_procedure_name(&p))
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

    // Bridge plug debug-session events (`plug_status` / `plug_log`
    // with no execution id) onto the loopback WS, same shape as the
    // upload bus in `run_dispatcher`: the RPC layer's sink sends here,
    // this pump publishes on the page's event stream.
    let (plug_debug_tx, mut plug_debug_rx) =
        tokio::sync::mpsc::unbounded_channel::<station_protocol::StationEvent>();
    server.set_plug_debug_event_sender(plug_debug_tx).await;
    let plug_debug_server = server.clone();
    let plug_debug_bridge = tokio::spawn(async move {
        while let Some(ev) = plug_debug_rx.recv().await {
            plug_debug_server.publish_event(ev).await;
        }
    });
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
    //
    // Base AND org slug come from the same credential record — the one
    // `handle_upload_run` re-resolves — so the URL is consistent by
    // construction with where the run actually uploads. They used to come
    // from different identity sources (user-first credentials vs the
    // last-writer whoami cache), which on a machine holding both a user
    // and a station login printed a hybrid URL pointing at no real org
    // (TP-1040).
    let base = creds
        .as_ref()
        .map(|c| c.base().to_string())
        .unwrap_or_else(|| crate::commands::auth::config::DEFAULT_BASE_URL.to_string());
    let org_slug = creds.as_ref().map(|c| c.organization_slug.clone());

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

    // Folder-dialog host for `pick_project`. This loop is the reason
    // the dialog works at all: `#[tokio::main]`'s `block_on` polls
    // run_cmd on the process's MAIN thread, and macOS only lets a
    // non-windowed process open an AppKit panel there (rfd panics on
    // any other thread). So the RPC handler — on a tokio worker —
    // posts a job, and this loop shows the dialog where it is legal.
    //
    // `run_cmd` must therefore stay directly awaited from `main`,
    // never `tokio::spawn`ed — spawning would move this loop to a
    // worker thread and the first pick would panic the daemon.
    let (dialog_tx, mut dialog_rx) =
        tokio::sync::mpsc::channel::<crate::local_ws::StudioDialogJob>(1);
    server.set_studio_dialog_host(dialog_tx).await;

    // Park until Ctrl-C, serving dialog jobs meanwhile. The server
    // lives on this task's stack; drop on return tears down the
    // listener and any kiosk attachments. The ctrl_c future is created
    // ONCE and pinned: registered before the loop, it keeps its
    // readiness through a blocking dialog, so a ^C pressed while a
    // panel is open lands right after the panel closes instead of
    // being lost.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Once a panel has been shown, this process is a UI app in the
    // window server's books, and a UI app that stops servicing its
    // event queue gets the unresponsive treatment: ~5s after the pick,
    // the cursor beachballs over whatever window residue the panel
    // teardown left. The post-pick drain below is bounded (~200ms) and
    // was measured LOSING that race on macOS 26 (2026-08-17): the open
    // panel lives in a separate XPC service, and its teardown handshake
    // can outlast the drain. So after the first dialog, keep servicing
    // AppKit for the life of the daemon — a 200ms tick is far inside
    // the multi-second unresponsiveness threshold, and each tick
    // returns immediately when the queue is empty.
    let mut appkit_tick = tokio::time::interval(std::time::Duration::from_millis(200));
    appkit_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut appkit_in_use = false;
    let exit_code = loop {
        tokio::select! {
            sig = &mut ctrl_c => {
                break match sig {
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
            }
            Some(reply) = dialog_rx.recv() => {
                // Say it in the terminal too. The page can only show
                // "waiting"; this is the one place that can name where
                // the dialog is, and it is the fallback if the panel
                // still ends up behind another window.
                crate::log::info(
                    "studio: folder dialog open — pick a folder (or cancel) to continue",
                );
                bring_dialog_to_front();
                // Deliberately blocks this (main) thread while the
                // panel is open: the panel is modal, one at a time by
                // the gate the job itself carries (released in its
                // Drop, right after this arm ends), and the server
                // keeps serving from its worker threads throughout.
                let picked = rfd::FileDialog::new()
                    .set_title("Open a project — TofuPilot Studio")
                    .pick_folder();
                // The panel is NOT gone when pick_folder returns: its
                // fade-out needs run-loop turns that never come once we
                // re-enter select!, leaving an invisible (alpha 0) but
                // still hit-testable window frozen at the panel's spot.
                // Measured via CGWindowList on macOS 26, 2026-08-14.
                // Drain the bulk of it here; the appkit_tick arm below
                // owns whatever outlasts the drain (see its comment).
                drain_dialog_teardown();
                appkit_in_use = true;
                // Receiver gone = the page dropped the request; the
                // human's click has nowhere to land, nothing to do.
                let _ = reply.send(picked);
            }
            _ = appkit_tick.tick(), if cfg!(target_os = "macos") && appkit_in_use => {
                pump_appkit();
            }
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
    // Same exit-skips-Drop reason as the run teardown above: debug
    // sessions hold Python children with instrument connections, and
    // their kill_on_drop never fires through `std::process::exit`.
    server.teardown_plug_debug().await;
    plug_debug_bridge.abort();
    drop(server);
    exit_code
}

/// Fixed procedure id advertised in the hello frame for the studio
/// project. The dispatcher runs the project root regardless of the id
/// a Run command carries.
const STUDIO_PROCEDURE_ID: &str = "studio-local";

/// Put the folder dialog in front of whatever the human is looking at.
///
/// rfd raises a `Prohibited` process to `Accessory` and stops there
/// (its `backend/macos/utils/policy_manager.rs`), and `Accessory` is by
/// definition a process that does not become the active app — so the
/// panel can open BEHIND the browser (never observed in practice, but
/// the policy mechanics allow it), leaving the page to wait on a dialog
/// nobody can see until its two-minute give-up. Raising the policy
/// ourselves and activating fixes the cause.
///
/// Best effort by contract: AppKit documents that `activate` may not
/// activate at all. The terminal line above is the backstop.
#[cfg(target_os = "macos")]
fn bring_dialog_to_front() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    // The dialog host loop runs on the process main thread (see the
    // select! in `run_cmd`, and the note there on never spawning it),
    // which is exactly what this marker asserts. `None` would mean that
    // invariant broke — do nothing rather than risk an AppKit call off
    // the main thread.
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // Accessory, not Regular: enough to show and focus a window,
    // without putting a Dock icon on a CLI daemon. rfd restores the
    // policy it found when its own guard drops, so setting it here is
    // not undone under us — its guard saw this value already.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.activate();
    // Do NOT add activateIgnoringOtherApps here: tried 2026-08-14, and
    // on macOS 26 the double activation left the panel visible but
    // INERT (keyboard focus stayed with the browser until the operator
    // clicked around). rfd's own policy dance plus plain activate() is
    // the least-bad combination we have measured.
}

#[cfg(not(target_os = "macos"))]
fn bring_dialog_to_front() {
    // Windows shows the common dialog in front already, and on Linux
    // the xdg-portal backend hands the request to the desktop portal,
    // which owns placement. Nothing to do.
}

/// Finish tearing down the closed rfd panel. Order every window of this
/// process out immediately (skips the fade the run loop would never
/// finish), then pump the run loop a few turns so the window-server
/// removal actually commits before the main thread stops serving AppKit.
#[cfg(target_os = "macos")]
fn drain_dialog_teardown() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};

    // Same invariant as bring_dialog_to_front: this runs on the main
    // thread or not at all.
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    for window in app.windows().iter() {
        window.orderOut(None);
    }
    let run_loop = NSRunLoop::currentRunLoop();
    for _ in 0..10 {
        let limit = NSDate::dateWithTimeIntervalSinceNow(0.02);
        let _ = unsafe { run_loop.runMode_beforeDate(NSDefaultRunLoopMode, &limit) };
    }
}

#[cfg(not(target_os = "macos"))]
fn drain_dialog_teardown() {
    // The other platforms' dialogs are torn down by their owners
    // (common dialog on Windows, the portal on Linux); nothing lingers.
}

/// Service whatever AppKit has queued, without parking. Called on a
/// timer from the dialog-host loop once a panel has been shown: the
/// open-panel XPC service tears itself down asynchronously, and any of
/// its callbacks that land after `drain_dialog_teardown`'s bounded pump
/// would otherwise sit unserviced until macOS flags the process
/// unresponsive (the beachball-over-a-dead-zone the drain alone did not
/// fully fix — observed again 2026-08-17 with the drain in place).
/// A zero-deadline turn returns as soon as the queue is empty, so an
/// idle tick costs essentially nothing.
#[cfg(target_os = "macos")]
fn pump_appkit() {
    use objc2::MainThreadMarker;
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};

    // Same invariant as the other two AppKit helpers: main thread or
    // nothing.
    if MainThreadMarker::new().is_none() {
        return;
    }
    let run_loop = NSRunLoop::currentRunLoop();
    for _ in 0..4 {
        let limit = NSDate::dateWithTimeIntervalSinceNow(0.0);
        let _ = unsafe { run_loop.runMode_beforeDate(NSDefaultRunLoopMode, &limit) };
    }
}

#[cfg(not(target_os = "macos"))]
fn pump_appkit() {
    // Unreachable: the tick arm is gated on target_os = "macos", and
    // the other platforms have no run loop of ours to service.
}

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
                // Pin the project NOW, not at `RunStarted`: venv
                // bootstrap and the identify prompt run before that
                // event, and a project switch inside that window used
                // to be allowed — reloading the page onto project B
                // while the engine boots project A. The run's terminal
                // events release the flag as before.
                server.set_studio_run_active(true);
                // Debug sessions hold the same instruments the run's
                // plugs are about to open: stop them all first.
                server.teardown_plug_debug().await;
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
                // The procedure the UI is on, not the one this task was
                // launched with: a multi-procedure project runs the
                // selected subdirectory. Read now rather than captured,
                // so a switch between two runs is honored. Falls back
                // to the launch root if the surface went away.
                let run_dir = server
                    .studio_run_dir()
                    .await
                    .unwrap_or_else(|| root.clone());
                crate::log::info(&format!("studio: running {}", run_dir.display()));
                active_run = Some(
                    crate::commands::run::start(
                        STUDIO_PROCEDURE_ID,
                        run_dir,
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
    let Some((_, queued_runs)) = taken else {
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
    // `procedure_version` came from the procedure's own file and stays
    // as-is.
    for mut queued in queued_runs {
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
}

/// Which project a bare `tofupilot studio` opens, in priority order:
/// an explicit `PATH` always wins, then the current directory when it
/// looks like a project, then the most recent root that still exists.
///
/// The cwd only wins when it *is* a project: running the command from
/// a home directory used to serve that home directory, which is both
/// useless and a wide surface. Falling through to the last project is
/// the VSCode behaviour, and the reason the recents list exists.
fn resolve_root(path: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(explicit) = path {
        return canonical_dir(explicit);
    }

    let cwd = std::env::current_dir().map_err(|e| format!("cannot resolve cwd: {e}"))?;
    resolve_without_path(&cwd, &crate::commands::studio_recents::existing())
}

/// The fallback decision, with both ambient inputs passed in: the
/// process cwd cannot be moved from a test without racing every other
/// test in the binary, and the recents list must not be read from the
/// developer's own `~/.tofupilot`.
///
/// `recents` is expected pre-filtered to roots that still exist
/// (`studio_recents::existing`) — only the head is considered.
fn resolve_without_path(cwd: &std::path::Path, recents: &[PathBuf]) -> Result<PathBuf, String> {
    if crate::commands::run::engine::find_procedure_yaml(cwd).is_some() {
        return canonical_dir(cwd.to_path_buf());
    }

    match recents.first() {
        Some(recent) => {
            crate::log::info(&format!(
                "no procedure here; reopening {}",
                recent.display()
            ));
            canonical_dir(recent.clone())
        }
        // Serving the cwd anyway would be surprising; naming the two
        // ways out is more useful than a bare "not found".
        None => Err(format!(
            "no procedure.yaml in {} and no previous project to reopen.\n  \
             Run `tofupilot studio <path>`, or start one from a project directory.",
            cwd.display()
        )),
    }
}

fn canonical_dir(candidate: PathBuf) -> Result<PathBuf, String> {
    let canon = candidate
        .canonicalize()
        .map_err(|e| format!("project directory {} not found: {e}", candidate.display()))?;
    if !canon.is_dir() {
        return Err(format!("{} is not a directory", canon.display()));
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory holding a procedure, i.e. something `resolve_root`
    /// is allowed to open.
    fn project(parent: &std::path::Path, name: &str) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("procedure.yaml"), "name: Demo\nversion: 1.0.0\n").unwrap();
        dir.canonicalize().unwrap()
    }

    /// A directory that is not a project — a home directory stands in
    /// for it.
    fn plain(parent: &std::path::Path, name: &str) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// The explicit argument is answered before either ambient input is
    /// read, so this one can go through the real entry point.
    #[test]
    fn an_explicit_path_is_opened_whatever_it_contains() {
        let tmp = tempfile::tempdir().unwrap();
        // Deliberately not a project: `tofupilot studio <path>` has
        // always served what it was pointed at, and the new fallback
        // order must not start second-guessing an explicit argument.
        let target = plain(tmp.path(), "somewhere");
        assert_eq!(resolve_root(Some(target.clone())).unwrap(), target);
    }

    #[test]
    fn an_explicit_path_that_does_not_exist_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-dir");
        assert!(resolve_root(Some(missing)).is_err());
    }

    #[test]
    fn a_project_cwd_wins_over_the_recents() {
        let tmp = tempfile::tempdir().unwrap();
        let here = project(tmp.path(), "here");
        let last = project(tmp.path(), "last");
        assert_eq!(resolve_without_path(&here, &[last]).unwrap(), here);
    }

    /// The behaviour change this fallback exists for: launched from a
    /// directory that is not a project, the session used to serve that
    /// directory. It now reopens the last project instead.
    #[test]
    fn a_non_project_cwd_reopens_the_most_recent_project() {
        let tmp = tempfile::tempdir().unwrap();
        let home = plain(tmp.path(), "home");
        let recent = project(tmp.path(), "recent");
        let older = project(tmp.path(), "older");
        assert_eq!(
            resolve_without_path(&home, &[recent.clone(), older]).unwrap(),
            recent,
            "the head of the recents list is the last project opened",
        );
    }

    #[test]
    fn a_non_project_cwd_with_no_history_refuses_instead_of_serving_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = plain(tmp.path(), "home");
        let err = resolve_without_path(&home, &[]).expect_err("nothing to open");
        assert!(
            err.contains(&home.display().to_string()),
            "the message must name the directory it looked in: {err}",
        );
        assert!(
            err.contains("tofupilot studio <path>"),
            "and the way out of it: {err}",
        );
    }
}
