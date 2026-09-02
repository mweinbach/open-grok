//! User-supplied ("bring your own endpoint") model providers.
//!
//! The custom-provider wizard collects a server address and one of the three
//! supported wire protocols, then asks that address for its model list before
//! anything is written to config. Everything the wizard needs that is not UI
//! lives here: address normalization, the wire-format choice, and model
//! discovery.
//!
//! Trust model: the address is untrusted third-party infrastructure. Nothing in
//! this module may attach a first-party credential, resolve an environment
//! secret, or follow a redirect away from the host the user typed. Only the
//! key the user just entered for that address is ever sent, and only to that
//! address.

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::time::Duration;
use url::Url;
use xai_grok_sampling_types::ApiBackend;

/// Context window assumed for a discovered model that does not advertise one.
/// Deliberately conservative: an over-large value defers compaction until the
/// endpoint itself rejects the request.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// Model discovery budget. Local runtimes can take seconds to warm a server.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);
/// Refuse absurd listings before parsing them; every real catalog fits.
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_ADDRESS_CHARS: usize = 512;
/// Anthropic Messages servers reject a request that omits this header.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Wire protocol selected for a user-supplied server address.
///
/// The format, not the host, decides the request shape and the credential
/// header. A host tells Open Grok nothing about which protocol it speaks, so
/// this stays a user choice rather than a guess from the URL or model slug.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomWireFormat {
    /// OpenAI Chat Completions (`{base}/chat/completions`).
    ChatCompletions,
    /// OpenAI Responses (`{base}/responses`).
    Responses,
    /// Anthropic Messages (`{base}/messages`).
    Messages,
    /// Google AI Studio (`{base}/models/{model}:generateContent`).
    GoogleAiStudio,
}

impl CustomWireFormat {
    pub const ALL: [Self; 4] = [
        Self::ChatCompletions,
        Self::Responses,
        Self::Messages,
        Self::GoogleAiStudio,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::Messages => "messages",
            Self::GoogleAiStudio => "google_ai_studio",
        }
    }

    /// Display label for the wizard's format step.
    pub const fn label(self) -> &'static str {
        match self {
            Self::ChatCompletions => "OpenAI Chat Completions",
            Self::Responses => "OpenAI Responses",
            Self::Messages => "Anthropic Messages",
            Self::GoogleAiStudio => "Google AI Studio",
        }
    }

    /// Accept the canonical value plus the spellings a user is likely to type
    /// (or find in a vendor's docs) for the same protocol.
    pub fn from_canonical(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "chat_completions" | "chat-completions" | "chat" | "openai_chat_completions" => {
                Some(Self::ChatCompletions)
            }
            "responses" | "openai_responses" | "response" => Some(Self::Responses),
            "messages" | "anthropic" | "anthropic_messages" | "claude" => Some(Self::Messages),
            "google_ai_studio" | "ai_studio" | "gemini" | "google" => Some(Self::GoogleAiStudio),
            _ => None,
        }
    }

    pub const fn api_backend(self) -> ApiBackend {
        match self {
            Self::ChatCompletions => ApiBackend::ChatCompletions,
            Self::Responses => ApiBackend::Responses,
            Self::Messages => ApiBackend::Messages,
            Self::GoogleAiStudio => ApiBackend::GoogleAiStudio,
        }
    }

    /// Credential header shape for this protocol: native Anthropic Messages
    /// servers want `x-api-key`, every OpenAI-compatible server wants a bearer
    /// token. Stored on the model entry so it stays visible and editable.
    pub const fn auth_scheme(self) -> &'static str {
        match self {
            Self::ChatCompletions | Self::Responses => "bearer",
            Self::Messages => "x_api_key",
            Self::GoogleAiStudio => "x_goog_api_key",
        }
    }

    /// Path suffix this protocol appends for inference and discovery.
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat/completions",
            Self::Responses => "responses",
            Self::Messages => "messages",
            Self::GoogleAiStudio => "models",
        }
    }
}

/// A server address after validation and default completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedEndpoint {
    pub base_url: String,
    /// Non-fatal things the user should see, e.g. "`/v1` was added".
    pub notes: Vec<String>,
}

