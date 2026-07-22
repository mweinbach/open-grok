use super::types::WebSearchConfig;
use crate::attribution::{SharedAttributionCallback, ToolConsumer};
use crate::types::SharedApiKeyProvider;
use async_openai::types::responses as rs;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

const PERPLEXITY_MAX_RESULTS: u8 = 10;
const PERPLEXITY_MEDIUM_CONTEXT_TOKENS_PER_PAGE: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebSearchBackend {
    Responses,
    Perplexity,
}
/// A minimal, purpose-built HTTP client for calling the Responses API
/// with web search capability.
#[derive(Clone)]
pub struct WebSearchClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    backend: WebSearchBackend,
    api_key_provider: Option<SharedApiKeyProvider>,
    /// Optional 401-attribution hook. Callers can wire this so a 401
    /// from the Responses API emits an `auth_401_attribution` event
    /// with `consumer == "WebSearch"`.
    attribution_callback: Option<SharedAttributionCallback>,
}
impl WebSearchClient {
    /// Create a new web search client from `WebSearchConfig::Enabled`.
    ///
    /// Returns `Err` if the config is `Disabled` or if header values are invalid.
    pub fn new(
        config: &WebSearchConfig,
        api_key_provider: Option<SharedApiKeyProvider>,
    ) -> Result<Self, xai_tool_runtime::ToolError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let (base_url, model, backend, api_key_provider) = match config {
            WebSearchConfig::Disabled => {
                return Err(web_search_error(
                    "Cannot create WebSearchClient from disabled config",
                ));
            }
            WebSearchConfig::Enabled {
                api_key,
                base_url,
                model,
                extra_headers,
                alpha_test_key,
            } => {
                insert_bearer_header(&mut headers, api_key)?;
                for (key, value) in extra_headers {
                    let header_name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| {
                        web_search_error("Invalid extra header name for web search")
                    })?;
                    let header_value = HeaderValue::from_str(value).map_err(|_| {
                        web_search_error("Invalid extra header value for web search")
                    })?;
                    headers.insert(header_name, header_value);
                }
                let _ = alpha_test_key;
                (
                    base_url.clone(),
                    model.clone(),
                    WebSearchBackend::Responses,
                    api_key_provider,
                )
            }
            WebSearchConfig::Perplexity { api_key, base_url } => {
                insert_bearer_header(&mut headers, api_key)?;
                (
                    base_url.clone(),
                    String::new(),
                    WebSearchBackend::Perplexity,
                    None,
                )
            }
        };
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|_| web_search_error("Failed to initialize web search client"))?;
        Ok(Self {
            http,
            base_url,
            model,
            backend,
            api_key_provider,
            attribution_callback: None,
        })
    }
    /// Wire a 401-attribution callback into this client. Idempotent;
    /// safe to call before or after the first request.
    pub fn with_attribution_callback(
        mut self,
        callback: Option<SharedAttributionCallback>,
    ) -> Self {
        self.attribution_callback = callback;
        self
    }
    async fn current_bearer(&self) -> Option<String> {
        crate::types::api_key_provider::resolve_bearer(self.api_key_provider.as_ref()).await
    }
    fn record_401_attribution(&self, sent_bearer: Option<&str>) {
        crate::attribution::emit_401(
            self.attribution_callback.as_ref(),
            ToolConsumer::WebSearch,
            sent_bearer,
        );
    }
    /// Perform a web search query using the Responses API.
    ///
    /// Returns `(content, citations)` where content is the assistant's text
    /// and citations are unique URLs found in the response annotations.
    pub async fn search(
        &self,
        query: &str,
        allowed_domains: Option<Vec<String>>,
    ) -> Result<(String, Vec<String>), xai_tool_runtime::ToolError> {
        if self.backend == WebSearchBackend::Perplexity {
            let result = self.search_perplexity(query, allowed_domains).await?;
            return Ok((result.content, result.citations));
        }
        let web_search = rs::WebSearchToolArgs::default()
            .filters(rs::WebSearchToolFilters { allowed_domains })
            .build()
            .map_err(|e| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                    format!("Failed to build web search tool: {e}"),
                )
            })?;
        let request = rs::CreateResponseArgs::default()
            .model(self.model.clone())
            .input(query.to_string())
            .tools(vec![rs::Tool::WebSearch(web_search)])
            .store(false)
            .temperature(0.1)
            .top_p(0.95)
            .max_output_tokens(8192u32)
            .build()
            .map_err(|e| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                    format!("Failed to build request: {e}"),
                )
            })?;
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let sent_bearer = self.current_bearer().await;
        let mut req = self.http.post(&url).json(&request);
        if let Some(ref key) = sent_bearer {
            req = req.header(AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = req.send().await.map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("HTTP request failed: {e}"),
            )
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.record_401_attribution(sent_bearer.as_deref());
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            return Err(xai_tool_runtime::ToolError::unauthorized(format!(
                "Responses API returned 401 Unauthorized: {body}"
            ))
            .with_details(serde_json::json!({
                "tool_id": "web_search",
                "status": 401,
            })));
        }
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            return Err(xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("Responses API returned {status}: {body}"),
            ));
        }
        let bytes = response.bytes().await.map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("Failed to read response body: {e}"),
            )
        })?;
        let response_obj: rs::Response = serde_json::from_slice(&bytes).map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("Failed to parse response: {e}"),
            )
        })?;
        let content = response_obj
            .output_text()
            .unwrap_or_else(|| "No search results found.".to_string());
        let citations = extract_citations(&response_obj);
        Ok((content, citations))
    }
    /// Same as [`Self::search`] but also extracts per-citation titles when
    /// the Responses API surfaces them. Returns `(content, citations_with_titles)`
    /// where each citation is `(title, url)`. Empty `title` strings indicate
    /// the upstream didn't supply one for that URL.
    ///
    /// Used by the cursor-compat `WebSearch` adapter to render a
    /// `Links:\n1. [title](url)` list instead of the LLM synthesis text.
    pub async fn search_with_titles(
        &self,
        query: &str,
        allowed_domains: Option<Vec<String>>,
    ) -> Result<(String, Vec<(String, String)>), xai_tool_runtime::ToolError> {
        if self.backend == WebSearchBackend::Perplexity {
            let result = self.search_perplexity(query, allowed_domains).await?;
            return Ok((result.content, result.citation_pairs));
        }
        let web_search = rs::WebSearchToolArgs::default()
            .filters(rs::WebSearchToolFilters { allowed_domains })
            .build()
            .map_err(|e| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                    format!("Failed to build web search tool: {e}"),
                )
            })?;
        let request = rs::CreateResponseArgs::default()
            .model(self.model.clone())
            .input(query.to_string())
            .tools(vec![rs::Tool::WebSearch(web_search)])
            .store(false)
            .temperature(0.1)
            .top_p(0.95)
            .max_output_tokens(8192u32)
            .build()
            .map_err(|e| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                    format!("Failed to build request: {e}"),
                )
            })?;
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let sent_bearer = self.current_bearer().await;
        let mut req = self.http.post(&url).json(&request);
        if let Some(ref key) = sent_bearer {
            req = req.header(AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = req.send().await.map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("HTTP request failed: {e}"),
            )
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.record_401_attribution(sent_bearer.as_deref());
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            return Err(xai_tool_runtime::ToolError::unauthorized(format!(
                "Responses API returned 401 Unauthorized: {body}"
            ))
            .with_details(serde_json::json!({
                "tool_id": "web_search",
                "status": 401,
            })));
        }
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            return Err(xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("Responses API returned {status}: {body}"),
            ));
        }
        let bytes = response.bytes().await.map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("Failed to read response body: {e}"),
            )
        })?;
        let response_obj: rs::Response = serde_json::from_slice(&bytes).map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("Failed to parse response: {e}"),
            )
        })?;
        let content = response_obj
            .output_text()
            .unwrap_or_else(|| "No search results found.".to_string());
        let pairs = extract_citation_pairs(&response_obj);
        Ok((content, pairs))
    }

    /// Perform an X (Twitter) search via the xAI Responses API.
    ///
    /// The `x_search` tool is an xAI extension with no typed representation
    /// in async-openai, so the typed request is serialized and the raw
    /// `{"type": "x_search"}` declaration is spliced into `tools` — the same
    /// shape the sampler uses for hosted X search. Only valid on the
    /// Responses backend; the Perplexity backend has no X search.
    pub async fn x_search(
        &self,
        query: &str,
    ) -> Result<(String, Vec<String>), xai_tool_runtime::ToolError> {
        if self.backend == WebSearchBackend::Perplexity {
            return Err(xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("x_search").expect("valid"),
                "X search requires the xAI backend".to_string(),
            ));
        }
        let request = rs::CreateResponseArgs::default()
            .model(self.model.clone())
            .input(query.to_string())
            .store(false)
            .temperature(0.1)
            .top_p(0.95)
            .max_output_tokens(8192u32)
            .build()
            .map_err(|e| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("x_search").expect("valid"),
                    format!("Failed to build request: {e}"),
                )
            })?;
        let mut request = serde_json::to_value(&request).map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("x_search").expect("valid"),
                format!("Failed to serialize request: {e}"),
            )
        })?;
        if let Some(map) = request.as_object_mut() {
            map.insert(
                "tools".to_string(),
                serde_json::json!([{ "type": "x_search" }]),
            );
        }
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let sent_bearer = self.current_bearer().await;
        let mut req = self.http.post(&url).json(&request);
        if let Some(ref key) = sent_bearer {
            req = req.header(AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = req.send().await.map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("x_search").expect("valid"),
                format!("HTTP request failed: {e}"),
            )
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.record_401_attribution(sent_bearer.as_deref());
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            return Err(xai_tool_runtime::ToolError::unauthorized(format!(
                "Responses API returned 401 Unauthorized: {body}"
            ))
            .with_details(serde_json::json!({ "tool_id" : "x_search", "status" : 401, })));
        }
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            return Err(xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("x_search").expect("valid"),
                format!("Responses API returned {status}: {body}"),
            ));
        }
        let bytes = response.bytes().await.map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("x_search").expect("valid"),
                format!("Failed to read response body: {e}"),
            )
        })?;
        let response_obj: rs::Response = serde_json::from_slice(&bytes).map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("x_search").expect("valid"),
                format!("Failed to parse response: {e}"),
            )
        })?;
        let content = response_obj
            .output_text()
            .unwrap_or_else(|| "No search results found.".to_string());
        let citations = extract_citations(&response_obj);
        Ok((content, citations))
    }

    async fn search_perplexity(
        &self,
        query: &str,
        allowed_domains: Option<Vec<String>>,
    ) -> Result<PerplexityFormattedResults, xai_tool_runtime::ToolError> {
        let request = PerplexitySearchRequest {
            query,
            max_results: PERPLEXITY_MAX_RESULTS,
            max_tokens_per_page: PERPLEXITY_MEDIUM_CONTEXT_TOKENS_PER_PAGE,
            search_domain_filter: allowed_domains.as_deref(),
        };
        let url = format!("{}/search", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .post(url)
            .json(&request)
            .send()
            .await
            .map_err(|_| web_search_error("Perplexity web search request failed. Try again."))?;
        let status = response.status();
        if !status.is_success() {
            return Err(perplexity_status_error(status, allowed_domains.is_some()));
        }
        let bytes = response.bytes().await.map_err(|_| {
            web_search_error("Perplexity web search returned an unreadable response.")
        })?;
        let response: PerplexitySearchResponse = serde_json::from_slice(&bytes).map_err(|_| {
            web_search_error("Perplexity web search returned a malformed response.")
        })?;
        format_perplexity_results(response.results)
    }
}

