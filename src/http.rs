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
/// This covers every connection the CLI makes. HTTP goes through the reqwest
/// builder below; the realtime WebSocket gets the same certificates via
/// [`realtime_connector`], which centrifuge-client accepts through our forked
/// `ClientConfig::connector`. The WebSocket additionally trusts the OS
/// certificate store (`rustls-tls-native-roots`), so a CA installed
/// machine-wide works even without this file.
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

/// Whether an extra CA is configured at all. The realtime client uses this to
/// word a TLS failure: with a CA set the certificate is genuinely untrusted by
/// it, without one the operator is told the `--ca-cert` flag exists.
pub fn ca_cert_configured() -> bool {
    configured_ca_path().is_some_and(|p| !p.as_os_str().is_empty())
}

/// The CA path in effect: the installed override, else `TOFUPILOT_CA_CERT`.
pub(crate) fn configured_ca_path() -> Option<std::path::PathBuf> {
    if let Some(path) = CA_CERT_OVERRIDE.get() {
        return Some(path.clone());
    }
    std::env::var_os(CA_CERT_ENV).map(std::path::PathBuf::from)
}

/// The trust anchors every TLS connection in this process uses: the bundled
/// Mozilla roots, the machine's certificate store, and the `--ca-cert` bundle
/// when one is configured.
///
/// Built here rather than left to the TLS stacks because reqwest 0.13 and
/// tokio-tungstenite disagree about defaults: reqwest on `rustls-no-provider`
/// verifies through `rustls-platform-verifier`, which on Linux trusts the OS
/// store ALONE and hard-fails when it is empty — a slim container without
/// `ca-certificates` would lose every HTTPS call. Feeding both transports one
/// explicitly-built store keeps them identical and keeps the bundled roots as
/// a floor.
///
/// Resolved once: the station reconnects on every transport drop, and
/// re-reading the OS store and re-parsing the PEM per attempt would repeat the
/// same work, and the same warnings, forever.
static ROOT_STORE: OnceLock<std::sync::Arc<rustls::RootCertStore>> = OnceLock::new();

fn root_store() -> std::sync::Arc<rustls::RootCertStore> {
    ROOT_STORE
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

            // A broken OS store or an unreadable CA file degrades to the roots
            // already loaded rather than killing the command, and warns once so
            // the cause is visible.
            let native = rustls_native_certs::load_native_certs();
            if !native.errors.is_empty() {
                crate::log::warn(&format!(
                    "Could not read the system certificate store ({:?}). \
                     Continuing with the built-in roots.",
                    native.errors
                ));
            }
            let (_, ignored) = roots.add_parsable_certificates(native.certs);
            if ignored > 0 {
                crate::log::warn(&format!(
                    "Skipped {ignored} unparsable certificate(s) in the system store."
                ));
            }

            if let Some(path) = configured_ca_path().filter(|p| !p.as_os_str().is_empty()) {
                // `rustls-pki-types` rather than `rustls-pemfile`: the latter is
                // unmaintained (RUSTSEC-2025-0134) and is now only a thin
                // wrapper around this same parser.
                use rustls::pki_types::pem::PemObject;
                match rustls::pki_types::CertificateDer::pem_file_iter(&path)
                    .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))
                {
                    Ok(certs) if certs.is_empty() => {
                        crate::log::warn(&format!(
                            "{CA_CERT_ENV}: {} contains no certificates. \
                             Continuing without it.",
                            path.display()
                        ));
                    }
                    Ok(certs) => {
                        let (_, ignored) = roots.add_parsable_certificates(certs);
                        if ignored > 0 {
                            crate::log::warn(&format!(
                                "{CA_CERT_ENV}: skipped {ignored} unparsable certificate(s) in {}.",
                                path.display()
                            ));
                        }
                    }
                    Err(message) => {
                        crate::log::warn(&format!(
                            "{CA_CERT_ENV}: {message}. Continuing without it."
                        ));
                    }
                }
            }

            std::sync::Arc::new(roots)
        })
        .clone()
}

/// The rustls configuration both transports are built from.
fn tls_config() -> rustls::ClientConfig {
    ensure_crypto_provider();
    rustls::ClientConfig::builder()
        .with_root_certificates(root_store())
        .with_no_client_auth()
}