/// The `://`-terminated scheme of an address, when it has a syntactically
/// valid one (`https://`, `http://`, `ftp://`, ...).
fn explicit_scheme(raw: &str) -> Option<&str> {
    let end = raw.find("://")?;
    let scheme = &raw[..end];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic()
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    Some(scheme)
}

/// Inference paths that must not be part of a base URL: Open Grok appends them
/// per request, so a pasted full endpoint would double them.
const OPERATION_PATHS: [&str; 5] = [
    "/chat/completions",
    "/completions",
    "/responses",
    "/messages",
    "/models",
];

/// Validate one server address and complete the parts a user cannot know.
///
/// Rules, in order: require a URL (assume `https://` when the scheme is
/// omitted), allow only `http`/`https`, reject embedded credentials and
/// query/fragment state, strip an accidentally pasted operation path, and add
/// `/v1` when the address carries no path at all. Plain `http` stays legal for
/// local runtimes (Ollama, LM Studio) but is flagged.
pub fn normalize_server_address(raw: &str) -> Result<NormalizedEndpoint> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("server address is required");
    }
    if trimmed.chars().count() > MAX_ADDRESS_CHARS {
        bail!("server address is too long (max {MAX_ADDRESS_CHARS} characters)");
    }
    if trimmed.chars().any(char::is_whitespace) {
        bail!("server address must not contain whitespace");
    }

    // Only complete `<scheme>://` counts as an explicit scheme. A bare host like
    // `api.example.com/v1` gets https, while `ftp://host` keeps its scheme so
    // the check below can reject it instead of silently rewriting it.
    let candidate = if explicit_scheme(trimmed).is_some() {
        Cow::Borrowed(trimmed)
    } else {
        Cow::Owned(format!("https://{trimmed}"))
    };
    let mut parsed = Url::parse(candidate.as_ref())
        .map_err(|_| anyhow!("server address is not a valid URL: `{trimmed}`"))?;
    let mut notes = Vec::new();

    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            bail!(
                "unsupported scheme `{other}://`; a custom provider must be reachable over http:// or https://"
            );
        }
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!(
            "the server address must not embed credentials; put the key in the API key field so it is stored as a secret"
        );
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        bail!("server address must include a host");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("server address must not include a query string or fragment");
    }
    if parsed.scheme() == "http" {
        notes.push(
            "plain http sends your API key unencrypted; this is fine on loopback or a trusted network, but use https for anything else"
                .to_owned(),
        );
    }

    let mut path = parsed.path().trim_end_matches('/').to_owned();
    // Open Grok appends the operation path itself, so a pasted full endpoint
    // must be reduced to its base rather than producing `/v1/responses/responses`.
    if let Some(suffix) = OPERATION_PATHS
        .iter()
        .find(|suffix| path.ends_with(*suffix) && path.len() > suffix.len())
    {
        path.truncate(path.len() - suffix.len());
        notes.push(format!(
            "removed the trailing `{suffix}` path; Open Grok appends it per request"
        ));
    }
    if path.is_empty() || path == "/" {
        path = "/v1".to_owned();
        notes.push(
            "added `/v1`; set the full path if your server mounts the API elsewhere".to_owned(),
        );
    }
    parsed.set_path(&path);
    // A base URL never keeps a trailing slash: every caller appends `/models`
    // or an operation path onto it.
    let base_url = parsed.to_string().trim_end_matches('/').to_owned();
    Ok(NormalizedEndpoint { base_url, notes })
}

/// One model reported by a user-supplied address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredModel {
    /// Catalog key this model would be saved under (`{host-slug}:{id}`).
    pub key: String,
    /// Provider-side model id, written to `[model.<key>].model`.
    pub id: String,
    /// Human name from the endpoint, falling back to the id.
    pub name: String,
    pub context_window: u64,
}

/// Params for `open-grok/custom-providers/discover`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CustomProviderDiscoverParams {
    /// Raw user input; normalized here so one rule set governs both discovery
    /// and what later gets persisted.
    pub server_address: String,
    pub format: String,
    /// Transient: used for this request only, never persisted by this call.
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Result for `open-grok/custom-providers/discover`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomProviderDiscoverResponse {
    pub base_url: String,
    pub format: String,
    /// Auth scheme the wizard should write onto the saved models.
    pub auth_scheme: String,
    pub models: Vec<DiscoveredModel>,
    pub notes: Vec<String>,
}

