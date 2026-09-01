//! `image_gen` tool — generates images via the configured Grok Imagine or
//! OpenAI Images service and saves them to the local filesystem so the model
//! can reference them in code
//! (e.g. `<img src="images/hero.jpg">`).
//!
//! Architecture follows the same pattern as `web_search`:
//!
//! - [`ImageGenConfig`] is built from session credentials by the host and
//!   injected into the tool registry.
//! - When `Enabled`, an [`ImageGenClient`] is constructed once and injected
//!   into `Resources`. The tool reads it at runtime via `resources.require()`.
//! - When `Disabled`, the tool is not registered so the model never sees it.
//!
//! The generated image is written to `<session_folder>/images/<n>.jpg`
//! where `<n>` is a session-scoped counter (1, 2, 3, ... — 1 token each).
//! The tool returns the absolute path so the model can copy or move the
//! image into the project working directory when it needs a persistent asset.

use base64::Engine as _;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};

use crate::attribution::{SharedAttributionCallback, ToolConsumer};
use crate::types::{ImageGenerationProvider, SharedApiKeyProvider};

use crate::types::output::{MediaGenOutput, ToolOutput};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::{ImageGenerationTurnId, SessionFolder};
use crate::types::tool::{ToolKind, ToolNamespace};

/// Default Imagine model for `image_gen`. Used unless an explicit
/// `model_override` is supplied via `ImageGenConfig::Enabled`.
const XAI_IMAGINE_MODEL: &str = "grok-imagine-image-quality";
/// Codex's image-generation extension model at the pinned upstream contract.
const OPENAI_IMAGE_MODEL: &str = "gpt-image-2";
const CODEX_IMAGE_TURN_ID_HEADER: &str = "x-codex-image-turn-id";
// Some Imagine models (e.g. `grok-imagine-image`, selectable via `model_override`)
// expand the prompt then generate, and the proxy buffers
// the whole image before sending any bytes — so the client may receive nothing
// for well over a minute. Keep these generous so a slow-but-progressing
// generation isn't cut off.
const IMAGE_GEN_TIMEOUT_SECS: u64 = 300;
const IMAGE_GEN_READ_TIMEOUT_SECS: u64 = 240;
const DEFAULT_IMAGE_DIR: &str = "images";

pub use xai_grok_tools_api::slash_commands::{
    IMAGE_GEN_TOOL_NAME, IMAGINE_COMMAND_NAME, imagine_instruction, imagine_usage_message,
};

/// Prose returned to the model (as a normal, successful tool result) when a
/// free / X Basic user calls `image_gen` or `image_edit`. The model relays it
/// to the user. The deliberate `/imagine` slash command shows the richer
/// SuperGrok upsell modal instead; this covers the natural-language path.
pub(crate) const TIER_RESTRICTED_UPSELL: &str = "Image generation is a SuperGrok feature and isn't available on the free or X Basic tier. Let the user know they can unlock image and video generation by upgrading to SuperGrok: https://grok.com/supergrok?referrer=grok-build. Do not retry this tool.";

/// HTTP client for the configured image provider. Cloned per-request; shares
/// `Arc` state.
#[derive(Clone)]
pub struct ImageGenClient {
    http: reqwest::Client,
    provider: ImageGenerationProvider,
    base_url: String,
    /// Imagine model slug used by `generate()`. Selected at construction
    /// from `ImageGenConfig::model_override` (falling back to
    /// [`XAI_IMAGINE_MODEL`]). `image_edit` uses its own model and is
    /// unaffected.
    model: String,
    edit_model: String,
    writer: super::storage::SessionFileWriter,
    api_key_provider: Option<SharedApiKeyProvider>,
    require_live_bearer: bool,
    /// Optional 401-attribution hook. Hosts wire this so a 401 from the
    /// Imagine API emits an `auth_401_attribution` event with
    /// `consumer == "ImageGen"` for unified auth-failure telemetry.
    attribution_callback: Option<SharedAttributionCallback>,
    /// When `true`, the user is on a tier the Imagine server zero-limits
    /// (free / X Basic). `image_gen` / `image_edit` short-circuit before any
    /// HTTP call and return the SuperGrok upsell prose instead. See
    /// [`ImageGenClient::is_tier_restricted`].
    tier_restricted: bool,
    session_header: Option<HeaderValue>,
    defaults_have_session_header: bool,
}

