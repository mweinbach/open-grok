//! `reqwest-middleware` layer: stamps auth headers and retries on 401.
//! Gated behind the `middleware` cargo feature.

use std::sync::Arc;

use reqwest::{Request, Response, StatusCode, header::HeaderValue};
use reqwest_middleware::{Error, Middleware, Next};

use crate::AuthCredentialProvider;

pub struct AuthRetryMiddleware {
    credentials: Arc<dyn AuthCredentialProvider>,
    max_retries: u32,
}

impl AuthRetryMiddleware {
    pub fn new(credentials: Arc<dyn AuthCredentialProvider>, max_retries: u32) -> Self {
        Self {
            credentials,
            max_retries,
        }
    }
}

fn apply_auth_header(req: &mut Request, token: &str) {
    match HeaderValue::from_str(&format!("Bearer {token}")) {
        Ok(val) => {
            req.headers_mut()
                .insert(reqwest::header::AUTHORIZATION, val);
        }
        Err(e) => {
            tracing::warn!(error = %e, "auth retry: failed to build Authorization header");
        }
    }
}

#[async_trait::async_trait]
impl Middleware for AuthRetryMiddleware {
    async fn handle(
        &self,
        mut req: Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> Result<Response, Error> {
        if let Some(ref token) = self.credentials.snapshot().token {
            apply_auth_header(&mut req, token);
        }

        let backup = req.try_clone();
        let resp = next.clone().run(req, extensions).await?;

        if resp.status() != StatusCode::UNAUTHORIZED || self.max_retries == 0 {
            return Ok(resp);
        }
        let Some(backup) = backup else {
            return Ok(resp);
        };

        let mut last_resp = resp;
        for _ in 0..self.max_retries {
            if !self.credentials.refresh_after_unauthorized().await {
                break;
            }
            let Some(ref token) = self.credentials.snapshot().token else {
                break;
            };
            let Some(mut retry) = backup.try_clone() else {
                break;
            };
            apply_auth_header(&mut retry, token);
            last_resp = next.clone().run(retry, extensions).await?;
            if last_resp.status() != StatusCode::UNAUTHORIZED {
                return Ok(last_resp);
            }
        }

        Ok(last_resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CredentialSnapshot, HttpAuth};
    use reqwest_middleware::ClientBuilder;
    use std::sync::Mutex;

    struct MockProvider {
        token: Mutex<Option<String>>,
        refresh_result: bool,
        refresh_count: Mutex<u32>,
    }

    impl MockProvider {
        fn new(token: Option<&str>, refresh_result: bool) -> Self {
            Self {
                token: Mutex::new(token.map(|s| s.to_owned())),
                refresh_result,
                refresh_count: Mutex::new(0),
            }
        }

        fn refresh_count(&self) -> u32 {
            *self.refresh_count.lock().unwrap()
        }
    }

    impl HttpAuth for MockProvider {
        fn apply(&self, b: reqwest::RequestBuilder, _: &str) -> reqwest::RequestBuilder {
            b
        }
    }

    #[async_trait::async_trait]
    impl AuthCredentialProvider for MockProvider {
        fn snapshot(&self) -> CredentialSnapshot {
            CredentialSnapshot {
                token: self.token.lock().unwrap().clone(),
                ..Default::default()
            }
        }

        async fn refresh_after_unauthorized(&self) -> bool {
            *self.refresh_count.lock().unwrap() += 1;
            self.refresh_result
        }
    }

    async fn build_client(
        provider: Arc<dyn AuthCredentialProvider>,
        max_retries: u32,
    ) -> reqwest_middleware::ClientWithMiddleware {
        ClientBuilder::new(reqwest::Client::new())
            .with(AuthRetryMiddleware::new(provider, max_retries))
            .build()
    }

    #[tokio::test]
    async fn test_401_no_refresh_returns_401() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/")
            .with_status(401)
            .expect(1)
            .create_async()
            .await;

        let p = Arc::new(MockProvider::new(Some("tok"), false));
        let client = build_client(p.clone(), 1).await;

        let resp = client.get(server.url()).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(p.refresh_count(), 1);
        m.assert_async().await;
    }

    /// Simulates a real auth manager: starts with stale token, refresh swaps to fresh.
    struct SimulatedAuthManager {
        token: Mutex<Option<String>>,
        fresh_token: String,
        refresh_count: Mutex<u32>,
    }

    impl SimulatedAuthManager {
        fn simulated(stale: &str, fresh: &str) -> Self {
            Self {
                token: Mutex::new(Some(stale.to_owned())),
                fresh_token: fresh.to_owned(),
                refresh_count: Mutex::new(0),
            }
        }
    }

    impl HttpAuth for SimulatedAuthManager {
        fn apply(&self, b: reqwest::RequestBuilder, _: &str) -> reqwest::RequestBuilder {
            b
        }
    }

    #[async_trait::async_trait]
    impl AuthCredentialProvider for SimulatedAuthManager {
        fn snapshot(&self) -> CredentialSnapshot {
            CredentialSnapshot {
                token: self.token.lock().unwrap().clone(),
                ..Default::default()
            }
        }

        async fn refresh_after_unauthorized(&self) -> bool {
            *self.refresh_count.lock().unwrap() += 1;
            *self.token.lock().unwrap() = Some(self.fresh_token.clone());
            true
        }
    }

    #[tokio::test]
    async fn test_e2e_stale_token_refreshed_and_retried() {
        let mut server = mockito::Server::new_async().await;

        let m401 = server
            .mock("GET", "/api")
            .match_header("authorization", "Bearer stale-token")
            .with_status(401)
            .create_async()
            .await;
        let m200 = server
            .mock("GET", "/api")
            .match_header("authorization", "Bearer fresh-token")
            .with_status(200)
            .with_body(r#"{"ok":true}"#)
            .create_async()
            .await;

        let p = Arc::new(SimulatedAuthManager::simulated(
            "stale-token",
            "fresh-token",
        ));
        let client = build_client(p.clone(), 1).await;

        let resp = client
            .get(format!("{}/api", server.url()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(*p.refresh_count.lock().unwrap(), 1);
        m401.assert_async().await;
        m200.assert_async().await;
    }

    #[tokio::test]
    async fn test_e2e_auth_header_stamped_automatically() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/api")
            .match_header("authorization", "Bearer my-token")
            .with_status(200)
            .create_async()
            .await;

        let p = Arc::new(MockProvider::new(Some("my-token"), false));
        let client = build_client(p.clone(), 1).await;

        let resp = client
            .get(format!("{}/api", server.url()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(p.refresh_count(), 0);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_max_retries_bounds_attempts() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/")
            .with_status(401)
            .expect(4)
            .create_async()
            .await;

        let p = Arc::new(MockProvider::new(Some("tok"), true));
        let client = build_client(p.clone(), 3).await;

        let resp = client.get(server.url()).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(p.refresh_count(), 3);
        m.assert_async().await;
    }
}