/// Run discovery: normalize the address, list models, and report the values the
/// wizard should persist alongside each selection.
pub async fn discover_models(
    params: CustomProviderDiscoverParams,
) -> Result<CustomProviderDiscoverResponse> {
    let format = CustomWireFormat::from_canonical(&params.format)
        .ok_or_else(|| anyhow!("unknown format `{}`", params.format))?;
    let endpoint = normalize_server_address(&params.server_address)?;
    let api_key = params
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned);

    let mut notes = endpoint.notes.clone();
    let models = list_models(&endpoint.base_url, format, api_key.as_deref())
        .await
        .inspect_err(|error| {
            let message = error.to_string();
            notes.push(format!("{} listing failed: {message}", format.label()));
            tracing::warn!(
                base_url = %endpoint.base_url,
                format = format.as_str(),
                error = %message,
                "custom provider model discovery failed"
            );
        })?;
    if models.is_empty() {
        notes.push(format!(
            "{} returned no models; the server may not expose a model list at {}/models",
            format.label(),
            endpoint.base_url
        ));
    }

    Ok(CustomProviderDiscoverResponse {
        base_url: endpoint.base_url,
        format: format.as_str().to_owned(),
        auth_scheme: format.auth_scheme().to_owned(),
        models,
        notes,
    })
}

/// `GET {base_url}/models` with the credential header this format expects.
async fn list_models(
    base_url: &str,
    format: CustomWireFormat,
    api_key: Option<&str>,
) -> Result<Vec<DiscoveredModel>> {
    let url = format!("{base_url}/models");
    // Never follow a redirect: it would carry the user's key to a host they did
    // not type. A server that needs a different address should be typed as one.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .context("failed to build the model discovery client")?;

    let mut request = http.get(&url).header("accept", "application/json");
    if let Some(api_key) = api_key {
        request = match format {
            CustomWireFormat::Messages => request
                .header("x-api-key", api_key)
                .header("anthropic-version", ANTHROPIC_VERSION),
            CustomWireFormat::GoogleAiStudio => request.header("x-goog-api-key", api_key),
            CustomWireFormat::ChatCompletions | CustomWireFormat::Responses => {
                request.bearer_auth(api_key)
            }
        };
    }

    let response = request.send().await.map_err(|error| {
        anyhow!(
            "could not reach {url}: {}",
            redact(&error.to_string(), api_key)
        )
    })?;
    let status = response.status();
    if status.is_redirection() {
        bail!(
            "{url} redirected ({status}); a custom provider must serve the model list directly so your key is not forwarded to another host"
        );
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "{url} returned {status}: {}",
            safe_error_excerpt(&body, api_key)
        );
    }
    let length = response.content_length().unwrap_or(0);
    if length > MAX_RESPONSE_BYTES as u64 {
        bail!("{url} returned an implausibly large model list ({length} bytes)");
    }
    let body = response.text().await.map_err(|error| {
        anyhow!(
            "could not read the model list: {}",
            redact(&error.to_string(), api_key)
        )
    })?;
    if body.len() > MAX_RESPONSE_BYTES {
        bail!("{url} returned an implausibly large model list");
    }

    let value: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
        anyhow!(
            "{url} did not return JSON; expected a model list. {}",
            safe_error_excerpt(&body, api_key)
        )
    })?;
    Ok(parse_model_entries(&value, base_url))
}