fn insert_bearer_header(
    headers: &mut HeaderMap,
    api_key: &str,
) -> Result<(), xai_tool_runtime::ToolError> {
    let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
        .map_err(|_| web_search_error("Invalid API key for web search"))?;
    headers.insert(AUTHORIZATION, value);
    Ok(())
}

fn web_search_error(message: impl Into<String>) -> xai_tool_runtime::ToolError {
    xai_tool_runtime::ToolError::execution(
        xai_tool_protocol::ToolId::new("web_search").expect("valid"),
        message.into(),
    )
}

fn perplexity_status_error(
    status: reqwest::StatusCode,
    had_domain_filter: bool,
) -> xai_tool_runtime::ToolError {
    match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            xai_tool_runtime::ToolError::unauthorized(
                "Perplexity web search authentication failed. Check the API key.",
            )
            .with_details(serde_json::json!({
                "tool_id": "web_search",
                "status": status.as_u16(),
            }))
        }
        reqwest::StatusCode::TOO_MANY_REQUESTS => {
            web_search_error("Perplexity web search is rate limited. Try again later.")
        }
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNPROCESSABLE_ENTITY
            if had_domain_filter =>
        {
            web_search_error("Perplexity web search rejected the allowed_domains filter.")
        }
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNPROCESSABLE_ENTITY => {
            web_search_error("Perplexity web search rejected the request.")
        }
        status if status.is_server_error() => {
            web_search_error("Perplexity web search is temporarily unavailable.")
        }
        _ => web_search_error(format!(
            "Perplexity web search failed with status {}.",
            status.as_u16()
        )),
    }
}

