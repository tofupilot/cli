//! Process-wide `reqwest::Client`.
//!
//! All HTTP calls go through `client()` so a single connection pool is
//! shared across the process. `pull` in particular issues an artifact
//! descriptor request followed by the download against the same host;
//! a shared pool reuses the TLS handshake instead of paying it twice.
//!
//! Centralizing also gives a single place to land future cross-cutting
//! concerns (custom default headers, per-process timeouts).

use std::sync::OnceLock;
use std::time::Duration;

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Extra CA certificate to trust, for a self-hosted instance behind a private
/// or corporate CA. Read from `TOFUPILOT_CA_CERT`, or from the `ca_cert` saved
/// at login (see `commands::auth`), installed via [`set_ca_cert`].
///
/// This ADDS to the bundled Mozilla root set rather than replacing it —
/// reqwest keeps its built-in roots unless `tls_built_in_root_certs(false)` is
/// called — so trusting a private CA never breaks a public host. Certificate
/// verification stays on: there is deliberately no "accept invalid certs"
/// escape hatch.
///
/// SCOPE: this covers HTTP only. The realtime link does not use it —
/// `centrifuge-client` calls `tokio_tungstenite::connect_async` with the
/// `rustls-tls-webpki-roots` feature, which pins the bundled Mozilla roots and
/// takes no connector, so there is nowhere to inject a certificate. A station
/// on a private CA runs and uploads over HTTP but cannot open the WebSocket,
/// and shows as offline on the dashboard. Fixing that needs
/// `centrifuge-client` to accept a rustls `ClientConfig` (or a custom
/// `Connector`) so this certificate can be threaded through to it.
pub const CA_CERT_ENV: &str = "TOFUPILOT_CA_CERT";

/// CA path resolved from stored credentials or `login --ca-cert`, installed
/// by [`set_ca_cert`]. A `OnceLock` rather than `std::env::set_var`: mutating
/// the environment after the tokio runtime has started races other threads
/// reading it (and `set_var` is `unsafe` from Rust 2024 for exactly that
/// reason). When set, it wins over the environment variable — `main` only
/// installs the stored path when the variable is absent, and `login` resolves
/// its own precedence before installing.
static CA_CERT_OVERRIDE: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Install the CA certificate path for this process. Call before the first
/// request; later calls are ignored (first writer wins), and the shared
/// `client()` snapshots the configuration on first use either way.
pub fn set_ca_cert(path: &str) {
    let _ = CA_CERT_OVERRIDE.set(std::path::PathBuf::from(path));
}

/// The CA path in effect: the installed override, else `TOFUPILOT_CA_CERT`.
fn configured_ca_path() -> Option<std::path::PathBuf> {
    if let Some(path) = CA_CERT_OVERRIDE.get() {
        return Some(path.clone());
    }
    std::env::var_os(CA_CERT_ENV).map(std::path::PathBuf::from)
}

/// Load the configured extra root certificate, if any.
///
/// A PEM bundle may hold several certificates (leaf + intermediates + root),
/// so every certificate in the file is added. A missing or malformed file is
/// an `Err` so callers can name the real cause up front — they log it as a
/// warning and continue on the public trust store, where a self-hosted
/// station then fails its requests with a TLS error the warning explains.
fn extra_root_certificates() -> Result<Vec<reqwest::Certificate>, String> {
    let Some(path) = configured_ca_path() else {
        return Ok(Vec::new());
    };
    if path.as_os_str().is_empty() {
        return Ok(Vec::new());
    }

    let pem = std::fs::read(&path)
        .map_err(|e| format!("{CA_CERT_ENV}: cannot read {}: {e}", path.display()))?;

    let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| {
        format!(
            "{CA_CERT_ENV}: {} is not a valid PEM bundle: {e}",
            path.display()
        )
    })?;

    if certs.is_empty() {
        return Err(format!(
            "{CA_CERT_ENV}: {} contains no certificates",
            path.display()
        ));
    }
    Ok(certs)
}

/// Parsed extra roots, resolved once per process. Parsing on every call had
/// the station's 5-minute auth probe re-reading and re-parsing the PEM file
/// forever, and repeated the same warning on every misconfigured call. A
/// parse failure resolves to an empty list after one warning — the fallback
/// is the public trust store, so a self-hosted station then fails its
/// requests with a TLS error the warning explains.
static EXTRA_CERTS: OnceLock<Vec<reqwest::Certificate>> = OnceLock::new();

fn extra_certs() -> &'static [reqwest::Certificate] {
    EXTRA_CERTS.get_or_init(|| match extra_root_certificates() {
        Ok(certs) => certs,
        Err(message) => {
            crate::log::warn(&format!("{message}. Continuing without it."));
            Vec::new()
        }
    })
}

/// Add the configured extra CA (if any) to a builder. The one shared path for
/// `client()` and `client_builder()`, so the two can never drift.
fn with_extra_roots(mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    for cert in extra_certs() {
        builder = builder.add_root_certificate(cert.clone());
    }
    builder
}

/// TCP connect must complete in 30s. A stalled SYN/TLS handshake
/// (intermittent network, hostile proxy) otherwise pins the calling
/// task indefinitely — `pull/sync.rs`, `uv_bootstrap.rs`, descriptor
/// fetch all share this client.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Read inactivity (no bytes for N seconds) trips a timeout. We do
/// NOT use `Client::timeout` here — that's an overall request-timeout
/// budget that breaks legitimate large downloads (100–300 MB
/// deployment bundles, uv installer). `read_timeout` is per-socket-
/// idle, so a slow-but-progressing transfer is fine but a slow-loris
/// upstream that stops feeding bytes gets cut.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Shared `reqwest::Client`. Cloning is cheap — it's an `Arc` under
/// the hood — so callers should clone freely if they need an owned
/// handle.
pub fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        let base = || {
            reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
        };
        // `from_pem_bundle` only validates the PEM envelope; rustls rejects a
        // corrupt or unsupported certificate later, at build(). That case must
        // degrade to the public roots like every other CA misconfiguration —
        // reaching the `.expect` below would crash every command instead.
        match with_extra_roots(base()).build() {
            Ok(client) => client,
            Err(e) => {
                crate::log::warn(&format!(
                    "{CA_CERT_ENV}: the TLS backend rejected the extra CA certificate ({e}). \
                     Continuing without it."
                ));
                base()
                    .build()
                    .expect("reqwest::Client build should never fail with rustls + default config")
            }
        }
    })
}

/// A `ClientBuilder` that already trusts any configured extra CA.
///
/// Call sites that need their own timeouts (the auth probes, the updater)
/// build a separate client rather than using `client()`. They must still go
/// through here — a builder constructed directly would skip the private CA
/// and fail on a self-hosted instance while the shared client succeeded,
/// which is a maddening thing to debug.
pub fn client_builder() -> reqwest::ClientBuilder {
    with_extra_roots(reqwest::Client::builder())
}

/// Convenience extension: `.bearer(api_key)` instead of
/// `.header("Authorization", format!("Bearer {api_key}"))` at every
/// authenticated request site.
pub trait RequestBuilderExt {
    fn bearer(self, token: &str) -> Self;
}

impl RequestBuilderExt for reqwest::RequestBuilder {
    fn bearer(self, token: &str) -> Self {
        self.header("Authorization", format!("Bearer {token}"))
    }
}
