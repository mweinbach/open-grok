//! Prompt-suggestion model sentinels.
//!
//! The fork never adopted upstream's full `prompt_suggest` resolve module;
//! session prompt-suggestion only needs the non-reasoning alias below.

/// This alias resolves server-side with `alias_default_effort = none`.
pub(crate) const NON_REASONING_PROMPT_SUGGEST_MODEL: &str = "grok-4-1-fast-non-reasoning";
