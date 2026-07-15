use std::time::Duration;

/// The outcome of a blocking (`pre_tool_use`) hook dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// All hooks allowed (or no hooks matched).
    Allow,
    /// At least one hook denied with the given reason.
    Deny { reason: String, hook_name: String },
}

/// HTTP-specific execution details for scrollback enrichment.
///
/// Populated only for `"http"` handler type hooks. Carries the target
/// URL, HTTP status, and a short preview of the response body so that
/// scrollback annotations can display them.
#[derive(Debug, Clone)]
pub struct HttpInfo {
    /// The URL that was POSTed to.
    ///
    /// **Post-expansion form**: this is the actual target the runner
    /// hit (or attempted to hit) and is intended for SSRF debugging.
    /// User `env` map values resolved at expand time can land here, so
    /// any new wire-DTO consumer that surfaces this field for **user
    /// display** MUST prefer [`raw_url`] when available -- otherwise
    /// secrets like API tokens embedded in the URL via `${TOKEN}`
    /// substitution will leak. See `HookSpec::url_raw` in
    /// `crate::config` for the parallel display-vs-execution split.
    ///
    /// [`raw_url`]: HttpInfo::raw_url
    pub url: String,
    /// Pre-expansion source URL exactly as written in the JSON file,
    /// when available. Mirrors `HookSpec::url_raw` so downstream wire
    /// DTOs / scrollback display layers can show the source string
    /// without ever leaking resolved `${VAR}` substitutions. `None`
    /// for legacy code paths that constructed the spec without the
    /// raw source (the runner falls back to displaying [`url`] in
    /// that case).
    ///
    /// [`url`]: HttpInfo::url
    pub raw_url: Option<String>,
    /// HTTP status code (e.g. 200, 500). `None` if the request never
    /// completed (timeout, connection error).
    pub status: Option<u16>,
    /// Short preview of the response body (truncated to ~200 chars).
    /// `None` if no body was read (e.g. non-blocking hooks, timeouts).
    pub response_preview: Option<String>,
}

/// The outcome of a single hook execution.
#[derive(Debug)]
pub enum HookRunResult {
    /// Hook executed successfully.
    Success {
        hook_name: String,
        elapsed: Duration,
        /// HTTP details, populated only for `"http"` handler type hooks.
        http_info: Option<HttpInfo>,
    },
    /// Hook was skipped because it is disabled.
    Skipped { hook_name: String },
    /// Hook failed (timeout, crash, bad output, etc.) — fail-open.
    Failed {
        hook_name: String,
        error: String,
        elapsed: Duration,
        /// HTTP details, populated only for `"http"` handler type hooks.
        http_info: Option<HttpInfo>,
    },
}
