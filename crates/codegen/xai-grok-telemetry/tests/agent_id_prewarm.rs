#[test]
fn prefetched_agent_id_resolves_and_persists() {
    if std::env::var_os("OPENGROK_TEST_AGENT_ID_CHILD").is_some() {
        xai_grok_telemetry::id::prefetch_agent_id();
        let identifier = xai_grok_telemetry::id::agent_id();
        let home = std::env::var_os("OPENGROK_HOME").expect("isolated home");
        assert_eq!(
            std::fs::read_to_string(std::path::PathBuf::from(home).join("agent_id"))
                .expect("agent_id cache")
                .trim(),
            identifier
        );
        return;
    }
    let home = tempfile::tempdir().expect("tempdir");
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "prefetched_agent_id_resolves_and_persists"])
        .env("OPENGROK_TEST_AGENT_ID_CHILD", "1")
        .env("OPENGROK_HOME", home.path())
        .env_remove("OPENGROK_AGENT_ID")
        .status()
        .expect("run child");
    assert!(status.success());
}