impl ImageGenClient {
    pub fn new(
        config: &ImageGenConfig,
        api_key_provider: Option<SharedApiKeyProvider>,
    ) -> Result<Self, xai_tool_runtime::ToolError> {
        let ImageGenConfig::Enabled {
            provider,
            api_key,
            base_url,
            extra_headers,
            api_key_provider: config_api_key_provider,
            model_override,
            edit_model_override,
            tier_restricted,
            ..
        } = config
        else {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "Cannot create ImageGenClient from disabled config",
            ));
        };
        let (model, edit_model, extension) = match provider {
            ImageGenerationProvider::Grok => (
                model_override
                    .clone()
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or_else(|| XAI_IMAGINE_MODEL.to_owned()),
                edit_model_override
                    .clone()
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or_else(|| super::image_edit::XAI_IMAGINE_EDIT_MODEL.to_owned()),
                "jpg",
            ),
            ImageGenerationProvider::OpenAi => (
                OPENAI_IMAGE_MODEL.to_owned(),
                OPENAI_IMAGE_MODEL.to_owned(),
                "png",
            ),
        };

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Grok Imagine bakes the static key as the fallback Authorization;
        // the live provider overrides per request. OpenAI Images is
        // ChatGPT-OAuth-only (matching upstream's `uses_codex_backend` gate,
        // which excludes API-key auth): no static bearer is ever baked, the
        // identity-anchored live resolver is mandatory, and it fails closed
        // on logout or account drift.
        if *provider == ImageGenerationProvider::Grok {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|e| {
                    xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "Invalid API key for header: {e}"
                    ))
                })?,
            );
        }

        extra_headers.into_iter().try_for_each(|(key, value)| {
            let header_name =
                reqwest::header::HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
                    xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "Invalid header name '{key}': {e}"
                    ))
                })?;
            let header_value = HeaderValue::from_str(value).map_err(|e| {
                xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "Invalid header value for '{key}': {e}"
                ))
            })?;
            headers.insert(header_name, header_value);
            Ok::<(), xai_tool_runtime::ToolError>(())
        })?;

        let defaults_have_session_header = headers.contains_key(SESSION_ID_HEADER);
        let key = crate::util::shared_http::cache_key(
            &format!("image_gen:{provider:?}:{base_url}"),
            &headers,
        );
        let http = crate::util::shared_http::cached_client(key, || {
            xai_grok_extra_ca::build_reqwest_client(|builder| {
                builder
                    .timeout(std::time::Duration::from_secs(IMAGE_GEN_TIMEOUT_SECS))
                    .read_timeout(std::time::Duration::from_secs(IMAGE_GEN_READ_TIMEOUT_SECS))
                    .default_headers(headers.clone())
            })
        })
        .map_err(|e| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Failed to build HTTP client: {e}"
            ))
        })?;

        // The legacy session-wide provider supplies the xAI bearer, so it may
        // only ever back the Grok route; the OpenAI route accepts just its
        // config-level Codex resolver and must fail closed without it.
        let api_key_provider = match provider {
            ImageGenerationProvider::Grok => config_api_key_provider.clone().or(api_key_provider),
            ImageGenerationProvider::OpenAi => config_api_key_provider.clone(),
        };

        Ok(Self {
            http,
            provider: *provider,
            base_url: base_url.clone(),
            model,
            edit_model,
            writer: super::storage::SessionFileWriter::new(DEFAULT_IMAGE_DIR, extension),
            api_key_provider,
            require_live_bearer: *provider == ImageGenerationProvider::OpenAi,
            attribution_callback: None,
            tier_restricted: *provider == ImageGenerationProvider::Grok && *tier_restricted,
            session_header: None,
            defaults_have_session_header,
        })
    }

    pub fn with_session_id(mut self, session_id: &str) -> Self {
        if self.provider == ImageGenerationProvider::Grok
            && !self.defaults_have_session_header
            && let Ok(value) = HeaderValue::from_str(session_id)
        {
            self.session_header = Some(value);
        }
        self
    }

    /// Whether the current user's tier (free / X Basic) is zero-limited on
    /// Imagine server-side. `image_gen` / `image_edit` use this to short-circuit
    /// with the SuperGrok upsell instead of issuing a doomed request.
    pub(crate) fn is_tier_restricted(&self) -> bool {
        self.tier_restricted
    }

    /// Wire a 401-attribution callback into this client. Idempotent;
    /// safe to call before or after the first request. Builder-style
    /// so `new()` callers that don't care can ignore it.
    pub fn with_attribution_callback(
        mut self,
        callback: Option<SharedAttributionCallback>,
    ) -> Self {
        self.attribution_callback = callback;
        self
    }

    pub(crate) async fn current_bearer(&self) -> Option<String> {
        crate::types::api_key_provider::resolve_bearer(self.api_key_provider.as_ref()).await
    }

    pub(crate) fn record_401_attribution(&self, consumer: ToolConsumer, sent_bearer: Option<&str>) {
        crate::attribution::emit_401(self.attribution_callback.as_ref(), consumer, sent_bearer);
    }

    pub fn provider(&self) -> ImageGenerationProvider {
        self.provider
    }

    fn post_json(
        &self,
        url: &str,
        payload: &serde_json::Value,
        sent_bearer: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut request = self.http.post(url).json(payload);
        if let Some(bearer) = sent_bearer {
            request = request.header(AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(session) = &self.session_header {
            request = request.header(SESSION_ID_HEADER, session.clone());
        }
        request
    }

    pub(crate) fn writer(&self) -> &super::storage::SessionFileWriter {
        &self.writer
    }

    pub async fn generate(
        &self,
        prompt: &str,
        aspect_ratio: &str,
        turn_id: &str,
    ) -> Result<Vec<u8>, xai_tool_runtime::ToolError> {
        let payload = match self.provider {
            ImageGenerationProvider::Grok => serde_json::json!({
                "model": self.model,
                "prompt": prompt,
                "n": 1,
                "aspect_ratio": aspect_ratio,
                "resolution": "1k",
                "response_format": "b64_json",
            }),
            ImageGenerationProvider::OpenAi => serde_json::json!({
                "model": self.model,
                "prompt": prompt,
                "background": "auto",
                "quality": "auto",
                "size": openai_size(aspect_ratio),
            }),
        };
        self.send_image_request("generations", "generation", payload, turn_id)
            .await
    }

    pub(crate) async fn edit(
        &self,
        prompt: &str,
        data_urls: &[String],
        aspect_ratio: &str,
        turn_id: &str,
    ) -> Result<Vec<u8>, xai_tool_runtime::ToolError> {
        if self.provider == ImageGenerationProvider::OpenAi && data_urls.len() > 5 {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "OpenAI image editing accepts at most 5 reference images.",
            ));
        }
        let payload = match self.provider {
            ImageGenerationProvider::Grok => {
                let mut payload = serde_json::json!({
                    "model": self.edit_model,
                    "prompt": prompt,
                    "n": 1,
                    "resolution": "1k",
                    "response_format": "b64_json",
                });
                let mut images: Vec<serde_json::Value> = data_urls
                    .iter()
                    .map(|url| serde_json::json!({ "url": url }))
                    .collect();
                if images.len() == 1 {
                    payload["image"] = images.pop().expect("one image exists");
                } else {
                    payload["images"] = serde_json::Value::Array(images);
                    payload["aspect_ratio"] = serde_json::json!(aspect_ratio);
                }
                payload
            }
            ImageGenerationProvider::OpenAi => serde_json::json!({
                "model": self.edit_model,
                "prompt": prompt,
                "background": "auto",
                "quality": "auto",
                "size": openai_size(aspect_ratio),
                "images": data_urls
                    .iter()
                    .map(|url| serde_json::json!({ "image_url": url }))
                    .collect::<Vec<_>>(),
            }),
        };
        self.send_image_request("edits", "edit", payload, turn_id)
            .await
    }

    async fn send_image_request(
        &self,
        endpoint: &str,
        operation: &str,
        payload: serde_json::Value,
        turn_id: &str,
    ) -> Result<Vec<u8>, xai_tool_runtime::ToolError> {
        let url = format!("{}/images/{endpoint}", self.base_url.trim_end_matches('/'));

        // Capture the bearer once so the request and the 401-attribution
        // emit see the same value (even if the provider rotates between
        // the send and the response handling).
        let sent_bearer = self.current_bearer().await;
        if self.require_live_bearer && sent_bearer.is_none() {
            return Err(xai_tool_runtime::ToolError::new(
                xai_tool_runtime::ToolErrorKind::Custom,
                "OpenAI image authentication is unavailable; sign in again with `open-grok login --codex`.",
            )
            .with_details(serde_json::json!({"code": "auth_required", "status": 401})));
        }
        let mut req = self.post_json(&url, &payload, sent_bearer.as_deref());
        if self.provider == ImageGenerationProvider::OpenAi {
            req = req.header(CODEX_IMAGE_TURN_ID_HEADER, turn_id);
        }

        let response = req.send().await.map_err(|e| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Image {operation} API request failed: {e}"
            ))
        })?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.record_401_attribution(ToolConsumer::ImageGen, sent_bearer.as_deref());
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let truncated: String = body.chars().take(200).collect();
            tracing::warn!(
                provider = self.provider.as_canonical(),
                http_status = %status,
                "image {operation} API error: {truncated}"
            );
            return Err(xai_tool_runtime::ToolError::new(
                xai_tool_runtime::ToolErrorKind::Custom,
                format!("Image {operation} failed with HTTP {status}: {truncated}"),
            )
            .with_details(serde_json::json!({"code": "http_failure", "status": status.as_u16()})));
        }

        let body = response.text().await.map_err(|e| {
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Failed to read image {operation} response body: {e}"
            ))
        })?;

        let resp_json: ImageGenResponse = serde_json::from_str(&body).map_err(|e| {
            let preview: String = body.chars().take(500).collect();
            tracing::warn!(
                provider = self.provider.as_canonical(),
                "image {operation} API returned unparseable body: {preview}"
            );
            xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Failed to parse image {operation} response: {e} — body preview: {preview}"
            ))
        })?;

        let b64_data = resp_json.b64_data().unwrap_or("");

        if b64_data.is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Image {operation} returned no image data."
            )));
        }

        base64::engine::general_purpose::STANDARD
            .decode(b64_data)
            .map_err(|e| {
                xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "Failed to decode base64 image data: {e}"
                ))
            })
    }
}

