//! Prompt cache hit rate analysis and cache break tracking.
//!
//! Prompt caching in LLMs (xAI Grok, OpenAI Codex/Responses, Anthropic Messages, DeepSeek)
//! requires the prompt prefix to remain byte-for-byte stable across consecutive turns.
//! Any modification to an earlier prefix section — model, effort, temperature, cache key,
//! tool choice, JSON schema, tools, hosted tools, or an earlier conversation item —
//! invalidates the KV cache at that exact position.
//!
//! This module provides:
//! 1. `RequestSummary`: Structural fingerprinting of `ConversationRequest`s (hashes +
//!    labels only; never prompt text).
//! 2. `analyze_prefix_divergence`: Detection of the first prefix section that changed.
//! 3. `CacheTracker`: Turn-by-turn evaluation, cache status categorization, and structured
//!    logging to `unified_log` and `tracing`.
//!
//! Rewind / trim (current request a strict prefix of the previous one) is `Shortened`,
//! not a break. A 0% hit on a stable prefix with a forwarded cache key and a prompt
//! large enough to cache is a provider miss, not "no cache support".

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use xai_grok_sampling_types::{
    ContentPart, ConversationItem, ConversationRequest, CustomToolOutputContent, HostedTool,
    SyntheticReason, ToolSpec,
};

/// Placeholder inserted when a tool result is hard-cleared (from `xai_chat_state`).
pub const HARD_CLEAR_PLACEHOLDER: &str = "[Tool result omitted — too old]";
/// Separator inserted between head and tail in soft-trimmed results.
pub const SOFT_TRIM_SEPARATOR: &str = "[…trimmed…]";

/// Minimum prompt size before a 0% hit on a stable prefix is treated as a
/// provider miss (routing / TTL / key not applied) rather than "too small
/// to cache" or "no cache support".
pub const PROVIDER_MISS_MIN_PROMPT_TOKENS: u32 = 1_024;

/// Maximum number of recent turn records kept in memory for interactive inspection.
const MAX_RECENT_TURNS: usize = 50;

/// Summarized fingerprint of a conversation item. Labels and hashes only —
/// never prompt or tool-result text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemSummary {
    pub index: usize,
    pub kind: String,
    pub identifier: Option<String>,
    pub byte_len: usize,
    pub content_hash: u64,
    pub is_pruned: bool,
    pub has_images: bool,
}

/// Summarized fingerprint of a tool definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSummary {
    pub name: String,
    pub description_hash: u64,
    pub params_hash: u64,
}

/// Summarized fingerprint of a full conversation request.
///
/// Prefix sections are compared in this order: model, reasoning effort,
/// temperature, prompt cache key, tool choice, JSON schema, tools, hosted
/// tools, then conversation items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestSummary {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub temperature_bits: Option<u32>,
    pub prompt_cache_key: Option<String>,
    pub tool_choice_hash: u64,
    pub json_schema_hash: u64,
    pub tools: Vec<ToolSummary>,
    pub hosted_tool_names: Vec<String>,
    pub hosted_tools_hash: u64,
    pub items: Vec<ItemSummary>,
    pub total_body_bytes: usize,
}

/// Category of cache status for a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatus {
    /// First turn of the session (cold cache).
    FirstTurn,
    /// Successful cache hit (substantial prefix served from cache).
    Hit,
    /// Partial cache hit (some cached tokens, but less than expected).
    PartialHit,
    /// Cache broke (0 cached tokens because an earlier prefix section changed).
    Break,
    /// Prefix intact, cache key forwarded, prompt large enough to cache, but
    /// the provider reported zero cached tokens.
    ProviderMiss,
    /// No prompt caching reported by provider (key not on the wire, or prompt
    /// too small to cache).
    NoCacheSupport,
}

impl CacheStatus {
    pub fn display_label(&self) -> &'static str {
        match self {
            Self::FirstTurn => "First turn (cold cache)",
            Self::Hit => "Cache hit",
            Self::PartialHit => "Partial cache hit",
            Self::Break => "Cache break",
            Self::ProviderMiss => "Provider cache miss",
            Self::NoCacheSupport => "No cache reported",
        }
    }
}

/// Specific reason why a conversation item diverged from the previous turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemDivergenceReason {
    /// Tool result was pruned / omitted due to context utilization limits.
    Pruned,
    /// Inline images were evicted from this item to reclaim byte budget.
    ImageEvicted,
    /// Item contents were edited, rewound, or modified.
    ContentModified,
    /// Item type variant changed at this position.
    VariantChanged,
}

/// Analysis of prefix differences between two consecutive conversation requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PrefixDivergence {
    /// First turn in session; no prior turn to compare against.
    FirstTurn,
    /// The entire previous prefix was preserved byte-stable up to `preserved_items` count.
    PrefixIntact {
        preserved_items: usize,
        new_items: usize,
    },
    /// Current request is a strict prefix of the previous one (rewind / trim).
    /// Remaining prefix hashes match, so this is not a cache break.
    Shortened {
        preserved_items: usize,
        dropped_items: usize,
    },
    /// Model slug changed between turns.
    ModelChanged {
        previous: Option<String>,
        current: Option<String>,
    },
    /// Reasoning effort changed between turns.
    ReasoningEffortChanged {
        previous: Option<String>,
        current: Option<String>,
    },
    /// Sampling temperature changed between turns.
    TemperatureChanged,
    /// Sticky prompt-cache routing key changed between turns.
    PromptCacheKeyChanged,
    /// Tool-choice policy changed between turns.
    ToolChoiceChanged,
    /// Structured-output JSON schema changed between turns.
    JsonSchemaChanged,
    /// System prompt changed between turns (lengths only; no prompt text).
    SystemPromptChanged { prev_len: usize, curr_len: usize },
    /// Tool definitions were added, removed, reordered, or modified.
    ToolsChanged { diff: String },
    /// Hosted tool definitions were added, removed, reordered, or modified.
    HostedToolsChanged { diff: String },
    /// Conversation item at `index` diverged from the previous turn.
    ItemDiverged {
        index: usize,
        kind: String,
        identifier: Option<String>,
        reason: ItemDivergenceReason,
        diagnostic: String,
    },
}

