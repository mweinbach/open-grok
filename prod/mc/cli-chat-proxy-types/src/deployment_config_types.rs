//! Signed deployment-config envelope: the wire contract between the
//! cli-chat-proxy signer and the client verifier. Shared so a field rename
//! breaks at compile time on both sides instead of silently failing verification.

use serde::{Deserialize, Serialize};

/// The payload format version the server currently signs. Bump when the payload
/// gains semantics (e.g. an anti-replay counter or a key-fingerprint binding) so
/// verifiers can distinguish generations; `0` means a pre-versioned payload.
pub const SIGNED_PAYLOAD_VERSION: u32 = 1;

/// The exact bytes the server signs: the served policy, the principal it is
/// bound to, and an expiry. Serialized once on the server and shipped verbatim
/// as `signed_payload`, so the client verifies the received bytes directly
/// instead of re-canonicalizing (no cross-language serialization drift).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedPayload {
    /// Payload format version ([`SIGNED_PAYLOAD_VERSION`]); `default` 0 so
    /// pre-versioned sidecars parse and verify unchanged.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub deployment_id: Option<String>,
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub managed_config: Option<String>,
    #[serde(default)]
    pub requirements: Option<String>,
    /// Strict (fail-closed) opt-in, carried in the SIGNED bytes so a local actor can't
    /// flip enforcement. `default` false so an older/unsigned payload stays lenient.
    #[serde(default)]
    pub fail_closed: bool,
    /// Unix seconds after which the signature is no longer trusted.
    pub expires_at: u64,
    /// Identifies the signing key, so a rotation can be distinguished.
    pub key_id: String,
}

/// One signed envelope carried alongside the legacy policy fields in the
/// deployment-config response (additive: old clients ignore it). Also the
/// shape the client persists as its on-disk signature sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureEnvelope {
    /// The exact JSON string that was signed (a serialized [`SignedPayload`]).
    pub signed_payload: String,
    /// Base64 (standard) Ed25519 signature over `signed_payload`'s UTF-8 bytes.
    pub signature: String,
    /// Untrusted (outside the signed bytes): a hint for picking among multiple
    /// envelopes, never for selecting the verifying key — only the signed
    /// payload's `key_id` is authoritative.
    #[serde(default)]
    pub key_id: String,
}

/// Unix seconds now (saturating to 0 on a pre-epoch clock).
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The `requirements.toml` opt-in key for strict (fail-closed) enforcement.
pub const FAIL_CLOSED_KEY: &str = "fail_closed";

/// Read the `fail_closed` opt-in from a requirements-TOML string — THE canonical parse,
/// shared by the cli-chat-proxy signer and the client so the two sides can't drift.
/// Invalid TOML or a non-bool value → `false`.
pub fn fail_closed_flag_from_str(requirements: &str) -> bool {
    toml::from_str::<toml::Value>(requirements)
        .ok()
        .and_then(|v| v.get(FAIL_CLOSED_KEY).and_then(toml::Value::as_bool))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version field round-trips, and a pre-versioned payload (no `version`
    /// key) defaults to 0 — old sidecars keep parsing.
    #[test]
    fn signed_payload_version_round_trips_and_defaults() {
        let versioned = SignedPayload {
            version: SIGNED_PAYLOAD_VERSION,
            deployment_id: None,
            team_id: Some("team-007".into()),
            managed_config: None,
            requirements: None,
            fail_closed: false,
            expires_at: 4_000_000_000,
            key_id: "v1".into(),
        };
        let json = serde_json::to_string(&versioned).unwrap();
        assert_eq!(
            serde_json::from_str::<SignedPayload>(&json).unwrap(),
            versioned
        );

        let legacy: SignedPayload =
            serde_json::from_str(r#"{"expires_at": 1, "key_id": "v1"}"#).unwrap();
        assert_eq!(legacy.version, 0, "pre-versioned payloads default to 0");
    }
}
