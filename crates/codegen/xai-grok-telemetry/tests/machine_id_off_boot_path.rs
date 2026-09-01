#[test]
fn env_override_pins_the_agent_id_without_persisting_it() {
    if std::env::var_os("OPENGROK_TEST_AGENT_ID_CHILD").is_some() {
        assert_eq!(xai_grok_telemetry::id::agent_id(), "pinned-agent-id");
        let home = std::env::var_os("OPENGROK_HOME").expect("isolated home");
        assert!(!std::path::PathBuf::from(home).join("agent_id").exists());
        return;
    }
    let home = tempfile::tempdir().expect("tempdir");
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "env_override_pins_the_agent_id_without_persisting_it",
        ])
        .env("OPENGROK_TEST_AGENT_ID_CHILD", "1")
        .env("OPENGROK_HOME", home.path())
        .env("OPENGROK_AGENT_ID", "pinned-agent-id")
        .env("GROK_AGENT_ID", "must-not-cross-from-upstream")
        .status()
        .expect("run child");
    assert!(status.success());
}