impl PrefixDivergence {
    pub fn is_intact(&self) -> bool {
        matches!(
            self,
            Self::PrefixIntact { .. } | Self::FirstTurn | Self::Shortened { .. }
        )
    }

    pub fn summary_diagnostic(&self) -> String {
        match self {
            Self::FirstTurn => "First turn in session (initial prompt submission).".to_string(),
            Self::PrefixIntact {
                preserved_items,
                new_items,
            } => {
                format!(
                    "Prefix 100% intact ({preserved_items} items preserved, {new_items} new item{} appended).",
                    if *new_items == 1 { "" } else { "s" }
                )
            }
            Self::Shortened {
                preserved_items,
                dropped_items,
            } => {
                format!(
                    "Prefix shortened (rewind/trim): {preserved_items} items remain, {dropped_items} dropped. Remaining prefix is intact."
                )
            }
            Self::ModelChanged { previous, current } => {
                format!(
                    "Model changed from {} to {}.",
                    fmt_opt(previous.as_deref()),
                    fmt_opt(current.as_deref())
                )
            }
            Self::ReasoningEffortChanged { previous, current } => {
                format!(
                    "Reasoning effort changed from {} to {}.",
                    fmt_opt(previous.as_deref()),
                    fmt_opt(current.as_deref())
                )
            }
            Self::TemperatureChanged => "Sampling temperature changed.".to_string(),
            Self::PromptCacheKeyChanged => "Prompt cache key changed.".to_string(),
            Self::ToolChoiceChanged => "Tool choice policy changed.".to_string(),
            Self::JsonSchemaChanged => "JSON schema changed.".to_string(),
            Self::SystemPromptChanged { prev_len, curr_len } => {
                format!(
                    "System prompt diverged (length changed from {prev_len} to {curr_len} bytes)."
                )
            }
            Self::ToolsChanged { diff } => {
                format!("Tool definitions changed: {diff}")
            }
            Self::HostedToolsChanged { diff } => {
                format!("Hosted tool definitions changed: {diff}")
            }
            Self::ItemDiverged {
                index,
                kind,
                identifier,
                reason,
                diagnostic,
            } => {
                let id_str = identifier
                    .as_deref()
                    .map(|id| format!(" '{id}'"))
                    .unwrap_or_default();
                let reason_str = match reason {
                    ItemDivergenceReason::Pruned => "was pruned/trimmed to save context tokens",
                    ItemDivergenceReason::ImageEvicted => "had inline images evicted",
                    ItemDivergenceReason::ContentModified => "was modified/edited",
                    ItemDivergenceReason::VariantChanged => "changed item type variant",
                };
                format!("Item #{index} ({kind}{id_str}) {reason_str}: {diagnostic}")
            }
        }
    }
}

fn fmt_opt(value: Option<&str>) -> &str {
    value.unwrap_or("(none)")
}

/// Recorded outcome of a single turn for cache telemetry and diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheTurnRecord {
    pub turn_idx: String,
    pub loop_index: u32,
    pub prompt_tokens: u32,
    pub cached_prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cache_hit_rate_pct: f64,
    pub status: CacheStatus,
    pub divergence: PrefixDivergence,
    pub diagnostic: String,
    pub timestamp_rfc3339: String,
}

/// Aggregated cache telemetry summary for the session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheSummary {
    pub total_input_tokens: u64,
    pub total_cached_tokens: u64,
    pub overall_hit_rate_pct: f64,
    pub total_turns: usize,
    pub hits: usize,
    pub partial_hits: usize,
    pub breaks: usize,
    pub provider_misses: usize,
    pub last_break_diagnostic: Option<String>,
}

/// Manages prompt cache tracking, divergence detection, and telemetry reporting for a session.
#[derive(Debug, Default)]
pub struct CacheTracker {
    previous_request_summary: Option<RequestSummary>,
    turn_records: Vec<CacheTurnRecord>,
    summary: CacheSummary,
}

