use serde::{Deserialize, Serialize};

use crate::error::VoiceError;

/// Default STT capture rate (Hz). Shared with the `__mic-capture` helper's
/// argv default so parent and child agree when `--rate` is omitted.
pub const DEFAULT_SAMPLE_RATE: u32 = 16_000;

/// Voice settings for the STT transport.
///
/// Prefer **https** `api_base` (same shape as chat). [`Self::stt_ws_url`] derives
/// `wss://`. When `[voice].api_base` is unset, this inherits
/// `[endpoints].xai_api_base_url` so enterprise proxies need no second knob.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct VoiceConfig {
    /// HTTPS API root (or bare host). Bases may end in `/v1` or `/xai/v1`; the
    /// default STT path de-duplicates a leading `v1/` so both become `.../v1/stt`.
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
            sample_rate: DEFAULT_SAMPLE_RATE,
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

    /// `api_base`: non-empty `[voice].api_base`, else
    /// `[endpoints].xai_api_base_url` from `root`, else
    /// `resolved_endpoints_base`, else `https://api.x.ai`.
    ///
    /// `resolved_endpoints_base` carries the caller's env / CLI overrides; it
    /// ranks below the raw table so config keeps beating env (shell precedence).
    pub fn from_config_table(root: &toml::Table, resolved_endpoints_base: Option<&str>) -> Self {
        let voice_table = root.get("voice").and_then(|value| value.as_table());
        let mut config: Self = voice_table
            .and_then(|table| toml::Value::Table(table.clone()).try_into().ok())
            .unwrap_or_default();

        // Read `[voice].api_base` from the raw table, not `config`: serde's
        // default makes "unset" and explicit `https://api.x.ai` identical.
        config.api_base = non_empty_str(
            voice_table
                .and_then(|table| table.get("api_base"))
                .and_then(|value| value.as_str()),
        )
        .or_else(|| {
            non_empty_str(
                root.get("endpoints")
                    .and_then(|endpoints| endpoints.get("xai_api_base_url"))
                    .and_then(|value| value.as_str()),
            )
        })
        .or_else(|| non_empty_str(resolved_endpoints_base))
        .map(|base| base.trim_end_matches('/').to_owned())
        .unwrap_or_else(|| Self::default().api_base);
        config
    }
}

fn non_empty_str(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// `strip_prefix` ignoring ASCII case: RFC 3986 schemes are case-insensitive,
/// so `HTTP://` must hit plaintext rejection and `HTTPS://` must work.
fn strip_scheme<'a>(value: &'a str, scheme: &str) -> Option<&'a str> {
    value
        .get(..scheme.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(scheme))
        .map(|_| &value[scheme.len()..])
}

