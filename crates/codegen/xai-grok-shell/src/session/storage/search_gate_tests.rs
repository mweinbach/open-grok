use serial_test::serial;

use super::*;
use xai_grok_config_types::ConfigSource;

#[test]
fn environment_overlay_off_reason_is_reported() {
    assert_eq!(
        session_search_off_reason(ConfigSource::EnvOverlay),
        "an environment overlay"
    );
}

#[test]
#[serial]
fn gate_never_reopens_once_it_closes() {
    let _gate = IndexGateGuard::snapshot();
    GATE.store(UNAPPLIED, Ordering::Release);

    apply_gate(&Resolved::new(true, ConfigSource::Default));
    assert!(is_index_enabled());

    apply_gate(&Resolved::new(false, ConfigSource::Env));
    apply_gate(&Resolved::new(true, ConfigSource::Requirement));

    assert!(
        !is_index_enabled(),
        "a switch that turned it off cannot be undone in place"
    );
}