impl CacheTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Access the running summary of cache metrics.
    pub fn summary(&self) -> CacheSummary {
        self.summary.clone()
    }

    /// Access recent turn cache records.
    pub fn recent_turns(&self) -> &[CacheTurnRecord] {
        &self.turn_records
    }

    /// Update the previous-request fingerprint without recording a turn.
    ///
    /// Used when a model call produced no usage so we do not invent a 0/0 row,
    /// but the next recorded turn should still compare against this request.
    pub fn remember_request(&mut self, summary: RequestSummary) {
        self.previous_request_summary = Some(summary);
    }

    /// Human-readable report for headless `/cache` and ACP clients.
    pub fn format_report(&self) -> String {
        let s = &self.summary;
        if s.total_turns == 0 {
            return "Prompt cache telemetry: no turns recorded yet in this session.".to_string();
        }

        let mut lines = vec![
            "Prompt Cache Telemetry & Diagnostics:".to_string(),
            format!(
                "  Cache hit rate: {:.1}% ({} of {} input tokens cached)",
                s.overall_hit_rate_pct, s.total_cached_tokens, s.total_input_tokens,
            ),
            format!(
                "  Turns tracked:  {} ({} hits · {} partial · {} breaks · {} provider misses)",
                s.total_turns, s.hits, s.partial_hits, s.breaks, s.provider_misses,
            ),
        ];
        if let Some(ref last_break) = s.last_break_diagnostic {
            lines.push(format!("  Last break:     {last_break}"));
        }
        if !self.turn_records.is_empty() {
            lines.push("  Recent turns:".to_string());
            for rec in self.turn_records.iter().rev().take(10) {
                lines.push(format!(
                    "    Turn #{} (loop {}) — {:.1}% hit ({} in, {} cached) · {}",
                    rec.turn_idx,
                    rec.loop_index,
                    rec.cache_hit_rate_pct,
                    rec.prompt_tokens,
                    rec.cached_prompt_tokens,
                    rec.diagnostic,
                ));
            }
        }
        lines.join("\n")
    }

    /// Extract a lightweight summary fingerprint from a `ConversationRequest`.
    pub fn summarize_request(request: &ConversationRequest) -> RequestSummary {
        let mut total_body_bytes = 0;

        let mut tools = Vec::with_capacity(request.tools.len());
        for tool in &request.tools {
            let desc_hash = tool.description.as_deref().map_or(0, hash_str);
            let params_hash = hash_json(&tool.parameters);
            total_body_bytes += tool.name.len();
            if let Some(desc) = &tool.description {
                total_body_bytes += desc.len();
            }
            tools.push(ToolSummary {
                name: tool.name.clone(),
                description_hash: desc_hash,
                params_hash,
            });
        }

        let hosted_tool_names: Vec<String> = request
            .hosted_tools
            .iter()
            .map(|tool| tool.wire_name().to_string())
            .collect();
        let hosted_tools_hash = hash_hosted_tools(&request.hosted_tools);
        for name in &hosted_tool_names {
            total_body_bytes += name.len();
        }

        let mut items = Vec::with_capacity(request.items.len());
        for (index, item) in request.items.iter().enumerate() {
            let (kind, identifier, byte_len, is_pruned, has_images) = summarize_item(item);
            let content_hash = hash_item(item);
            total_body_bytes += byte_len;
            items.push(ItemSummary {
                index,
                kind,
                identifier,
                byte_len,
                content_hash,
                is_pruned,
                has_images,
            });
        }

        RequestSummary {
            model: request.model.clone(),
            reasoning_effort: request
                .reasoning_effort
                .map(|effort| format!("{effort:?}").to_lowercase()),
            temperature_bits: request.temperature.map(f32::to_bits),
            prompt_cache_key: request.prompt_cache_key.clone(),
            tool_choice_hash: request.tool_choice.as_ref().map_or(0, |choice| {
                hash_json(&serde_json::to_value(choice).unwrap_or_default())
            }),
            json_schema_hash: request.json_schema.as_ref().map_or(0, hash_json),
            tools,
            hosted_tool_names,
            hosted_tools_hash,
            items,
            total_body_bytes,
        }
    }

    /// Compare the previous request summary against the current request summary to find prefix divergence.
    pub fn analyze_prefix_divergence(
        previous: Option<&RequestSummary>,
        current: &RequestSummary,
    ) -> PrefixDivergence {
        let Some(prev) = previous else {
            return PrefixDivergence::FirstTurn;
        };

        if prev.model != current.model {
            return PrefixDivergence::ModelChanged {
                previous: prev.model.clone(),
                current: current.model.clone(),
            };
        }
        if prev.reasoning_effort != current.reasoning_effort {
            return PrefixDivergence::ReasoningEffortChanged {
                previous: prev.reasoning_effort.clone(),
                current: current.reasoning_effort.clone(),
            };
        }
        if prev.temperature_bits != current.temperature_bits {
            return PrefixDivergence::TemperatureChanged;
        }
        if prev.prompt_cache_key != current.prompt_cache_key {
            return PrefixDivergence::PromptCacheKeyChanged;
        }
        if prev.tool_choice_hash != current.tool_choice_hash {
            return PrefixDivergence::ToolChoiceChanged;
        }
        if prev.json_schema_hash != current.json_schema_hash {
            return PrefixDivergence::JsonSchemaChanged;
        }
        if prev.tools != current.tools {
            return PrefixDivergence::ToolsChanged {
                diff: describe_tools_diff(&prev.tools, &current.tools),
            };
        }
        if prev.hosted_tools_hash != current.hosted_tools_hash {
            return PrefixDivergence::HostedToolsChanged {
                diff: describe_names_diff(&prev.hosted_tool_names, &current.hosted_tool_names),
            };
        }

        let min_items = prev.items.len().min(current.items.len());
        for i in 0..min_items {
            let prev_item = &prev.items[i];
            let curr_item = &current.items[i];

            if prev_item.content_hash != curr_item.content_hash {
                if prev_item.kind == "system" || curr_item.kind == "system" {
                    return PrefixDivergence::SystemPromptChanged {
                        prev_len: prev_item.byte_len,
                        curr_len: curr_item.byte_len,
                    };
                }

                let (reason, diagnostic) = if prev_item.kind != curr_item.kind {
                    (
                        ItemDivergenceReason::VariantChanged,
                        format!("Changed from '{}' to '{}'", prev_item.kind, curr_item.kind),
                    )
                } else if !prev_item.is_pruned && curr_item.is_pruned {
                    (
                        ItemDivergenceReason::Pruned,
                        format!(
                            "Tool output pruned from {} bytes to {} bytes",
                            prev_item.byte_len, curr_item.byte_len
                        ),
                    )
                } else if prev_item.has_images && !curr_item.has_images {
                    (
                        ItemDivergenceReason::ImageEvicted,
                        "Inline images were evicted from this message".to_string(),
                    )
                } else {
                    (
                        ItemDivergenceReason::ContentModified,
                        format!(
                            "Length changed from {} to {} bytes",
                            prev_item.byte_len, curr_item.byte_len
                        ),
                    )
                };

                return PrefixDivergence::ItemDiverged {
                    index: i,
                    kind: curr_item.kind.clone(),
                    identifier: curr_item.identifier.clone(),
                    reason,
                    diagnostic,
                };
            }
        }

        if current.items.len() < prev.items.len() {
            return PrefixDivergence::Shortened {
                preserved_items: current.items.len(),
                dropped_items: prev.items.len() - current.items.len(),
            };
        }

        PrefixDivergence::PrefixIntact {
            preserved_items: prev.items.len(),
            new_items: current.items.len().saturating_sub(prev.items.len()),
        }
    }

    /// Record a turn outcome with token usage and request summary, emitting unified logs and tracing.
    pub fn record_turn_outcome(
        &mut self,
        session_id: Option<&str>,
        turn_idx: &str,
        loop_index: u32,
        prompt_tokens: u32,
        cached_prompt_tokens: u32,
        completion_tokens: u32,
        current_request_summary: RequestSummary,
        cache_key_forwarded: bool,
    ) -> CacheTurnRecord {
        let divergence = Self::analyze_prefix_divergence(
            self.previous_request_summary.as_ref(),
            &current_request_summary,
        );

        let hit_rate_pct = if prompt_tokens > 0 {
            (cached_prompt_tokens as f64 / prompt_tokens as f64) * 100.0
        } else {
            0.0
        };

        let status = if self.previous_request_summary.is_none() {
            CacheStatus::FirstTurn
        } else if cached_prompt_tokens > 0 {
            if hit_rate_pct >= 50.0 {
                CacheStatus::Hit
            } else {
                CacheStatus::PartialHit
            }
        } else if prompt_tokens > 0 && divergence.is_intact() {
            if cache_key_forwarded && prompt_tokens >= PROVIDER_MISS_MIN_PROMPT_TOKENS {
                CacheStatus::ProviderMiss
            } else {
                CacheStatus::NoCacheSupport
            }
        } else {
            CacheStatus::Break
        };

        let diagnostic = match status {
            CacheStatus::FirstTurn => "First turn in session (cold cache).".to_string(),
            CacheStatus::Hit => format!(
                "Cache hit: {hit_rate_pct:.1}% ({cached_prompt_tokens}/{prompt_tokens} tokens cached)."
            ),
            CacheStatus::PartialHit => format!(
                "Partial cache hit: {hit_rate_pct:.1}% ({cached_prompt_tokens}/{prompt_tokens} tokens cached). {}",
                divergence.summary_diagnostic()
            ),
            CacheStatus::Break => {
                format!("Cache break: 0% hit rate. {}", divergence.summary_diagnostic())
            }
            CacheStatus::ProviderMiss => format!(
                "Provider cache miss: prefix intact, cache key forwarded, 0/{prompt_tokens} cached tokens (routing, TTL, or provider did not apply the cache)."
            ),
            CacheStatus::NoCacheSupport => {
                "0 cached tokens reported (provider may not support prompt caching, cache key was not forwarded, or prompt is too small to cache).".to_string()
            }
        };

        self.summary.total_turns += 1;
        self.summary.total_input_tokens += u64::from(prompt_tokens);
        self.summary.total_cached_tokens += u64::from(cached_prompt_tokens);
        if self.summary.total_input_tokens > 0 {
            self.summary.overall_hit_rate_pct = (self.summary.total_cached_tokens as f64
                / self.summary.total_input_tokens as f64)
                * 100.0;
        }

        match status {
            CacheStatus::Hit => self.summary.hits += 1,
            CacheStatus::PartialHit => self.summary.partial_hits += 1,
            CacheStatus::Break => {
                self.summary.breaks += 1;
                self.summary.last_break_diagnostic = Some(diagnostic.clone());
            }
            CacheStatus::ProviderMiss => self.summary.provider_misses += 1,
            _ => {}
        }

        xai_grok_telemetry::unified_log::info(
            "shell.turn.cache_status",
            session_id,
            Some(serde_json::json!({
                "turn_idx": turn_idx,
                "loop_index": loop_index,
                "prompt_tokens": prompt_tokens,
                "cached_prompt_tokens": cached_prompt_tokens,
                "completion_tokens": completion_tokens,
                "hit_rate_pct": (hit_rate_pct * 10.0).round() / 10.0,
                "status": status,
                "divergence": divergence,
                "diagnostic": diagnostic,
                "cache_key_forwarded": cache_key_forwarded,
            })),
        );

        if matches!(status, CacheStatus::Break) {
            xai_grok_telemetry::unified_log::warn(
                "shell.turn.cache_break",
                session_id,
                Some(serde_json::json!({
                    "turn_idx": turn_idx,
                    "loop_index": loop_index,
                    "prompt_tokens": prompt_tokens,
                    "divergence": divergence,
                    "diagnostic": diagnostic,
                })),
            );
            tracing::warn!(
                turn_idx = %turn_idx,
                loop_index = %loop_index,
                prompt_tokens = %prompt_tokens,
                diagnostic = %diagnostic,
                "Prompt cache break detected"
            );
        } else if matches!(status, CacheStatus::ProviderMiss) {
            xai_grok_telemetry::unified_log::warn(
                "shell.turn.cache_miss",
                session_id,
                Some(serde_json::json!({
                    "turn_idx": turn_idx,
                    "loop_index": loop_index,
                    "prompt_tokens": prompt_tokens,
                    "divergence": divergence,
                    "diagnostic": diagnostic,
                })),
            );
            tracing::warn!(
                turn_idx = %turn_idx,
                loop_index = %loop_index,
                prompt_tokens = %prompt_tokens,
                diagnostic = %diagnostic,
                "Prompt cache provider miss"
            );
        } else {
            tracing::info!(
                turn_idx = %turn_idx,
                loop_index = %loop_index,
                hit_rate = format!("{hit_rate_pct:.1}%"),
                cached_tokens = %cached_prompt_tokens,
                total_tokens = %prompt_tokens,
                "Prompt cache outcome"
            );
        }

        let record = CacheTurnRecord {
            turn_idx: turn_idx.to_string(),
            loop_index,
            prompt_tokens,
            cached_prompt_tokens,
            completion_tokens,
            cache_hit_rate_pct: (hit_rate_pct * 10.0).round() / 10.0,
            status,
            divergence,
            diagnostic,
            timestamp_rfc3339: Utc::now().to_rfc3339(),
        };

        if self.turn_records.len() >= MAX_RECENT_TURNS {
            self.turn_records.remove(0);
        }
        self.turn_records.push(record.clone());
        self.previous_request_summary = Some(current_request_summary);

        record
    }
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn hash_json(v: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    v.to_string().hash(&mut hasher);
    hasher.finish()
}

