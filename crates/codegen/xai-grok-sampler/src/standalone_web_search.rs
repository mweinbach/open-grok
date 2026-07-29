//! Codex-compatible standalone web-search wire contract.
//!
//! The model-facing command schema lives in `xai-grok-tools`; this module owns
//! only the provider request/response envelope sent to `/alpha/search`.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StandaloneSearchRequest {
    pub id: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<StandaloneSearchInput>,
    pub commands: JsonValue,
    pub settings: StandaloneSearchSettings,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum StandaloneSearchInput {
    Text(String),
    Items(Vec<StandaloneSearchMessage>),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StandaloneSearchMessage {
    pub r#type: StandaloneSearchMessageType,
    pub role: StandaloneSearchRole,
    pub content: Vec<StandaloneSearchContent>,
}

impl StandaloneSearchMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self::new(StandaloneSearchRole::User, text)
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            r#type: StandaloneSearchMessageType::Message,
            role: StandaloneSearchRole::Assistant,
            content: vec![StandaloneSearchContent::OutputText { text: text.into() }],
        }
    }

    fn new(role: StandaloneSearchRole, text: impl Into<String>) -> Self {
        Self {
            r#type: StandaloneSearchMessageType::Message,
            role,
            content: vec![StandaloneSearchContent::InputText { text: text.into() }],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StandaloneSearchMessageType {
    Message,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StandaloneSearchRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StandaloneSearchContent {
    InputText { text: String },
    OutputText { text: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandaloneSearchSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_location: Option<StandaloneSearchApproximateLocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_context_size: Option<StandaloneSearchContextSize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<StandaloneSearchFilters>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_settings: Option<StandaloneSearchImageSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_callers: Option<Vec<StandaloneSearchCaller>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_web_access: Option<StandaloneExternalWebAccess>,
}

impl StandaloneSearchSettings {
    pub fn direct_with_external_web_access() -> Self {
        Self {
            user_location: None,
            search_context_size: None,
            filters: None,
            image_settings: None,
            allowed_callers: Some(vec![StandaloneSearchCaller::Direct]),
            external_web_access: Some(StandaloneExternalWebAccess::Boolean(true)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandaloneSearchApproximateLocation {
    pub r#type: StandaloneSearchLocationType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StandaloneSearchLocationType {
    Approximate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StandaloneSearchContextSize {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandaloneSearchFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandaloneSearchImageSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StandaloneExternalWebAccessMode {
    Cached,
    Indexed,
    Live,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StandaloneExternalWebAccess {
    Boolean(bool),
    Mode(StandaloneExternalWebAccessMode),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StandaloneSearchCaller {
    Direct,
    Shell,
    CodeInterpreter,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct StandaloneSearchResponse {
    #[serde(default)]
    pub encrypted_output: Option<String>,
    pub output: String,
    /// Keep result DTOs opaque so newer endpoint variants remain compatible.
    #[serde(default)]
    pub results: Option<Vec<JsonValue>>,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use axum::Json;
    use axum::Router;
    use axum::extract::OriginalUri;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::http::StatusCode;
    use axum::routing::post;
    use indexmap::IndexMap;
    use serde_json::Value as JsonValue;
    use serde_json::json;
    use tokio::sync::Mutex;
    use tokio::sync::Notify;
    use xai_grok_sampling_types::ApiBackend;
    use xai_grok_sampling_types::ModelProvider;

    use super::StandaloneSearchInput;
    use super::StandaloneSearchMessage;
    use super::StandaloneSearchRequest;
    use super::StandaloneSearchSettings;
    use crate::Auth401AttributionCallback;
    use crate::AuthScheme;
    use crate::SamplerConfig;
    use crate::SamplingClient;
    use crate::SamplingConsumer;

    #[derive(Clone, Default)]
    struct Capture {
        request: Arc<Mutex<Option<(HeaderMap, String, JsonValue)>>>,
    }

    async fn capture_search(
        State(capture): State<Capture>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        Json(body): Json<JsonValue>,
    ) -> Json<JsonValue> {
        *capture.request.lock().await = Some((headers, uri.to_string(), body));
        Json(json!({
            "encrypted_output": "opaque",
            "output": "search result",
            "results": [{
                "type": "text_result",
                "ref_id": "turn0search0",
                "future_field": {"preserved": true}
            }]
        }))
    }

    #[derive(Debug, Default)]
    struct CountingAttribution {
        count: AtomicUsize,
    }

    impl Auth401AttributionCallback for CountingAttribution {
        fn record_401(&self, consumer: SamplingConsumer, _sent_bearer_prefix: Option<&str>) {
            assert_eq!(consumer, SamplingConsumer::StandaloneWebSearch);
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn unauthorized_search() -> (StatusCode, &'static str) {
        (StatusCode::UNAUTHORIZED, "expired")
    }

    #[derive(Clone, Default)]
    struct BlockingSearch {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    async fn blocking_search(State(state): State<BlockingSearch>) -> Json<JsonValue> {
        state.started.notify_one();
        state.release.notified().await;
        Json(json!({"encrypted_output": null, "output": "late"}))
    }

    async fn flaky_search(
        State(attempts): State<Arc<AtomicUsize>>,
    ) -> (
        StatusCode,
        [(&'static str, &'static str); 1],
        Json<JsonValue>,
    ) {
        let attempt = attempts.fetch_add(1, Ordering::Relaxed);
        if attempt == 0 {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("x-should-retry", "true")],
                Json(json!({"error": {"message": "retry"}})),
            )
        } else {
            (
                StatusCode::OK,
                [("x-should-retry", "false")],
                Json(json!({"encrypted_output": null, "output": "recovered"})),
            )
        }
    }

    fn config(base_url: String) -> SamplerConfig {
        SamplerConfig {
            api_key: Some("codex-token".to_string()),
            base_url,
            model: "gpt-test".to_string(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: ApiBackend::Responses,
            provider: ModelProvider::Codex,
            auth_scheme: AuthScheme::Bearer,
            extra_headers: IndexMap::new(),
            query_params: IndexMap::from([("api-version".to_string(), "test".to_string())]),
            env_http_headers: IndexMap::new(),
            context_window: 8192,
            force_http1: false,
            max_retries: Some(0),
            stream_tool_calls: false,
            idle_timeout_secs: None,
            reasoning_effort: None,
            service_tier: None,
            reasoning_summary: None,
            origin_client: None,
            client_identifier: None,
            deployment_id: None,
            user_id: None,
            client_version: None,
            attribution_callback: None,
            bearer_resolver: None,
            supports_backend_search: false,
            supports_standalone_web_search: true,
            codex_multi_agent_v2: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            doom_loop_recovery: None,
            header_injector: None,
        }
    }

    #[tokio::test]
    async fn posts_provider_authenticated_search_request() {
        let capture = Capture::default();
        let app = Router::new()
            .route("/v1/alpha/search", post(capture_search))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client =
            SamplingClient::new(config(format!("http://{address}/v1"))).expect("valid client");
        let request = StandaloneSearchRequest {
            id: "session-1".to_string(),
            model: "gpt-test".to_string(),
            reasoning: None,
            input: Some(StandaloneSearchInput::Items(vec![
                StandaloneSearchMessage::user("find this"),
                StandaloneSearchMessage::assistant("prior answer"),
            ])),
            commands: json!({
                "search_query": [{"q": "Open Grok", "recency": 7}]
            }),
            settings: StandaloneSearchSettings::direct_with_external_web_access(),
            max_output_tokens: Some(2500),
        };

        let response = client.standalone_web_search(&request).await.unwrap();
        assert_eq!(response.output, "search result");
        assert_eq!(response.encrypted_output.as_deref(), Some("opaque"));
        assert_eq!(
            response.results.unwrap()[0]["future_field"],
            json!({"preserved": true})
        );

        let (headers, uri, body) = capture.request.lock().await.take().unwrap();
        assert_eq!(uri, "/v1/alpha/search?api-version=test");
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer codex-token")
        );
        assert_eq!(body, serde_json::to_value(request).unwrap());
    }

    #[tokio::test]
    async fn attributes_standalone_search_unauthorized_response() {
        let app = Router::new().route("/v1/alpha/search", post(unauthorized_search));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let attribution = Arc::new(CountingAttribution::default());
        let mut config = config(format!("http://{address}/v1"));
        config.query_params.clear();
        config.attribution_callback = Some(attribution.clone());
        let client = SamplingClient::new(config).expect("valid client");
        let request = StandaloneSearchRequest {
            id: "session-1".to_string(),
            model: "gpt-test".to_string(),
            reasoning: None,
            input: None,
            commands: json!({"time": [{"utc_offset": "+00:00"}]}),
            settings: StandaloneSearchSettings::direct_with_external_web_access(),
            max_output_tokens: None,
        };

        let error = client.standalone_web_search(&request).await.unwrap_err();
        assert!(error.is_auth_error());
        assert_eq!(attribution.count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn standalone_search_future_is_cancellable() {
        let state = BlockingSearch::default();
        let app = Router::new()
            .route("/v1/alpha/search", post(blocking_search))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut config = config(format!("http://{address}/v1"));
        config.query_params.clear();
        let client = SamplingClient::new(config).expect("valid client");
        let request = StandaloneSearchRequest {
            id: "session-1".to_string(),
            model: "gpt-test".to_string(),
            reasoning: None,
            input: None,
            commands: json!({"time": [{"utc_offset": "+00:00"}]}),
            settings: StandaloneSearchSettings::direct_with_external_web_access(),
            max_output_tokens: None,
        };

        let task = tokio::spawn(async move { client.standalone_web_search(&request).await });
        state.started.notified().await;
        task.abort();
        state.release.notify_one();
        assert!(
            task.await
                .expect_err("search task should be cancelled")
                .is_cancelled()
        );
    }

    #[tokio::test]
    async fn retries_transient_standalone_search_failures() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/alpha/search", post(flaky_search))
            .with_state(attempts.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut config = config(format!("http://{address}/v1"));
        config.query_params.clear();
        config.max_retries = Some(2);
        let client = SamplingClient::new(config).expect("valid client");
        let request = StandaloneSearchRequest {
            id: "session-1".to_string(),
            model: "gpt-test".to_string(),
            reasoning: None,
            input: None,
            commands: json!({"time": [{"utc_offset": "+00:00"}]}),
            settings: StandaloneSearchSettings::direct_with_external_web_access(),
            max_output_tokens: None,
        };

        let response = client.standalone_web_search(&request).await.unwrap();
        assert_eq!(response.output, "recovered");
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn accepts_older_response_without_results() {
        let response: super::StandaloneSearchResponse = serde_json::from_value(json!({
            "encrypted_output": null,
            "output": "search result"
        }))
        .expect("response without results should deserialize");

        assert_eq!(response.output, "search result");
        assert!(response.results.is_none());
    }
}
