//! Custom root certificate support, for self-hosted instances behind a private CA.
//!
//! The certificate is generated at test runtime (no committed fixtures, nothing
//! to expire) and is signed by a CA no trust store knows about, so a passing
//! handshake proves the certificate the caller supplied is the reason it
//! succeeded.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, OnceLock};

use tofupilot::{ClientConfig, TofuPilot};

/// Self-signed cert + key for localhost, generated once per test process.
fn test_cert() -> &'static (String, String) {
    static CERT: OnceLock<(String, String)> = OnceLock::new();
    CERT.get_or_init(|| {
        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
                .expect("generate self-signed cert");
        (cert.pem(), key_pair.serialize_pem())
    })
}

/// Serve HTTPS requests with the runtime-generated cert. Returns the bound port.
fn spawn_tls_server() -> u16 {
    spawn_tls_server_with_delay(std::time::Duration::ZERO)
}

/// Same server, but sleeps before responding — for timeout tests.
fn spawn_tls_server_with_delay(delay: std::time::Duration) -> u16 {
    // reqwest and this test both pull in rustls, so the provider is ambiguous
    // unless one is installed explicitly.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // `rustls-pki-types` rather than `rustls-pemfile`: the latter is
    // unmaintained (RUSTSEC-2025-0134) and is now only a thin wrapper around
    // this same parser.
    use rustls::pki_types::pem::PemObject;

    let (cert_pem, key_pem) = test_cert();
    let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .expect("parse cert");
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .expect("parse key");

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("server config");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let config = Arc::new(config);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut conn = match rustls::ServerConnection::new(config.clone()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut tls = rustls::Stream::new(&mut conn, &mut stream);

            // Read the request line so the client's write completes.
            let mut reader = BufReader::new(&mut tls);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);

            if !delay.is_zero() {
                std::thread::sleep(delay);
            }

            let body = b"{}";
            let _ = write!(
                tls,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = tls.write_all(body);
            let _ = tls.flush();
        }
    });

    port
}

fn config_for(port: u16) -> ClientConfig {
    ClientConfig::new("test_api_key").base_url(format!("https://localhost:{port}"))
}

fn ca_config_for(port: u16) -> ClientConfig {
    let (cert_pem, _) = test_cert();
    config_for(port)
        .add_root_certificate(cert_pem.as_bytes())
        .expect("parse pem")
}

/// Lowercased messages of the whole error chain — reqwest's Display is
/// shallow, so the interesting cause is often several levels deep.
fn error_chain(err: &dyn std::error::Error) -> Vec<String> {
    let mut messages = vec![err.to_string().to_lowercase()];
    let mut source = err.source();
    while let Some(cause) = source {
        messages.push(cause.to_string().to_lowercase());
        source = cause.source();
    }
    messages
}

/// Without the certificate the handshake must fail — otherwise the positive
/// case below would prove nothing.
#[tokio::test]
async fn rejects_unknown_ca_by_default() {
    let port = spawn_tls_server();
    let client = TofuPilot::with_config(config_for(port));

    let err = client
        .procedures()
        .list()
        .send()
        .await
        .expect_err("handshake should fail without the CA");

    let messages = error_chain(&err);
    assert!(
        messages.iter().any(|m| {
            m.contains("certificate") || m.contains("tls") || m.contains("unknown")
        }),
        "expected a TLS trust failure in the error chain, got: {messages:?}"
    );
}

#[tokio::test]
async fn accepts_configured_root_certificate() {
    let port = spawn_tls_server();
    let client = TofuPilot::with_config(ca_config_for(port));

    // A 200 with an empty object is not a valid procedures payload, so the
    // request gets past TLS and fails on the body instead. Either way the
    // handshake itself succeeded, which is what this asserts. The whole error
    // source chain is checked — the top-level Display hides TLS causes, which
    // would make a shallow check pass vacuously.
    match client.procedures().list().send().await {
        Ok(_) => {}
        Err(err) => {
            let messages = error_chain(&err);
            assert!(
                !messages.iter().any(|m| {
                    m.contains("certificate") || m.contains("tls") || m.contains("handshake")
                }),
                "TLS should have succeeded with the CA configured, got: {messages:?}"
            );
        }
    }
}

/// A per-call `.timeout()` must win over config.timeout — a slow import with
/// an explicit long timeout must not be capped by the 30s default, so the
/// short explicit timeout here must not be stretched by the long config one.
#[tokio::test]
async fn builder_timeout_takes_precedence_over_config() {
    let port = spawn_tls_server_with_delay(std::time::Duration::from_secs(3));
    // Retries off: the default 3 add exponential backoff between attempts, so
    // the wall-clock bound below would be measuring the backoff schedule
    // rather than which timeout applied.
    let client = TofuPilot::with_config(ca_config_for(port).max_retries(0));

    let started = std::time::Instant::now();
    let err = client
        .procedures()
        .list()
        .timeout(std::time::Duration::from_millis(200))
        .send()
        .await
        .expect_err("should time out at the builder's 200ms");
    let elapsed = started.elapsed();

    let messages = error_chain(&err);
    assert!(
        messages.iter().any(|m| m.contains("timed out") || m.contains("timeout")),
        "expected a timeout error, got: {messages:?}"
    );
    // The server sleeps 3s; anything well under that proves the 200ms builder
    // timeout applied rather than the 30s config one. Generous against a
    // loaded CI box, still far below the alternative.
    assert!(
        // Must stay under the mock server's 3s delay — that is what proves the
        // 200ms timeout ended the call. 2s flaked on loaded CI runners.
        elapsed < std::time::Duration::from_millis(2900),
        "builder timeout was not applied; call took {elapsed:?}"
    );
}

