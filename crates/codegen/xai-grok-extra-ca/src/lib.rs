//! Shared TLS roots for Open Grok, with `OPENGROK_EXTRA_CA_BUNDLE` and
//! `SSL_CERT_FILE` support.
//!
//! Native and extra roots are parsed once into process-wide caches,
//! additive to Mozilla roots. An empty explicit bundle disables fallback.
//! Each DER is validated with
//! `rustls::RootCertStore::add` before caching so a bad bundle cannot fail
//! `ClientBuilder::build()`. Unreadable/oversized/empty/unparsable → warn and
//! continue. Size cap: [`MAX_EXTRA_CA_BUNDLE_BYTES`].
//!
//! Source of truth is validated DER ([`extra_root_ders`]) so reqwest 0.12
//! (this crate's adapters) and MCP's 0.13 can each build their own
//! `Certificate`s.

use std::io::Read;
use std::sync::{Arc, OnceLock};

use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

/// Hard cap on `OPENGROK_EXTRA_CA_BUNDLE` (1 MiB) — avoids unbounded startup reads.
pub const MAX_EXTRA_CA_BUNDLE_BYTES: u64 = 1024 * 1024;

/// Env var name for the opt-in extra CA bundle (PEM path).
pub const ENV_OPENGROK_EXTRA_CA_BUNDLE: &str = "OPENGROK_EXTRA_CA_BUNDLE";

pub const ENV_SSL_CERT_FILE: &str = "SSL_CERT_FILE";

pub fn ensure_default_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .is_err()
            && !rustls::crypto::CryptoProvider::get_default().is_some_and(|provider| {
                provider.signature_verification_algorithms.supported_schemes()
                    .contains(&rustls::SignatureScheme::ECDSA_NISTP521_SHA512)
            })
        {
            tracing::warn!("the installed rustls provider lacks ECDSA P-521 support; some enterprise proxy certificates will not verify");
        }
    });
}

pub fn build_reqwest_client(
    configure: impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
) -> reqwest::Result<reqwest::Client> {
    ensure_default_crypto_provider();
    let mut builder = configure(reqwest::Client::builder())
        .use_rustls_tls()
        .tls_built_in_native_certs(false)
        .tls_built_in_webpki_certs(true);
    for certificate in shared_reqwest_roots() {
        builder = builder.add_root_certificate(certificate);
    }
    builder.build()
}

pub fn build_blocking_reqwest_client(
    configure: impl Fn(reqwest::blocking::ClientBuilder) -> reqwest::blocking::ClientBuilder,
) -> reqwest::Result<reqwest::blocking::Client> {
    ensure_default_crypto_provider();
    let mut builder = configure(reqwest::blocking::Client::builder())
        .use_rustls_tls()
        .tls_built_in_native_certs(false)
        .tls_built_in_webpki_certs(true);
    for certificate in shared_reqwest_roots() {
        builder = builder.add_root_certificate(certificate);
    }
    builder.build()
}

#[cfg(test)]
static NATIVE_ROOT_LOADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn cached_native_der() -> &'static [CertificateDer<'static>] {
    static CERTS: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();
    CERTS.get_or_init(|| {
        #[cfg(test)]
        NATIVE_ROOT_LOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let native = if std::env::var_os(ENV_SSL_CERT_FILE).is_some() {
            rustls_native_certs::CertificateResult::default()
        } else {
            rustls_native_certs::load_native_certs()
        };
        if !native.errors.is_empty() {
            tracing::warn!(
                native_root_error_count = native.errors.len(),
                "skipping unreadable native root certificates"
            );
        }
        native
            .certs
            .into_iter()
            .filter(|certificate| RootCertStore::empty().add(certificate.clone()).is_ok())
            .collect()
    })
}

fn shared_reqwest_roots() -> impl Iterator<Item = reqwest::Certificate> {
    static ROOTS: OnceLock<Vec<reqwest::Certificate>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            cached_native_der()
                .iter()
                .map(|certificate| certificate.as_ref())
                .chain(extra_root_ders().iter().map(Vec::as_slice))
                .filter_map(|der| {
                    reqwest::Certificate::from_der(der)
                        .inspect_err(|error| {
                            tracing::warn!(%error, "root rejected by reqwest; skipping");
                        })
                        .ok()
                })
                .collect()
        })
        .iter()
        .cloned()
}

pub fn rustls_client_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            ensure_default_crypto_provider();
            let mut roots = RootCertStore::empty();
            roots.add_parsable_certificates(cached_native_der().iter().cloned());
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            roots.add_parsable_certificates(
                extra_root_ders().iter().cloned().map(CertificateDer::from),
            );
            let mut config = rustls::ClientConfig::builder_with_provider(
                rustls::crypto::aws_lc_rs::default_provider().into(),
            )
            .with_safe_default_protocol_versions()
            .expect("aws-lc-rs supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Arc::new(config)
        })
        .clone()
}

/// Process-wide extra roots as validated DER, parsed once.
///
/// Empty when the env var is unset/empty or the file yields no usable certs.
pub fn extra_root_ders() -> &'static [Vec<u8>] {
    bundle_snapshot().ders.as_slice()
}

pub fn configured_bundle_env() -> Option<&'static str> {
    bundle_snapshot().source
}

struct BundleSnapshot {
    source: Option<&'static str>,
    ders: Vec<Vec<u8>>,
}

fn bundle_snapshot() -> &'static BundleSnapshot {
    static SNAPSHOT: OnceLock<BundleSnapshot> = OnceLock::new();
    SNAPSHOT.get_or_init(|| {
        match select_bundle(
            std::env::var_os(ENV_OPENGROK_EXTRA_CA_BUNDLE),
            std::env::var_os(ENV_SSL_CERT_FILE),
        ) {
            Some((source, path)) => BundleSnapshot {
                source: Some(source),
                ders: load_extra_root_ders(&path),
            },
            None => BundleSnapshot {
                source: None,
                ders: Vec::new(),
            },
        }
    })
}

