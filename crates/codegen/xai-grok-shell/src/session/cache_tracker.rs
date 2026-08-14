//! Prompt cache hit rate analysis and cache break tracking.
//!
//! Prompt caching in LLMs (xAI Grok, OpenAI Codex/Responses, Anthropic Messages, DeepSeek)
//! requires the prompt prefix to remain byte-for-byte stable across consecutive turns.
//! Any modification to the system prompt, tool definitions, or earlier conversation items
//! (pruning, image eviction, message mutation, or history truncation) invalidates the KV cache
//! at that exact position.
//!
//! This module provides:
//! 1. `RequestSummary`: Fast structural fingerprinting of `ConversationRequest`s.
//! 2. `analyze_prefix_divergence`: Exact detection of where and why a prompt prefix diverged.
//! 3. `CacheTracker`: Turn-by-turn evaluation, cache status categorization, and structured logging
//!    to `unified_log` and `tracing`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use xai_grok_sampling_types::{
    ContentPart, ConversationItem, ConversationRequest, CustomToolOutputContent, ToolSpec,
};

/// Placeholder inserted when a tool result is hard-cleared (from `xai_chat_state`).
pub const HARD_CLEAR_PLACEHOLDER: &str = "[Tool result omitted — too old]";
/// Separator inserted between head and tail in soft-trimmed results.
pub const SOFT_TRIM_SEPARATOR: &str = "[…trimmed…]";

/// Maximum number of recent turn records kept in memory for interactive inspection.
const MAX_RECENT_TURNS: usize = 50;

/// Summarized fingerprint of a conversation item.
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
    pub preview: String,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestSummary {
    pub system_prompt: Option<String>,
    pub system_prompt_hash: u64,
    pub tools: Vec<ToolSummary>,
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
    /// Cache broke (0 cached tokens on turn > 1, or complete miss).
    Break,
    /// No prompt caching reported by provider.
    NoCacheSupport,
}

impl CacheStatus {
    pub fn display_label(&self) -> &'static str {
        match self {
            Self::FirstTurn => "First turn (cold cache)",
            Self::Hit => "Cache hit",
            Self::PartialHit => "Partial cache hit",
            Self::Break => "Cache break",
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
    /// System prompt changed between turns.
    SystemPromptChanged {
        diff_offset: usize,
        prev_len: usize,
        curr_len: usize,
    },
    /// Tool definitions were added, removed, reordered, or modified.
    ToolsChanged {
        diff: String,
    },
    /// Conversation item at `index` diverged from the previous turn.
    ItemDiverged {
        index: usize,
        kind: String,
        identifier: Option<String>,
        reason: ItemDivergenceReason,
        diagnostic: String,
    },
    /// History was truncated or rewound.
    HistoryTruncated {
        prev_count: usize,
        curr_count: usize,
    },
}

impl PrefixDivergence {
    pub fn is_intact(&self) -> bool {
        matches!(self, Self::PrefixIntact { .. } | Self::FirstTurn)
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
            Self::SystemPromptChanged {
                diff_offset,
                prev_len,
                curr_len,
            } => {
                format!(
                    "System prompt diverged at character offset {diff_offset} (length changed from {prev_len} to {curr_len})."
                )
            }
            Self::ToolsChanged { diff } => {
                format!("Tool definitions changed: {diff}")
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
            Self::HistoryTruncated {
                prev_count,
                curr_count,
            } => {
                format!("Conversation history truncated from {prev_count} to {curr_count} items.")
            }
        }
    }
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
    /// Input tokens from steady-state turns only (excludes the cold first turn).
    ///
    /// The first request of a session cannot hit the cache by definition, so the
    /// overall hit rate is computed over steady-state turns to avoid permanently
    /// diluting it with the cold start.
    pub steady_input_tokens: u64,
    /// Cached tokens from steady-state turns only (excludes the cold first turn).
    pub steady_cached_tokens: u64,
    pub overall_hit_rate_pct: f64,
    pub total_turns: usize,
    pub hits: usize,
    pub partial_hits: usize,
    pub breaks: usize,
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

