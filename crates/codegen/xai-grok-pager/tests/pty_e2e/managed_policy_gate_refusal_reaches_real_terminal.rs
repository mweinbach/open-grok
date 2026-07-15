// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// The fail-closed gate refuses during bootstrap, after the TUI redirects fd 2 to `/dev/null`; its refusal must
/// still reach the real terminal (a regression that doesn't restore fd 2 leaves a managed user a blank `exit 1`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run with cargo test -p xai-grok-pager --test pty_e2e -- --ignored"]
async fn managed_policy_gate_refusal_reaches_real_terminal() {
    let home = tempfile::tempdir().expect("tempdir");
    let home_path = home.path();
    std::fs::write(
        home_path.join("config.toml"),
        // Dead local port so any incidental fetch fails fast offline (the gate is synchronous anyway).
        "[endpoints]\n\
         deployment_key = \"KEY-AAA\"\n\
         managed_config_url = \"http://127.0.0.1:1/deployment/config\"\n\
         cli_chat_proxy_base_url = \"http://127.0.0.1:1\"\n",
    )
    .expect("write config.toml");
    std::fs::write(
        home_path.join("managed_config.toml"),
        "[cli]\ntheme = \"dark\"\n",
    )
    .expect("write managed_config.toml");
    // requirements.toml is deliberately absent (served-then-deleted).
    let synced_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    std::fs::write(
        home_path.join("managed_config_cache.json"),
        format!(
            "{{\"synced_at\":{synced_at},\"principal\":\"deploy-A\",\
             \"had_managed_config\":true,\"had_requirements\":true,\
             \"key_fingerprint\":null,\"fail_closed\":true}}"
        ),
    )
    .expect("write marker");

    let binary = pager_binary().expect("resolve pager binary");
    let home_str = home_path.to_str().expect("utf8 home path");
    let mut harness = PtyHarness::new(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &["--no-auto-update"],
        // GROK_MANAGED_CONFIG=0 disables the background refetch so the gate decision is deterministic and offline.
        &[
            ("GROK_HOME", home_str),
            ("GROK_MANAGED_CONFIG", "0"),
            ("NO_COLOR", "1"),
        ],
    )
    .expect("spawn pager");

    // The gate refuses synchronously and exits; drain output, capturing the exit code once.
    let gate_msg = "Managed policy is required for this account";
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut exit_code = None;
    while Instant::now() < deadline {
        harness.update(Duration::from_millis(100));
        // Poll non-blocking; `wait_exit_code` reaps, so capture it exactly once.
        if exit_code.is_none() {
            exit_code = harness.wait_exit_code(Duration::ZERO);
            if exit_code.is_some() {
                harness.update(Duration::from_millis(200)); // final drain after exit
                break;
            }
        }
    }

    let raw = String::from_utf8_lossy(harness.raw_output()).into_owned();
    assert!(
        harness.contains_text(gate_msg) || raw.contains(gate_msg),
        "fail-closed gate refusal must reach the real terminal (fd 2 restored \
         from the /dev/null redirect); screen:\n{}\nraw:\n{raw}",
        harness.screen_contents()
    );
    assert_eq!(
        exit_code,
        Some(1),
        "fail-closed gate must exit 1 (refusal); got {exit_code:?}\nraw:\n{raw}"
    );
}