fn openai_size(aspect_ratio: &str) -> &'static str {
    match aspect_ratio.trim() {
        "16:9" | "3:2" | "4:3" | "2:1" | "19.5:9" | "20:9" => "1536x1024",
        "9:16" | "2:3" | "3:4" | "1:2" | "9:19.5" | "9:20" => "1024x1536",
        "1:1" => "1024x1024",
        _ => "auto",
    }
}

/// `Enabled` means credentials are present; each tool has its own gate.
#[derive(Clone, Default)]
pub enum ImageGenConfig {
    #[default]
    Disabled,
    Enabled {
        provider: ImageGenerationProvider,
        /// Static credential baked as the fallback Authorization header for
        /// the Grok route. Ignored for OpenAI, which is ChatGPT-OAuth-only
        /// and resolves every bearer live from `api_key_provider`.
        api_key: String,
        base_url: String,
        extra_headers: indexmap::IndexMap<String, String>,
        /// Provider-specific live bearer source. OpenAI images require the
        /// identity-anchored Codex resolver (mandatory — requests fail closed
        /// without it); Grok Imagine uses the xAI auth manager. For Grok this
        /// wins over the legacy session-wide provider; the legacy provider is
        /// never consulted for OpenAI.
        api_key_provider: Option<SharedApiKeyProvider>,
        image_gen_enabled: bool,
        image_edit_enabled: bool,
        /// Optional Imagine model override for `image_gen`. When `Some(non-empty)`,
        /// `image_gen` calls that model instead of the default quality model
        /// ([`XAI_IMAGINE_MODEL`]). Driven by the remote
        /// `image_gen_model_override` config flag. `image_edit` is unaffected.
        model_override: Option<String>,
        edit_model_override: Option<String>,
        /// `true` when the user is on a tier the Imagine server zero-limits
        /// (free / X Basic). The tools stay advertised to the model, but
        /// `image_gen` / `image_edit` short-circuit at call time with the
        /// SuperGrok upsell prose instead of a doomed request. Set by the
        /// host from the subscription tier; always `false` for team /
        /// API-key / workspace callers.
        tier_restricted: bool,
    },
}