    /// Extract a lightweight summary fingerprint from a `ConversationRequest`.
    pub fn summarize_request(request: &ConversationRequest) -> RequestSummary {
        let mut total_body_bytes = 0;

        let system_prompt = request.items.iter().find_map(|item| match item {
            ConversationItem::System(s) => Some(s.content.to_string()),
            _ => None,
        });
        let system_prompt_hash = system_prompt.as_ref().map_or(0, |s| {
            total_body_bytes += s.len();
            hash_str(s)
        });

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

        let mut items = Vec::with_capacity(request.items.len());
        for (index, item) in request.items.iter().enumerate() {
            let (kind, identifier, byte_len, is_pruned, has_images, preview) =
                summarize_item(item);
            let content_hash = hash_item(item);
            total_body_bytes += byte_len;
            items.push(ItemSummary {
                index,
                kind: kind.to_string(),
                identifier,
                byte_len,
                content_hash,
                is_pruned,
                has_images,
                preview,
            });
        }

        RequestSummary {
            system_prompt,
            system_prompt_hash,
            tools,
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

        // 1. Check system prompt
        if prev.system_prompt_hash != current.system_prompt_hash {
            let prev_str = prev.system_prompt.as_deref().unwrap_or("");
            let curr_str = current.system_prompt.as_deref().unwrap_or("");
            let diff_offset = find_first_char_diff(prev_str, curr_str);
            return PrefixDivergence::SystemPromptChanged {
                diff_offset,
                prev_len: prev_str.len(),
                curr_len: curr_str.len(),
            };
        }

        // 2. Check tools
        if prev.tools != current.tools {
            let diff = describe_tools_diff(&prev.tools, &current.tools);
            return PrefixDivergence::ToolsChanged { diff };
        }

        // 3. Check items prefix
        let min_items = prev.items.len().min(current.items.len());
        for i in 0..min_items {
            let prev_item = &prev.items[i];
            let curr_item = &current.items[i];

            if prev_item.content_hash != curr_item.content_hash {
                // Determine the divergence reason
                let (reason, diagnostic) = if prev_item.kind != curr_item.kind {
                    (
                        ItemDivergenceReason::VariantChanged,
                        format!("Changed from '{}' to '{}'", prev_item.kind, curr_item.kind),
                    )
                } else if !prev_item.is_pruned && curr_item.is_pruned {
                    (
                        ItemDivergenceReason::Pruned,
                        format!(
                            "Tool output pruned from {} bytes to {} bytes ('{}')",
                            prev_item.byte_len, curr_item.byte_len, curr_item.preview
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
                            "Length changed from {} to {} bytes (preview: '{}')",
                            prev_item.byte_len, curr_item.byte_len, curr_item.preview
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

        // 4. Check if history was truncated
        if current.items.len() < prev.items.len() {
            return PrefixDivergence::HistoryTruncated {
                prev_count: prev.items.len(),
                curr_count: current.items.len(),
            };
        }

        // 5. Prefix is completely intact
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

        // Determine CacheStatus
        let status = if self.previous_request_summary.is_none() {
            CacheStatus::FirstTurn
        } else if cached_prompt_tokens > 0 {
            if hit_rate_pct >= 50.0 {
                CacheStatus::Hit
            } else {
                CacheStatus::PartialHit
            }
        } else if prompt_tokens > 0 && divergence.is_intact() {
            // Prefix intact but 0 cached tokens reported
            CacheStatus::NoCacheSupport
        } else {
            CacheStatus::Break
        };

        let diagnostic = match status {
            CacheStatus::FirstTurn => "First turn in session (cold cache).".to_string(),
            CacheStatus::Hit => {
                let mut d = format!(
                    "Cache hit: {hit_rate_pct:.1}% ({cached_prompt_tokens}/{prompt_tokens} tokens cached)."
                );
                // A dip with an intact prefix is expected: the uncached remainder is
                // content appended since the previous request, not an invalidation.
                if hit_rate_pct < 90.0 && divergence.is_intact() {
                    d.push_str(
                        " Remaining tokens are new content appended since the previous request.",
                    );
                }
                d
            }
            CacheStatus::PartialHit => format!(
                "Partial cache hit: {hit_rate_pct:.1}% ({cached_prompt_tokens}/{prompt_tokens} tokens cached). {}",
                divergence.summary_diagnostic()
            ),
            CacheStatus::Break => format!("Cache break: 0% hit rate. {}", divergence.summary_diagnostic()),
            CacheStatus::NoCacheSupport => "0 cached tokens reported (provider may not support prompt caching or cache expired).".to_string(),
        };

        // Update running totals
        self.summary.total_turns += 1;
        self.summary.total_input_tokens += prompt_tokens as u64;
        self.summary.total_cached_tokens += cached_prompt_tokens as u64;
        if status != CacheStatus::FirstTurn {
            self.summary.steady_input_tokens += prompt_tokens as u64;
            self.summary.steady_cached_tokens += cached_prompt_tokens as u64;
        }
        // Overall hit rate covers steady-state turns only: the cold first turn
        // cannot hit by definition and would permanently dilute the rate.
        if self.summary.steady_input_tokens > 0 {
            self.summary.overall_hit_rate_pct = (self.summary.steady_cached_tokens as f64
                / self.summary.steady_input_tokens as f64)
                * 100.0;
        }

        match status {
            CacheStatus::Hit => self.summary.hits += 1,
            CacheStatus::PartialHit => self.summary.partial_hits += 1,
            CacheStatus::Break => {
                self.summary.breaks += 1;
                self.summary.last_break_diagnostic = Some(diagnostic.clone());
            }
            _ => {}
        }

        // Emit telemetry log
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

        // Update previous summary
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

fn hash_item(item: &ConversationItem) -> u64 {
    let mut hasher = DefaultHasher::new();
    match item {
        ConversationItem::System(s) => {
            0u8.hash(&mut hasher);
            s.content.hash(&mut hasher);
        }
        ConversationItem::User(u) => {
            1u8.hash(&mut hasher);
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
            serde_json::to_string(btc).unwrap_or_default().hash(&mut hasher);
        }
        ConversationItem::Reasoning(r) => {
            6u8.hash(&mut hasher);
            serde_json::to_string(r).unwrap_or_default().hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn summarize_item(
    item: &ConversationItem,
) -> (String, Option<String>, usize, bool, bool, String) {
    match item {
        ConversationItem::System(s) => {
            let len = s.content.len();
            let preview = truncate_preview(&s.content, 40);
            ("system".into(), None, len, false, false, preview)
        }
        ConversationItem::User(u) => {
            let mut len = 0;
            let mut has_images = false;
            let mut text_buf = String::new();
            for part in &u.content {
                match part {
                    ContentPart::Text { text } => {
                        len += text.len();
                        if text_buf.len() < 40 {
                            text_buf.push_str(text);
                        }
                    }
                    ContentPart::Image { url } => {
                        len += url.len();
                        has_images = true;
                    }
                }
            }
            let preview = truncate_preview(&text_buf, 40);
            ("user".into(), None, len, false, has_images, preview)
        }
        ConversationItem::Assistant(a) => {
            let text_len = a.content.len();
            let tools_len = a
                .tool_calls
                .iter()
                .map(|c| c.name.len() + c.arguments.len())
                .sum::<usize>();
            let len = text_len + tools_len;
            let id = a.tool_calls.first().map(|c| c.name.clone());
            let preview = if let Some(ref first) = a.tool_calls.first() {
                format!("calls '{}' ({} args)", first.name, first.arguments.len())
            } else {
                truncate_preview(&a.content, 40)
            };
            ("assistant".into(), id, len, false, false, preview)
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
            let preview = truncate_preview(&tr.content, 40);
            (
                "tool_result".into(),
                Some(tr.tool_call_id.clone()),
                len,
                is_pruned,
                has_images,
                preview,
            )
        }
        ConversationItem::CustomToolOutput(co) => {
            let mut len = 0;
            let mut has_images = false;
            let mut text_buf = String::new();
            for block in &co.content {
                match block {
                    CustomToolOutputContent::Text { text } => {
                        len += text.len();
                        if text_buf.len() < 40 {
                            text_buf.push_str(text);
                        }
                    }
                    CustomToolOutputContent::Image { url, .. } => {
                        len += url.len();
                        has_images = true;
                    }
                }
            }
            let preview = truncate_preview(&text_buf, 40);
            (
                "custom_tool_output".into(),
                Some(co.call_id.clone()),
                len,
                false,
                has_images,
                preview,
            )
        }
        ConversationItem::BackendToolCall(btc) => {
            let s = serde_json::to_string(btc).unwrap_or_default();
            let preview = truncate_preview(&s, 40);
            ("backend_tool_call".into(), None, s.len(), false, false, preview)
        }
        ConversationItem::Reasoning(r) => {
            let s = serde_json::to_string(r).unwrap_or_default();
            let preview = truncate_preview(&s, 40);
            ("reasoning".into(), None, s.len(), false, false, preview)
        }
    }
}

fn truncate_preview(text: &str, max_chars: usize) -> String {
    let single_line: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if single_line.chars().count() <= max_chars {
        single_line
    } else {
        let truncated: String = single_line.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn find_first_char_diff(a: &str, b: &str) -> usize {
    a.chars()
        .zip(b.chars())
        .take_while(|(ca, cb)| ca == cb)
        .count()
}

fn describe_tools_diff(prev: &[ToolSummary], curr: &[ToolSummary]) -> String {
    let prev_names: Vec<&str> = prev.iter().map(|t| t.name.as_str()).collect();
    let curr_names: Vec<&str> = curr.iter().map(|t| t.name.as_str()).collect();

    let added: Vec<&str> = curr_names
        .iter()
        .copied()
        .filter(|n| !prev_names.contains(n))
        .collect();
    let removed: Vec<&str> = prev_names
        .iter()
        .copied()
        .filter(|n| !curr_names.contains(n))
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
        "tools reordered".to_string()
    } else {
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use xai_grok_sampling_types::{AssistantItem, SystemItem, ToolResultItem};

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
                ..
            } => {
                assert_eq!(index, 1);
                assert_eq!(kind, "tool_result");
                assert_eq!(identifier.as_deref(), Some("call_1"));
                assert_eq!(reason, ItemDivergenceReason::Pruned);
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

        // Turn 1
        let req1 = ConversationRequest {
            items: vec![ConversationItem::user("turn 1")],
            ..Default::default()
        };
        let sum1 = CacheTracker::summarize_request(&req1);
        let rec1 = tracker.record_turn_outcome(None, "1", 0, 1000, 0, 100, sum1);
        assert_eq!(rec1.status, CacheStatus::FirstTurn);
        assert_eq!(rec1.cache_hit_rate_pct, 0.0);

        // Turn 2 (Hit)
        let req2 = ConversationRequest {
            items: vec![
                ConversationItem::user("turn 1"),
                ConversationItem::user("turn 2"),
            ],
            ..Default::default()
        };
        let sum2 = CacheTracker::summarize_request(&req2);
        let rec2 = tracker.record_turn_outcome(None, "2", 1, 1500, 1000, 150, sum2);
        assert_eq!(rec2.status, CacheStatus::Hit);
        assert!((rec2.cache_hit_rate_pct - 66.66).abs() < 0.1);

        // Summary
        let summary = tracker.summary();
        assert_eq!(summary.total_turns, 2);
        assert_eq!(summary.total_input_tokens, 2500);
        assert_eq!(summary.total_cached_tokens, 1000);
        // Steady-state excludes the cold first turn.
        assert_eq!(summary.steady_input_tokens, 1500);
        assert_eq!(summary.steady_cached_tokens, 1000);
        assert_eq!(summary.hits, 1);
        assert_eq!(summary.breaks, 0);
        assert!((summary.overall_hit_rate_pct - 66.7).abs() < 0.1);
    }

    #[test]
    fn test_first_turn_only_summary_has_no_steady_state() {
        let mut tracker = CacheTracker::new();
        let req = ConversationRequest {
            items: vec![ConversationItem::user("turn 1")],
            ..Default::default()
        };
        let sum = CacheTracker::summarize_request(&req);
        let rec = tracker.record_turn_outcome(None, "1", 0, 1000, 0, 100, sum);
        assert_eq!(rec.status, CacheStatus::FirstTurn);

        let summary = tracker.summary();
        assert_eq!(summary.total_turns, 1);
        assert_eq!(summary.total_input_tokens, 1000);
        assert_eq!(summary.steady_input_tokens, 0);
        assert_eq!(summary.steady_cached_tokens, 0);
        assert_eq!(summary.overall_hit_rate_pct, 0.0);
    }

    #[test]
    fn test_hit_dip_with_intact_prefix_explains_new_content() {
        let mut tracker = CacheTracker::new();

        let req1 = ConversationRequest {
            items: vec![ConversationItem::user("turn 1")],
            ..Default::default()
        };
        tracker.record_turn_outcome(
            None,
            "1",
            0,
            1000,
            0,
            100,
            CacheTracker::summarize_request(&req1),
        );

        // Turn 2: large new append, half cached — Hit status at 50%.
        let req2 = ConversationRequest {
            items: vec![
                ConversationItem::user("turn 1"),
                ConversationItem::user("turn 2 with a large new append"),
            ],
            ..Default::default()
        };
        let rec2 = tracker.record_turn_outcome(
            None,
            "2",
            1,
            2000,
            1000,
            100,
            CacheTracker::summarize_request(&req2),
        );
        assert_eq!(rec2.status, CacheStatus::Hit);
        assert!(
            rec2.diagnostic.contains("new content appended"),
            "got: {}",
            rec2.diagnostic
        );
    }
}