fn ws_url(api_base: &str, path: &str) -> Result<String, VoiceError> {
    let base = api_base.trim().trim_end_matches('/');
    let path = path.trim().trim_start_matches('/');
    if strip_scheme(base, "http://").is_some() || strip_scheme(base, "ws://").is_some() {
        return Err(VoiceError::Config(format!(
            "insecure voice api_base {api_base:?}: voice requires a TLS endpoint \
             (https:// / wss://). Refusing to send the bearer token over a \
             plaintext connection."
        )));
    }
    let rest = strip_scheme(base, "https://")
        .or_else(|| strip_scheme(base, "wss://"))
        .unwrap_or(base);
    // Default path `/v1/stt`; bases often end in `/v1` or `/xai/v1`.
    let path = match (rest.ends_with("/v1"), path.strip_prefix("v1/")) {
        (true, Some(rest_path)) => rest_path,
        _ => path,
    };
    Ok(format!("wss://{rest}/{path}"))
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
    fn schemes_are_case_insensitive() {
        let secure = VoiceConfig {
            api_base: "HTTPS://api.x.ai".into(),
            ..VoiceConfig::default()
        };
        assert_eq!(secure.stt_ws_url().unwrap(), "wss://api.x.ai/v1/stt");

        for base in ["HTTP://localhost:8080", "Ws://localhost:8080"] {
            let insecure = VoiceConfig {
                api_base: base.into(),
                ..VoiceConfig::default()
            };
            assert!(matches!(insecure.stt_ws_url(), Err(VoiceError::Config(_))));
        }
    }

    #[test]
    fn v1_base_dedupes_default_path() {
        let cfg = VoiceConfig {
            api_base: "https://proxy.example.com/v1".into(),
            ..VoiceConfig::default()
        };
        assert_eq!(cfg.stt_ws_url().unwrap(), "wss://proxy.example.com/v1/stt");
    }

    #[test]
    fn xai_v1_base_preserves_prefix() {
        let cfg = VoiceConfig {
            api_base: "https://proxy.example.com/xai/v1".into(),
            ..VoiceConfig::default()
        };
        assert_eq!(
            cfg.stt_ws_url().unwrap(),
            "wss://proxy.example.com/xai/v1/stt"
        );
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
        let cfg = VoiceConfig::from_config_table(&table, None);
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
        let cfg = VoiceConfig::from_config_table(&table, None);
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

    #[test]
    fn inherits_endpoints_when_voice_api_base_unset() {
        let table: toml::Table = toml::from_str(
            r#"
[endpoints]
xai_api_base_url = "https://proxy.example.com/xai/v1"
"#,
        )
        .unwrap();
        let cfg = VoiceConfig::from_config_table(&table, None);
        assert_eq!(cfg.api_base, "https://proxy.example.com/xai/v1");
        assert_eq!(
            cfg.stt_ws_url().unwrap(),
            "wss://proxy.example.com/xai/v1/stt"
        );
    }

    #[test]
    fn empty_voice_api_base_still_inherits_endpoints() {
        let table: toml::Table = toml::from_str(
            r#"
[endpoints]
xai_api_base_url = "https://proxy.example.com/xai/v1"
[voice]
api_base = "  "
language = "fr"
"#,
        )
        .unwrap();
        let cfg = VoiceConfig::from_config_table(&table, None);
        assert_eq!(cfg.api_base, "https://proxy.example.com/xai/v1");
        assert_eq!(cfg.language, "fr");
    }

    #[test]
    fn whitespace_voice_api_base_without_endpoints_uses_default() {
        let table: toml::Table = toml::from_str(
            r#"
[voice]
api_base = "  "
"#,
        )
        .unwrap();
        let cfg = VoiceConfig::from_config_table(&table, None);
        assert_eq!(cfg.api_base, VoiceConfig::default().api_base);
        assert_eq!(cfg.stt_ws_url().unwrap(), "wss://api.x.ai/v1/stt");
    }

    #[test]
    fn resolved_endpoints_base_used_when_table_has_none() {
        let cfg = VoiceConfig::from_config_table(
            &toml::Table::new(),
            Some("https://proxy.example.com/v1/"),
        );
        assert_eq!(cfg.api_base, "https://proxy.example.com/v1");
        assert_eq!(cfg.stt_ws_url().unwrap(), "wss://proxy.example.com/v1/stt");

        let cfg = VoiceConfig::from_config_table(&toml::Table::new(), Some("  "));
        assert_eq!(cfg.api_base, VoiceConfig::default().api_base);
    }

    /// config.toml beats the env/CLI fallback (shell endpoints precedence).
    #[test]
    fn table_endpoints_beat_resolved_endpoints_base() {
        let table: toml::Table = toml::from_str(
            r#"
[endpoints]
xai_api_base_url = "https://config.example.com"
"#,
        )
        .unwrap();
        let cfg = VoiceConfig::from_config_table(&table, Some("https://env.example.com"));
        assert_eq!(cfg.api_base, "https://config.example.com");
    }

    #[test]
    fn voice_api_base_overrides_endpoints() {
        let table: toml::Table = toml::from_str(
            r#"
[endpoints]
xai_api_base_url = "https://proxy.example.com/xai/v1"
[voice]
api_base = "https://api.x.ai"
language = "es"
"#,
        )
        .unwrap();
        let cfg = VoiceConfig::from_config_table(&table, None);
        assert_eq!(cfg.api_base, "https://api.x.ai");
        assert_eq!(cfg.language, "es");
        assert_eq!(cfg.stt_ws_url().unwrap(), "wss://api.x.ai/v1/stt");
    }
}
