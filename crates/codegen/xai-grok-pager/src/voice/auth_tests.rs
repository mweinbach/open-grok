use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use xai_grok_test_support::EnvGuard;
use xai_grok_tools::types::api_key_provider::{ApiKeyProvider, SideCallBearerError};
use xai_grok_voice::{VoiceAuthError, VoiceAuthProvider};

use super::{AuthManagerVoiceAuth, build_voice_auth, voice_auth_error};

struct StubProvider(Result<String, SideCallBearerError>);

impl ApiKeyProvider for StubProvider {
    fn current_api_key(&self) -> Option<String> {
        self.0.clone().ok()
    }

    fn side_call_bearer(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<String, SideCallBearerError>> + Send + '_>> {
        let result = self.0.clone();
        Box::pin(async move { result })
    }
}

#[test]
fn side_call_errors_map_to_voice_errors() {
    assert_eq!(
        voice_auth_error(SideCallBearerError::ForeignSession),
        VoiceAuthError::ForeignSession
    );
    assert_eq!(
        voice_auth_error(SideCallBearerError::Missing),
        VoiceAuthError::NotSignedIn
    );
}

#[tokio::test]
async fn bearer_serves_xai_credential_and_refuses_foreign_session() {
    let ok = AuthManagerVoiceAuth(Arc::new(StubProvider(Ok("xai-static-key".to_owned()))));
    assert_eq!(ok.bearer().await, Ok("xai-static-key".to_owned()));

    let foreign = AuthManagerVoiceAuth(Arc::new(StubProvider(Err(
        SideCallBearerError::ForeignSession,
    ))));
    assert_eq!(foreign.bearer().await, Err(VoiceAuthError::ForeignSession));

    let missing = AuthManagerVoiceAuth(Arc::new(StubProvider(Err(SideCallBearerError::Missing))));
    assert_eq!(missing.bearer().await, Err(VoiceAuthError::NotSignedIn));
}

/// An empty home serves no bearer: the side-call resolver reports `Missing`,
/// which surfaces as [`VoiceAuthError::NotSignedIn`].
#[tokio::test]
#[serial_test::serial]
async fn empty_home_is_not_signed_in() {
    let _xai = EnvGuard::unset("XAI_API_KEY");
    let _home = EnvGuard::set("OPENGROK_HOME", "/tmp/opengrok-slice-pagercore-voice-auth");
    let dir = tempfile::tempdir().unwrap();
    let mgr = Arc::new(xai_grok_shell::auth::AuthManager::new(
        dir.path(),
        xai_grok_shell::auth::GrokComConfig::default(),
    ));
    let auth = build_voice_auth(mgr);
    assert_eq!(auth.bearer().await, Err(VoiceAuthError::NotSignedIn));
}
