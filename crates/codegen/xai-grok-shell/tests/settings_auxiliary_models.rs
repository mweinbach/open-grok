use xai_grok_shell::util::config::{MAX_DEFAULT_MODEL_LEN, set_memory_model, set_recap_model};

fn read_config(root: &std::path::Path) -> toml::Value {
    let text = std::fs::read_to_string(root.join("config.toml")).expect("config.toml must exist");
    toml::from_str(&text).expect("written config must be valid TOML")
}

#[tokio::test]
async fn auxiliary_model_pins_persist_and_clear_independently() {
    let home = tempfile::tempdir().expect("temporary OPENGROK_HOME");
    unsafe {
        std::env::set_var("OPENGROK_HOME", home.path());
    }

    set_recap_model("gpt-5.6-terra".to_string())
        .await
        .expect("recap model should persist");
    set_memory_model("grok-4.5".to_string())
        .await
        .expect("memory model should persist");

    let config = read_config(home.path());
    assert_eq!(config["models"]["recap"].as_str(), Some("gpt-5.6-terra"));
    assert_eq!(config["models"]["memory"].as_str(), Some("grok-4.5"));

    set_recap_model(String::new())
        .await
        .expect("empty recap model should clear the pin");
    let config = read_config(home.path());
    assert!(config["models"].get("recap").is_none());
    assert_eq!(config["models"]["memory"].as_str(), Some("grok-4.5"));

    set_memory_model(String::new())
        .await
        .expect("empty memory model should clear the pin");
    let config = read_config(home.path());
    assert!(config["models"].get("memory").is_none());

    let error = set_recap_model("x".repeat(MAX_DEFAULT_MODEL_LEN + 1))
        .await
        .expect_err("oversized model IDs must be rejected");
    assert!(error.to_string().contains("recap model id too long"));
}
