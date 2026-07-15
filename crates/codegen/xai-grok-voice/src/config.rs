use serde::{Deserialize, Serialize};

use crate::error::VoiceError;

/// Voice settings for the STT transport.
///
/// Carries the transport knobs parsed from optional `[voice]` in
/// `~/.grok/config.toml` (STT URL pieces, language, sample rate, endpointing)
/// plus two `#[serde(skip)]` request-identity fields the pager stamps in after
/// parsing (documented on the fields below). Whether voice is available is
/// resolved by the pager (GA default on; remote kill switch /
/// `GROK_VOICE_MODE` override) — there is deliberately no local enable/disable
/// knob in this config table. All serde fields have defaults, so the `[voice]`
/// table is optional.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct VoiceConfig {
    pub api_base: String,
    pub stt_ws_path: String,
    /// Preferred STT language: a catalog code from [`crate::STT_LANGUAGES`], or
    /// the client-only sentinel `"auto"` (system locale). Resolved to a concrete
    /// API code via [`crate::language_for_api`] at connect time — never send the
    /// raw field when it may be `"auto"`.
    pub language: String,
    pub sample_rate: u32,
    pub stt_endpointing_ms: u32,
    pub stt_interim_results: bool,

    /// Request-identity headers attached to every STT handshake so the backend
    /// can attribute and meter voice usage by client — mirroring the
    /// `x-grok-client-identifier` / `User-Agent` headers the sampler and imagine
    /// request paths send. These are **runtime identity, not user config**:
    /// `#[serde(skip)]` keeps them out of the parsed `[voice]` table (a user
    /// can't spoof them) and the pager fills them in after parsing. Empty →
    /// the corresponding header is omitted.
    ///
    /// `x-grok-client-identifier` value (e.g. `"grok-shell"`).
    #[serde(skip)]
    pub client_identifier: String,
    /// `User-Agent` value (e.g. `"grok-shell/1.2.3 (macos; aarch64)"`).
    #[serde(skip)]
    pub user_agent: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.x.ai".into(),
            stt_ws_path: "/v1/stt".into(),
            language: "en".into(),
            sample_rate: 16_000,
            stt_endpointing_ms: 400,
            stt_interim_results: true,
            client_identifier: String::new(),
            user_agent: String::new(),
        }
    }
}

impl VoiceConfig {
    /// Build the streaming-STT WebSocket URL.
    ///
    /// Only TLS endpoints are allowed: an `https://` / `wss://` (or scheme-less)
    /// `api_base` maps to `wss://`. An `http://`/`ws://` `api_base` is rejected with a
    /// [`VoiceError::Config`] rather than silently downgrading, since the bearer
    /// token is sent as a header on this connection and must never traverse a
    /// plaintext socket.
    pub fn stt_ws_url(&self) -> Result<String, VoiceError> {
        ws_url(&self.api_base, &self.stt_ws_path)
    }

    /// Parse `[voice]` from the root of an effective config document.
    pub fn from_config_table(root: &toml::Table) -> Self {
        root.get("voice")
            .and_then(|v| v.clone().try_into().ok())
            .unwrap_or_default()
    }
}

fn ws_url(api_base: &str, path: &str) -> Result<String, VoiceError> {
    let base = api_base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if base.starts_with("http://") || base.starts_with("ws://") {
        return Err(VoiceError::Config(format!(
            "insecure voice api_base {api_base:?}: voice requires a TLS endpoint \
             (https:// / wss://). Refusing to send the bearer token over a \
             plaintext connection."
        )));
    }
    let host = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("wss://"))
        .unwrap_or(base);
    Ok(format!("wss://{host}/{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stt_ws_uses_wss() {
        let cfg = VoiceConfig::default();
        assert_eq!(cfg.stt_ws_url().unwrap(), "wss://api.x.ai/v1/stt");
    }

    #[test]
    fn scheme_less_api_base_uses_wss() {
        let cfg = VoiceConfig {
            api_base: "api.x.ai".into(),
            ..VoiceConfig::default()
        };
        assert_eq!(cfg.stt_ws_url().unwrap(), "wss://api.x.ai/v1/stt");
    }

    #[test]
    fn wss_api_base_is_not_doubled() {
        let cfg = VoiceConfig {
            api_base: "wss://api.x.ai".into(),
            ..VoiceConfig::default()
        };
        assert_eq!(cfg.stt_ws_url().unwrap(), "wss://api.x.ai/v1/stt");
    }

    #[test]
    fn http_api_base_is_rejected_not_downgraded() {
        let cfg = VoiceConfig {
            api_base: "http://localhost:8080".into(),
            ..VoiceConfig::default()
        };
        let err = cfg.stt_ws_url().unwrap_err();
        assert!(matches!(err, VoiceError::Config(_)), "got {err:?}");
    }

    #[test]
    fn ws_api_base_is_rejected() {
        let cfg = VoiceConfig {
            api_base: "ws://localhost:8080".into(),
            ..VoiceConfig::default()
        };
        assert!(cfg.stt_ws_url().is_err());
    }

    /// Legacy / unknown keys — including the removed local `enabled` opt-out —
    /// must be ignored without failing the parse (no `deny_unknown_fields`), so
    /// old configs still load (the key is now a silent no-op; the pager owns the
    /// voice gate — default on, remote kill switch / `GROK_VOICE_MODE`).
    #[test]
    fn ignores_additional_fields() {
        let raw = r#"
[voice]
enabled = false
push_to_talk = true
language = "es"
"#;
        let table: toml::Table = toml::from_str(raw).unwrap();
        let cfg = VoiceConfig::from_config_table(&table);
        // Known fields still apply; unknown/legacy keys are dropped silently.
        assert_eq!(cfg.language, "es");
        assert_eq!(cfg.sample_rate, 16_000);
    }

    /// `client_identifier` / `user_agent` are `#[serde(skip)]` runtime identity,
    /// not user config: a value placed in `[voice]` must be ignored so a user
    /// can't spoof the attribution headers. The pager stamps them after parsing.
    #[test]
    fn identity_fields_are_not_parsed_from_config() {
        let raw = r#"
[voice]
client_identifier = "spoofed"
user_agent = "malicious/9.9"
language = "es"
"#;
        let table: toml::Table = toml::from_str(raw).unwrap();
        let cfg = VoiceConfig::from_config_table(&table);
        assert_eq!(cfg.language, "es", "ordinary fields still parse");
        assert!(
            cfg.client_identifier.is_empty(),
            "client_identifier must not be settable via config"
        );
        assert!(
            cfg.user_agent.is_empty(),
            "user_agent must not be settable via config"
        );
    }
}
