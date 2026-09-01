#[test]
fn crypto_provider_initialization_is_idempotent_and_preserves_host_selection() {
    let mut selected = rustls::crypto::aws_lc_rs::default_provider();
    selected.cipher_suites.reverse();
    let selected_suites = selected.cipher_suites.clone();
    selected.install_default().expect("first install wins");
    xai_grok_extra_ca::ensure_default_crypto_provider();
    xai_grok_extra_ca::ensure_default_crypto_provider();
    assert_eq!(
        rustls::crypto::CryptoProvider::get_default()
            .unwrap()
            .cipher_suites,
        selected_suites
    );
    let _ = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
}