fn select_bundle(
    bundle: Option<std::ffi::OsString>,
    ssl: Option<std::ffi::OsString>,
) -> Option<(&'static str, std::path::PathBuf)> {
    match bundle {
        Some(path) if !path.is_empty() => Some((ENV_OPENGROK_EXTRA_CA_BUNDLE, path.into())),
        Some(_) => None,
        None => ssl
            .filter(|path| !path.is_empty())
            .map(|path| (ENV_SSL_CERT_FILE, path.into())),
    }
}

/// Apply [`extra_root_ders`] to a workspace (reqwest 0.12) async `ClientBuilder`.
pub fn with_extra_root_certificates(mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    ensure_default_crypto_provider();
    for der in extra_root_ders() {
        match reqwest::Certificate::from_der(der) {
            Ok(certificate) => builder = builder.add_root_certificate(certificate),
            Err(error) => tracing::warn!(%error, "extra root rejected by reqwest; skipping"),
        }
    }
    builder
}

/// Apply [`extra_root_ders`] to a workspace (reqwest 0.12) blocking `ClientBuilder`.
pub fn with_extra_root_certificates_blocking(
    mut builder: reqwest::blocking::ClientBuilder,
) -> reqwest::blocking::ClientBuilder {
    ensure_default_crypto_provider();
    for der in extra_root_ders() {
        match reqwest::Certificate::from_der(der) {
            Ok(certificate) => builder = builder.add_root_certificate(certificate),
            Err(error) => tracing::warn!(%error, "extra root rejected by reqwest; skipping"),
        }
    }
    builder
}

fn load_extra_root_ders(path: &std::path::Path) -> Vec<Vec<u8>> {
    let bytes = match read_bundle_capped(path) {
        Ok(b) => b,
        Err(BundleReadError::Io(e)) => {
            // WHY: MITM CA is optional; a missing path must not brick HTTP.
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "OPENGROK_EXTRA_CA_BUNDLE unreadable; continuing without extra roots"
            );
            return Vec::new();
        }
        Err(BundleReadError::TooLarge) => {
            tracing::warn!(
                path = %path.display(),
                max_bytes = MAX_EXTRA_CA_BUNDLE_BYTES,
                "OPENGROK_EXTRA_CA_BUNDLE exceeds size cap; continuing without extra roots"
            );
            return Vec::new();
        }
    };

    let outcome = parse_and_validate_pem(&bytes);
    if outcome.no_pem_blocks {
        tracing::warn!(
            path = %path.display(),
            "OPENGROK_EXTRA_CA_BUNDLE contains no PEM certificate blocks; continuing without extra roots"
        );
        return outcome.accepted;
    }
    if outcome.rejected > 0 {
        tracing::warn!(
            path = %path.display(),
            accepted = outcome.accepted.len(),
            rejected = outcome.rejected,
            "OPENGROK_EXTRA_CA_BUNDLE: dropped unusable certificate block(s)"
        );
    }
    if outcome.accepted.is_empty() {
        tracing::warn!(
            path = %path.display(),
            "OPENGROK_EXTRA_CA_BUNDLE produced zero usable certificates; continuing without extra roots"
        );
    } else {
        tracing::info!(
            path = %path.display(),
            accepted = outcome.accepted.len(),
            "OPENGROK_EXTRA_CA_BUNDLE: loaded extra root certificate(s)"
        );
    }
    outcome.accepted
}

#[derive(Debug)]
enum BundleReadError {
    Io(std::io::Error),
    TooLarge,
}

fn read_bundle_capped(path: &std::path::Path) -> Result<Vec<u8>, BundleReadError> {
    let file = std::fs::File::open(path).map_err(BundleReadError::Io)?;
    let mut buf = Vec::new();
    let n = file
        .take(MAX_EXTRA_CA_BUNDLE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(BundleReadError::Io)?;
    if (n as u64) > MAX_EXTRA_CA_BUNDLE_BYTES {
        return Err(BundleReadError::TooLarge);
    }
    Ok(buf)
}

/// Result of parsing a PEM bundle into rustls-validated DER roots.
#[derive(Debug, Default)]
pub(crate) struct ParseOutcome {
    pub(crate) accepted: Vec<Vec<u8>>,
    /// PEM blocks that failed decode or rustls X.509 validation.
    pub(crate) rejected: usize,
    /// Input (non-empty) contained no PEM certificate blocks at all.
    pub(crate) no_pem_blocks: bool,
}

/// Parse PEM into rustls-validated DER (no env / OnceLock). Input with no PEM
/// certificate blocks (including empty) → empty accepted, zero rejected,
/// `no_pem_blocks` set.
pub(crate) fn parse_and_validate_pem(pem: &[u8]) -> ParseOutcome {
    let mut accepted = Vec::new();
    let mut rejected = 0usize;
    let mut saw_block = false;

    // WHY: reject non-X.509 DER before any ClientBuilder sees it; `add`
    // validates per certificate, so one store serves the whole bundle.
    let mut store = RootCertStore::empty();
    for item in CertificateDer::pem_slice_iter(pem) {
        saw_block = true;
        match item {
            Ok(der) => match store.add(der.clone()) {
                Ok(()) => accepted.push(der.as_ref().to_vec()),
                Err(_) => rejected += 1,
            },
            Err(_) => rejected += 1,
        }
    }

    ParseOutcome {
        accepted,
        rejected,
        no_pem_blocks: !saw_block,
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
