//! End-to-end tests for the studio surface: real `Server::start` on an
//! ephemeral loopback port, real HTTP/WS clients against it. Covers
//! the auth boundary (bearer token on RPC, token-vs-Origin on the WS
//! upgrade) and the full RPC matrix the dashboard Studio page uses.

use super::*;

/// Boot a server with the studio surface enabled on `root`.
/// Ephemeral port keeps parallel tests and any developer daemon on
/// 7321 out of each other's way.
async fn studio_server(root: &std::path::Path) -> Server {
    let server = Server::start(
        "studio-test".into(),
        "Studio Test".into(),
        HelloIdentity::default(),
        HostMode::Local,
        PortChoice::Ephemeral,
    )
    .await
    .expect("bind ephemeral loopback server");
    server
        .enable_studio_with_recents(root.to_path_buf(), test_recents(root))
        .await
        .expect("enable studio");
    server
}

/// Recents file for a test session. Inside the project's own tempdir so
/// it is unique per test and disappears with it, and dotted so the file
/// RPC hides it from listings the way it hides `.env`. The default
/// location is the developer's real `~/.tofupilot`, which the tests
/// must neither read (it would seed `granted` with their projects) nor
/// write (a project switch would reorder their list).
fn test_recents(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".tofupilot").join("studio-recents.json")
}

/// A client for the loopback server. reqwest is built on
/// `rustls-no-provider`, so building one before a provider is installed
/// panics — these tests construct clients directly and so must install it
/// themselves, exactly as `http.rs` does at its own construction sites.
fn test_client() -> reqwest::Client {
    crate::http::ensure_crypto_provider();
    reqwest::Client::new()
}