impl std::fmt::Debug for ImageGenConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("ImageGenConfig::Disabled"),
            Self::Enabled {
                provider,
                base_url,
                extra_headers,
                api_key_provider,
                image_gen_enabled,
                image_edit_enabled,
                model_override,
                edit_model_override,
                tier_restricted,
                ..
            } => formatter
                .debug_struct("ImageGenConfig::Enabled")
                .field("provider", provider)
                .field("base_url", base_url)
                .field(
                    "extra_header_names",
                    &extra_headers.keys().collect::<Vec<_>>(),
                )
                .field("has_live_api_key_provider", &api_key_provider.is_some())
                .field("image_gen_enabled", image_gen_enabled)
                .field("image_edit_enabled", image_edit_enabled)
                .field("model_override", model_override)
                .field("edit_model_override", edit_model_override)
                .field("tier_restricted", tier_restricted)
                .finish_non_exhaustive(),
        }
    }
}

/// Session-id header attached to imagine API requests; matches the header
/// chat requests already carry.
pub const SESSION_ID_HEADER: &str = "x-grok-session-id";

impl ImageGenConfig {
    /// Credentials present — required to construct any of the clients.
    pub fn has_credentials(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    pub fn provider(&self) -> Option<ImageGenerationProvider> {
        match self {
            Self::Enabled { provider, .. } => Some(*provider),
            Self::Disabled => None,
        }
    }

    /// Stamp [`SESSION_ID_HEADER`] onto `extra_headers`. A caller-provided
    /// value is never overwritten. No-op when `Disabled`.
    pub fn stamp_session_id_header(&mut self, session_id: &str) {
        if let Self::Enabled {
            provider: ImageGenerationProvider::Grok,
            extra_headers,
            ..
        } = self
        {
            extra_headers
                .entry(SESSION_ID_HEADER.to_string())
                .or_insert_with(|| session_id.to_string());
        }
    }

    pub fn image_gen_enabled(&self) -> bool {
        matches!(
            self,
            Self::Enabled {
                image_gen_enabled: true,
                ..
            }
        )
    }

    pub fn image_edit_enabled(&self) -> bool {
        matches!(
            self,
            Self::Enabled {
                image_edit_enabled: true,
                ..
            }
        )
    }

    /// The configured `image_gen` model override, if any. `None` means the
    /// default quality model ([`XAI_IMAGINE_MODEL`]) is used.
    pub fn model_override(&self) -> Option<&str> {
        match self {
            Self::Enabled { model_override, .. } => {
                model_override.as_deref().filter(|m| !m.trim().is_empty())
            }
            Self::Disabled => None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ImageGenInput {
    #[schemars(description = "Text description of the image to generate.")]
    pub prompt: String,

    #[serde(default = "default_aspect_ratio")]
    #[schemars(
        description = "Aspect ratio of the generated image, decide it based on the user's request. Defaults to 'auto'. 1:1 for square (icons, profiles), 16:9 for wide (landscapes, cinematic), 9:16 for tall (phone wallpapers, stories), 3:2 for horizontal photos, 2:3 for vertical (portraits, posters)."
    )]
    pub aspect_ratio: String,
}

fn default_aspect_ratio() -> String {
    "auto".to_owned()
}

#[derive(Debug, serde::Deserialize)]
pub struct ImageGenResponse {
    #[serde(default)]
    data: Vec<ImageGenData>,
}

impl ImageGenResponse {
    pub fn b64_data(&self) -> Option<&str> {
        self.data.first().and_then(|d| d.b64_json.as_deref())
    }
}

#[derive(Debug, serde::Deserialize)]
struct ImageGenData {
    b64_json: Option<String>,
}

#[derive(Debug, Default)]
pub struct ImageGenTool;

impl crate::types::tool_metadata::ToolMetadata for ImageGenTool {
    fn kind(&self) -> ToolKind {
        ToolKind::ImageGen
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        "Generate a new image from a text description using the configured image provider; returns the saved image's absolute path. When telling the user where it was saved, refer to it by its short session-relative path (for example `images/1.png`) rather than the absolute path, so it renders as a clickable link that opens the image. To produce multiple images, emit multiple tool calls with distinct prompts."
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for ImageGenTool {
    type Args = ImageGenInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("image_gen").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "image_gen",
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.image_gen",
        skip_all,
        fields(prompt_len = input.prompt.len(), aspect_ratio = %input.aspect_ratio)
    )]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ImageGenInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let (client, turn_id) = {
            let res = resources.lock().await;
            let client = res.require::<ImageGenClient>()?.clone();
            let turn_id = res
                .get::<ImageGenerationTurnId>()
                .map(|value| value.0.clone())
                .unwrap_or_else(|| ctx.call_id.as_str().to_owned());
            (client, turn_id)
        };

        // Free / X Basic users are zero-limited on Imagine server-side; return
        // the upsell prose instead of a doomed request (the tool stays
        // advertised so the model can surface the nudge in-conversation).
        if client.is_tier_restricted() {
            return Ok(ToolOutput::Text(TIER_RESTRICTED_UPSELL.into()));
        }

        let image_bytes = client
            .generate(&input.prompt, &input.aspect_ratio, &turn_id)
            .await?;

        let session_folder = {
            let res = resources.lock().await;
            res.require::<SessionFolder>()?.0.clone()
        };

        let absolute_path = client
            .writer
            .save(&session_folder, &image_bytes, None)
            .await
            .map_err(|e| xai_tool_runtime::ToolError::invalid_arguments(e.to_string()))?;

        tracing::info!(
            path = %absolute_path.display(),
            bytes = image_bytes.len(),
            "image saved to disk"
        );

        Ok(ToolOutput::ImageGen(MediaGenOutput::new(absolute_path)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::types::tool_metadata::test_ctx_with_call_id;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Debug)]
    struct StaticBearer(&'static str);
    impl crate::types::ApiKeyProvider for StaticBearer {
        fn current_api_key(&self) -> Option<String> {
            Some(self.0.to_owned())
        }
    }

    fn openai_config(base_url: String) -> ImageGenConfig {
        let mut headers = indexmap::IndexMap::new();
        headers.insert("originator".to_owned(), "codex_cli_rs".to_owned());
        ImageGenConfig::Enabled {
            provider: ImageGenerationProvider::OpenAi,
            // Never sent: the OpenAI route is OAuth-only and resolves the
            // bearer live from `api_key_provider`.
            api_key: "ignored-static-key".into(),
            base_url,
            extra_headers: headers,
            api_key_provider: Some(Arc::new(StaticBearer("codex-token"))),
            image_gen_enabled: true,
            image_edit_enabled: true,
            model_override: Some("must-not-cross-providers".into()),
            edit_model_override: Some("must-not-cross-providers".into()),
            tier_restricted: true,
        }
    }

    #[test]
    fn session_headers_remain_provider_local_and_live_bearers_win() {
        let url = "https://example.test/images/generations";
        let codex = ImageGenClient::new(&openai_config("https://example.test".into()), None)
            .unwrap()
            .with_session_id("xai-session");
        let codex_request = codex
            .post_json(url, &serde_json::json!({}), Some("live-codex"))
            .build()
            .unwrap();
        assert!(!codex_request.headers().contains_key(SESSION_ID_HEADER));
        assert_eq!(codex_request.headers()[AUTHORIZATION], "Bearer live-codex");

        let config = ImageGenConfig::Enabled {
            provider: ImageGenerationProvider::Grok,
            api_key: "static-example".into(),
            base_url: "https://example.test".into(),
            extra_headers: indexmap::IndexMap::new(),
            api_key_provider: None,
            image_gen_enabled: true,
            image_edit_enabled: true,
            model_override: None,
            edit_model_override: None,
            tier_restricted: false,
        };
        let grok = ImageGenClient::new(&config, None)
            .unwrap()
            .with_session_id("xai-session");
        let grok_request = grok
            .post_json(url, &serde_json::json!({}), Some("live-grok"))
            .build()
            .unwrap();
        assert_eq!(grok_request.headers()[SESSION_ID_HEADER], "xai-session");
        assert_eq!(grok_request.headers()[AUTHORIZATION], "Bearer live-grok");
    }

    #[test]
    fn tool_name_and_description() {
        let tool = ImageGenTool;
        assert_eq!(xai_tool_runtime::Tool::id(&tool).as_str(), "image_gen");
        assert!(
            crate::types::tool_metadata::ToolMetadata::description_template(&tool)
                .contains("Generate a new image from a text description")
        );
    }

    #[test]
    fn default_aspect_ratio_is_auto() {
        let input: ImageGenInput = serde_json::from_str(r#"{"prompt": "test"}"#).unwrap();
        assert_eq!(input.aspect_ratio, "auto");
    }

    #[test]
    fn openai_size_maps_supported_orientations() {
        assert_eq!(openai_size("1:1"), "1024x1024");
        assert_eq!(openai_size("16:9"), "1536x1024");
        assert_eq!(openai_size("9:16"), "1024x1536");
        assert_eq!(openai_size("auto"), "auto");
        assert_eq!(openai_size("unexpected"), "auto");
    }

    #[tokio::test]
    async fn openai_generation_matches_codex_images_contract() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .and(header("authorization", "Bearer codex-token"))
            .and(header("originator", "codex_cli_rs"))
            .and(header(CODEX_IMAGE_TURN_ID_HEADER, "turn-123"))
            .and(body_json(serde_json::json!({
                "model": OPENAI_IMAGE_MODEL,
                "prompt": "a red fox",
                "background": "auto",
                "quality": "auto",
                "size": "1536x1024",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"b64_json": base64::engine::general_purpose::STANDARD.encode(b"png")}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = ImageGenClient::new(&openai_config(server.uri()), None).unwrap();
        assert_eq!(client.provider(), ImageGenerationProvider::OpenAi);
        assert_eq!(client.model, OPENAI_IMAGE_MODEL);
        assert_eq!(
            client
                .generate("a red fox", "16:9", "turn-123")
                .await
                .unwrap(),
            b"png"
        );
        assert!(!client.is_tier_restricted());
    }

    #[tokio::test]
    async fn openai_edit_uses_image_url_array() {
        let server = MockServer::start().await;
        let references = vec!["data:image/png;base64,AAAA".to_owned()];
        Mock::given(method("POST"))
            .and(path("/images/edits"))
            .and(header(CODEX_IMAGE_TURN_ID_HEADER, "turn-edit"))
            .and(body_json(serde_json::json!({
                "model": OPENAI_IMAGE_MODEL,
                "prompt": "make it blue",
                "background": "auto",
                "quality": "auto",
                "size": "1024x1536",
                "images": [{"image_url": "data:image/png;base64,AAAA"}],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"b64_json": base64::engine::general_purpose::STANDARD.encode(b"edited")}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = ImageGenClient::new(&openai_config(server.uri()), None).unwrap();
        assert_eq!(
            client
                .edit("make it blue", &references, "9:16", "turn-edit")
                .await
                .unwrap(),
            b"edited"
        );
    }

    #[tokio::test]
    async fn image_tool_reuses_logical_turn_id_across_calls() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .and(header(CODEX_IMAGE_TURN_ID_HEADER, "logical-turn"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"b64_json": base64::engine::general_purpose::STANDARD.encode(b"png")}]
            })))
            .expect(2)
            .mount(&server)
            .await;

        let client = ImageGenClient::new(&openai_config(server.uri()), None).unwrap();
        let session = tempfile::tempdir().unwrap();
        let mut resources = crate::types::resources::Resources::new();
        resources.insert(client);
        resources.insert(SessionFolder(session.path().to_path_buf()));
        resources.insert(ImageGenerationTurnId("logical-turn".to_owned()));
        let resources = resources.into_shared();

        for call_id in ["call-one", "call-two"] {
            let output = xai_tool_runtime::Tool::run(
                &ImageGenTool,
                test_ctx_with_call_id(resources.clone(), call_id),
                ImageGenInput {
                    prompt: "test".to_owned(),
                    aspect_ratio: "auto".to_owned(),
                },
            )
            .await
            .unwrap();
            assert!(matches!(output, ToolOutput::ImageGen(_)));
        }
    }

    #[tokio::test]
    async fn openai_live_auth_fails_closed_without_current_bearer() {
        #[derive(Debug)]
        struct MissingBearer;
        impl crate::types::ApiKeyProvider for MissingBearer {
            fn current_api_key(&self) -> Option<String> {
                None
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"b64_json": base64::engine::general_purpose::STANDARD.encode(b"png")}]
            })))
            .expect(0)
            .mount(&server)
            .await;
        let mut config = openai_config(server.uri());
        let ImageGenConfig::Enabled {
            api_key_provider, ..
        } = &mut config
        else {
            unreachable!()
        };
        *api_key_provider = Some(Arc::new(MissingBearer));

        let error = ImageGenClient::new(&config, None)
            .unwrap()
            .generate("test", "auto", "turn")
            .await
            .expect_err("a missing live Codex bearer must fail before egress");
        assert!(error.to_string().contains("login --codex"));
        let requests = server.received_requests().await.unwrap();
        assert!(requests.is_empty(), "no prompt may leave without live auth");
    }

