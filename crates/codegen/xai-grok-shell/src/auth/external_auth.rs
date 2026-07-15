use crate::auth::{AuthMode, GrokAuth};

#[derive(serde::Deserialize)]
pub(crate) struct ExternalAuthOutput {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// Parse process output (stdout) into a `GrokAuth`. Accepts bare token or JSON.
pub(crate) fn parse_output(output: &std::process::Output) -> anyhow::Result<GrokAuth> {
    if !output.status.success() {
        anyhow::bail!("exited with {}", output.status);
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if stdout.is_empty() {
        anyhow::bail!("produced no output on stdout");
    }

    let (token, refresh_token, expires_at) =
        if let Ok(parsed) = serde_json::from_str::<ExternalAuthOutput>(&stdout) {
            tracing::debug!(
                has_refresh_token = parsed.refresh_token.is_some(),
                expires_in = ?parsed.expires_in,
                "auth: parsed external provider output as JSON"
            );
            let expires_at = parsed
                .expires_in
                .map(|secs| chrono::Utc::now() + chrono::Duration::seconds(secs as i64));
            (parsed.access_token, parsed.refresh_token, expires_at)
        } else {
            tracing::debug!(
                stdout_len = stdout.len(),
                "auth: treating output as bare token"
            );
            (stdout, None, None)
        };

    Ok(GrokAuth {
        key: token,
        auth_mode: AuthMode::External,
        create_time: chrono::Utc::now(),
        user_id: String::new(),
        email: None,
        first_name: None,
        last_name: None,
        profile_image_asset_id: None,
        principal_type: None,
        principal_id: None,
        team_id: None,
        team_name: None,
        team_role: None,
        organization_id: None,
        organization_name: None,
        organization_role: None,
        user_blocked_reason: None,
        team_blocked_reasons: vec![],
        coding_data_retention_opt_out: false,
        has_grok_code_access: None,
        refresh_token,
        expires_at,
        oidc_issuer: None,
        oidc_client_id: None,
    })
}

/// Sync version for mid-session refresh. 5s timeout for refresh, 60s for initial.
pub(crate) fn run_external_auth_sync(command: &str, is_refresh: bool) -> Option<GrokAuth> {
    use std::process::{Command, Stdio};

    let timeout_secs = if is_refresh { 5 } else { 60 };

    tracing::info!(cmd = %command, is_refresh, timeout_secs, "auth: running external auth provider (sync)");

    let mut cmd = Command::new("sh");
    cmd.args(["-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Pipe stderr — inherit would corrupt the TUI alternate screen.
        .stderr(Stdio::piped());
    if is_refresh {
        cmd.env("GROK_AUTH_EXPIRED", "1");
    }
    xai_grok_tools::util::detach_std_command(&mut cmd);
    cmd.envs(xai_grok_tools::util::pager_env());
    let mut child = cmd.spawn()
        .map_err(|e| {
            tracing::warn!(error = %e, cmd = %command, "auth: failed to start external auth provider");
            e
        })
        .ok()?;

    let timeout = std::time::Duration::from_secs(timeout_secs);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    tracing::warn!(
                        cmd = %command,
                        timeout_secs,
                        "auth: external auth provider timed out (likely needs interactive auth), killing"
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                tracing::warn!(error = %e, "auth: error waiting for external auth provider");
                return None;
            }
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| {
            tracing::warn!(error = %e, "auth: failed to read external auth provider output");
            e
        })
        .ok()?;

    match parse_output(&output) {
        Ok(auth) => {
            tracing::info!("auth: external auth provider returned fresh token");
            Some(auth)
        }
        Err(e) => {
            tracing::warn!(error = %e, "auth: external auth provider failed");
            None
        }
    }
}

/// Run external auth provider, carrying forward `/user`-derived fields from previous auth.
pub(crate) fn refresh_with_command(command: &str, prev_auth: &GrokAuth) -> Option<GrokAuth> {
    let mut auth = run_external_auth_sync(command, true)?;
    auth.carry_user_profile_from(prev_auth);
    Some(auth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_nonzero_exit_is_err() {
        let output = std::process::Output {
            status: std::process::Command::new("false").status().unwrap(),
            stdout: b"token".to_vec(),
            stderr: vec![],
        };
        assert!(parse_output(&output).is_err());
    }

    #[test]
    fn parse_output_empty_stdout_is_err() {
        let output = std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: b"  \n".to_vec(),
            stderr: vec![],
        };
        assert!(parse_output(&output).is_err());
    }

    #[test]
    fn parse_output_malformed_json_falls_back_to_bare() {
        let output = std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: b"{not valid json}".to_vec(),
            stderr: vec![],
        };
        let auth = parse_output(&output).unwrap();
        assert_eq!(auth.key, "{not valid json}");
    }

    #[test]
    fn sync_spawn_failure_returns_none() {
        assert!(run_external_auth_sync("/nonexistent/binary", false).is_none());
    }

    #[test]
    fn sync_sets_grok_auth_expired_env_on_refresh() {
        let auth = run_external_auth_sync("echo $GROK_AUTH_EXPIRED", true).unwrap();
        assert_eq!(auth.key, "1");
    }

    #[test]
    fn refresh_carries_zdr_flags_forward() {
        let prev = GrokAuth {
            user_blocked_reason: Some("BLOCKED_REASON_OTHER".into()),
            team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
            coding_data_retention_opt_out: true,
            organization_id: Some("org-1".into()),
            ..GrokAuth::test_default()
        };
        let auth = refresh_with_command("echo fresh-token", &prev).unwrap();
        assert_eq!(auth.key, "fresh-token");
        assert!(auth.is_zdr_team(), "ZDR flag must survive refresh");
        assert!(auth.coding_data_retention_opt_out);
        assert_eq!(
            auth.user_blocked_reason.as_deref(),
            Some("BLOCKED_REASON_OTHER")
        );
        assert_eq!(auth.user_id, "test-user", "profile must survive refresh");
        assert_eq!(auth.organization_id.as_deref(), Some("org-1"));
    }

    #[test]
    fn sync_refresh_interactive_times_out() {
        // Binary writes link to stderr then blocks — 5s refresh timeout kills it.
        let cmd = r#"echo 'Visit http://example.com/auth' >&2; sleep 20; echo token"#;
        let start = std::time::Instant::now();
        let result = run_external_auth_sync(cmd, true);
        let elapsed = start.elapsed();
        assert!(result.is_none(), "should timeout and return None");
        assert!(
            elapsed.as_secs() < 10,
            "refresh should use 5s timeout, not 60s (took {}s)",
            elapsed.as_secs()
        );
    }
}
