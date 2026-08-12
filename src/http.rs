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

/// TLS connector for the realtime WebSocket, mirroring the trust the HTTP
/// client gets: bundled Mozilla roots, the OS certificate store, and the
/// configured CA file. `None` when no CA is configured — tokio-tungstenite
/// then builds its own default connector, which already covers the first two.
///
/// Built once: the station reconnects on every transport drop, and re-reading
/// plus re-parsing the PEM on each attempt would repeat the same work (and the
/// same warning) forever.
static REALTIME_CONNECTOR: OnceLock<Option<tokio_tungstenite::Connector>> = OnceLock::new();

pub fn realtime_connector() -> Option<tokio_tungstenite::Connector> {
    REALTIME_CONNECTOR
        .get_or_init(|| {
            let path = configured_ca_path().filter(|p| !p.as_os_str().is_empty())?;

            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

            // Same policy as the HTTP path: a broken OS store or an unreadable
            // CA file degrades to the roots we do have rather than killing the
            // command, and warns once so the cause is visible.
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

            // `rustls-pki-types` rather than `rustls-pemfile`: the latter is
            // unmaintained (RUSTSEC-2025-0134) and is now only a thin wrapper
            // around this same parser.
            use rustls::pki_types::pem::PemObject;
            match rustls::pki_types::CertificateDer::pem_file_iter(&path)
                .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
                .map_err(|e| format!("cannot read {}: {e}", path.display()))
            {
                Ok(certs) if certs.is_empty() => {
                    crate::log::warn(&format!(
                        "{CA_CERT_ENV}: {} contains no certificates. \
                         The realtime link will not trust it.",
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
                        "{CA_CERT_ENV}: {message}. The realtime link will not trust it."
                    ));
                }
            }

            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Some(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(
                config,
            )))
        })
        .clone()
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

// HTTP must trust exactly what the realtime WebSocket trusts. reqwest tracks
// its two root sets independently, so dropping either one leaves the transports
// disagreeing: without the OS store a machine-wide corporate CA brings the
// station "online" while every upload fails; without webpki roots the public
// cloud starts depending on the machine's store.
#[cfg(not(feature = "reqwest-webpki-roots"))]
compile_error!(
    "reqwest lost its webpki roots: public hosts would depend on the OS \
     certificate store (see Cargo.toml)"
);
#[cfg(not(feature = "reqwest-native-roots"))]
compile_error!(
    "reqwest lost the OS certificate store: a machine-wide CA would work for \
     the realtime link but fail every HTTP call (see Cargo.toml)"
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

    /// No CA configured must leave the connector unset, so tokio-tungstenite
    /// keeps its own default. Guards against a regression that would force our
    /// connector on every user, including the public cloud.
    #[test]
    fn no_ca_configured_means_no_connector() {
        if super::configured_ca_path().is_some() {
            return; // developer machine has TOFUPILOT_CA_CERT set
        }
        assert!(super::realtime_connector().is_none());
    }
}