#[derive(Serialize)]
struct PerplexitySearchRequest<'a> {
    query: &'a str,
    max_results: u8,
    max_tokens_per_page: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    search_domain_filter: Option<&'a [String]>,
}

#[derive(Deserialize)]
struct PerplexitySearchResponse {
    results: Vec<PerplexitySearchResult>,
}

#[derive(Deserialize)]
struct PerplexitySearchResult {
    title: String,
    url: String,
    snippet: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    last_updated: Option<String>,
}

struct PerplexityFormattedResults {
    content: String,
    citations: Vec<String>,
    citation_pairs: Vec<(String, String)>,
}

fn format_perplexity_results(
    results: Vec<PerplexitySearchResult>,
) -> Result<PerplexityFormattedResults, xai_tool_runtime::ToolError> {
    if results.is_empty() {
        return Err(web_search_error(
            "Perplexity web search returned no results.",
        ));
    }
    let mut content = String::from("Ranked web search results:\n");
    let mut citations = Vec::new();
    let mut citation_pairs = Vec::new();
    let mut seen_urls = std::collections::HashSet::new();
    for (index, result) in results.into_iter().enumerate() {
        let date = result
            .date
            .as_deref()
            .or(result.last_updated.as_deref())
            .unwrap_or("Unavailable");
        content.push_str(&format!(
            "\n{}. {}\nURL: {}\nDate: {}\nSnippet: {}\n",
            index + 1,
            result.title,
            result.url,
            date,
            result.snippet
        ));
        if seen_urls.insert(result.url.clone()) {
            citations.push(result.url.clone());
            citation_pairs.push((result.title, result.url));
        }
    }
    Ok(PerplexityFormattedResults {
        content,
        citations,
        citation_pairs,
    })
}
/// Extract citation URLs from the Response output items.
/// The async-openai crate doesn't provide a helper for this, and the `url` field
/// in `UrlCitationBody` is private, so we serialize to JSON to extract it.
fn extract_citations(response: &rs::Response) -> Vec<String> {
    let mut citations = Vec::new();
    for output_item in &response.output {
        if let rs::OutputItem::Message(output_message) = output_item {
            for message_content in &output_message.content {
                if let rs::OutputMessageContent::OutputText(text_content) = message_content {
                    for annotation in &text_content.annotations {
                        if let rs::Annotation::UrlCitation(url_citation) = annotation
                            && let Ok(json) = serde_json::to_value(url_citation)
                            && let Some(url) = json.get("url").and_then(|v| v.as_str())
                        {
                            citations.push(url.to_string());
                        }
                    }
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    citations.retain(|url| seen.insert(url.clone()));
    citations
}
/// Extract `(title, url)` pairs from the Responses API annotations.
///
/// `title` may be an empty string when upstream doesn't supply one. URLs
/// are deduplicated while preserving the first-seen order so the rendered
/// `Links:` list is stable and free of duplicates.
fn extract_citation_pairs(response: &rs::Response) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for output_item in &response.output {
        if let rs::OutputItem::Message(output_message) = output_item {
            for message_content in &output_message.content {
                if let rs::OutputMessageContent::OutputText(text_content) = message_content {
                    for annotation in &text_content.annotations {
                        if let rs::Annotation::UrlCitation(url_citation) = annotation
                            && let Ok(json) = serde_json::to_value(url_citation)
                        {
                            let url = json.get("url").and_then(|v| v.as_str()).unwrap_or("");
                            if url.is_empty() {
                                continue;
                            }
                            let title = json
                                .get("title")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            pairs.push((title, url.to_string()));
                        }
                    }
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    pairs.retain(|(_t, url)| seen.insert(url.clone()));
    pairs
}
#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    /// Helper to create a Response from JSON for testing.
    fn response_from_json(json: serde_json::Value) -> rs::Response {
        serde_json::from_value(json).expect("Failed to parse test Response JSON")
    }
    #[test]
    fn test_new_client_uses_configured_model() {
        let config = WebSearchConfig::Enabled {
            api_key: "test-key".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "custom-enterprise-model".to_string(),
            extra_headers: IndexMap::new(),
            alpha_test_key: None,
        };
        let client = WebSearchClient::new(&config, None).expect("client should build");
        assert_eq!(client.model, "custom-enterprise-model");
    }
    /// Counts attribution callback invocations for the test below.
    #[derive(Default, Debug)]
    struct CountingCallback {
        invocations: std::sync::Mutex<Vec<(ToolConsumer, Option<String>)>>,
    }
    impl crate::attribution::Auth401AttributionCallback for CountingCallback {
        fn record_401(&self, consumer: ToolConsumer, sent_bearer_prefix: Option<&str>) {
            self.invocations
                .lock()
                .unwrap()
                .push((consumer, sent_bearer_prefix.map(|s| s.to_string())));
        }
    }
    /// `record_401_attribution` invokes the wired callback with
    /// `ToolConsumer::WebSearch` and the truncated bearer prefix.
    /// The full bearer never crosses the trait boundary.
    #[test]
    fn record_401_attribution_passes_truncated_prefix_to_callback() {
        let cb = std::sync::Arc::new(CountingCallback::default());
        let cb_dyn: crate::attribution::SharedAttributionCallback = cb.clone();
        let config = WebSearchConfig::Enabled {
            api_key: "ignored".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            alpha_test_key: None,
        };
        let client = WebSearchClient::new(&config, None)
            .expect("client should build")
            .with_attribution_callback(Some(cb_dyn));
        client.record_401_attribution(Some("bearer-with-long-tail-aaaaaaaaaa"));
        let calls = cb.invocations.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, ToolConsumer::WebSearch);
        assert_eq!(calls[0].1.as_deref(), Some("bearer-with-"));
        assert_eq!(
            calls[0].1.as_deref().map(str::len),
            Some(crate::attribution::SENT_BEARER_PREFIX_LEN),
        );
    }
    /// `record_401_attribution` is a no-op when no callback is wired
    /// -- the BYOK / standalone case must not panic or allocate.
    #[test]
    fn record_401_attribution_is_noop_without_callback() {
        let config = WebSearchConfig::Enabled {
            api_key: "test-key".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            alpha_test_key: None,
        };
        let client = WebSearchClient::new(&config, None).expect("client should build");
        client.record_401_attribution(Some("any-bearer"));
        client.record_401_attribution(None);
    }
    #[test]
    fn test_extract_citations_empty_response() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "output": [],
            "model": "test-model"
        }));
        let citations = extract_citations(&response);
        assert!(citations.is_empty());
    }
    #[test]
    fn test_extract_citations_with_url_citations() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "test-model",
            "output": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "Here is some info about Rust.",
                            "annotations": [
                                {
                                    "type": "url_citation",
                                    "url": "https://www.rust-lang.org/",
                                    "title": "Rust Programming Language",
                                    "start_index": 0,
                                    "end_index": 10
                                },
                                {
                                    "type": "url_citation",
                                    "url": "https://docs.rs/",
                                    "title": "Docs.rs",
                                    "start_index": 11,
                                    "end_index": 20
                                }
                            ]
                        }
                    ]
                }
            ]
        }));
        let citations = extract_citations(&response);
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0], "https://www.rust-lang.org/");
        assert_eq!(citations[1], "https://docs.rs/");
    }
    #[test]
    fn test_extract_citations_deduplicates() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "test-model",
            "output": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "Info with duplicate citations.",
                            "annotations": [
                                {
                                    "type": "url_citation",
                                    "url": "https://example.com/page1",
                                    "title": "Page 1",
                                    "start_index": 0,
                                    "end_index": 5
                                },
                                {
                                    "type": "url_citation",
                                    "url": "https://example.com/page2",
                                    "title": "Page 2",
                                    "start_index": 6,
                                    "end_index": 10
                                },
                                {
                                    "type": "url_citation",
                                    "url": "https://example.com/page1",
                                    "title": "Page 1 Again",
                                    "start_index": 11,
                                    "end_index": 15
                                }
                            ]
                        }
                    ]
                }
            ]
        }));
        let citations = extract_citations(&response);
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0], "https://example.com/page1");
        assert_eq!(citations[1], "https://example.com/page2");
    }
    #[test]
    fn test_extract_citations_multiple_messages() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "test-model",
            "output": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "First message",
                            "annotations": [
                                {
                                    "type": "url_citation",
                                    "url": "https://first.com/",
                                    "title": "First",
                                    "start_index": 0,
                                    "end_index": 5
                                }
                            ]
                        }
                    ]
                },
                {
                    "type": "message",
                    "id": "msg_2",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "Second message",
                            "annotations": [
                                {
                                    "type": "url_citation",
                                    "url": "https://second.com/",
                                    "title": "Second",
                                    "start_index": 0,
                                    "end_index": 6
                                }
                            ]
                        }
                    ]
                }
            ]
        }));
        let citations = extract_citations(&response);
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0], "https://first.com/");
        assert_eq!(citations[1], "https://second.com/");
    }
    #[test]
    fn test_extract_citations_ignores_non_url_annotations() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "test-model",
            "output": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "Some text",
                            "annotations": [
                                {
                                    "type": "url_citation",
                                    "url": "https://valid.com/",
                                    "title": "Valid",
                                    "start_index": 0,
                                    "end_index": 4
                                }
                            ]
                        }
                    ]
                }
            ]
        }));
        let citations = extract_citations(&response);
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0], "https://valid.com/");
    }
    /// A provider that always returns `None`, simulating an API-key user
    /// whose token has aged past the client-side TTL.
    struct NoneProvider;
    impl crate::types::ApiKeyProvider for NoneProvider {
        fn current_api_key(&self) -> Option<String> {
            None
        }
    }
    /// When the dynamic provider returns `None`, the static `api_key`
    /// from config must still be sent as the Authorization header.
    /// This is a regression scenario: API-key users
    /// past the 30-day client TTL saw 401 because no auth was sent.
    #[tokio::test]
    async fn static_api_key_is_fallback_when_provider_returns_none() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("Authorization", "Bearer static-key-from-config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_test",
                "object": "response",
                "created_at": 1234567890,
                "status": "completed",
                "model": "test-model",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": "search result",
                        "annotations": []
                    }]
                }]
            })))
            .mount(&server)
            .await;
        let config = WebSearchConfig::Enabled {
            api_key: "static-key-from-config".to_string(),
            base_url: server.uri(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            alpha_test_key: None,
        };
        let provider: SharedApiKeyProvider = std::sync::Arc::new(NoneProvider);
        let client = WebSearchClient::new(&config, Some(provider)).expect("client should build");
        let (content, _citations) = client
            .search("test query", None)
            .await
            .expect("search must succeed with static key fallback");
        assert_eq!(content, "search result");
    }
    /// When the provider returns a fresh key, it overrides the static one.
    #[tokio::test]
    async fn provider_key_overrides_static_key() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        struct FreshProvider;
        impl crate::types::ApiKeyProvider for FreshProvider {
            fn current_api_key(&self) -> Option<String> {
                Some("fresh-key-from-provider".to_string())
            }
        }
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("Authorization", "Bearer fresh-key-from-provider"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_test",
                "object": "response",
                "created_at": 1234567890,
                "status": "completed",
                "model": "test-model",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": "fresh result",
                        "annotations": []
                    }]
                }]
            })))
            .mount(&server)
            .await;
        let config = WebSearchConfig::Enabled {
            api_key: "stale-static-key".to_string(),
            base_url: server.uri(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            alpha_test_key: None,
        };
        let provider: SharedApiKeyProvider = std::sync::Arc::new(FreshProvider);
        let client = WebSearchClient::new(&config, Some(provider)).expect("client should build");
        let (content, _citations) = client
            .search("test query", None)
            .await
            .expect("search must succeed with provider key");
        assert_eq!(content, "fresh result");
    }

    fn perplexity_config(base_url: String) -> WebSearchConfig {
        WebSearchConfig::Perplexity {
            api_key: "pplx-test-key".to_owned(),
            base_url,
        }
    }

    #[tokio::test]
    async fn perplexity_request_forwards_domains_and_formats_ranked_results() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("Authorization", "Bearer pplx-test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "search-1",
                "results": [
                    {
                        "title": "Rust Language",
                        "url": "https://www.rust-lang.org/",
                        "snippet": "A language empowering everyone.",
                        "date": "2026-07-01"
                    },
                    {
                        "title": "Docs.rs",
                        "url": "https://docs.rs/",
                        "snippet": "Rust crate documentation.",
                        "last_updated": "2026-07-02"
                    },
                    {
                        "title": "Rust Duplicate",
                        "url": "https://www.rust-lang.org/",
                        "snippet": "Duplicate URL for citation testing."
                    }
                ]
            })))
            .mount(&server)
            .await;

        let client = WebSearchClient::new(&perplexity_config(server.uri()), None)
            .expect("Perplexity client should build");
        let domains = vec!["rust-lang.org".to_owned(), "docs.rs".to_owned()];
        let (content, citations) = client
            .search("Rust documentation", Some(domains.clone()))
            .await
            .expect("Perplexity search should succeed");

        assert!(content.contains("1. Rust Language"));
        assert!(content.contains("URL: https://www.rust-lang.org/"));
        assert!(content.contains("Date: 2026-07-01"));
        assert!(content.contains("Snippet: A language empowering everyone."));
        assert!(content.contains("2. Docs.rs"));
        assert!(content.contains("Date: 2026-07-02"));
        assert!(content.contains("3. Rust Duplicate"));
        assert_eq!(
            citations,
            vec![
                "https://www.rust-lang.org/".to_owned(),
                "https://docs.rs/".to_owned()
            ]
        );

        let requests = server
            .received_requests()
            .await
            .expect("request recording should be available");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("request body should be JSON");
        assert_eq!(body["query"], "Rust documentation");
        assert_eq!(body["max_results"], PERPLEXITY_MAX_RESULTS);
        assert_eq!(
            body["max_tokens_per_page"],
            PERPLEXITY_MEDIUM_CONTEXT_TOKENS_PER_PAGE
        );
        assert_eq!(body["search_domain_filter"], serde_json::json!(domains));
    }

    #[tokio::test]
    async fn perplexity_empty_results_are_reported_without_raw_response_data() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": []
            })))
            .mount(&server)
            .await;
        let client = WebSearchClient::new(&perplexity_config(server.uri()), None).unwrap();

        let error = client.search("no matches", None).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Perplexity web search returned no results")
        );
    }

    #[tokio::test]
    async fn perplexity_malformed_json_is_sanitized() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{not-json", "application/json"))
            .mount(&server)
            .await;
        let client = WebSearchClient::new(&perplexity_config(server.uri()), None).unwrap();

        let error = client.search("malformed", None).await.unwrap_err();
        let message = error.to_string();

        assert!(message.contains("Perplexity web search returned a malformed response"));
        assert!(!message.contains("not-json"));
    }

    async fn perplexity_status_error_message(
        status: u16,
        allowed_domains: Option<Vec<String>>,
    ) -> String {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(
                ResponseTemplate::new(status).set_body_string("upstream-secret-error-details"),
            )
            .mount(&server)
            .await;
        let client = WebSearchClient::new(&perplexity_config(server.uri()), None).unwrap();

        client
            .search("status failure", allowed_domains)
            .await
            .unwrap_err()
            .to_string()
    }

    #[tokio::test]
    async fn perplexity_auth_rate_filter_and_server_errors_are_sanitized() {
        for status in [401, 403] {
            let message = perplexity_status_error_message(status, None).await;
            assert!(message.contains("Perplexity web search authentication failed"));
            assert!(!message.contains("upstream-secret-error-details"));
        }

        let rate_limit = perplexity_status_error_message(429, None).await;
        assert!(rate_limit.contains("Perplexity web search is rate limited"));

        let invalid_filter =
            perplexity_status_error_message(400, Some(vec!["bad filter".to_owned()])).await;
        assert!(invalid_filter.contains("rejected the allowed_domains filter"));

        let server_failure = perplexity_status_error_message(503, None).await;
        assert!(server_failure.contains("temporarily unavailable"));
        assert!(!server_failure.contains("upstream-secret-error-details"));
    }

    #[tokio::test]
    async fn perplexity_network_failure_is_sanitized() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = WebSearchClient::new(&perplexity_config(base_url), None).unwrap();

        let error = client.search("network failure", None).await.unwrap_err();
        let message = error.to_string();

        assert!(message.contains("Perplexity web search request failed"));
        assert!(!message.contains("127.0.0.1"));
    }

    #[test]
    fn test_extract_citations_no_annotations() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "test-model",
            "output": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "Plain text with no annotations",
                            "annotations": []
                        }
                    ]
                }
            ]
        }));
        let citations = extract_citations(&response);
        assert!(citations.is_empty());
    }
}
