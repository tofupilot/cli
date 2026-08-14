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
        .enable_studio(root.to_path_buf())
        .await
        .expect("enable studio");
    server
}

async fn rpc(
    server: &Server,
    token: Option<&str>,
    body: &str,
) -> (reqwest::StatusCode, serde_json::Value) {
    let client = reqwest::Client::new();
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
        let res = reqwest::Client::new()
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
    let client = reqwest::Client::new();

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