/// Without a per-call timeout, config.timeout applies — including for clients
/// supplied via with_client, which have no timeout of their own.
#[tokio::test]
async fn config_timeout_applies_as_fallback() {
    let port = spawn_tls_server_with_delay(std::time::Duration::from_secs(3));
    // Retries off for the same reason as the test above: their backoff would
    // dominate the wall-clock bound.
    let config = ca_config_for(port)
        .timeout(std::time::Duration::from_millis(200))
        .max_retries(0);
    let client = TofuPilot::with_config(config);

    let started = std::time::Instant::now();
    let err = client
        .procedures()
        .list()
        .send()
        .await
        .expect_err("should time out at config's 200ms");
    let elapsed = started.elapsed();

    let messages = error_chain(&err);
    assert!(
        messages.iter().any(|m| m.contains("timed out") || m.contains("timeout")),
        "expected a timeout error, got: {messages:?}"
    );
    assert!(
        // Must stay under the mock server's 3s delay — that is what proves the
        // 200ms timeout ended the call. 2s flaked on loaded CI runners.
        elapsed < std::time::Duration::from_millis(2900),
        "config timeout fallback was not applied; call took {elapsed:?}"
    );
}

/// The file path and the in-memory bytes must reach the same trust store, so
/// this asserts on a handshake rather than on a counter.
#[tokio::test]
async fn loads_root_certificate_from_pem_file() {
    let port = spawn_tls_server();
    let (cert_pem, _) = test_cert();
    let path = std::env::temp_dir().join(format!("tofupilot-test-ca-{}.pem", std::process::id()));
    std::fs::write(&path, cert_pem).expect("write pem");

    let config = config_for(port)
        .root_certificate_from_pem_file(&path)
        .expect("load pem from disk");
    let _ = std::fs::remove_file(&path);

    let err = TofuPilot::with_config(config)
        .procedures()
        .list()
        .send()
        .await
        .expect_err("the stub server never returns a valid payload");

    let messages = error_chain(&err);
    assert!(
        !messages.iter().any(|m| m.contains("certificate") || m.contains("unknownissuer")),
        "the CA loaded from disk was not trusted: {messages:?}"
    );
}

#[tokio::test]
async fn pem_file_error_is_surfaced() {
    let err = config_for(0)
        .root_certificate_from_pem_file("/nonexistent/ca.pem")
        .expect_err("missing file should error");

    assert!(matches!(err, tofupilot::Error::Io(_)), "expected Io error, got: {err}");
}

/// PEM that parses as a file but holds no certificate must be rejected at
/// configuration time. Accepting it would build a client that silently trusts
/// nothing extra, and the CA failure would surface much later as a handshake
/// error against the real server.
#[tokio::test]
async fn empty_pem_is_rejected() {
    let err = config_for(0)
        .add_root_certificate(b"not a certificate")
        .expect_err("PEM without certificates should error");

    assert!(
        matches!(err, tofupilot::Error::Validation(_)),
        "expected Validation error, got: {err}"
    );
}

/// A bundle (root + intermediate) is the shape a corporate PKI ships, and the
/// server's CA is deliberately the LAST entry: an implementation that stops
/// after the first certificate parses would still fail the handshake here.
#[tokio::test]
async fn pem_bundle_trusts_every_certificate() {
    let port = spawn_tls_server();
    let (cert_pem, _) = test_cert();

    let unrelated = rcgen::generate_simple_self_signed(vec!["unrelated.example".to_string()])
        .expect("generate unrelated cert")
        .cert
        .pem();
    let bundle = format!("{unrelated}{cert_pem}");

    let config = config_for(port)
        .add_root_certificate(bundle.as_bytes())
        .expect("parse bundle");

    let err = TofuPilot::with_config(config)
        .procedures()
        .list()
        .send()
        .await
        .expect_err("the stub server never returns a valid payload");

    let messages = error_chain(&err);
    assert!(
        !messages.iter().any(|m| m.contains("certificate") || m.contains("unknownissuer")),
        "the last certificate in the bundle was not trusted: {messages:?}"
    );
}

/// The regression this design exists to prevent: public HTTPS must keep
/// working when the OS trust store is empty.
///
/// reqwest 0.13 verifies through rustls-platform-verifier, which on Unix
/// trusts the OS store ALONE and fails with "No CA certificates were loaded
/// from the system" when it is empty — a slim container without
/// `ca-certificates` would lose every call. `client.rs` supplies its own store
/// with the bundled Mozilla roots as a floor precisely so that cannot happen.
///
/// `SSL_CERT_FILE` pointed at an empty file is how the Linux loader
/// (`openssl-probe`) sees "no system certificates". On macOS the keychain
/// still answers, so there this asserts the floor holds rather than that the
/// store is truly empty.
#[tokio::test]
#[ignore = "network"]
async fn public_https_works_without_a_system_store() {
    let empty = std::env::temp_dir().join(format!("tofupilot-empty-ca-{}.pem", std::process::id()));
    std::fs::write(&empty, b"").expect("write empty ca file");
    std::env::set_var("SSL_CERT_FILE", &empty);

    let client = TofuPilot::with_config(ClientConfig::new("test_api_key"));
    let result = client.procedures().list().send().await;

    let _ = std::fs::remove_file(&empty);

    // A 401 from the real API is a success for this test: the TLS handshake
    // completed. Only a transport/TLS failure means the trust floor was lost.
    if let Err(err) = &result {
        let messages = error_chain(err);
        assert!(
            !messages.iter().any(|m| {
                m.contains("certificate") || m.contains("ca certificates") || m.contains("tls")
            }),
            "public HTTPS hit a trust failure with an empty OS store: {messages:?}"
        );
    }
}
