//! Input/output types for memory tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Optional objective-outcome filter for experience-memory search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceOutcomeFilter {
    /// Return strategies that demonstrably worked.
    Success,
    /// Return failed strategies and anti-patterns.
    Failure,
}

impl ExperienceOutcomeFilter {
    /// Translate the wire outcome into the backend's objective verdict.
    pub fn as_bool(self) -> bool {
        matches!(self, Self::Success)
    }
}

/// Input for the `experience_search` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ExperienceSearchInput {
    /// Technical task, command, failure, or strategy to look up.
    pub query: String,
    /// Maximum number of matching experiences; bounded by the tool.
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Restrict results to successful or failed experiences.
    ///
    /// Omit this field to search both successes and failures.
    #[serde(default)]
    pub outcome: Option<ExperienceOutcomeFilter>,
}

/// Input for the `memory_search` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemorySearchInput {
    /// The search query string. Use specific technical terms rather than
    /// conversational language. Good: "authentication middleware patterns".
    /// Bad: "that thing we discussed about auth".
    pub query: String,
    /// Maximum number of results to return.
    ///
    /// When omitted the backend-configured value is used (typically 6 from
    /// `[memory.search].max_results`), so leaving this unset is preferred
    /// for normal queries.
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Minimum relevance score threshold.
    ///
    /// When omitted the backend-configured value is used (typically 0.0 from
    /// `[memory.search].min_score`).
    #[serde(default)]
    pub min_score: Option<f64>,
}

/// Output schema for `memory_search` (used for JSON Schema generation only).
#[derive(Debug, JsonSchema)]
pub struct MemorySearchOutput {
    /// Formatted search results as markdown text.
    pub results: String,
}

/// Input for the `memory_get` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryGetInput {
    /// Path to the memory file to read.
    pub path: String,
    /// 1-based start line, matching the line numbers in the tool's output
    /// (default: beginning of file). 0 is accepted and treated as 1.
    #[serde(default)]
    pub from: Option<usize>,
    /// Maximum number of lines to return (default: all).
    #[serde(default)]
    pub lines: Option<usize>,
}

/// Output schema for `memory_get` (used for JSON Schema generation only).
#[derive(Debug, JsonSchema)]
pub struct MemoryGetOutput {
    /// File content (optionally line-limited).
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experience_outcome_filter_uses_typed_wire_values() {
        let success: ExperienceSearchInput = serde_json::from_value(serde_json::json!({
            "query": "cargo test",
            "outcome": "success"
        }))
        .expect("success is a supported outcome");
        assert_eq!(success.outcome, Some(ExperienceOutcomeFilter::Success));
        assert_eq!(
            success.outcome.map(ExperienceOutcomeFilter::as_bool),
            Some(true)
        );

        let failure: ExperienceSearchInput = serde_json::from_value(serde_json::json!({
            "query": "cargo test",
            "outcome": "failure"
        }))
        .expect("failure is a supported outcome");
        assert_eq!(failure.outcome, Some(ExperienceOutcomeFilter::Failure));
        assert_eq!(
            failure.outcome.map(ExperienceOutcomeFilter::as_bool),
            Some(false)
        );
    }

    #[test]
    fn experience_outcome_filter_rejects_unknown_values_and_defaults_to_both() {
        let both: ExperienceSearchInput = serde_json::from_value(serde_json::json!({
            "query": "authentication timeout"
        }))
        .expect("an omitted outcome searches both verdicts");
        assert_eq!(both.outcome, None);

        let invalid = serde_json::from_value::<ExperienceSearchInput>(serde_json::json!({
            "query": "authentication timeout",
            "outcome": "maybe"
        }));
        assert!(invalid.is_err(), "unknown outcomes must fail closed");
    }
}
