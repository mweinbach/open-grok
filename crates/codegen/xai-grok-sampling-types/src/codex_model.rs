//! Provider-local Codex catalog metadata. No credentials or endpoint inference.
//!
//! Contract: openai/codex rust-v0.153.1 (985641272869835d01d025ed2a218fbbce35fa9f).

use crate::ReasoningEffort;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexModelMetadata {
    /// Runtime opt-in, never enabled by an untrusted catalog or persisted cache.
    #[serde(skip)]
    pub persistent_mode: bool,
    /// Advertised raw default and maximum, retained even after a user override.
    pub context_window: Option<u64>,
    pub max_context_window: Option<u64>,
    pub effective_context_window_percent: u8,
    /// Explicit raw budget. This may exceed the advertised maximum.
    pub context_window_override: Option<u64>,
    pub auto_compact_token_limit: Option<u64>,
    pub auto_compact_token_limit_override: Option<u64>,
    pub comp_hash: Option<String>,
    pub multi_agent_reasoning_effort: Option<ReasoningEffort>,
    pub supported_reasoning_efforts: Vec<ReasoningEffort>,
    pub upgrade: Option<CodexModelUpgrade>,
    pub model_messages: CodexModelMessages,
    pub support_verbosity: bool,
    pub default_verbosity: Option<String>,
    pub input_modalities: Vec<String>,
}

impl Default for CodexModelMetadata {
    fn default() -> Self {
        Self {
            persistent_mode: false,
            context_window: None,
            max_context_window: None,
            effective_context_window_percent: 95,
            context_window_override: None,
            auto_compact_token_limit: None,
            auto_compact_token_limit_override: None,
            comp_hash: None,
            multi_agent_reasoning_effort: None,
            supported_reasoning_efforts: Vec::new(),
            upgrade: None,
            model_messages: CodexModelMessages::default(),
            support_verbosity: false,
            default_verbosity: None,
            input_modalities: Vec::new(),
        }
    }
}

impl CodexModelMetadata {
    pub fn raw_context_window(&self) -> Option<u64> {
        self.context_window_override
            .or(self.context_window)
            .or(self.max_context_window)
            .filter(|value| *value > 0)
    }

    pub fn effective_percent(&self) -> u64 {
        u64::from(self.effective_context_window_percent.clamp(1, 100))
    }

    pub fn effective_context_window(&self) -> Option<u64> {
        self.raw_context_window()
            .map(|raw| ((u128::from(raw) * u128::from(self.effective_percent())) / 100) as u64)
            .filter(|value| *value > 0)
    }

    pub fn compact_limit(&self) -> Option<u64> {
        let cap = self
            .raw_context_window()
            .map(|raw| (u128::from(raw) * 90 / 100) as u64);
        // A larger user context must not retain the old catalog's small limit.
        let selected = self.auto_compact_token_limit_override.or_else(|| {
            self.context_window_override
                .is_none()
                .then_some(self.auto_compact_token_limit)
                .flatten()
        });
        match (selected, cap) {
            (Some(limit), Some(cap)) => Some(limit.min(cap)),
            (Some(limit), None) => Some(limit),
            (None, cap) => cap,
        }
    }

    /// Ultra is a local delegation mode, not a Responses effort. Match codex-rs:
    /// valid advertised override -> max -> last non-Ultra preset -> medium.
    pub fn ultra_effort(&self) -> ReasoningEffort {
        let supported = &self.supported_reasoning_efforts;
        self.multi_agent_reasoning_effort
            .filter(|effort| *effort != ReasoningEffort::Ultra && supported.contains(effort))
            .or_else(|| {
                supported
                    .contains(&ReasoningEffort::Max)
                    .then_some(ReasoningEffort::Max)
            })
            .or_else(|| {
                supported
                    .iter()
                    .rev()
                    .copied()
                    .find(|effort| *effort != ReasoningEffort::Ultra)
            })
            .unwrap_or(ReasoningEffort::Medium)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexModelUpgrade {
    pub model: String,
    pub migration_markdown: Option<String>,
    pub retirement_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexModelMessages {
    pub persistent_instructions: Option<String>,
    pub confirmation_policies: Option<CodexConfirmationPolicies>,
    pub guardian_v2: Option<CodexGuardianConfig>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexConfirmationPolicies {
    pub browser_use: Option<String>,
    pub computer_use: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexGuardianConfig {
    pub classifier_instructions: Option<String>,
    pub reasoning_effort: Option<String>,
    pub review_threshold_basis_points: Option<u16>,
    pub max_action_tokens: Option<usize>,
    pub max_classifier_instruction_tokens: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_catalog_cannot_enable_persistent_mode() {
        let mut metadata: CodexModelMetadata =
            serde_json::from_value(serde_json::json!({"persistent_mode":true})).unwrap();
        assert!(!metadata.persistent_mode);
        metadata.persistent_mode = true;
        assert!(
            serde_json::to_value(metadata)
                .unwrap()
                .get("persistent_mode")
                .is_none()
        );
    }

    #[test]
    fn context_override_exceeds_catalog_and_rebases_compaction() {
        let mut metadata = CodexModelMetadata {
            context_window: Some(272_000),
            max_context_window: Some(372_000),
            auto_compact_token_limit: Some(240_000),
            ..Default::default()
        };
        assert_eq!(metadata.effective_context_window(), Some(258_400));
        assert_eq!(metadata.compact_limit(), Some(240_000));
        metadata.context_window_override = Some(1_000_000);
        assert_eq!(metadata.effective_context_window(), Some(950_000));
        assert_eq!(metadata.compact_limit(), Some(900_000));
        assert_eq!(metadata.max_context_window, Some(372_000));
        metadata.auto_compact_token_limit_override = Some(850_000);
        assert_eq!(metadata.compact_limit(), Some(850_000));
        metadata.context_window_override = Some(100_000);
        assert_eq!(metadata.compact_limit(), Some(90_000));
    }

    #[test]
    fn ultra_obeys_catalog_override_and_validated_fallbacks() {
        let mut metadata = CodexModelMetadata {
            multi_agent_reasoning_effort: Some(ReasoningEffort::Xhigh),
            supported_reasoning_efforts: vec![
                ReasoningEffort::High,
                ReasoningEffort::Xhigh,
                ReasoningEffort::Max,
                ReasoningEffort::Ultra,
            ],
            ..Default::default()
        };
        assert_eq!(metadata.ultra_effort(), ReasoningEffort::Xhigh);
        metadata.multi_agent_reasoning_effort = Some(ReasoningEffort::Low);
        assert_eq!(metadata.ultra_effort(), ReasoningEffort::Max);
        metadata
            .supported_reasoning_efforts
            .retain(|effort| *effort != ReasoningEffort::Max);
        assert_eq!(metadata.ultra_effort(), ReasoningEffort::Xhigh);
        metadata.multi_agent_reasoning_effort = Some(ReasoningEffort::Ultra);
        metadata.supported_reasoning_efforts = vec![ReasoningEffort::Ultra];
        assert_eq!(metadata.ultra_effort(), ReasoningEffort::Medium);
    }
}