/// Accept both documented envelopes: `{"data": [...]}` (OpenAI and Anthropic)
/// and a bare array (several local runtimes), plus `{...: {"data": [...]}}`.
fn parse_model_entries(value: &serde_json::Value, base_url: &str) -> Vec<DiscoveredModel> {
    let entries = match value {
        serde_json::Value::Array(items) => items.clone(),
        _ => value
            .get("data")
            .or_else(|| value.get("models"))
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default(),
    };

    let mut models = Vec::new();
    let mut seen = Vec::new();
    for entry in entries {
        let id = ["id", "model", "name"]
            .iter()
            .filter_map(|field| entry.get(*field).and_then(serde_json::Value::as_str))
            .map(str::trim)
            .find(|id| !id.is_empty());
        let Some(id) = id else { continue };
        if !seen.iter().any(|known: &String| known == id) {
            seen.push(id.to_owned());
        } else {
            continue;
        }
        let name = ["display_name", "displayName", "name", "title"]
            .iter()
            .filter_map(|field| entry.get(*field).and_then(serde_json::Value::as_str))
            .map(str::trim)
            .find(|name| !name.is_empty() && *name != id)
            .map(str::to_owned)
            .unwrap_or_else(|| id.to_owned());
        let context_window = [
            "context_window",
            "context_length",
            "max_model_len",
            "max_input_tokens",
            "inputTokenLimit",
        ]
        .iter()
        .filter_map(|field| entry.get(*field).and_then(serde_json::Value::as_u64))
        .find(|value| *value > 0)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        models.push(DiscoveredModel {
            key: catalog_key(base_url, id),
            id: id.to_owned(),
            name,
            context_window,
        });
    }
    models
}

/// Stable `[model.<key>]` table name for one discovered model.
///
/// Grouping by host slug keeps every model of one endpoint adjacent in the
/// catalog, mirrors the `provider:{id}` convention used by the live catalogs,
/// and cannot collide with a built-in key. Model ids routinely carry `/` (which
/// is illegal in a TOML table suffix), so it is folded to `-`.
fn catalog_key(base_url: &str, model_id: &str) -> String {
    format!(
        "{}:{}",
        endpoint_slug(base_url),
        sanitize_key_part(model_id)
    )
}

/// Host(+port) slug used as the catalog key prefix.
pub fn endpoint_slug(base_url: &str) -> String {
    let host = Url::parse(base_url)
        .ok()
        .and_then(|parsed| {
            let host = parsed.host_str()?.to_owned();
            match parsed.port() {
                Some(port) => Some(format!("{host}-{port}")),
                None => Some(host),
            }
        })
        .unwrap_or_else(|| "custom".to_owned());
    let slug = sanitize_key_part(&host);
    if slug.is_empty() {
        "custom".to_owned()
    } else {
        slug
    }
}

fn sanitize_key_part(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            out.push(ch);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_matches(['-', '.', '_']).to_owned()
}

/// Trim an error body to one line, drop credentials, and cap it: this text is
/// surfaced in the TUI and recorded in logs.
fn safe_error_excerpt(body: &str, api_key: Option<&str>) -> String {
    let one_line = body.trim();
    if one_line.is_empty() {
        return "(empty response body)".to_owned();
    }
    let excerpt: String = one_line.chars().take(300).collect();
    redact(&excerpt, api_key)
}

