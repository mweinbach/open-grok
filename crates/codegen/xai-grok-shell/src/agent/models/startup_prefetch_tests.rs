use super::*;
use crate::agent::config::{ConfigModelOverride, EnvKeys};
use xai_grok_sampling_types::ModelProvider;

fn configured_model(provider: ModelProvider) -> Config {
    let mut config = Config::default();
    config.default_model_override = Some("startup-model".into());
    config.config_models.insert(
        "startup-model".into(),
        ConfigModelOverride {
            model: Some("same-routing-slug".into()),
            provider: Some(provider),
            base_url: Some("https://api.x.ai/v1".into()),
            ..Default::default()
        },
    );
    config
}

fn marker() -> Option<RemoteSettings> {
    Some(RemoteSettings {
        path_not_found_hints: Some(true),
        ..RemoteSettings::default()
    })
}

fn cell(finished: bool) -> Arc<Inflight> {
    Arc::new(Inflight {
        origin: "https://startup.invalid".into(),
        receipt: None,
        state: Mutex::new(State {
            finished,
            settings: marker(),
            ..State::default()
        }),
        done: Condvar::new(),
    })
}

#[test]
fn startup_selection_requires_xai_metadata_not_a_slug_or_backend() {
    assert!(selected_xai_model(&configured_model(ModelProvider::Xai)).is_some());
    for provider in [
        ModelProvider::Codex,
        ModelProvider::Kimi,
        ModelProvider::Fireworks,
        ModelProvider::DeepSeek,
        ModelProvider::Custom,
    ] {
        let mut config = configured_model(provider);
        config
            .config_models
            .get_mut("startup-model")
            .unwrap()
            .api_backend = Some(xai_grok_sampling_types::ApiBackend::Responses);
        assert!(selected_xai_model(&config).is_none());
    }
    let mut unknown = Config::default();
    unknown.default_model_override = Some("unknown-grok-model".into());
    assert!(selected_xai_model(&unknown).is_none());
    let mut unselected = Config::default();
    unselected.default_model_override = None;
    unselected.models.default = None;
    assert!(selected_xai_model(&unselected).is_none());
}

#[test]
fn startup_selection_rejects_explicit_model_credentials() {
    for (api_key, env_key) in [
        (Some("fixture-only-key".into()), None),
        (None, Some(EnvKeys::single("UNSET_STARTUP_FIXTURE_KEY"))),
    ] {
        let mut config = configured_model(ModelProvider::Xai);
        let entry = config.config_models.get_mut("startup-model").unwrap();
        entry.api_key = api_key;
        entry.env_key = env_key;
        assert!(selected_xai_model(&config).is_none());
    }
}

#[test]
#[serial_test::serial(remote_sig_disarm)]
fn begin_does_not_replace_an_inflight_fetch() {
    clear_for_tests();
    let registered = cell(true);
    *INFLIGHT.lock().unwrap() = Some(registered.clone());
    begin_inner(&configured_model(ModelProvider::Xai), || {
        panic!("existing fetch must not resolve auth")
    });
    assert!(Arc::ptr_eq(
        INFLIGHT.lock().unwrap().as_ref().unwrap(),
        &registered
    ));
    clear_for_tests();
}

#[test]
#[serial_test::serial(remote_sig_disarm)]
fn accept_rejects_changed_policy_without_retaining_finished_fetch() {
    clear_for_tests();
    *INFLIGHT.lock().unwrap() = Some(cell(true));
    assert!(matches!(accept_if(Duration::ZERO, |_| false), Accept::Miss));
    assert!(!inflight_for_tests());
}

#[test]
#[serial_test::serial(remote_sig_disarm)]
fn accept_deadline_retains_tombstone_and_spends_budget() {
    clear_for_tests();
    *INFLIGHT.lock().unwrap() = Some(cell(false));
    assert!(matches!(
        accept_if(Duration::ZERO, |_| true),
        Accept::Consumed(None)
    ));
    assert!(inflight_for_tests());
    clear_for_tests();
}

#[test]
#[serial_test::serial(remote_sig_disarm)]
fn wait_settings_leaves_results_registered_for_acceptance() {
    clear_for_tests();
    *INFLIGHT.lock().unwrap() = Some(cell(true));
    assert_eq!(
        wait_settings_if(Duration::ZERO, |_| true)
            .and_then(|settings| settings.path_not_found_hints),
        Some(true)
    );
    assert!(inflight_for_tests());
    match accept_if(Duration::ZERO, |_| true) {
        Accept::Consumed(settings) => assert_eq!(
            settings.and_then(|settings| settings.path_not_found_hints),
            Some(true)
        ),
        Accept::Miss => panic!("settings read consumed the shared fetch"),
    }
    assert!(!inflight_for_tests());
}

#[test]
#[serial_test::serial(remote_sig_disarm)]
fn stale_finished_receipt_cannot_remove_a_newer_worker() {
    clear_for_tests();
    let previous = cell(true);
    let current = cell(false);
    *INFLIGHT.lock().unwrap() = Some(current.clone());
    Finished(previous).take();
    assert!(Arc::ptr_eq(
        INFLIGHT.lock().unwrap().as_ref().unwrap(),
        &current
    ));
    clear_for_tests();
}

#[test]
fn panic_marks_the_worker_finished_and_wakes_waiters() {
    let pending = cell(false);
    let worker = pending.clone();
    let failure = std::panic::catch_unwind(move || {
        let _guard = FinishGuard(worker);
        panic!("prefetch fixture panic");
    });
    assert!(failure.is_err());
    let finished = wait_finished(pending, Duration::ZERO).expect("panic must finish the worker");
    assert!(finished.0.state.lock().unwrap().panicked);
}

#[test]
fn receipt_auth_identity_changes_when_user_or_key_changes() {
    let auth = GrokAuth::test_default();
    let mut changed = auth.clone();
    changed.user_id.push_str("-other");
    assert_ne!(auth_identity(&auth), auth_identity(&changed));
    changed = auth.clone();
    changed.key.push_str("-rotated");
    assert_ne!(auth_identity(&auth), auth_identity(&changed));
}