async fn rpc(
    server: &Server,
    token: Option<&str>,
    body: &str,
) -> (reqwest::StatusCode, serde_json::Value) {
    let client = test_client();
    let mut req = client
        .post(format!("http://127.0.0.1:{}/studio/rpc", server.port()))
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.expect("rpc request");
    let status = res.status();
    let value = res
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// Minimal WS upgrade handshake returning the HTTP status line code.
/// Raw TcpStream instead of a WS client dep — only the accept/reject
/// decision is under test, not the frame protocol (the kiosk SPA and
/// Studio page cover that in real use).
async fn ws_upgrade_status(port: u16, path_qs: &str, origin: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let req = format!(
        "GET {path_qs} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {origin}\r\n\
         Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = [0u8; 128];
    let n = stream.read(&mut buf).await.expect("read");
    let head = String::from_utf8_lossy(&buf[..n]);
    head.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn seed_project(root: &std::path::Path) {
    std::fs::create_dir(root.join("phases")).unwrap();
    std::fs::write(
        root.join("procedure.yaml"),
        "name: Demo\nversion: 1.0.0\nmain:\n  - name: Check\n    python: phases.main:check\n",
    )
    .unwrap();
    std::fs::write(
        root.join("phases/main.py"),
        "def check():\n    return True\n",
    )
    .unwrap();
    std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
}

#[tokio::test]
async fn rpc_auth_boundary() {
    let dir = tempfile::tempdir().unwrap();
    seed_project(dir.path());
    let server = studio_server(dir.path()).await;

    let (status, _) = rpc(&server, None, r#"{"op":"project_info"}"#).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);

    let (status, _) = rpc(&server, Some("wrong-token"), r#"{"op":"project_info"}"#).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);

    let token = server.session_token().to_string();
    let (status, value) = rpc(&server, Some(&token), r#"{"op":"project_info"}"#).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(value["result"], "project_info");
    assert_eq!(value["procedure_path"], "procedure.yaml");
}

/// The RPC route allow-lists Origins instead of accepting `Any`.
/// 127.0.0.1 is only reachable through the operator's own browser, so
/// this is what stops a leaked session token from being usable by a
/// hostile page the operator happens to visit.
///
/// Only environment-independent origins are asserted: `localhost:3000`
/// is pushed unconditionally, and an unknown origin is never allowed.
/// The credentialed dashboard origin varies per machine, so it is
/// deliberately not asserted here.
#[tokio::test]
async fn rpc_cors_allow_list_pins_the_origin() {
    let dir = tempfile::tempdir().unwrap();
    seed_project(dir.path());
    let server = studio_server(dir.path()).await;

    async fn preflight_allows(server: &Server, origin: &str) -> bool {
        let res = test_client()
            .request(
                reqwest::Method::OPTIONS,
                format!("http://127.0.0.1:{}/studio/rpc", server.port()),
            )
            .header("origin", origin)
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "authorization,content-type",
            )
            .send()
            .await
            .expect("preflight");
        // A refused origin still answers 200; what blocks the browser
        // is the absence of the echo header, so assert on that.
        res.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == origin)
    }

    assert!(
        preflight_allows(&server, "http://localhost:3000").await,
        "the dev dashboard origin must stay allowed",
    );
    assert!(
        !preflight_allows(&server, "https://evil.example").await,
        "an unknown origin must not be echoed back — `Any` would leak \
         the loopback surface to any page the operator visits",
    );
    // Same posture for a near-miss on the dev port: allow-listing is
    // exact-origin, not a localhost wildcard.
    assert!(
        !preflight_allows(&server, "http://localhost:3001").await,
        "a different localhost port is a different origin",
    );
}

#[tokio::test]
async fn rpc_surface_off_without_enable_studio() {
    let server = Server::start(
        "kiosk-test".into(),
        "Kiosk".into(),
        HelloIdentity::default(),
        HostMode::Local,
        PortChoice::Ephemeral,
    )
    .await
    .expect("bind");
    // Even the process's own valid token is refused while no studio
    // root is configured: kiosk/daemon processes keep file access off.
    let token = server.session_token().to_string();
    let (status, _) = rpc(&server, Some(&token), r#"{"op":"project_info"}"#).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rpc_file_roundtrip_and_conflict() {
    let dir = tempfile::tempdir().unwrap();
    seed_project(dir.path());
    let server = studio_server(dir.path()).await;
    let token = server.session_token().to_string();

    let (_, listing) = rpc(&server, Some(&token), r#"{"op":"list_files"}"#).await;
    let names: Vec<&str> = listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    // Dirs first; the dotfile is hidden.
    assert_eq!(names, vec!["phases", "procedure.yaml"]);

    let (_, read) = rpc(
        &server,
        Some(&token),
        r#"{"op":"read_file","path":"procedure.yaml"}"#,
    )
    .await;
    assert_eq!(read["result"], "file_content");
    let sha = read["sha256"].as_str().unwrap().to_string();

    let write = serde_json::json!({
        "op": "write_file",
        "path": "procedure.yaml",
        "content": "name: Demo2\nversion: 1.0.0\nmain:\n  - name: Check\n    python: phases.main:check\n",
        "expected_sha256": sha,
    });
    let (_, written) = rpc(&server, Some(&token), &write.to_string()).await;
    assert_eq!(written["result"], "written");

    // Same baseline again: content moved on, must conflict.
    let (_, conflict) = rpc(&server, Some(&token), &write.to_string()).await;
    assert_eq!(conflict["result"], "error");
    assert_eq!(conflict["code"], "conflict");

    let (_, validated) = rpc(&server, Some(&token), r#"{"op":"validate"}"#).await;
    assert_eq!(validated["result"], "diagnostics");
    assert_eq!(validated["diagnostics"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn rpc_refuses_escapes_dotfiles_and_symlink_laundering() {
    let dir = tempfile::tempdir().unwrap();
    seed_project(dir.path());
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join(".env"), dir.path().join("link.yaml")).unwrap();
    let server = studio_server(dir.path()).await;
    let token = server.session_token().to_string();

    let (_, escape) = rpc(
        &server,
        Some(&token),
        r#"{"op":"read_file","path":"../../etc/hosts"}"#,
    )
    .await;
    assert_eq!(escape["result"], "error");

    let (_, dotfile) = rpc(&server, Some(&token), r#"{"op":"read_file","path":".env"}"#).await;
    assert_eq!(dotfile["result"], "error");
    assert_eq!(dotfile["code"], "forbidden");

    // An in-root symlink with an allow-listed name must not launder a
    // forbidden target through the extension check.
    #[cfg(unix)]
    {
        let (_, laundered) = rpc(
            &server,
            Some(&token),
            r#"{"op":"read_file","path":"link.yaml"}"#,
        )
        .await;
        assert_eq!(laundered["result"], "error");
        assert_eq!(laundered["code"], "forbidden");
    }

    // Unknown op: typed forward-compat error, not a bare 400.
    let (status, unknown) = rpc(&server, Some(&token), r#"{"op":"time_travel"}"#).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(unknown["code"], "unsupported");
}

#[tokio::test]
async fn ws_token_and_origin_gating() {
    let dir = tempfile::tempdir().unwrap();
    seed_project(dir.path());
    let server = studio_server(dir.path()).await;
    let port = server.port();
    let token = server.session_token().to_string();

    // Foreign origin, no token: rejected.
    assert_eq!(
        ws_upgrade_status(port, "/ws", "https://evil.example").await,
        403
    );
    // Foreign origin, bad token: rejected.
    assert_eq!(
        ws_upgrade_status(port, "/ws?token=nope", "https://www.tofupilot.app").await,
        403
    );
    // Foreign origin, valid token: upgraded.
    assert_eq!(
        ws_upgrade_status(
            port,
            &format!("/ws?token={token}"),
            "https://www.tofupilot.app"
        )
        .await,
        101
    );
    // Allow-listed loopback origin still works with no token (kiosk).
    assert_eq!(
        ws_upgrade_status(port, "/ws", &format!("http://127.0.0.1:{port}")).await,
        101
    );
}

#[tokio::test]
async fn ws_token_refused_without_studio() {
    let server = Server::start(
        "kiosk-test-2".into(),
        "Kiosk".into(),
        HelloIdentity::default(),
        HostMode::Local,
        PortChoice::Ephemeral,
    )
    .await
    .expect("bind");
    // Token path is honored only while the studio surface is enabled;
    // kiosk-only processes keep the historic Origin-only posture.
    let token = server.session_token().to_string();
    assert_eq!(
        ws_upgrade_status(
            server.port(),
            &format!("/ws?token={token}"),
            "https://www.tofupilot.app"
        )
        .await,
        403
    );
}

/// Project switching over the wire. The unit tests cover
/// `StudioConfig::activate` in isolation; what only the full path can
/// show is that the switch actually moves file confinement, that the
/// reply shapes are the ones the dashboard will parse, and that the
/// granted set is seeded from the recents file at boot.
#[tokio::test]
async fn rpc_project_switch_selects_among_granted_roots_only() {
    let tmp = tempfile::tempdir().unwrap();
    let launched = tmp.path().join("launched");
    let past = tmp.path().join("past");
    let never = tmp.path().join("never");
    for dir in [&launched, &past, &never] {
        std::fs::create_dir(dir).unwrap();
        seed_project(dir);
    }
    let (launched, past, never) = (
        launched.canonicalize().unwrap(),
        past.canonicalize().unwrap(),
        never.canonicalize().unwrap(),
    );
    // Tell the two apart once switched.
    std::fs::write(
        past.join("procedure.yaml"),
        "name: Past\nversion: 1.0.0\nmain:\n  - name: Check\n    python: phases.main.check\n",
    )
    .unwrap();

    // A root opened in a previous session is granted at boot — that is
    // what gives the switcher something to offer on the first session
    // after the update.
    let recents = test_recents(&launched);
    crate::commands::studio_recents::record_in(&recents, &past).unwrap();

    let server = studio_server(&launched).await;
    let token = server.session_token().to_string();

    let (_, listed) = rpc(&server, Some(&token), r#"{"op":"list_projects"}"#).await;
    assert_eq!(listed["result"], "projects");
    assert_eq!(listed["active"], launched.display().to_string());
    let offered: Vec<&str> = listed["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        offered,
        vec![launched.display().to_string(), past.display().to_string()],
        "active first, then the root restored from the recents file",
    );

    // Never granted: refused, and with the same answer a non-existent
    // path gets — the reply must not tell a token holder what is real.
    let unknown = serde_json::json!({"op": "open_project", "path": never.display().to_string()});
    let (_, refused) = rpc(&server, Some(&token), &unknown.to_string()).await;
    assert_eq!(refused["code"], "forbidden");
    let ghost = serde_json::json!({"op": "open_project", "path": "/nowhere/at/all"});
    let (_, refused_ghost) = rpc(&server, Some(&token), &ghost.to_string()).await;
    assert_eq!(refused_ghost["code"], refused["code"]);
    assert_eq!(refused_ghost["message"], refused["message"]);

    // Granted: switches, and the file surface follows.
    let switch = serde_json::json!({"op": "open_project", "path": past.display().to_string()});
    let (_, opened) = rpc(&server, Some(&token), &switch.to_string()).await;
    assert_eq!(opened["result"], "opened");
    assert_eq!(opened["project"]["name"], "past");

    let (_, info) = rpc(&server, Some(&token), r#"{"op":"project_info"}"#).await;
    assert_eq!(info["root"], past.display().to_string());
    let (_, read) = rpc(
        &server,
        Some(&token),
        r#"{"op":"read_file","path":"procedure.yaml"}"#,
    )
    .await;
    assert!(
        read["content"].as_str().unwrap().contains("name: Past"),
        "reads must resolve against the newly active root",
    );

    // Persisted, so the next launch reopens it.
    assert_eq!(
        crate::commands::studio_recents::load_from(&recents).first(),
        Some(&past),
        "the switch must reach the recents file, not just memory",
    );
}

/// The `busy` refusal. Its flag is set by the server's own event pump on
/// the run lifecycle — no unit test on `StudioConfig` can reach it,
/// because neither the dispatcher's `RunHandle` nor `procedure_dir`
/// knows whether a run is in flight *now*.
#[tokio::test]
async fn rpc_project_switch_is_refused_while_a_run_is_in_flight() {
    let tmp = tempfile::tempdir().unwrap();
    let launched = tmp.path().join("launched");
    let past = tmp.path().join("past");
    for dir in [&launched, &past] {
        std::fs::create_dir(dir).unwrap();
        seed_project(dir);
    }
    let (launched, past) = (
        launched.canonicalize().unwrap(),
        past.canonicalize().unwrap(),
    );
    crate::commands::studio_recents::record_in(&test_recents(&launched), &past).unwrap();

    let server = studio_server(&launched).await;
    let token = server.session_token().to_string();
    let switch =
        serde_json::json!({"op": "open_project", "path": past.display().to_string()}).to_string();

    // Attach a run the way the studio dispatcher does, then drive its
    // lifecycle by hand. `event_tx` and `ui_tx` stay in scope: dropping
    // either would tear the pump down mid-test.
    let (event_tx, _keep_alive) = broadcast::channel::<StationEvent>(16);
    let (ui_tx, _ui_rx) = mpsc::channel::<StationCommand>(1);
    let (cancel, _cancel_rx) = crate::commands::run::cancel::CancelToken::new();
    let _attachment = server
        .attach_run(
            event_tx.clone(),
            ui_tx,
            cancel,
            Vec::new(),
            None,
            HostMode::Local,
        )
        .await;

    event_tx.send(run_started()).expect("publish RunStarted");
    wait_for_run_flag(&server, true).await;

    let (_, busy) = rpc(&server, Some(&token), &switch).await;
    assert_eq!(busy["result"], "error");
    assert_eq!(
        busy["code"], "busy",
        "switching mid-run would leave the engine and the UI on two \
         different projects",
    );

    event_tx.send(run_complete()).expect("publish RunComplete");
    wait_for_run_flag(&server, false).await;

    let (_, opened) = rpc(&server, Some(&token), &switch).await;
    assert_eq!(
        opened["result"], "opened",
        "the refusal must lift when the run ends, not persist for the \
         life of the session",
    );
}

/// The pump is a spawned task, so there is no synchronous point between
/// `event_tx.send` and the guard seeing the flag. Poll the flag itself
/// rather than retrying the RPC: a retry loop would switch the project
/// on the first call that lands before the flag, and the assertion that
/// follows would then prove nothing. The deadline only exists so a
/// regression fails instead of hanging.
async fn wait_for_run_flag(server: &Server, want: bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while server.state.studio_run_active.load(Ordering::Acquire) != want {
        assert!(
            std::time::Instant::now() < deadline,
            "the event pump never set the in-flight flag to {want}",
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

fn run_started() -> StationEvent {
    StationEvent::RunStarted {
        procedure_id: "proc-1".into(),
        procedure_name: "Demo".into(),
        execution_id: "exec-1".into(),
        phases: Vec::new(),
        slots: Vec::new(),
        plugs: Vec::new(),
        timestamp: None,
        run_id: None,
        deployment_id: None,
        unit: None,
        only_phase: None,
    }
}

fn run_complete() -> StationEvent {
    StationEvent::RunComplete {
        outcome: "PASS".into(),
        run_id: None,
        execution_id: Some("exec-1".into()),
    }
}

/// `/files/*` serves component images from the studio root between
/// runs (the Builder/Sequence previews resolve against it), with the
/// same clamps as the run path: image extensions only, no escapes.
/// Non-studio daemons keep the 404 (covered by the enable-gating
/// asserts below).
#[tokio::test]
async fn files_served_from_studio_root_while_idle() {
    let dir = tempfile::tempdir().unwrap();
    seed_project(dir.path());
    std::fs::create_dir(dir.path().join("images")).unwrap();
    std::fs::write(dir.path().join("images/board.png"), b"not-really-a-png").unwrap();

    let server = studio_server(dir.path()).await;
    let base = format!("http://127.0.0.1:{}", server.port());
    let client = test_client();

    let res = client
        .get(format!("{base}/files/images/board.png"))
        .send()
        .await
        .expect("files request");
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"not-really-a-png");

    // Non-image extensions stay off the HTTP surface even idle.
    let res = client
        .get(format!("{base}/files/procedure.yaml"))
        .send()
        .await
        .expect("files request");
    assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);

    // A plain kiosk/station daemon (no studio surface) keeps the 404.
    let plain = Server::start(
        "kiosk-files-test".into(),
        "Kiosk".into(),
        HelloIdentity::default(),
        HostMode::Local,
        PortChoice::Ephemeral,
    )
    .await
    .expect("bind");
    let res = client
        .get(format!(
            "http://127.0.0.1:{}/files/images/board.png",
            plain.port()
        ))
        .send()
        .await
        .expect("files request");
    assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);
}

/// `pick_project` — the one op that grants a NEW root. The tests stand
/// in for the human: the dialog host here is a channel stub answering
/// what the OS folder picker would have. What the full path must show
/// is that the grant lands (active root, file confinement, recents)
/// and that the op stays typed when there is no dialog to show.
#[tokio::test]
async fn rpc_pick_project_grants_and_activates_the_dialog_choice() {
    let tmp = tempfile::tempdir().unwrap();
    let launched = tmp.path().join("launched");
    let picked = tmp.path().join("picked");
    for dir in [&launched, &picked] {
        std::fs::create_dir(dir).unwrap();
        seed_project(dir);
    }
    let (launched, picked) = (
        launched.canonicalize().unwrap(),
        picked.canonicalize().unwrap(),
    );
    std::fs::write(
        picked.join("procedure.yaml"),
        "name: Picked\nversion: 1.0.0\nmain:\n  - name: Check\n    python: phases.main.check\n",
    )
    .unwrap();

    let server = studio_server(&launched).await;
    let token = server.session_token().to_string();

    // Before the pick, the path is not granted: `open_project` cannot
    // reach it. This is what makes the dialog the only door in.
    let select =
        serde_json::json!({"op": "open_project", "path": picked.display().to_string()}).to_string();
    let (_, refused) = rpc(&server, Some(&token), &select).await;
    assert_eq!(refused["code"], "forbidden");

    // The human picks the folder.
    let (dialog_tx, mut dialog_rx) = mpsc::channel::<StudioDialogJob>(1);
    server.set_studio_dialog_host(dialog_tx).await;
    let choice = picked.clone();
    tokio::spawn(async move {
        let job = dialog_rx.recv().await.expect("dialog job");
        let _ = job.send(Some(choice));
    });

    let (_, opened) = rpc(&server, Some(&token), r#"{"op":"pick_project"}"#).await;
    assert_eq!(opened["result"], "opened", "reply: {opened}");
    assert_eq!(opened["project"]["path"], picked.display().to_string());

    // The grant is real: active root, file confinement, and the
    // recents file all moved to the picked project.
    let (_, listed) = rpc(&server, Some(&token), r#"{"op":"list_projects"}"#).await;
    assert_eq!(listed["active"], picked.display().to_string());
    let (_, read) = rpc(
        &server,
        Some(&token),
        r#"{"op":"read_file","path":"procedure.yaml"}"#,
    )
    .await;
    assert!(
        read["content"].as_str().unwrap().contains("name: Picked"),
        "file ops must follow the granted root",
    );
    let recents = crate::commands::studio_recents::existing_from(&test_recents(&launched));
    assert_eq!(
        recents.first(),
        Some(&picked),
        "the pick must reach the recents file, not just memory",
    );
}

/// A picked folder with NO procedure is parked, not granted: almost
/// always a mis-click, and a grant is permanent full read/write. The
/// page must confirm with `confirm_pick` — which carries no path, so
/// the browser can only confirm what the human already chose in the
/// native dialog. A replayed confirmation grants nothing.
#[tokio::test]
async fn rpc_pick_project_parks_an_empty_folder_until_confirm_pick() {
    let tmp = tempfile::tempdir().unwrap();
    let launched = tmp.path().join("launched");
    std::fs::create_dir(&launched).unwrap();
    seed_project(&launched);
    let launched = launched.canonicalize().unwrap();
    // No procedure.yaml anywhere under it: the mis-click shape.
    let empty = tmp.path().join("empty-folder");
    std::fs::create_dir(&empty).unwrap();
    let empty = empty.canonicalize().unwrap();

    let server = studio_server(&launched).await;
    let token = server.session_token().to_string();
    let (dialog_tx, mut dialog_rx) = mpsc::channel::<StudioDialogJob>(1);
    server.set_studio_dialog_host(dialog_tx).await;
    let choice = empty.clone();
    tokio::spawn(async move {
        let job = dialog_rx.recv().await.expect("dialog job");
        let _ = job.send(Some(choice));
    });

    // The pick parks: typed reply, and the folder is NOT granted.
    let (_, parked) = rpc(&server, Some(&token), r#"{"op":"pick_project"}"#).await;
    assert_eq!(parked["result"], "picked_empty", "reply: {parked}");
    assert_eq!(parked["path"], empty.display().to_string());
    let select =
        serde_json::json!({"op": "open_project", "path": empty.display().to_string()}).to_string();
    let (_, refused) = rpc(&server, Some(&token), &select).await;
    assert_eq!(
        refused["code"], "forbidden",
        "parked must not mean granted: {refused}"
    );

    // The explicit yes grants exactly the parked path.
    let (_, opened) = rpc(&server, Some(&token), r#"{"op":"confirm_pick"}"#).await;
    assert_eq!(opened["result"], "opened", "reply: {opened}");
    assert_eq!(opened["project"]["path"], empty.display().to_string());

    // Single-use: replaying the confirmation has nothing to grant.
    let (_, replay) = rpc(&server, Some(&token), r#"{"op":"confirm_pick"}"#).await;
    assert_eq!(replay["code"], "invalid", "reply: {replay}");
}

/// The "no" must reach the daemon and disarm the parked offer: a
/// declined pick left armed would let a later bare `confirm_pick` (a
/// second tab, a replay) grant a folder no human ever said yes to.
#[tokio::test]
async fn rpc_discard_pick_disarms_a_declined_offer() {
    let tmp = tempfile::tempdir().unwrap();
    let launched = tmp.path().join("launched");
    std::fs::create_dir(&launched).unwrap();
    seed_project(&launched);
    let launched = launched.canonicalize().unwrap();
    let empty = tmp.path().join("home-misclick");
    std::fs::create_dir(&empty).unwrap();
    let empty = empty.canonicalize().unwrap();

    let server = studio_server(&launched).await;
    let token = server.session_token().to_string();
    let (dialog_tx, mut dialog_rx) = mpsc::channel::<StudioDialogJob>(1);
    server.set_studio_dialog_host(dialog_tx).await;
    let choice = empty.clone();
    tokio::spawn(async move {
        let job = dialog_rx.recv().await.expect("dialog job");
        let _ = job.send(Some(choice));
    });

    let (_, parked) = rpc(&server, Some(&token), r#"{"op":"pick_project"}"#).await;
    assert_eq!(parked["result"], "picked_empty", "reply: {parked}");

    // The human clicks Cancel on the warning: the page reports it.
    let (_, discarded) = rpc(&server, Some(&token), r#"{"op":"discard_pick"}"#).await;
    assert_eq!(discarded["result"], "pick_discarded", "reply: {discarded}");

    // Nothing left to confirm — the offer is disarmed, not parked.
    let (_, replay) = rpc(&server, Some(&token), r#"{"op":"confirm_pick"}"#).await;
    assert_eq!(replay["code"], "invalid", "reply: {replay}");
    let select =
        serde_json::json!({"op": "open_project", "path": empty.display().to_string()}).to_string();
    let (_, refused) = rpc(&server, Some(&token), &select).await;
    assert_eq!(
        refused["code"], "forbidden",
        "declined must never mean granted: {refused}"
    );

    // Discarding again is a no-op, not an error.
    let (_, again) = rpc(&server, Some(&token), r#"{"op":"discard_pick"}"#).await;
    assert_eq!(again["result"], "pick_discarded", "reply: {again}");
}

/// The two typed non-grants: a daemon with no dialog host refuses
/// (`unsupported`, never a hang), and a dismissed dialog answers
/// `cancelled` — which the UI treats as "nothing happened", not as an
/// error to render.
#[tokio::test]
async fn rpc_pick_project_missing_host_and_cancel_are_typed() {
    let dir = tempfile::tempdir().unwrap();
    seed_project(dir.path());
    let server = studio_server(dir.path()).await;
    let token = server.session_token().to_string();

    // No host installed (any non-`tofupilot studio` process).
    let (_, no_host) = rpc(&server, Some(&token), r#"{"op":"pick_project"}"#).await;
    assert_eq!(no_host["code"], "unsupported");

    // Host installed, human dismisses the dialog.
    let (dialog_tx, mut dialog_rx) = mpsc::channel::<StudioDialogJob>(1);
    server.set_studio_dialog_host(dialog_tx).await;
    tokio::spawn(async move {
        let job = dialog_rx.recv().await.expect("dialog job");
        let _ = job.send(None);
    });
    let (_, cancelled) = rpc(&server, Some(&token), r#"{"op":"pick_project"}"#).await;
    assert_eq!(cancelled["code"], "cancelled");
}

/// The re-check after the dialog: a run that starts while the picker
/// window sits open must void the pick entirely. Granting-but-not-
/// activating would leave a root nobody asked for; activating would
/// put the engine and the UI on two different projects.
#[tokio::test]
async fn rpc_pick_project_grants_nothing_when_a_run_started_mid_dialog() {
    let tmp = tempfile::tempdir().unwrap();
    let launched = tmp.path().join("launched");
    let picked = tmp.path().join("picked");
    for dir in [&launched, &picked] {
        std::fs::create_dir(dir).unwrap();
        seed_project(dir);
    }
    let (launched, picked) = (
        launched.canonicalize().unwrap(),
        picked.canonicalize().unwrap(),
    );

    let server = studio_server(&launched).await;
    let token = server.session_token().to_string();
    let port = server.port();

    let (dialog_tx, mut dialog_rx) = mpsc::channel::<StudioDialogJob>(1);
    server.set_studio_dialog_host(dialog_tx).await;

    // The RPC call runs in its own task (it blocks on the dialog);
    // the test body plays both the event pump and the human.
    let rpc_call = tokio::spawn(async move {
        test_client()
            .post(format!("http://127.0.0.1:{port}/studio/rpc"))
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(r#"{"op":"pick_project"}"#)
            .send()
            .await
            .expect("rpc request")
            .json::<serde_json::Value>()
            .await
            .expect("json reply")
    });

    // Dialog is open (the job arrived); now a run starts.
    let job = dialog_rx.recv().await.expect("dialog job");
    let (event_tx, _keep_alive) = broadcast::channel::<StationEvent>(16);
    let (ui_tx, _ui_rx) = mpsc::channel::<StationCommand>(1);
    let (cancel, _cancel_rx) = crate::commands::run::cancel::CancelToken::new();
    let _attachment = server
        .attach_run(
            event_tx.clone(),
            ui_tx,
            cancel,
            Vec::new(),
            None,
            HostMode::Local,
        )
        .await;
    event_tx.send(run_started()).expect("publish RunStarted");
    wait_for_run_flag(&server, true).await;

    // Only now does the human confirm the folder.
    let _ = job.send(Some(picked.clone()));

    let reply = rpc_call.await.expect("rpc task");
    assert_eq!(reply["code"], "busy", "reply: {reply}");

    // Nothing was granted: the switcher still cannot reach the path.
    let token = server.session_token().to_string();
    let (_, listed) = rpc(&server, Some(&token), r#"{"op":"list_projects"}"#).await;
    let offered: Vec<&str> = listed["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["path"].as_str().unwrap())
        .collect();
    assert!(
        !offered.contains(&picked.display().to_string().as_str()),
        "a voided pick must not leave a granted root behind",
    );
}