fn redact(text: &str, api_key: Option<&str>) -> String {
    match api_key {
        Some(key) if !key.is_empty() => text.replace(key, "[redacted]"),
        _ => text.replace(['\r', '\n'], " "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, header};
    use axum::routing::get;
    use axum::{Json, Router};
    use std::sync::{Arc, Mutex};

    /// `Authorization`, `x-api-key`, `anthropic-version` as the stub server saw
    /// them.
    type SeenHeaders = (Option<String>, Option<String>, Option<String>);

    /// Captured request headers from the last discovery call, so a test can
    /// prove which credential header each wire format sends.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<SeenHeaders>>>);

    impl Captured {
        fn last(&self) -> SeenHeaders {
            self.0
                .lock()
                .expect("capture lock")
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    async fn models(State(state): State<Captured>, headers: HeaderMap) -> Json<serde_json::Value> {
        state.0.lock().expect("capture lock").push((
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            headers
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        ));
        Json(serde_json::json!({
            "object": "list",
            "data": [
                { "id": "qwen3-coder", "name": "Qwen3 Coder", "context_length": 131072 },
                { "id": "namespace/llama-3.3", "display_name": "Llama 3.3" },
                { "id": "qwen3-coder" }
            ]
        }))
    }

    async fn spawn_stub(state: Captured) -> String {
        let app = Router::new()
            .route("/v1/models", get(models))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}/v1")
    }

    #[test]
    fn server_address_gets_a_scheme_and_the_v1_mount_point() {
        let endpoint = normalize_server_address("api.example.com").unwrap();
        assert_eq!(endpoint.base_url, "https://api.example.com/v1");
        assert!(
            endpoint
                .notes
                .iter()
                .any(|note| note.contains("/v1") && !note.contains("http sends"))
        );
    }

    #[test]
    fn server_address_keeps_an_explicit_path_and_strips_the_operation_suffix() {
        let endpoint =
            normalize_server_address("http://localhost:11434/v1/chat/completions/").unwrap();
        assert_eq!(endpoint.base_url, "http://localhost:11434/v1");
        assert!(
            endpoint
                .notes
                .iter()
                .any(|note| note.contains("/chat/completions"))
        );
        assert!(
            endpoint
                .notes
                .iter()
                .any(|note| note.contains("unencrypted"))
        );
    }

    #[test]
    fn server_address_keeps_a_custom_gateway_prefix() {
        let endpoint = normalize_server_address("https://gateway.internal/team-a/openai").unwrap();
        assert_eq!(endpoint.base_url, "https://gateway.internal/team-a/openai");
        assert!(endpoint.notes.is_empty());
    }

    #[test]
    fn server_address_rejects_credentials_queries_and_non_http() {
        for raw in [
            "https://user:sekret@api.example.com/v1",
            "https://api.example.com/v1?key=abc",
            "https://api.example.com/v1#section",
            "ftp://api.example.com/v1",
            "",
            "ht tp://api.example.com",
        ] {
            assert!(
                normalize_server_address(raw).is_err(),
                "must reject `{raw}`"
            );
        }
    }

    #[test]
    fn empty_address_is_an_error_not_a_panic() {
        let error = normalize_server_address("   ").unwrap_err().to_string();
        assert!(error.contains("required"), "{error}");
    }

    #[test]
    fn wire_format_round_trips_and_picks_the_credential_header() {
        for format in CustomWireFormat::ALL {
            assert_eq!(
                CustomWireFormat::from_canonical(format.as_str()),
                Some(format)
            );
        }
        assert_eq!(
            CustomWireFormat::from_canonical("Anthropic"),
            Some(CustomWireFormat::Messages)
        );
        assert_eq!(
            CustomWireFormat::from_canonical("openai_chat_completions"),
            Some(CustomWireFormat::ChatCompletions)
        );
        assert_eq!(
            CustomWireFormat::from_canonical("google_ai_studio"),
            Some(CustomWireFormat::GoogleAiStudio)
        );
        assert_eq!(
            CustomWireFormat::from_canonical("gemini"),
            Some(CustomWireFormat::GoogleAiStudio)
        );
        assert_eq!(CustomWireFormat::from_canonical("grpc"), None);
        assert_eq!(CustomWireFormat::Messages.auth_scheme(), "x_api_key");
        assert_eq!(CustomWireFormat::Responses.auth_scheme(), "bearer");
        assert_eq!(
            CustomWireFormat::GoogleAiStudio.auth_scheme(),
            "x_goog_api_key"
        );
        assert_eq!(
            CustomWireFormat::Responses.api_backend(),
            xai_grok_sampling_types::ApiBackend::Responses
        );
        assert_eq!(
            CustomWireFormat::GoogleAiStudio.api_backend(),
            xai_grok_sampling_types::ApiBackend::GoogleAiStudio
        );
    }

    #[test]
    fn catalog_keys_stay_legal_as_toml_table_names() {
        let key = catalog_key("http://localhost:11434/v1", "namespace/llama-3.3:70b");
        assert_eq!(key, "localhost-11434:namespace-llama-3.3-70b");
        assert_eq!(
            endpoint_slug("https://api.example.com/v1"),
            "api.example.com"
        );
        assert_eq!(endpoint_slug("not a url"), "custom");
    }

    #[test]
    fn model_list_parses_both_envelopes_and_dedupes() {
        let openai: serde_json::Value = serde_json::from_str(
            r#"{"data":[{"id":"a"},{"id":"b","context_window":8192},{"id":"a"}]}"#,
        )
        .unwrap();
        let parsed = parse_model_entries(&openai, "https://gw.example.com/v1");
        assert_eq!(
            parsed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(parsed[1].context_window, 8192);
        assert_eq!(parsed[0].context_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(parsed[0].key, "gw.example.com:a");

        let ollama: serde_json::Value =
            serde_json::from_str(r#"[{"name":"llama3:latest"}]"#).unwrap();
        let parsed = parse_model_entries(&ollama, "http://localhost:11434/v1");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "llama3:latest");

        assert!(
            parse_model_entries(&serde_json::json!({"error": "boom"}), "https://x.test/v1")
                .is_empty()
        );
    }

    #[test]
    fn error_excerpts_never_leak_the_key() {
        let body = "invalid x-api-key provided: sk-secret-value sk-secret-value";
        let excerpt = safe_error_excerpt(body, Some("sk-secret-value"));
        assert!(excerpt.contains("[redacted]"), "{excerpt}");
        assert!(!excerpt.contains("sk-secret-value"));
        let long = "x".repeat(5_000);
        assert_eq!(safe_error_excerpt(&long, None).chars().count(), 300);
    }

    #[tokio::test]
    async fn discovery_sends_the_openai_credential_header() {
        let state = Captured::default();
        let base = spawn_stub(state.clone()).await;
        let models = list_models(&base, CustomWireFormat::Responses, Some("sk-test"))
            .await
            .expect("responses discovery");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "Qwen3 Coder");
        assert_eq!(models[1].id, "namespace/llama-3.3");
        assert_eq!(
            state.last().0.as_deref(),
            Some("Bearer sk-test"),
            "OpenAI-compatible servers take a bearer token"
        );
        assert_eq!(state.last().1, None);
    }

    #[tokio::test]
    async fn discovery_sends_the_anthropic_headers_and_requires_the_version() {
        let state = Captured::default();
        let base = spawn_stub(state.clone()).await;
        list_models(&base, CustomWireFormat::Messages, Some("sk-ant-test"))
            .await
            .expect("messages discovery");
        let (authorization, api_key, version) = state.last();
        assert_eq!(authorization, None, "Messages must not send a bearer token");
        assert_eq!(api_key.as_deref(), Some("sk-ant-test"));
        assert_eq!(version.as_deref(), Some(ANTHROPIC_VERSION));
    }

    #[tokio::test]
    async fn discovery_works_without_a_key_for_open_local_servers() {
        let state = Captured::default();
        let base = spawn_stub(state.clone()).await;
        let models = list_models(&base, CustomWireFormat::ChatCompletions, None)
            .await
            .expect("unauthenticated local discovery");
        assert_eq!(models.len(), 2);
        assert_eq!(state.last(), (None, None, None));
    }

    #[tokio::test]
    async fn discovery_errors_name_the_url_and_redact_the_key() {
        let state = Captured::default();
        let base = spawn_stub(state.clone()).await;
        // A path with no stubbed route answers 404 from the router's fallback.
        let error = list_models(
            &format!("{base}/missing"),
            CustomWireFormat::Responses,
            Some("sk-leak"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("/models"), "{error}");
        assert!(!error.contains("sk-leak"), "{error}");
    }

    #[tokio::test]
    async fn discovery_reports_unreachable_hosts_without_panicking() {
        let error = list_models("http://127.0.0.1:1/v1", CustomWireFormat::Responses, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("could not reach"), "{error}");
    }

    #[tokio::test]
    async fn discover_models_reports_the_base_url_and_auth_scheme_together() {
        let state = Captured::default();
        let base = spawn_stub(state.clone()).await;
        let response = discover_models(CustomProviderDiscoverParams {
            server_address: base.clone(),
            format: "messages".to_owned(),
            api_key: Some("sk-ant-test".to_owned()),
        })
        .await
        .expect("discover");
        assert_eq!(response.base_url, base);
        assert_eq!(response.format, "messages");
        assert_eq!(response.auth_scheme, "x_api_key");
        assert_eq!(response.models.len(), 2);
    }

    #[tokio::test]
    async fn discover_models_rejects_an_unknown_format_before_the_network() {
        let error = discover_models(CustomProviderDiscoverParams {
            server_address: "https://api.example.com/v1".to_owned(),
            format: "graphql".to_owned(),
            api_key: None,
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown format"), "{error}");
    }
}
