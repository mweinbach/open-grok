use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Why a provider has no bearer for a direct `api.x.ai` call (Imagine, voice), which resolves only
/// an xAI API key or xAI OAuth2 token. Neither variant is a cue to fall back to another credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SideCallBearerError {
    #[error(
        "this account's login is not an xAI credential; image, video, and voice need an xAI API key or an xAI login"
    )]
    ForeignSession,
    #[error("no xAI credential available; run `open-grok login` or set XAI_API_KEY")]
    Missing,
}

/// Resolves the current API key for tool HTTP requests.
pub trait ApiKeyProvider: Send + Sync + 'static {
    /// Sync cached read (no refresh). Override point for static providers.
    fn current_api_key(&self) -> Option<String>;

    /// Per-request resolve. `AuthManager` overrides this to drive the
    /// refresh chain; default delegates to the sync method.
    fn current_api_key_async(&self) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        Box::pin(std::future::ready(self.current_api_key()))
    }

    /// Bearer for a direct call to an xAI host (Imagine, voice), or why there is none.
    ///
    /// The default refuses: a provider that cannot say whose credential it holds must not have it
    /// sent to `api.x.ai`. A provider that can classify its credential overrides this.
    fn side_call_bearer(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<String, SideCallBearerError>> + Send + '_>> {
        Box::pin(std::future::ready(Err(SideCallBearerError::Missing)))
    }
}

/// Shared provider used across tool clients.
pub type SharedApiKeyProvider = Arc<dyn ApiKeyProvider>;

/// Resolve the bearer for the next request from the provider.
pub(crate) async fn resolve_bearer(provider: Option<&SharedApiKeyProvider>) -> Option<String> {
    match provider {
        Some(p) => p.current_api_key_async().await,
        None => None,
    }
}