    #[tokio::test]
    async fn openai_without_live_resolver_never_falls_back_to_static_or_legacy_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"b64_json": base64::engine::general_purpose::STANDARD.encode(b"png")}]
            })))
            .expect(0)
            .mount(&server)
            .await;
        let mut config = openai_config(server.uri());
        let ImageGenConfig::Enabled {
            api_key_provider, ..
        } = &mut config
        else {
            unreachable!()
        };
        // API-key-style configuration: only a static key, no live resolver.
        *api_key_provider = None;

        // The legacy session-wide provider (xAI bearer) must also be ignored
        // for the OpenAI route — no credential may cross providers.
        let error =
            ImageGenClient::new(&config, Some(Arc::new(StaticBearer("xai-session-bearer"))))
                .unwrap()
                .generate("test", "auto", "turn")
                .await
                .expect_err("OpenAI images are OAuth-only; static and legacy keys are off-limits");
        assert!(error.to_string().contains("login --codex"));
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.is_empty(),
            "no prompt may leave with a static or legacy-provider bearer"
        );
    }

    #[test]
    fn per_tool_gates_are_independent() {
        let cfg = ImageGenConfig::Enabled {
            provider: ImageGenerationProvider::Grok,
            api_key: "k".into(),
            base_url: "https://api.x.ai/v1".into(),
            extra_headers: indexmap::IndexMap::new(),
            api_key_provider: None,
            image_gen_enabled: false,
            image_edit_enabled: true,
            model_override: Some("grok-imagine-image".into()),
            edit_model_override: None,
            tier_restricted: false,
        };
        assert!(cfg.has_credentials());
        assert!(!cfg.image_gen_enabled());
        assert!(cfg.image_edit_enabled());
        assert_eq!(cfg.model_override(), Some("grok-imagine-image"));

        assert!(!ImageGenConfig::Disabled.has_credentials());
    }

    #[test]
    fn stamp_session_id_header_sets_and_preserves() {
        let mk = |headers: indexmap::IndexMap<String, String>| ImageGenConfig::Enabled {
            provider: ImageGenerationProvider::Grok,
            api_key: "k".into(),
            base_url: "https://api.x.ai/v1".into(),
            extra_headers: headers,
            api_key_provider: None,
            image_gen_enabled: true,
            image_edit_enabled: true,
            model_override: None,
            edit_model_override: None,
            tier_restricted: false,
        };
        let hdrs = |cfg: &ImageGenConfig| match cfg {
            ImageGenConfig::Enabled { extra_headers, .. } => extra_headers.clone(),
            _ => unreachable!(),
        };

        let mut cfg = mk(indexmap::IndexMap::new());
        cfg.stamp_session_id_header("sess-123");
        assert_eq!(
            hdrs(&cfg).get(SESSION_ID_HEADER).map(String::as_str),
            Some("sess-123")
        );

        let mut preset = indexmap::IndexMap::new();
        preset.insert(SESSION_ID_HEADER.to_string(), "caller-set".to_string());
        let mut cfg = mk(preset);
        cfg.stamp_session_id_header("sess-123");
        assert_eq!(
            hdrs(&cfg).get(SESSION_ID_HEADER).map(String::as_str),
            Some("caller-set")
        );

        let mut disabled = ImageGenConfig::Disabled;
        disabled.stamp_session_id_header("sess-123");
        assert!(!disabled.has_credentials());
    }

    #[test]
    fn client_selects_model_from_override() {
        let mk = |model_override: Option<&str>| ImageGenConfig::Enabled {
            provider: ImageGenerationProvider::Grok,
            api_key: "k".into(),
            base_url: "https://api.x.ai/v1".into(),
            extra_headers: indexmap::IndexMap::new(),
            api_key_provider: None,
            image_gen_enabled: true,
            image_edit_enabled: true,
            model_override: model_override.map(String::from),
            edit_model_override: None,
            tier_restricted: false,
        };
        // No override → default quality model.
        assert_eq!(
            ImageGenClient::new(&mk(None), None).unwrap().model,
            XAI_IMAGINE_MODEL
        );
        // Empty override → treated as no override.
        assert_eq!(
            ImageGenClient::new(&mk(Some("")), None).unwrap().model,
            XAI_IMAGINE_MODEL
        );
        // Override → that exact model slug.
        assert_eq!(
            ImageGenClient::new(&mk(Some("grok-imagine-image")), None)
                .unwrap()
                .model,
            "grok-imagine-image"
        );
    }

    #[test]
    fn client_selects_edit_model_from_override() {
        let mk = |edit_model_override: Option<&str>| ImageGenConfig::Enabled {
            provider: ImageGenerationProvider::Grok,
            api_key: "k".into(),
            base_url: "https://api.x.ai/v1".into(),
            extra_headers: indexmap::IndexMap::new(),
            api_key_provider: None,
            image_gen_enabled: true,
            image_edit_enabled: true,
            model_override: None,
            edit_model_override: edit_model_override.map(String::from),
            tier_restricted: false,
        };
        assert_eq!(
            ImageGenClient::new(&mk(None), None).unwrap().edit_model,
            super::super::image_edit::XAI_IMAGINE_EDIT_MODEL
        );
        assert_eq!(
            ImageGenClient::new(&mk(Some("  ")), None)
                .unwrap()
                .edit_model,
            super::super::image_edit::XAI_IMAGINE_EDIT_MODEL
        );
        let client = ImageGenClient::new(&mk(Some("grok-imagine-image-v2")), None).unwrap();
        assert_eq!(client.edit_model, "grok-imagine-image-v2");
        assert_eq!(client.model, XAI_IMAGINE_MODEL);
    }

    #[tokio::test]
    async fn errors_when_client_missing() {
        let tool = ImageGenTool;
        let resources = crate::types::resources::Resources::new();
        let result = xai_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ImageGenInput {
                prompt: "a test image".into(),
                aspect_ratio: "auto".into(),
            },
        )
        .await;

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("missing required resource"),
            "Expected MissingResource error, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn tier_restricted_short_circuits_with_upsell() {
        // A free / X Basic user's image_gen call returns the SuperGrok upsell
        // prose as a normal result (no HTTP, no error card) so the model can
        // relay it. Only the client is inserted — the short-circuit returns
        // before any other resource (e.g. SessionFolder) is required.
        let cfg = ImageGenConfig::Enabled {
            provider: ImageGenerationProvider::Grok,
            api_key: "k".into(),
            base_url: "https://api.x.ai/v1".into(),
            extra_headers: indexmap::IndexMap::new(),
            api_key_provider: None,
            image_gen_enabled: true,
            image_edit_enabled: true,
            model_override: None,
            edit_model_override: None,
            tier_restricted: true,
        };
        let mut resources = crate::types::resources::Resources::new();
        resources.insert(ImageGenClient::new(&cfg, None).unwrap());

        let result = xai_tool_runtime::Tool::run(
            &ImageGenTool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            ImageGenInput {
                prompt: "a cat".into(),
                aspect_ratio: "auto".into(),
            },
        )
        .await
        .expect("tier-restricted call must succeed with upsell prose");

        match result {
            ToolOutput::Text(t) => {
                assert!(t.text.contains("SuperGrok"), "got: {}", t.text);
                assert!(t.text.contains("supergrok?referrer=grok-build"));
            }
            other => panic!("expected Text upsell, got {other:?}"),
        }
    }
}
