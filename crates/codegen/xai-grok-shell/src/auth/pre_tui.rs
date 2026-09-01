use std::sync::Arc;

use super::flow::{apply_post_login_config, report_signed_in, run_external_auth_provider};
use super::{AuthManager, GrokAuth, GrokComConfig, SilentRefresh};
use crate::util::grok_home;

#[derive(Debug)]
pub enum PreTuiLoginOutcome {
    Skipped,
    SignedIn(Box<GrokAuth>),
}

pub async fn maybe_run_pre_tui_external_login(
    grok_com_config: &GrokComConfig,
    force_login: bool,
    stdin_is_tty: bool,
) -> anyhow::Result<PreTuiLoginOutcome> {
    let Some(command) = grok_com_config.auth_provider_command.as_deref() else {
        return Ok(PreTuiLoginOutcome::Skipped);
    };
    if !stdin_is_tty {
        return Ok(PreTuiLoginOutcome::Skipped);
    }
    let manager = Arc::new(AuthManager::new(
        &grok_home::grok_home(),
        grok_com_config.clone(),
    ));
    manager.configure_refresher(Some(command.to_owned()), None);
    if !force_login && matches!(manager.silent_refresh().await, SilentRefresh::Renewed(_)) {
        return Ok(PreTuiLoginOutcome::Skipped);
    }
    let auth = run_pre_tui_external_login_with(&manager, command, force_login).await?;
    report_signed_in(&auth);
    apply_post_login_config(auth.clone()).await?;
    Ok(PreTuiLoginOutcome::SignedIn(Box::new(auth)))
}

async fn run_pre_tui_external_login_with(
    manager: &Arc<AuthManager>,
    command: &str,
    force_login: bool,
) -> anyhow::Result<GrokAuth> {
    let over_stale_credential = force_login || manager.is_expired();
    let (auth, _) =
        run_external_auth_provider(command, manager, over_stale_credential, None).await?;
    Ok(auth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn skips_without_provider_or_terminal() {
        assert!(matches!(
            maybe_run_pre_tui_external_login(&GrokComConfig::default(), false, true)
                .await
                .unwrap(),
            PreTuiLoginOutcome::Skipped
        ));
        let config = GrokComConfig {
            auth_provider_command: Some("must-not-run".into()),
            ..GrokComConfig::default()
        };
        assert!(matches!(
            maybe_run_pre_tui_external_login(&config, true, false)
                .await
                .unwrap(),
            PreTuiLoginOutcome::Skipped
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forced_provider_login_replaces_cached_credential_without_browser_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new(directory.path(), GrokComConfig::default()));
        manager.hot_swap(GrokAuth {
            key: "cached".into(),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..GrokAuth::test_default()
        });
        let auth =
            run_pre_tui_external_login_with(&manager, "printf '%s' fresh-provider-token", true)
                .await
                .unwrap();
        assert_eq!(auth.key, "fresh-provider-token");
        let error = run_pre_tui_external_login_with(&manager, "false", true)
            .await
            .unwrap_err();
        assert!(!format!("{error:#}").contains("Signing in with browser"));
        assert_eq!(manager.current().unwrap().key, "fresh-provider-token");
    }
}