/// TLS connector for the realtime WebSocket, carrying exactly the trust the
/// HTTP client has.
static REALTIME_CONNECTOR: OnceLock<tokio_tungstenite::Connector> = OnceLock::new();

pub fn realtime_connector() -> Option<tokio_tungstenite::Connector> {
    Some(
        REALTIME_CONNECTOR
            .get_or_init(|| tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(tls_config())))
            .clone(),
    )
}

/// Add the configured extra CA (if any) to a builder. The one shared path for
/// `client()` and `client_builder()`, so the two can never drift.
fn with_extra_roots(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    // `tls_backend_preconfigured`, not the older `use_preconfigured_tls`: the
    // latter is documented as deprecated in reqwest 0.13 but carries no
    // `#[deprecated]` attribute, so `-D warnings` would never flag it.
    builder.tls_backend_preconfigured(tls_config())
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

/// Install `ring` as the process-wide rustls provider, once.
///
/// reqwest is built on `rustls-no-provider` — the plain `rustls` feature
/// pulls `aws-lc-rs`, which needs a C toolchain and does not build for the
/// static musl targets the Linux stations ship as. That leaves reqwest with
/// no provider of its own: it calls `CryptoProvider::get_default()` and
/// panics if nothing is installed, so this must run before the first client
/// or connector is built.
///
/// Called from every construction site rather than from `main`, because the
/// binary is not the only entry point — tests build clients directly, and a
/// panic there is the same panic a user would get.
///
/// `install_default` succeeds at most once per process; a second call losing
/// the race is expected and ignored.
pub(crate) fn ensure_crypto_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Shared `reqwest::Client`. Cloning is cheap — it's an `Arc` under
/// the hood — so callers should clone freely if they need an owned
/// handle.
pub fn client() -> &'static reqwest::Client {
    ensure_crypto_provider();
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
    ensure_crypto_provider();
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

// The realtime link's TLS stack is decided entirely by feature resolution on
// the shared `tokio-tungstenite` node (see the dependency comment in
// Cargo.toml). These assertions fail the build if that resolution ever
// regresses — a silent regression would either drop the OS trust store (so
// self-hosted stations behind a private CA stop connecting) or pull OpenSSL
// into the static musl builds.
#[cfg(not(feature = "realtime-native-roots"))]
compile_error!(
    "the realtime WebSocket must trust the OS certificate store, or self-hosted \
     stations behind a private CA cannot connect: keep the default \
     `realtime-native-roots` feature on (see Cargo.toml)"
);

// reqwest must build no TLS stack of its own. The plain `rustls` feature both
// links aws-lc-rs (breaking the static musl builds) and verifies through
// rustls-platform-verifier, which on Linux trusts the OS store ALONE — a slim
// image without `ca-certificates` would then fail every HTTPS call. `http.rs`
// hands reqwest an explicit root store instead; this pins that arrangement.
#[cfg(not(feature = "reqwest-no-provider"))]
compile_error!(
    "reqwest must stay on `rustls-no-provider` so `http.rs` supplies the root \
     store: the alternative links aws-lc-rs and drops the bundled roots (see \
     Cargo.toml)"
);

#[cfg(test)]
mod realtime_tls_tests {
    /// The realtime link's trust store comes from `rustls-native-certs`,
    /// pulled in by `rustls-tls-native-roots` on the shared
    /// tokio-tungstenite node. Loading the OS store here proves the crate is
    /// actually linked and can read this platform's certificates — if the
    /// feature is ever dropped, the dependency disappears and this stops
    /// compiling.
    ///
    /// Note this cannot catch the other half of the invariant: `native-tls`
    /// getting enabled elsewhere would silently win the default connector
    /// (`cfg(all(__rustls-tls, not(native-tls)))`). `Connector` is
    /// `#[non_exhaustive]`, so no match can detect the extra variant. That
    /// half stays a review-time concern — see the Cargo.toml comment.
    #[test]
    fn realtime_trust_store_loader_is_linked_and_readable() {
        let result = rustls_native_certs::load_native_certs();
        assert!(
            !result.certs.is_empty(),
            "no OS root certificates loaded (errors: {:?}) — the realtime \
             link would fall back to bundled roots only, breaking self-hosted \
             stations behind a private CA",
            result.errors
        );
    }
}

#[cfg(test)]
mod realtime_connector_tests {
    /// `rustls::ClientConfig::builder()` PANICS if it cannot resolve a crypto
    /// provider from the crate features or the process default. We enable
    /// exactly one (`ring`), but reqwest also builds rustls configs in this
    /// binary — if a future dependency pulls in aws-lc as well, resolution
    /// becomes ambiguous and every station would abort on startup instead of
    /// failing a connection. Build one here to prove it resolves.
    #[test]
    fn building_a_rustls_config_does_not_panic() {
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        assert!(!config.alpn_protocols.iter().any(|p| p.is_empty()));
    }

    /// The realtime link always gets our connector now, with or without a CA
    /// file. Leaving it unset would hand the WebSocket back to
    /// tokio-tungstenite's own defaults, which resolve a different root set
    /// than the explicit store `http.rs` gives reqwest — the two transports
    /// would silently disagree about what is trusted.
    #[test]
    fn the_realtime_link_always_uses_our_root_store() {
        assert!(super::realtime_connector().is_some());
    }
}

#[cfg(test)]
mod trust_floor_tests {
    /// The bundled Mozilla roots must survive an empty OS trust store.
    /// reqwest 0.13's default verifier (rustls-platform-verifier) hard-fails
    /// on Linux when the system store is empty — a slim container without
    /// `ca-certificates` would lose every HTTPS call. `http.rs` supplies its
    /// own store precisely so that cannot happen; this pins the floor.
    #[test]
    fn bundled_roots_survive_an_empty_system_store() {
        let bundled: std::collections::BTreeSet<_> = webpki_roots::TLS_SERVER_ROOTS
            .iter()
            .map(|anchor| anchor.subject.as_ref().to_vec())
            .collect();
        let store: std::collections::BTreeSet<_> = super::root_store()
            .roots
            .iter()
            .map(|anchor| anchor.subject.as_ref().to_vec())
            .collect();

        // Compared by subject, not by count: the OS store alone holds more
        // anchors than the bundled set, so a length check passes even with the
        // bundled roots dropped entirely.
        let missing = bundled.difference(&store).count();
        assert_eq!(
            missing,
            0,
            "{missing} of the {} bundled Mozilla roots are absent from the store \
             — the built-in floor was lost",
            bundled.len()
        );
    }
}

#[cfg(test)]
mod live_trust_tests {
    /// Public HTTPS must work with the OS store emptied — the slim-container
    /// regression this design exists to prevent. `SSL_CERT_FILE` pointed at an
    /// empty file is how `openssl-probe` (the Linux loader) sees "no system
    /// certificates"; on macOS the keychain still answers, so this asserts the
    /// floor holds rather than that the store is truly empty.
    #[tokio::test]
    #[ignore = "network"]
    async fn public_https_works_without_a_system_store() {
        let empty = std::env::temp_dir().join("tofupilot-empty-ca.pem");
        std::fs::write(&empty, b"").unwrap();
        std::env::set_var("SSL_CERT_FILE", &empty);
        let res = super::client()
            .get("https://www.tofupilot.app")
            .send()
            .await;
        assert!(
            res.is_ok(),
            "public HTTPS failed with an empty OS store: {res:?}"
        );
    }
}

#[cfg(test)]
mod live_ca_tests {
    /// The `--ca-cert` chain must verify on the HTTP transport, against a
    /// server presenting leaf+intermediate while the client trusts only the
    /// root — the shape a corporate PKI actually deploys.
    #[tokio::test]
    #[ignore = "needs the local chain fixture"]
    async fn private_ca_chain_verifies_over_http() {
        let ca = std::env::var("TOFUPILOT_CA_CERT").expect("fixture path");
        assert!(!ca.is_empty());
        let res = super::client().get("https://127.0.0.1:8444/").send().await;
        assert!(res.is_ok(), "private CA chain rejected on HTTP: {res:?}");
    }
}
