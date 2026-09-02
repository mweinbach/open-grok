//! Verify `requirements.toml` can pin the base sandbox `profile`.

use xai_grok_shell::agent::config::{ConfigSource, SandboxSettingsConfig};
use xai_grok_test_support::EnvGuard;

#[test]
#[serial_test::serial]
fn new_sessions_default_to_no_sandbox() {
    let _environment = EnvGuard::unset("GROK_SANDBOX");
    let resolved = SandboxSettingsConfig::default().resolve_profile(None, None);
    assert_eq!(resolved.value, "off");
    assert_eq!(resolved.source, ConfigSource::Default);
}

#[test]
#[serial_test::serial]
fn explicit_cli_off_disables_a_configured_sandbox() {
    let _environment = EnvGuard::unset("GROK_SANDBOX");
    let config = SandboxSettingsConfig {
        profile: Some("workspace".to_owned()),
        ..Default::default()
    };
    let resolved = config.resolve_profile(Some("off"), None);
    assert_eq!(resolved.value, "off");
    assert_eq!(resolved.source, ConfigSource::Cli);
}

#[test]
#[serial_test::serial]
fn configured_off_keeps_sandbox_disabled() {
    let _environment = EnvGuard::unset("GROK_SANDBOX");
    let config = SandboxSettingsConfig {
        profile: Some("off".to_owned()),
        ..Default::default()
    };
    let resolved = config.resolve_profile(None, None);
    assert_eq!(resolved.value, "off");
    assert_eq!(resolved.source, ConfigSource::Config);
}

#[test]
#[serial_test::serial]
fn environment_profile_explicitly_enables_sandboxing() {
    let _environment = EnvGuard::set("GROK_SANDBOX", "read-only");
    let resolved = SandboxSettingsConfig::default().resolve_profile(None, None);
    assert_eq!(resolved.value, "read-only");
    assert_eq!(resolved.source, ConfigSource::Env);
}

#[test]
#[serial_test::serial]
fn requirements_pin_profile() {
    let _environment = EnvGuard::unset("GROK_SANDBOX");
    let config = SandboxSettingsConfig::default();
    let resolved = config.resolve_profile(Some("off"), Some("strict"));
    assert_eq!(resolved.value, "strict");
    assert_eq!(resolved.source, ConfigSource::Requirement);
}

#[test]
#[serial_test::serial]
fn cli_flag_overrides_config_but_not_requirement() {
    let _environment = EnvGuard::unset("GROK_SANDBOX");
    let config = SandboxSettingsConfig {
        profile: Some("workspace".to_string()),
        ..Default::default()
    };
    let resolved = config.resolve_profile(Some("read-only"), Some("strict"));
    assert_eq!(resolved.value, "strict");
    assert_eq!(resolved.source, ConfigSource::Requirement);
}

#[test]
#[serial_test::serial]
fn workspace_sandbox_requires_an_explicit_profile() {
    let _environment = EnvGuard::unset("GROK_SANDBOX");
    let resolved = SandboxSettingsConfig::default().resolve_profile(Some("workspace"), None);
    assert_eq!(resolved.value, "workspace");
    assert_eq!(resolved.source, ConfigSource::Cli);

    let config = SandboxSettingsConfig {
        profile: Some("workspace".to_owned()),
        ..Default::default()
    };
    let resolved = config.resolve_profile(None, None);
    assert_eq!(resolved.value, "workspace");
    assert_eq!(resolved.source, ConfigSource::Config);
}

#[tokio::test]
#[serial_test::serial]
async fn sandbox_setting_persists_opt_in_and_opt_out_without_changing_other_settings() {
    use xai_grok_shell::util::config::set_local_feature_flag;

    let home = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("OPENGROK_HOME", home.path());
    let _environment = EnvGuard::unset("GROK_SANDBOX");
    let config_path = home.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[sandbox]\nauto_allow_bash = true\n[ui]\nyolo = false\n",
    )
    .unwrap();

    for (enabled, profile) in [(true, "workspace"), (false, "off")] {
        set_local_feature_flag("sandbox.profile", enabled)
            .await
            .unwrap();
        let root: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        let config: SandboxSettingsConfig = root["sandbox"].clone().try_into().unwrap();
        assert_eq!(config.resolve_profile(None, None).value, profile);
        assert_eq!(config.auto_allow_bash, Some(true));
        assert_eq!(root["ui"]["yolo"].as_bool(), Some(false));
        assert_eq!(config.resolve_profile(None, Some("strict")).value, "strict");
        assert_eq!(
            config.resolve_profile(Some("read-only"), None).value,
            "read-only"
        );
    }

    let contents = "[sandbox\nprofile = \"strict\"\n";
    std::fs::write(&config_path, contents).unwrap();
    assert!(
        xai_grok_shell::util::config::set_local_feature_flag("sandbox.profile", true)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), contents);
}
