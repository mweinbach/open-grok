//! Verify `requirements.toml` can pin the base sandbox `profile`.

use xai_grok_shell::agent::config::{ConfigSource, SandboxSettingsConfig};
use xai_grok_test_support::EnvGuard;

#[test]
#[serial_test::serial]
fn new_sessions_default_to_the_workspace_sandbox() {
    let _environment = EnvGuard::unset("GROK_SANDBOX");
    let resolved = SandboxSettingsConfig::default().resolve_profile(None, None);
    assert_eq!(resolved.value, "workspace");
    assert_eq!(resolved.source, ConfigSource::Default);
}

#[test]
#[serial_test::serial]
fn explicit_cli_off_can_disable_the_workspace_default() {
    let _environment = EnvGuard::unset("GROK_SANDBOX");
    let resolved = SandboxSettingsConfig::default().resolve_profile(Some("off"), None);
    assert_eq!(resolved.value, "off");
    assert_eq!(resolved.source, ConfigSource::Cli);
}

#[test]
#[serial_test::serial]
fn configured_off_can_disable_the_workspace_default() {
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
fn environment_profile_overrides_the_workspace_default() {
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
    let resolved = config.resolve_profile(None, Some("strict"));
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