fn hash_hosted_tools(tools: &[HostedTool]) -> u64 {
    let mut hasher = DefaultHasher::new();
    tools.len().hash(&mut hasher);
    for tool in tools {
        format!("{tool:?}").hash(&mut hasher);
    }
    hasher.finish()
}

fn hash_item(item: &ConversationItem) -> u64 {
    let mut hasher = DefaultHasher::new();
    match item {
        ConversationItem::System(s) => {
            0u8.hash(&mut hasher);
            s.content.hash(&mut hasher);
        }
        ConversationItem::User(u) => {
            1u8.hash(&mut hasher);
            if let Some(reason) = &u.synthetic_reason {
                synthetic_reason_slug(reason).hash(&mut hasher);
            }
            for part in &u.content {
                match part {
                    ContentPart::Text { text } => {
                        0u8.hash(&mut hasher);
                        text.hash(&mut hasher);
                    }
                    ContentPart::Image { url } => {
                        1u8.hash(&mut hasher);
                        url.hash(&mut hasher);
                    }
                }
            }
        }
        ConversationItem::Assistant(a) => {
            2u8.hash(&mut hasher);
            a.content.hash(&mut hasher);
            for call in &a.tool_calls {
                call.id.hash(&mut hasher);
                call.name.hash(&mut hasher);
                call.arguments.hash(&mut hasher);
            }
        }
        ConversationItem::ToolResult(tr) => {
            3u8.hash(&mut hasher);
            tr.tool_call_id.hash(&mut hasher);
            tr.content.hash(&mut hasher);
            for img in &tr.images {
                match img {
                    ContentPart::Text { text } => {
                        0u8.hash(&mut hasher);
                        text.hash(&mut hasher);
                    }
                    ContentPart::Image { url } => {
                        1u8.hash(&mut hasher);
                        url.hash(&mut hasher);
                    }
                }
            }
            for block in &tr.ordered_content {
                match block {
                    CustomToolOutputContent::Text { text } => {
                        2u8.hash(&mut hasher);
                        text.hash(&mut hasher);
                    }
                    CustomToolOutputContent::Image { url, .. } => {
                        3u8.hash(&mut hasher);
                        url.hash(&mut hasher);
                    }
                }
            }
        }
        ConversationItem::CustomToolOutput(co) => {
            4u8.hash(&mut hasher);
            co.call_id.hash(&mut hasher);
            for block in &co.content {
                match block {
                    CustomToolOutputContent::Text { text } => text.hash(&mut hasher),
                    CustomToolOutputContent::Image { url, .. } => url.hash(&mut hasher),
                }
            }
        }
        ConversationItem::BackendToolCall(btc) => {
            5u8.hash(&mut hasher);
            serde_json::to_string(btc)
                .unwrap_or_default()
                .hash(&mut hasher);
        }
        ConversationItem::Reasoning(r) => {
            6u8.hash(&mut hasher);
            serde_json::to_string(r)
                .unwrap_or_default()
                .hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn summarize_item(item: &ConversationItem) -> (String, Option<String>, usize, bool, bool) {
    match item {
        ConversationItem::System(s) => ("system".into(), None, s.content.len(), false, false),
        ConversationItem::User(u) => {
            let mut len = 0;
            let mut has_images = false;
            for part in &u.content {
                match part {
                    ContentPart::Text { text } => len += text.len(),
                    ContentPart::Image { url } => {
                        len += url.len();
                        has_images = true;
                    }
                }
            }
            (
                user_kind_label(u.synthetic_reason.as_ref()),
                None,
                len,
                false,
                has_images,
            )
        }
        ConversationItem::Assistant(a) => {
            let text_len = a.content.len();
            let tools_len = a
                .tool_calls
                .iter()
                .map(|c| c.name.len() + c.arguments.len())
                .sum::<usize>();
            let id = a.tool_calls.first().map(|c| c.name.clone());
            ("assistant".into(), id, text_len + tools_len, false, false)
        }
        ConversationItem::ToolResult(tr) => {
            let mut len = tr.content.len();
            let mut has_images = !tr.images.is_empty();
            let is_pruned = tr.content.contains(HARD_CLEAR_PLACEHOLDER)
                || tr.content.contains(SOFT_TRIM_SEPARATOR);
            for img in &tr.images {
                match img {
                    ContentPart::Text { text } => len += text.len(),
                    ContentPart::Image { url } => len += url.len(),
                }
            }
            for block in &tr.ordered_content {
                match block {
                    CustomToolOutputContent::Text { text } => len += text.len(),
                    CustomToolOutputContent::Image { url, .. } => {
                        len += url.len();
                        has_images = true;
                    }
                }
            }
            (
                "tool_result".into(),
                Some(tr.tool_call_id.clone()),
                len,
                is_pruned,
                has_images,
            )
        }
        ConversationItem::CustomToolOutput(co) => {
            let mut len = 0;
            let mut has_images = false;
            for block in &co.content {
                match block {
                    CustomToolOutputContent::Text { text } => len += text.len(),
                    CustomToolOutputContent::Image { url, .. } => {
                        len += url.len();
                        has_images = true;
                    }
                }
            }
            (
                "custom_tool_output".into(),
                Some(co.call_id.clone()),
                len,
                false,
                has_images,
            )
        }
        ConversationItem::BackendToolCall(btc) => {
            let s = serde_json::to_string(btc).unwrap_or_default();
            ("backend_tool_call".into(), None, s.len(), false, false)
        }
        ConversationItem::Reasoning(r) => {
            let s = serde_json::to_string(r).unwrap_or_default();
            ("reasoning".into(), None, s.len(), false, false)
        }
    }
}

fn user_kind_label(reason: Option<&SyntheticReason>) -> String {
    match reason {
        Some(reason) => format!("user:{}", synthetic_reason_slug(reason)),
        None => "user".into(),
    }
}

fn synthetic_reason_slug(reason: &SyntheticReason) -> &'static str {
    match reason {
        SyntheticReason::CompactionMeta => "compaction_meta",
        SyntheticReason::SystemReminder => "system_reminder",
        SyntheticReason::ProjectInstructions => "project_instructions",
        SyntheticReason::AutoContinue => "auto_continue",
        SyntheticReason::AutoRecovery => "auto_recovery",
        SyntheticReason::Interjection => "interjection",
        SyntheticReason::TaskCompleted => "task_completed",
        SyntheticReason::SubagentCompleted => "subagent_completed",
        SyntheticReason::NotificationDrain => "notification_drain",
        SyntheticReason::GoalSummary => "goal_summary",
        SyntheticReason::GoalClassifierNudge => "goal_classifier_nudge",
        SyntheticReason::SchedulerFired => "scheduler_fired",
        SyntheticReason::AgentMessage => "agent_message",
        SyntheticReason::StopHookFeedback => "stop_hook_feedback",
        SyntheticReason::WorkingDirectorySwitch => "working_directory_switch",
        SyntheticReason::Unknown => "unknown",
    }
}

fn describe_tools_diff(prev: &[ToolSummary], curr: &[ToolSummary]) -> String {
    describe_names_diff(
        &prev.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
        &curr.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
    )
}

fn describe_names_diff(prev_names: &[String], curr_names: &[String]) -> String {
    let added: Vec<&str> = curr_names
        .iter()
        .map(String::as_str)
        .filter(|n| !prev_names.iter().any(|p| p == n))
        .collect();
    let removed: Vec<&str> = prev_names
        .iter()
        .map(String::as_str)
        .filter(|n| !curr_names.iter().any(|c| c == n))
        .collect();

    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("added [{}]", added.join(", ")));
    }
    if !removed.is_empty() {
        parts.push(format!("removed [{}]", removed.join(", ")));
    }
    if prev_names.len() == curr_names.len() && added.is_empty() && removed.is_empty() {
        parts.push("parameters or descriptions modified".to_string());
    }
    if parts.is_empty() {
        "reordered".to_string()
    } else {
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use xai_grok_sampling_types::{
        AssistantItem, ContentPart, ReasoningEffort, SystemItem, ToolResultItem, UserItem,
    };

    fn record(
        tracker: &mut CacheTracker,
        turn: &str,
        loop_index: u32,
        prompt: u32,
        cached: u32,
        summary: RequestSummary,
        forwarded: bool,
    ) -> CacheTurnRecord {
        tracker.record_turn_outcome(
            None, turn, loop_index, prompt, cached, 100, summary, forwarded,
        )
    }

    fn secret_request() -> ConversationRequest {
        ConversationRequest {
            items: vec![
                ConversationItem::System(SystemItem {
                    content: Arc::from("SECRET_SYSTEM_PROMPT_XYZ"),
                }),
                ConversationItem::user("hello secret user text"),
            ],
            model: Some("grok-4".into()),
            prompt_cache_key: Some("sess-1".into()),
            ..Default::default()
        }
    }

    #[test]
    fn test_first_turn_analysis() {
        let req = ConversationRequest {
            items: vec![ConversationItem::user("hello world")],
            ..Default::default()
        };
        let summary = CacheTracker::summarize_request(&req);
        let div = CacheTracker::analyze_prefix_divergence(None, &summary);
        assert_eq!(div, PrefixDivergence::FirstTurn);
        assert!(div.is_intact());
    }

    #[test]
    fn test_intact_prefix() {
        let item1 = ConversationItem::user("turn 1 prompt");
        let item2 = ConversationItem::Assistant(AssistantItem {
            content: Arc::from("turn 1 reply"),
            tool_calls: vec![],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        });
        let item3 = ConversationItem::user("turn 2 prompt");

        let req1 = ConversationRequest {
            items: vec![item1.clone(), item2.clone()],
            ..Default::default()
        };
        let req2 = ConversationRequest {
            items: vec![item1, item2, item3],
            ..Default::default()
        };

        let sum1 = CacheTracker::summarize_request(&req1);
        let sum2 = CacheTracker::summarize_request(&req2);

        let div = CacheTracker::analyze_prefix_divergence(Some(&sum1), &sum2);
        assert_eq!(
            div,
            PrefixDivergence::PrefixIntact {
                preserved_items: 2,
                new_items: 1,
            }
        );
        assert!(div.is_intact());
    }

    #[test]
    fn test_system_prompt_divergence() {
        let sys1 = ConversationItem::System(SystemItem {
            content: Arc::from("You are a helpful assistant."),
        });
        let sys2 = ConversationItem::System(SystemItem {
            content: Arc::from("You are a specialized coding agent."),
        });

        let req1 = ConversationRequest {
            items: vec![sys1, ConversationItem::user("hi")],
            ..Default::default()
        };
        let req2 = ConversationRequest {
            items: vec![sys2, ConversationItem::user("hi")],
            ..Default::default()
        };

        let sum1 = CacheTracker::summarize_request(&req1);
        let sum2 = CacheTracker::summarize_request(&req2);

        let div = CacheTracker::analyze_prefix_divergence(Some(&sum1), &sum2);
        assert!(matches!(div, PrefixDivergence::SystemPromptChanged { .. }));
        assert!(!div.is_intact());
    }

    #[test]
    fn test_model_change_breaks_before_items() {
        let req1 = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            model: Some("grok-4".into()),
            ..Default::default()
        };
        let req2 = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            model: Some("grok-4-fast".into()),
            ..Default::default()
        };
        let div = CacheTracker::analyze_prefix_divergence(
            Some(&CacheTracker::summarize_request(&req1)),
            &CacheTracker::summarize_request(&req2),
        );
        assert!(matches!(div, PrefixDivergence::ModelChanged { .. }));
    }

    #[test]
    fn test_reasoning_effort_change_breaks() {
        let req1 = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            reasoning_effort: Some(ReasoningEffort::Low),
            ..Default::default()
        };
        let req2 = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let div = CacheTracker::analyze_prefix_divergence(
            Some(&CacheTracker::summarize_request(&req1)),
            &CacheTracker::summarize_request(&req2),
        );
        assert!(matches!(
            div,
            PrefixDivergence::ReasoningEffortChanged { .. }
        ));
    }

    #[test]
    fn test_prompt_cache_key_change_breaks() {
        let req1 = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            prompt_cache_key: Some("sess-1".into()),
            ..Default::default()
        };
        let req2 = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            prompt_cache_key: Some("sess-2".into()),
            ..Default::default()
        };
        let div = CacheTracker::analyze_prefix_divergence(
            Some(&CacheTracker::summarize_request(&req1)),
            &CacheTracker::summarize_request(&req2),
        );
        assert_eq!(div, PrefixDivergence::PromptCacheKeyChanged);
    }

    #[test]
    fn test_hosted_tools_change_breaks() {
        let req1 = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            hosted_tools: vec![HostedTool::web_search(None)],
            ..Default::default()
        };
        let req2 = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            hosted_tools: vec![HostedTool::web_search(Some(vec!["example.com".into()]))],
            ..Default::default()
        };
        let div = CacheTracker::analyze_prefix_divergence(
            Some(&CacheTracker::summarize_request(&req1)),
            &CacheTracker::summarize_request(&req2),
        );
        assert!(matches!(div, PrefixDivergence::HostedToolsChanged { .. }));
    }

    #[test]
    fn test_rewind_is_shortened_not_a_break() {
        let sys = ConversationItem::System(SystemItem {
            content: Arc::from("sys"),
        });
        let user = ConversationItem::user("hi");
        let asst = ConversationItem::Assistant(AssistantItem {
            content: Arc::from("hello"),
            tool_calls: vec![],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        });

        let prev = ConversationRequest {
            items: vec![sys.clone(), user.clone(), asst],
            ..Default::default()
        };
        let curr = ConversationRequest {
            items: vec![sys, user],
            ..Default::default()
        };
        let div = CacheTracker::analyze_prefix_divergence(
            Some(&CacheTracker::summarize_request(&prev)),
            &CacheTracker::summarize_request(&curr),
        );
        assert_eq!(
            div,
            PrefixDivergence::Shortened {
                preserved_items: 2,
                dropped_items: 1,
            }
        );
        assert!(div.is_intact());

        let mut tracker = CacheTracker::new();
        record(
            &mut tracker,
            "1",
            0,
            1000,
            0,
            CacheTracker::summarize_request(&prev),
            true,
        );
        let rec = record(
            &mut tracker,
            "2",
            1,
            800,
            400,
            CacheTracker::summarize_request(&curr),
            true,
        );
        assert_eq!(rec.status, CacheStatus::Hit);
        assert_eq!(tracker.summary().breaks, 0);
    }

    #[test]
    fn test_tool_result_pruned_divergence() {
        let user = ConversationItem::user("run command");
        let tr_full = ConversationItem::ToolResult(ToolResultItem {
            tool_call_id: "call_1".into(),
            content: Arc::from("A".repeat(50_000)),
            images: vec![],
            ordered_content: vec![],
        });
        let tr_pruned = ConversationItem::ToolResult(ToolResultItem {
            tool_call_id: "call_1".into(),
            content: Arc::from(HARD_CLEAR_PLACEHOLDER),
            images: vec![],
            ordered_content: vec![],
        });

        let req1 = ConversationRequest {
            items: vec![user.clone(), tr_full],
            ..Default::default()
        };
        let req2 = ConversationRequest {
            items: vec![user, tr_pruned],
            ..Default::default()
        };

        let sum1 = CacheTracker::summarize_request(&req1);
        let sum2 = CacheTracker::summarize_request(&req2);

        let div = CacheTracker::analyze_prefix_divergence(Some(&sum1), &sum2);
        match div {
            PrefixDivergence::ItemDiverged {
                index,
                kind,
                identifier,
                reason,
                diagnostic,
            } => {
                assert_eq!(index, 1);
                assert_eq!(kind, "tool_result");
                assert_eq!(identifier.as_deref(), Some("call_1"));
                assert_eq!(reason, ItemDivergenceReason::Pruned);
                assert!(!diagnostic.contains("A".repeat(20).as_str()));
            }
            other => panic!("expected ItemDiverged(Pruned), got {other:?}"),
        }
    }

    #[test]
    fn test_tools_changed_divergence() {
        let req1 = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            tools: vec![ToolSpec {
                name: "bash".into(),
                description: Some("run bash".into()),
                parameters: serde_json::json!({}),
            }],
            ..Default::default()
        };
        let req2 = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            tools: vec![
                ToolSpec {
                    name: "bash".into(),
                    description: Some("run bash".into()),
                    parameters: serde_json::json!({}),
                },
                ToolSpec {
                    name: "edit".into(),
                    description: Some("edit file".into()),
                    parameters: serde_json::json!({}),
                },
            ],
            ..Default::default()
        };

        let sum1 = CacheTracker::summarize_request(&req1);
        let sum2 = CacheTracker::summarize_request(&req2);

        let div = CacheTracker::analyze_prefix_divergence(Some(&sum1), &sum2);
        assert!(matches!(div, PrefixDivergence::ToolsChanged { .. }));
    }

    #[test]
    fn test_turn_outcome_recording_and_hit_rate() {
        let mut tracker = CacheTracker::new();

        let req1 = ConversationRequest {
            items: vec![ConversationItem::user("turn 1")],
            ..Default::default()
        };
        let rec1 = record(
            &mut tracker,
            "1",
            0,
            1000,
            0,
            CacheTracker::summarize_request(&req1),
            true,
        );
        assert_eq!(rec1.status, CacheStatus::FirstTurn);
        assert_eq!(rec1.cache_hit_rate_pct, 0.0);

        let req2 = ConversationRequest {
            items: vec![
                ConversationItem::user("turn 1"),
                ConversationItem::user("turn 2"),
            ],
            ..Default::default()
        };
        let rec2 = record(
            &mut tracker,
            "2",
            1,
            1500,
            1000,
            CacheTracker::summarize_request(&req2),
            true,
        );
        assert_eq!(rec2.status, CacheStatus::Hit);
        assert!((rec2.cache_hit_rate_pct - 66.66).abs() < 0.1);

        let summary = tracker.summary();
        assert_eq!(summary.total_turns, 2);
        assert_eq!(summary.total_input_tokens, 2500);
        assert_eq!(summary.total_cached_tokens, 1000);
        assert_eq!(summary.hits, 1);
        assert_eq!(summary.breaks, 0);
        assert_eq!(summary.provider_misses, 0);
        assert!((summary.overall_hit_rate_pct - 40.0).abs() < 0.1);
    }

    #[test]
    fn test_stable_prefix_zero_hit_on_large_prompt_is_provider_miss() {
        let mut tracker = CacheTracker::new();
        let req1 = ConversationRequest {
            items: vec![ConversationItem::user("turn 1")],
            ..Default::default()
        };
        record(
            &mut tracker,
            "1",
            0,
            2000,
            0,
            CacheTracker::summarize_request(&req1),
            true,
        );

        let req2 = ConversationRequest {
            items: vec![
                ConversationItem::user("turn 1"),
                ConversationItem::user("turn 2"),
            ],
            ..Default::default()
        };
        let rec = record(
            &mut tracker,
            "2",
            1,
            2500,
            0,
            CacheTracker::summarize_request(&req2),
            true,
        );
        assert_eq!(rec.status, CacheStatus::ProviderMiss);
        assert_eq!(tracker.summary().provider_misses, 1);
        assert_eq!(tracker.summary().breaks, 0);
    }

    #[test]
    fn test_unforwarded_cache_key_is_not_a_provider_miss() {
        let mut tracker = CacheTracker::new();
        let req = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            ..Default::default()
        };
        record(
            &mut tracker,
            "1",
            0,
            2000,
            0,
            CacheTracker::summarize_request(&req),
            false,
        );
        let rec = record(
            &mut tracker,
            "2",
            1,
            2000,
            0,
            CacheTracker::summarize_request(&req),
            false,
        );
        assert_eq!(rec.status, CacheStatus::NoCacheSupport);
        assert_eq!(tracker.summary().provider_misses, 0);
        assert_eq!(tracker.summary().breaks, 0);
    }

    #[test]
    fn test_small_intact_prompt_is_not_a_provider_miss() {
        let mut tracker = CacheTracker::new();
        let req = ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            ..Default::default()
        };
        record(
            &mut tracker,
            "1",
            0,
            200,
            0,
            CacheTracker::summarize_request(&req),
            true,
        );
        let rec = record(
            &mut tracker,
            "2",
            1,
            200,
            0,
            CacheTracker::summarize_request(&req),
            true,
        );
        assert_eq!(rec.status, CacheStatus::NoCacheSupport);
        assert_eq!(tracker.summary().provider_misses, 0);
    }

    #[test]
    fn test_summaries_and_reports_omit_prompt_text() {
        let req = secret_request();
        let summary = CacheTracker::summarize_request(&req);
        let json = serde_json::to_string(&summary).unwrap();
        assert!(
            !json.contains("SECRET_SYSTEM_PROMPT_XYZ"),
            "summary leaked system prompt: {json}"
        );
        assert!(
            !json.contains("hello secret user text"),
            "summary leaked user text: {json}"
        );

        let mut tracker = CacheTracker::new();
        record(&mut tracker, "1", 0, 2000, 0, summary, true);
        let report = tracker.format_report();
        assert!(!report.contains("SECRET_SYSTEM_PROMPT_XYZ"));
        assert!(!report.contains("hello secret user text"));
        assert!(report.contains("First turn"));
    }

    #[test]
    fn test_user_synthetic_reason_is_in_the_item_kind() {
        let item = ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::from("AGENTS.md"),
            }],
            synthetic_reason: Some(SyntheticReason::ProjectInstructions),
            ..Default::default()
        });
        let req = ConversationRequest {
            items: vec![item],
            ..Default::default()
        };
        let summary = CacheTracker::summarize_request(&req);
        assert_eq!(summary.items[0].kind, "user:project_instructions");
    }
}
