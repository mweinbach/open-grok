//! [`AuthProvider`] that refreshes OIDC tokens before they expire.
//!
//! `current()` checks token expiry and, if needed, performs OIDC
//! discovery + token exchange before returning the credential.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::auth::{AuthCredential, AuthIdentity, AuthProvider};

pub type OnRefreshCallback = Arc<dyn Fn(&RefreshEvent) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct RefreshEvent {
    pub access_token: String,
    pub new_refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

struct TokenState {
    access_token: String,
    refresh_token: String,
    expires_at: Option<DateTime<Utc>>,
}

pub struct OidcAuthProvider {
    state: Mutex<TokenState>,
    issuer: String,
    client_id: String,
    user_id: Option<String>,
    principal_type: Option<String>,
    principal_id: Option<String>,
    on_refresh: Option<OnRefreshCallback>,
}

const REFRESH_MARGIN: Duration = Duration::from_secs(60);

impl std::fmt::Debug for OidcAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcAuthProvider")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

pub struct OidcAuthProviderBuilder {
    access_token: String,
    refresh_token: String,
    issuer: String,
    client_id: String,
    expires_at: Option<DateTime<Utc>>,
    user_id: Option<String>,
    principal_type: Option<String>,
    principal_id: Option<String>,
    on_refresh: Option<OnRefreshCallback>,
}

impl OidcAuthProviderBuilder {
    pub fn new(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        issuer: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Self {
        Self {
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            issuer: issuer.into(),
            client_id: client_id.into(),
            expires_at: None,
            user_id: None,
            principal_type: None,
            principal_id: None,
            on_refresh: None,
        }
    }

    pub fn expires_at(mut self, expires_at: DateTime<Utc>) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Owner user id parsed from the auth source, surfaced via
    /// [`AuthProvider::identity`].
    pub fn user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    pub fn principal_type(mut self, pt: impl Into<String>) -> Self {
        self.principal_type = Some(pt.into());
        self
    }

    pub fn principal_id(mut self, pid: impl Into<String>) -> Self {
        self.principal_id = Some(pid.into());
        self
    }

    pub fn on_refresh(mut self, cb: OnRefreshCallback) -> Self {
        self.on_refresh = Some(cb);
        self
    }

    pub fn build(self) -> OidcAuthProvider {
        OidcAuthProvider {
            state: Mutex::new(TokenState {
                access_token: self.access_token,
                refresh_token: self.refresh_token,
                expires_at: self.expires_at,
            }),
            issuer: self.issuer,
            client_id: self.client_id,
            user_id: self.user_id,
            principal_type: self.principal_type,
            principal_id: self.principal_id,
            on_refresh: self.on_refresh,
        }
    }
}

impl AuthProvider for OidcAuthProvider {
    fn current(&self) -> AuthCredential {
        let expired = {
            let s = self.state.lock();
            s.expires_at.is_some_and(|exp| {
                Utc::now() + chrono::Duration::from_std(REFRESH_MARGIN).unwrap() >= exp
            })
        };
        if expired && let Err(e) = self.try_refresh() {
            tracing::warn!(error = %e, "OIDC refresh failed, using stale token");
        }
        let s = self.state.lock();
        AuthCredential::bearer(&s.access_token)
    }

    /// Surface the principal fields parsed from the auth source. `None` only
    /// when no `user_id` was supplied (nothing to attribute).
    fn identity(&self) -> Option<AuthIdentity> {
        let user_id = self.user_id.clone()?;
        Some(AuthIdentity {
            user_id,
            principal_type: self.principal_type.clone(),
            principal_id: self.principal_id.clone(),
        })
    }
}

impl OidcAuthProvider {
    fn try_refresh(&self) -> Result<(), Box<dyn std::error::Error>> {
        tracing::info!(issuer = %self.issuer, "refreshing OIDC token");
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(self.do_refresh()))
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(self.do_refresh())
        }
    }

    async fn do_refresh(&self) -> Result<(), Box<dyn std::error::Error>> {
        let refresh_token = self.state.lock().refresh_token.clone();
        let issuer = self.issuer.trim_end_matches('/');
        let client = reqwest::Client::new();

        #[derive(serde::Deserialize)]
        struct Discovery {
            token_endpoint: String,
        }

        let disc: Discovery = client
            .get(format!("{issuer}/.well-known/openid-configuration"))
            .timeout(Duration::from_secs(10))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let mut params = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", self.client_id.as_str()),
        ];
        let pt = self.principal_type.clone();
        let pid = self.principal_id.clone();
        if let Some(ref v) = pt {
            params.push(("principal_type", v));
        }
        if let Some(ref v) = pid {
            params.push(("principal_id", v));
        }

        #[derive(serde::Deserialize)]
        struct Tokens {
            access_token: String,
            #[serde(default)]
            refresh_token: Option<String>,
            #[serde(default)]
            expires_in: Option<u64>,
        }

        let tokens: Tokens = client
            .post(&disc.token_endpoint)
            .form(&params)
            .timeout(Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let expires_at = tokens
            .expires_in
            .map(|s| Utc::now() + chrono::Duration::seconds(s as i64));

        tracing::info!(expires_at = ?expires_at, "OIDC token refreshed");

        if let Some(ref cb) = self.on_refresh {
            cb(&RefreshEvent {
                access_token: tokens.access_token.clone(),
                new_refresh_token: tokens.refresh_token.clone(),
                expires_at,
            });
        }

        let mut s = self.state.lock();
        s.access_token = tokens.access_token;
        if let Some(rt) = tokens.refresh_token {
            s.refresh_token = rt;
        }
        s.expires_at = expires_at;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_returns_token_when_not_expired() {
        let provider = OidcAuthProviderBuilder::new(
            "access-tok",
            "refresh-tok",
            "https://auth.example.com",
            "client1",
        )
        .expires_at(Utc::now() + chrono::Duration::hours(1))
        .build();

        let cred = provider.current();
        match cred {
            AuthCredential::Bearer { token } => {
                assert_eq!(token, "access-tok");
            }
            _ => panic!("expected Bearer"),
        }
    }

    #[test]
    fn current_returns_token_when_no_expiry() {
        let provider = OidcAuthProviderBuilder::new(
            "no-expiry-tok",
            "refresh-tok",
            "https://auth.example.com",
            "client1",
        )
        .build();

        let cred = provider.current();
        match cred {
            AuthCredential::Bearer { token } => assert_eq!(token, "no-expiry-tok"),
            _ => panic!("expected Bearer"),
        }
    }

    #[test]
    fn current_returns_stale_token_when_refresh_fails() {
        // Expired token, but issuer is unreachable — should return stale
        let provider = OidcAuthProviderBuilder::new(
            "stale-tok",
            "refresh-tok",
            "https://localhost:1", // unreachable
            "client1",
        )
        .expires_at(Utc::now() - chrono::Duration::hours(1))
        .build();

        let cred = provider.current();
        match cred {
            AuthCredential::Bearer { token } => assert_eq!(token, "stale-tok"),
            _ => panic!("expected Bearer"),
        }
    }

    #[test]
    fn identity_surfaces_principal_fields() {
        let provider = OidcAuthProviderBuilder::new("tok", "rt", "https://auth.example.com", "c1")
            .user_id("user-1")
            .principal_type("Team")
            .principal_id("team-9")
            .build();
        let id = provider.identity().expect("identity present");
        assert_eq!(id.user_id, "user-1");
        assert_eq!(id.principal_type.as_deref(), Some("Team"));
        assert_eq!(id.principal_id.as_deref(), Some("team-9"));
    }

    #[test]
    fn identity_none_without_user_id() {
        let provider =
            OidcAuthProviderBuilder::new("tok", "rt", "https://auth.example.com", "c1").build();
        assert!(provider.identity().is_none());
    }

    #[test]
    fn debug_does_not_leak_tokens() {
        let provider = OidcAuthProviderBuilder::new(
            "secret-access-token",
            "secret-refresh-token",
            "https://auth.example.com",
            "client1",
        )
        .build();

        let debug = format!("{provider:?}");
        assert!(!debug.contains("secret-access-token"));
        assert!(!debug.contains("secret-refresh-token"));
    }
}
