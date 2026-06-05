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
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .expect("reqwest::Client build should never fail with rustls + default config")
    })
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
