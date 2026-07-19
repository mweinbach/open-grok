use std::fs;

#[tokio::test]
async fn swarm_mode_persists_in_ui_config() {
    let home = tempfile::tempdir().expect("temporary Open Grok home");
    unsafe {
        std::env::set_var("OPENGROK_HOME", home.path());
        std::env::set_var("HOME", home.path());
        std::env::set_var("USERPROFILE", home.path());
    }

    xai_grok_shell::util::config::set_swarm_mode(true)
        .await
        .expect("enable swarm mode");
    let raw = fs::read_to_string(home.path().join("config.toml")).expect("read config.toml");
    let toml: toml::Value = toml::from_str(&raw).expect("parse config.toml");
    let config = xai_grok_shell::util::config::load_config_from_toml(&toml);
    assert_eq!(config.ui.swarm_mode, Some(true));

    xai_grok_shell::util::config::set_swarm_mode(false)
        .await
        .expect("disable swarm mode");
    let raw = fs::read_to_string(home.path().join("config.toml")).expect("read config.toml");
    let toml: toml::Value = toml::from_str(&raw).expect("parse config.toml");
    let config = xai_grok_shell::util::config::load_config_from_toml(&toml);
    assert_eq!(config.ui.swarm_mode, Some(false));
}
