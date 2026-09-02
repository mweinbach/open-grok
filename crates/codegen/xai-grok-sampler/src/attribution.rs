//! 401 attribution hook for the sampling client.
//!
//! The caller wires an [`Auth401AttributionCallback`] into
//! [`crate::SamplerConfig::attribution_callback`]; the sampler invokes it at
//! each UNAUTHORIZED arm with the bearer fragment that went on the wire, so an
//! observer can split "sent a stale snapshot" from "sent the live token and
//! was still rejected". `None` (the default) makes the 401 sites silent.
//!
//! This crate stays decoupled from `xai-grok-shell`: no shell types, no
//! auth-manager dependency.

use std::sync::Arc;

pub use xai_grok_auth::bearer_fragment::BEARER_SUFFIX_LEN;

/// A logical 401-emitting site inside the sampling client. The string
/// identifier ends up in the consumer field of the attribution event
/// so downstream queries can break down 401s by API path.
///
/// # Scope: sampler endpoints only
///
/// This enum enumerates the HTTP endpoints owned by
/// `SamplingClient` (chat completions, responses, messages -- each in
/// streaming and non-streaming form -- plus standalone provider search). It
/// does *not* cover image generation, video generation, fallback web search,
/// or embedding -- those tools live in `xai-grok-tools`
/// (`crates/codegen/xai-grok-tools/src/implementations/`), have their
/// own HTTP clients that do not flow through `SamplingClient`, and
/// hook into the `xai_grok_tools::ApiKeyProvider` trait rather than
/// this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingConsumer {
    /// `chat_completion_stream`: OpenAI-compatible streaming OpenAI Chat Completions API.
    ChatCompletionsStream,
    /// `chat_completion`: OpenAI-compatible non-streaming OpenAI Chat Completions API.
    ChatCompletions,
    /// `create_response_stream`: Responses API streaming.
    ResponsesStream,
    /// `create_response`: Responses API non-streaming.
    Responses,
    /// `messages_stream`: Anthropic Messages API streaming.
    MessagesStream,
    /// `messages`: Anthropic Messages API non-streaming.
    Messages,
    /// `alpha/search`: provider-authenticated standalone web search.
    StandaloneWebSearch,
    /// `streamGenerateContent`: Google AI Studio streaming API.
    GoogleAiStudioStream,
    /// `generateContent`: Google AI Studio non-streaming API.
    GoogleAiStudio,
}

impl SamplingConsumer {
    /// Stable string identifier for this emit site. Callbacks
    /// typically combine this with a fixed prefix (e.g. the client
    /// type) when building the consumer field of the attribution
    /// event.
    pub fn as_endpoint(self) -> &'static str {
        match self {
            Self::ChatCompletionsStream => "chat_completions_stream",
            Self::ChatCompletions => "chat_completions",
            Self::ResponsesStream => "responses_stream",
            Self::Responses => "responses",
            Self::MessagesStream => "messages_stream",
            Self::Messages => "messages",
            Self::StandaloneWebSearch => "standalone_web_search",
            Self::GoogleAiStudioStream => "google_ai_studio_stream",
            Self::GoogleAiStudio => "google_ai_studio",
        }
    }
}

/// Hook invoked by [`crate::SamplingClient`] at every 401 response site.
///
/// Must be cheap and non-blocking — it runs on the user-visible 401 error path.
///
/// The `Debug` bound is structural: [`crate::SamplerConfig`] derives `Debug`
/// and holds an `Option<Arc<dyn Auth401AttributionCallback>>`. Do not remove it.
pub trait Auth401AttributionCallback: Send + Sync + std::fmt::Debug {
    /// Record a 401 attribution event for one logical 401 response.
    ///
    /// `sent_bearer_suffix` is the last [`BEARER_SUFFIX_LEN`] characters of the bearer
    /// that was actually sent on the wire. The sampler extracts the
    /// bearer from the `Authorization` header (or `x-api-key` for
    /// Anthropic Messages API backends) and truncates it to that
    /// fragment **before crossing this trait boundary** -- the full
    /// bearer never leaves [`crate::SamplingClient`]. This is the
    /// scrub-at-the-boundary invariant: even a misbehaving callback
    /// implementation that logs `sent_bearer_prefix` directly leaks
    /// only the fragment, never the full credential.
    ///
    /// `None` indicates the request had no bearer header at all
    /// (distinct from "had a bearer that turned out to be stale").
    fn record_401(&self, consumer: SamplingConsumer, sent_bearer_suffix: Option<&str>);
}

/// Shared, cheap-to-clone alias for the attribution callback.
pub type SharedAttributionCallback = Arc<dyn Auth401AttributionCallback>;
