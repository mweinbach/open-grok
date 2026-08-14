//! Prompt-cache hit rate and prefix-break diagnostics.
//!
//! Provider KV caches are prefix-addressed. A rewrite of an early request
//! section (model, tools, system prompt, an earlier message) drops the hit
//! rate even when later tokens are new. This module fingerprints those
//! sections, diffs them across calls, and reports where the prefix first
//! changed — without logging prompt text.

use std::collections::VecDeque;
use std::hash::Hasher;

use serde::{Deserialize, Serialize};

use crate::conversation::{
    ContentPart, ConversationItem, ConversationRequest, CustomToolOutputContent, HostedTool,
    SyntheticReason, TokenUsage, ToolSpec,
};

/// Minimum prompt size before a 0% hit on a stable prefix is treated as a
/// provider miss (routing / TTL / key not applied) rather than "too small
/// to cache".
pub const PROVIDER_MISS_MIN_PROMPT_TOKENS: u64 = 1_024;

const MAX_RECENT_BREAKS: usize = 16;

/// `cached / prompt` as a percentage in `[0, 100]`. `None` when `prompt` is 0.
pub fn cache_hit_rate(cached_tokens: u64, prompt_tokens: u64) -> Option<f64> {
    (prompt_tokens > 0).then(|| (cached_tokens as f64 / prompt_tokens as f64) * 100.0)
}

/// Human-readable hit rate (`"72.4%"` or `"n/a"`).
pub fn format_cache_hit_rate(rate: Option<f64>) -> String {
    match rate {
        Some(rate) => format!("{rate:.1}%"),
        None => "n/a".to_string(),
    }
}

/// Kind of a prefix-stable request section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheSectionKind {
    Model,
    ReasoningEffort,
    Temperature,
    PromptCacheKey,
    ToolChoice,
    JsonSchema,
    Tools,
    HostedTools,
    Item,
}

/// One ordered prefix section and its content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheSection {
    pub kind: PromptCacheSectionKind,
    /// Stable label such as `tools` or `item[3]:user`. Never prompt text.
    pub label: String,
    pub hash: u64,
}

/// Ordered fingerprint of the request prefix that the KV cache keys on.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PromptCacheFingerprint {
    pub sections: Vec<PromptCacheSection>,
}

impl PromptCacheFingerprint {
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

/// How the current request prefix compares to the previous one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CachePrefixDiff {
    /// First request in this process — no prior prefix to compare.
    ColdStart,
    /// Every previous section is unchanged and no new sections were added.
    Identical,
    /// Previous sections are a prefix of the current request (new suffix only).
    PrefixExtended { shared_sections: usize },
    /// Current request is a strict prefix of the previous one (rewind / trim).
    /// Remaining prefix is intact, so this is not counted as a cache break.
    Shortened { at: usize, section: String },
    /// An earlier section changed. This is the first place the KV cache breaks.
    Broke {
        at: usize,
        section: String,
        previous_label: String,
        previous_hash: String,
        current_label: String,
        current_hash: String,
    },
}

impl CachePrefixDiff {
    pub fn is_break(&self) -> bool {
        matches!(self, Self::Broke { .. })
    }

    pub fn break_section(&self) -> Option<&str> {
        match self {
            Self::Broke { section, .. } => Some(section.as_str()),
            _ => None,
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Self::ColdStart => "cold start".to_string(),
            Self::Identical => "intact".to_string(),
            Self::PrefixExtended { shared_sections } => {
                format!("extended ({shared_sections} shared sections)")
            }
            Self::Shortened { section, .. } => format!("shortened after {section}"),
            Self::Broke {
                section,
                previous_label,
                previous_hash,
                current_label,
                current_hash,
                ..
            } => {
                if previous_label == current_label {
                    format!("broke at {section} ({previous_hash} → {current_hash})")
                } else {
                    format!(
                        "broke at {section} (was {previous_label} {previous_hash}, now {current_label} {current_hash})"
                    )
                }
            }
        }
    }
}

/// One recorded prefix rewrite, for `/cache` and logs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheBreak {
    pub call_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loop_index: Option<u32>,
    pub section: String,
    pub previous_label: String,
    pub previous_hash: String,
    pub current_label: String,
    pub current_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_rate_percent: Option<f64>,
}

/// Outcome of folding one model call's usage into the tracker.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheCallOutcome {
    pub diff: CachePrefixDiff,
    pub hit_rate: Option<f64>,
    pub cached_tokens: u64,
    pub prompt_tokens: u64,
    /// Stable prefix, prompt large enough to cache, but the provider reported
    /// zero cached tokens.
    pub provider_miss: bool,
}

/// Session-scoped prompt-cache accumulator.
#[derive(Debug, Clone, Default)]
pub struct PromptCacheTracker {
    last: Option<PromptCacheFingerprint>,
    last_diff: Option<CachePrefixDiff>,
    pending_break: Option<CacheBreak>,
    calls: u64,
    prompt_tokens: u64,
    cached_tokens: u64,
    cache_creation_tokens: u64,
    prefix_breaks: u64,
    provider_misses: u64,
    last_call_hit_rate: Option<f64>,
    last_call_cached: u64,
    last_call_prompt: u64,
    last_call_cache_key_forwarded: Option<bool>,
    breaks: VecDeque<CacheBreak>,
}

impl PromptCacheTracker {
    /// Fingerprint `request` and compare it to the previous main-turn prefix.
    /// Auxiliary calls must not use this — they would poison the next turn.
    pub fn record_request(&mut self, request: &ConversationRequest) -> CachePrefixDiff {
        let current = fingerprint_request(request);
        let diff = match self.last.as_ref() {
            None => CachePrefixDiff::ColdStart,
            Some(previous) => diff_fingerprints(previous, &current),
        };
        if let CachePrefixDiff::Broke {
            section,
            previous_label,
            previous_hash,
            current_label,
            current_hash,
            ..
        } = &diff
        {
            self.pending_break = Some(CacheBreak {
                call_kind: String::new(),
                loop_index: None,
                section: section.clone(),
                previous_label: previous_label.clone(),
                previous_hash: previous_hash.clone(),
                current_label: current_label.clone(),
                current_hash: current_hash.clone(),
                hit_rate_percent: None,
            });
        } else {
            self.pending_break = None;
        }
        self.last = Some(current);
        self.last_diff = Some(diff.clone());
        diff
    }

    /// Fold provider-reported cache usage for the request last passed to
    /// [`Self::record_request`].
    pub fn record_usage(
        &mut self,
        usage: &TokenUsage,
        cache_key_forwarded: bool,
        call_kind: &str,
        loop_index: Option<u32>,
    ) -> CacheCallOutcome {
        let prompt_tokens = u64::from(usage.prompt_tokens);
        let cached_tokens = u64::from(usage.cached_prompt_tokens);
        let hit_rate = cache_hit_rate(cached_tokens, prompt_tokens);
        let diff = self
            .last_diff
            .clone()
            .unwrap_or(CachePrefixDiff::ColdStart);
        let prefix_stable = matches!(
            diff,
            CachePrefixDiff::Identical | CachePrefixDiff::PrefixExtended { .. }
        );
        let provider_miss = prefix_stable
            && cache_key_forwarded
            && prompt_tokens >= PROVIDER_MISS_MIN_PROMPT_TOKENS
            && cached_tokens == 0;

        self.calls = self.calls.saturating_add(1);
        self.prompt_tokens = self.prompt_tokens.saturating_add(prompt_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(cached_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(u64::from(usage.cache_creation_prompt_tokens));
        self.last_call_hit_rate = hit_rate;
        self.last_call_cached = cached_tokens;
        self.last_call_prompt = prompt_tokens;
        self.last_call_cache_key_forwarded = Some(cache_key_forwarded);

        if let Some(mut brk) = self.pending_break.take() {
            brk.call_kind = call_kind.to_string();
            brk.loop_index = loop_index;
            brk.hit_rate_percent = hit_rate;
            self.prefix_breaks = self.prefix_breaks.saturating_add(1);
            push_break(&mut self.breaks, brk);
        }
        if provider_miss {
            self.provider_misses = self.provider_misses.saturating_add(1);
        }

        CacheCallOutcome {
            diff,
            hit_rate,
            cached_tokens,
            prompt_tokens,
            provider_miss,
        }
    }

    pub fn snapshot(&self) -> PromptCacheReport {
        PromptCacheReport {
            session_hit_rate_percent: cache_hit_rate(self.cached_tokens, self.prompt_tokens),
            last_call_hit_rate_percent: self.last_call_hit_rate,
            calls: self.calls,
            prompt_tokens: self.prompt_tokens,
            cached_tokens: self.cached_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            prefix_breaks: self.prefix_breaks,
            provider_misses: self.provider_misses,
            last_call_cached_tokens: self.last_call_cached,
            last_call_prompt_tokens: self.last_call_prompt,
            last_call_cache_key_forwarded: self.last_call_cache_key_forwarded,
            last_prefix: self.last_diff.as_ref().map(CachePrefixDiff::summary),
            recent_breaks: self.breaks.iter().cloned().collect(),
        }
    }
}

fn push_break(breaks: &mut VecDeque<CacheBreak>, brk: CacheBreak) {
    if breaks.len() == MAX_RECENT_BREAKS {
        breaks.pop_front();
    }
    breaks.push_back(brk);
}

/// Wire snapshot for `x.ai/session/cache` and `/cache`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCacheReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_hit_rate_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_call_hit_rate_percent: Option<f64>,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub cache_creation_tokens: u64,
    pub prefix_breaks: u64,
    pub provider_misses: u64,
    pub last_call_cached_tokens: u64,
    pub last_call_prompt_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_call_cache_key_forwarded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_prefix: Option<String>,
    pub recent_breaks: Vec<CacheBreak>,
}

impl PromptCacheReport {
    /// Human-readable `/cache` body. Contains labels and hashes only.
    pub fn format_report(&self) -> String {
        if self.calls == 0 {
            return "Prompt cache: no model calls yet in this session.\n\
                    Prefix tracking starts on the first inference."
                .to_string();
        }

        let mut lines = vec![
            "Prompt cache (this session):".to_string(),
            format!(
                "  Session hit rate:  {}  ({} / {})",
                format_cache_hit_rate(self.session_hit_rate_percent),
                group_thousands(self.cached_tokens),
                group_thousands(self.prompt_tokens),
            ),
            format!(
                "  Last call:         {}  ({} / {})",
                format_cache_hit_rate(self.last_call_hit_rate_percent),
                group_thousands(self.last_call_cached_tokens),
                group_thousands(self.last_call_prompt_tokens),
            ),
            format!("  Model calls:       {}", group_thousands(self.calls)),
            format!("  Prefix breaks:     {}", group_thousands(self.prefix_breaks)),
            format!(
                "  Provider misses:   {}  (stable prefix, 0% hit, prompt ≥ {})",
                group_thousands(self.provider_misses),
                group_thousands(PROVIDER_MISS_MIN_PROMPT_TOKENS),
            ),
        ];
        if self.cache_creation_tokens > 0 {
            lines.push(format!(
                "  Cache writes:      {}",
                group_thousands(self.cache_creation_tokens)
            ));
        }
        if let Some(forwarded) = self.last_call_cache_key_forwarded {
            lines.push(format!(
                "  Cache key on wire: {}",
                if forwarded { "yes" } else { "no (backend does not send it)" }
            ));
        }
        if let Some(prefix) = &self.last_prefix {
            lines.push(format!("  Last prefix:       {prefix}"));
        }
        if !self.recent_breaks.is_empty() {
            lines.push("  Recent breaks:".to_string());
            for (i, brk) in self.recent_breaks.iter().enumerate() {
                let hit = format_cache_hit_rate(brk.hit_rate_percent);
                let loop_bit = brk
                    .loop_index
                    .map(|n| format!(" loop {n}"))
                    .unwrap_or_default();
                lines.push(format!(
                    "    {}. {}{}  {}  ({} → {})  hit {hit}",
                    i + 1,
                    brk.section,
                    loop_bit,
                    brk.call_kind,
                    brk.previous_hash,
                    brk.current_hash,
                ));
            }
        }
        lines.push(
            "  Logs: search for shell.turn.prompt_cache_break / shell.turn.prompt_cache".to_string(),
        );
        lines.join("\n")
    }
}

/// Fingerprint the prefix-stable parts of a conversation request.
pub fn fingerprint_request(request: &ConversationRequest) -> PromptCacheFingerprint {
    let mut sections = Vec::with_capacity(8 + request.items.len());
    push_section(
        &mut sections,
        PromptCacheSectionKind::Model,
        "model",
        hash_opt_str(request.model.as_deref()),
    );
    push_section(
        &mut sections,
        PromptCacheSectionKind::ReasoningEffort,
        "reasoning_effort",
        hash_opt_str(request.reasoning_effort.map(|e| e.as_str())),
    );
    push_section(
        &mut sections,
        PromptCacheSectionKind::Temperature,
        "temperature",
        hash_opt_f32(request.temperature),
    );
    push_section(
        &mut sections,
        PromptCacheSectionKind::PromptCacheKey,
        "prompt_cache_key",
        hash_opt_str(request.prompt_cache_key.as_deref()),
    );
    push_section(
        &mut sections,
        PromptCacheSectionKind::ToolChoice,
        "tool_choice",
        hash_json(&request.tool_choice),
    );
    push_section(
        &mut sections,
        PromptCacheSectionKind::JsonSchema,
        "json_schema",
        hash_json(&request.json_schema),
    );
    push_section(
        &mut sections,
        PromptCacheSectionKind::Tools,
        "tools",
        hash_tools(&request.tools),
    );
    push_section(
        &mut sections,
        PromptCacheSectionKind::HostedTools,
        "hosted_tools",
        hash_hosted_tools(&request.hosted_tools),
    );
    for (index, item) in request.items.iter().enumerate() {
        let (kind_label, hash) = hash_item(item);
        push_section(
            &mut sections,
            PromptCacheSectionKind::Item,
            format!("item[{index}]:{kind_label}"),
            hash,
        );
    }
    PromptCacheFingerprint { sections }
}

/// First differing section, or an expected grow/shrink of an intact prefix.
pub fn diff_fingerprints(
    previous: &PromptCacheFingerprint,
    current: &PromptCacheFingerprint,
) -> CachePrefixDiff {
    let shared = previous.sections.len().min(current.sections.len());
    for i in 0..shared {
        let prev = &previous.sections[i];
        let curr = &current.sections[i];
        if prev.hash != curr.hash || prev.label != curr.label {
            return CachePrefixDiff::Broke {
                at: i,
                section: curr.label.clone(),
                previous_label: prev.label.clone(),
                previous_hash: format_hash(prev.hash),
                current_label: curr.label.clone(),
                current_hash: format_hash(curr.hash),
            };
        }
    }
    match previous.sections.len().cmp(&current.sections.len()) {
        std::cmp::Ordering::Equal => CachePrefixDiff::Identical,
        std::cmp::Ordering::Less => CachePrefixDiff::PrefixExtended {
            shared_sections: shared,
        },
        std::cmp::Ordering::Greater => CachePrefixDiff::Shortened {
            at: shared,
            section: previous
                .sections
                .get(shared)
                .map(|s| s.label.clone())
                .unwrap_or_else(|| "end".to_string()),
        },
    }
}

fn push_section(
    sections: &mut Vec<PromptCacheSection>,
    kind: PromptCacheSectionKind,
    label: impl Into<String>,
    hash: u64,
) {
    sections.push(PromptCacheSection {
        kind,
        label: label.into(),
        hash,
    });
}

fn format_hash(hash: u64) -> String {
    format!("{hash:016x}")
}

fn group_thousands(n: u64) -> String {
    let raw = n.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (i, ch) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn hash_finish(hasher: impl Hasher) -> u64 {
    hasher.finish()
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(bytes);
    hash_finish(hasher)
}

fn hash_opt_str(value: Option<&str>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match value {
        Some(s) => {
            hasher.write_u8(1);
            hasher.write(s.as_bytes());
        }
        None => hasher.write_u8(0),
    }
    hash_finish(hasher)
}

fn hash_opt_f32(value: Option<f32>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match value {
        Some(v) => {
            hasher.write_u8(1);
            hasher.write_u32(v.to_bits());
        }
        None => hasher.write_u8(0),
    }
    hash_finish(hasher)
}

fn hash_json<T: Serialize>(value: &T) -> u64 {
    match serde_json::to_vec(value) {
        Ok(bytes) => hash_bytes(&bytes),
        Err(_) => 0,
    }
}

fn hash_tools(tools: &[ToolSpec]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write_usize(tools.len());
    for tool in tools {
        hasher.write(tool.name.as_bytes());
        hasher.write_u8(0);
        if let Some(description) = tool.description.as_deref() {
            hasher.write(description.as_bytes());
        }
        hasher.write_u8(0);
        if let Ok(params) = serde_json::to_vec(&tool.parameters) {
            hasher.write(&params);
        }
        hasher.write_u8(0xff);
    }
    hash_finish(hasher)
}

fn hash_hosted_tools(tools: &[HostedTool]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write_usize(tools.len());
    for tool in tools {
        hasher.write(tool.wire_name().as_bytes());
        hasher.write_u8(0);
        match tool {
            HostedTool::WebSearch { options } => {
                if let Ok(bytes) = serde_json::to_vec(options) {
                    hasher.write(&bytes);
                }
            }
            HostedTool::XSearch { options } => {
                if let Ok(bytes) = serde_json::to_vec(options) {
                    hasher.write(&bytes);
                }
            }
            HostedTool::ClientCustom(spec) => {
                if let Ok(bytes) = serde_json::to_vec(spec) {
                    hasher.write(&bytes);
                }
            }
        }
        hasher.write_u8(0xff);
    }
    hash_finish(hasher)
}

fn hash_item(item: &ConversationItem) -> (&'static str, u64) {
    match item {
        ConversationItem::System(sys) => ("system", hash_bytes(sys.content.as_bytes())),
        ConversationItem::User(user) => {
            let label = match user.synthetic_reason.as_ref() {
                Some(SyntheticReason::ProjectInstructions) => "user:project_instructions",
                Some(SyntheticReason::SystemReminder) => "user:system_reminder",
                Some(SyntheticReason::CompactionMeta) => "user:compaction_meta",
                Some(SyntheticReason::AutoContinue) => "user:auto_continue",
                Some(SyntheticReason::AutoRecovery) => "user:auto_recovery",
                Some(SyntheticReason::Interjection) => "user:interjection",
                Some(SyntheticReason::TaskCompleted) => "user:task_completed",
                Some(SyntheticReason::SubagentCompleted) => "user:subagent_completed",
                Some(SyntheticReason::NotificationDrain) => "user:notification_drain",
                Some(SyntheticReason::GoalSummary) => "user:goal_summary",
                Some(SyntheticReason::GoalClassifierNudge) => "user:goal_classifier_nudge",
                Some(SyntheticReason::SchedulerFired) => "user:scheduler_fired",
                Some(SyntheticReason::AgentMessage) => "user:agent_message",
                Some(SyntheticReason::StopHookFeedback) => "user:stop_hook_feedback",
                Some(SyntheticReason::WorkingDirectorySwitch) => "user:cwd_switch",
                Some(SyntheticReason::Unknown) => "user:synthetic",
                None => "user",
            };
            (label, hash_content_parts(&user.content))
        }
        ConversationItem::Assistant(asst) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            hasher.write(asst.content.as_bytes());
            hasher.write_u8(0);
            hasher.write_usize(asst.tool_calls.len());
            for call in &asst.tool_calls {
                hasher.write(call.id.as_bytes());
                hasher.write(call.name.as_bytes());
                hasher.write(call.arguments.as_bytes());
            }
            ("assistant", hash_finish(hasher))
        }
        ConversationItem::ToolResult(result) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            hasher.write(result.tool_call_id.as_bytes());
            hasher.write(result.content.as_bytes());
            hasher.write_u8(0);
            hash_content_parts_into(&mut hasher, &result.images);
            for part in &result.ordered_content {
                hash_custom_output_into(&mut hasher, part);
            }
            ("tool_result", hash_finish(hasher))
        }
        ConversationItem::CustomToolOutput(output) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            hasher.write(output.call_id.as_bytes());
            if let Some(name) = output.name.as_deref() {
                hasher.write(name.as_bytes());
            }
            for part in &output.content {
                hash_custom_output_into(&mut hasher, part);
            }
            ("custom_tool_output", hash_finish(hasher))
        }
        ConversationItem::BackendToolCall(call) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            hasher.write(call.id().as_bytes());
            hasher.write(call.text_summary().as_bytes());
            ("backend_tool_call", hash_finish(hasher))
        }
        ConversationItem::Reasoning(reasoning) => ("reasoning", hash_json(reasoning)),
    }
}

fn hash_content_parts(parts: &[ContentPart]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_content_parts_into(&mut hasher, parts);
    hash_finish(hasher)
}

fn hash_content_parts_into(hasher: &mut std::collections::hash_map::DefaultHasher, parts: &[ContentPart]) {
    hasher.write_usize(parts.len());
    for part in parts {
        match part {
            ContentPart::Text { text } => {
                hasher.write_u8(1);
                hasher.write(text.as_bytes());
            }
            ContentPart::Image { url } => {
                hasher.write_u8(2);
                hasher.write(url.as_bytes());
            }
        }
    }
}

fn hash_custom_output_into(
    hasher: &mut std::collections::hash_map::DefaultHasher,
    part: &CustomToolOutputContent,
) {
    match part {
        CustomToolOutputContent::Text { text } => {
            hasher.write_u8(1);
            hasher.write(text.as_bytes());
        }
        CustomToolOutputContent::Image { url, detail } => {
            hasher.write_u8(2);
            hasher.write(url.as_bytes());
            hasher.write_u8(*detail as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{AssistantItem, ConversationItem, SystemItem, UserItem};
    use std::sync::Arc;

    fn usage(prompt: u32, cached: u32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: 10,
            total_tokens: prompt + 10,
            reasoning_tokens: 0,
            cached_prompt_tokens: cached,
            cache_creation_prompt_tokens: 0,
        }
    }

    fn request_with_items(items: Vec<ConversationItem>) -> ConversationRequest {
        ConversationRequest {
            items,
            model: Some("grok-4".to_string()),
            prompt_cache_key: Some("sess-1".to_string()),
            ..ConversationRequest::default()
        }
    }

    fn system(text: &str) -> ConversationItem {
        ConversationItem::System(SystemItem {
            content: Arc::<str>::from(text),
        })
    }

    fn user(text: &str) -> ConversationItem {
        ConversationItem::user(text)
    }

    fn assistant(text: &str) -> ConversationItem {
        ConversationItem::Assistant(AssistantItem {
            content: Arc::<str>::from(text),
            tool_calls: vec![],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        })
    }

    #[test]
    fn hit_rate_is_none_when_prompt_is_zero() {
        assert_eq!(cache_hit_rate(0, 0), None);
        assert_eq!(format_cache_hit_rate(None), "n/a");
    }

    #[test]
    fn hit_rate_covers_zero_full_and_partial() {
        assert_eq!(cache_hit_rate(0, 100), Some(0.0));
        assert_eq!(cache_hit_rate(100, 100), Some(100.0));
        let partial = cache_hit_rate(1_000_000, 1_234_567).expect("rate");
        assert!((partial - 81.000_072).abs() < 0.001);
        assert_eq!(format_cache_hit_rate(Some(72.41)), "72.4%");
    }

    #[test]
    fn fingerprint_is_stable_for_identical_requests() {
        let req = request_with_items(vec![system("sys"), user("hi")]);
        assert_eq!(fingerprint_request(&req), fingerprint_request(&req));
    }

    #[test]
    fn system_rewrite_breaks_at_first_item() {
        let prev = fingerprint_request(&request_with_items(vec![system("sys-a"), user("hi")]));
        let curr = fingerprint_request(&request_with_items(vec![system("sys-b"), user("hi")]));
        match diff_fingerprints(&prev, &curr) {
            CachePrefixDiff::Broke { section, .. } => {
                assert_eq!(section, "item[0]:system");
            }
            other => panic!("expected break, got {other:?}"),
        }
    }

    #[test]
    fn tool_rewrite_breaks_before_items() {
        let mut prev_req = request_with_items(vec![system("sys")]);
        prev_req.tools = vec![ToolSpec {
            name: "bash".to_string(),
            description: Some("run".to_string()),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let mut curr_req = prev_req.clone();
        curr_req.tools[0].description = Some("run a command".to_string());
        match diff_fingerprints(&fingerprint_request(&prev_req), &fingerprint_request(&curr_req))
        {
            CachePrefixDiff::Broke { section, .. } => assert_eq!(section, "tools"),
            other => panic!("expected tools break, got {other:?}"),
        }
    }

    #[test]
    fn appended_user_turn_is_prefix_extended() {
        let prev = fingerprint_request(&request_with_items(vec![system("sys"), user("hi")]));
        let curr = fingerprint_request(&request_with_items(vec![
            system("sys"),
            user("hi"),
            assistant("hello"),
            user("again"),
        ]));
        assert_eq!(
            diff_fingerprints(&prev, &curr),
            CachePrefixDiff::PrefixExtended { shared_sections: 10 }
        );
    }

    #[test]
    fn rewind_is_shortened_not_a_break() {
        let prev = fingerprint_request(&request_with_items(vec![
            system("sys"),
            user("hi"),
            assistant("hello"),
        ]));
        let curr = fingerprint_request(&request_with_items(vec![system("sys"), user("hi")]));
        match diff_fingerprints(&prev, &curr) {
            CachePrefixDiff::Shortened { .. } => {}
            other => panic!("expected shortened, got {other:?}"),
        }
        assert!(!diff_fingerprints(&prev, &curr).is_break());
    }

    #[test]
    fn tracker_session_hit_rate_and_break_history() {
        let mut tracker = PromptCacheTracker::default();
        let mut req = request_with_items(vec![system("sys"), user("hi")]);
        let cold = tracker.record_request(&req);
        assert_eq!(cold, CachePrefixDiff::ColdStart);
        let outcome = tracker.record_usage(&usage(2_000, 0), true, "turn", Some(0));
        assert!(!outcome.provider_miss, "cold start is not a provider miss");

        req.items.push(assistant("ok"));
        req.items.push(user("next"));
        let extended = tracker.record_request(&req);
        assert!(matches!(extended, CachePrefixDiff::PrefixExtended { .. }));
        let hit = tracker.record_usage(&usage(3_000, 2_000), true, "turn", Some(1));
        assert!(!hit.provider_miss);
        assert_eq!(hit.hit_rate, Some(2_000.0 / 3_000.0 * 100.0));

        req.items[0] = system("rewritten");
        let broke = tracker.record_request(&req);
        assert!(broke.is_break());
        tracker.record_usage(&usage(3_000, 100), true, "turn", Some(2));

        let report = tracker.snapshot();
        assert_eq!(report.calls, 3);
        assert_eq!(report.prefix_breaks, 1);
        assert_eq!(report.prompt_tokens, 8_000);
        assert_eq!(report.cached_tokens, 2_100);
        assert_eq!(report.recent_breaks.len(), 1);
        assert_eq!(report.recent_breaks[0].section, "item[0]:system");
        assert!(report.format_report().contains("Session hit rate"));
        assert!(report.format_report().contains("item[0]:system"));
    }

    #[test]
    fn stable_prefix_zero_hit_on_large_prompt_is_provider_miss() {
        let mut tracker = PromptCacheTracker::default();
        let req = request_with_items(vec![system("sys"), user("hi")]);
        tracker.record_request(&req);
        tracker.record_usage(&usage(2_000, 1_500), true, "turn", Some(0));

        let mut next = req;
        next.items.push(assistant("ok"));
        tracker.record_request(&next);
        let miss = tracker.record_usage(&usage(2_500, 0), true, "turn", Some(1));
        assert!(miss.provider_miss);
        assert_eq!(tracker.snapshot().provider_misses, 1);
    }

    #[test]
    fn unforwarded_cache_key_is_not_a_provider_miss() {
        let mut tracker = PromptCacheTracker::default();
        let req = request_with_items(vec![system("sys")]);
        tracker.record_request(&req);
        tracker.record_usage(&usage(2_000, 0), false, "turn", Some(0));
        tracker.record_request(&req);
        let outcome = tracker.record_usage(&usage(2_000, 0), false, "turn", Some(1));
        assert!(!outcome.provider_miss);
        assert_eq!(tracker.snapshot().provider_misses, 0);
    }

    #[test]
    fn empty_report_explains_no_calls() {
        let text = PromptCacheTracker::default().snapshot().format_report();
        assert!(text.contains("no model calls yet"));
    }

    #[test]
    fn user_synthetic_reason_is_in_the_section_label() {
        let mut item = UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from("AGENTS.md"),
            }],
            synthetic_reason: Some(SyntheticReason::ProjectInstructions),
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        };
        let fp = fingerprint_request(&request_with_items(vec![ConversationItem::User(item.clone())]));
        assert!(
            fp.sections
                .iter()
                .any(|s| s.label == "item[0]:user:project_instructions"),
            "{:?}",
            fp.sections
        );
        item.content = vec![ContentPart::Text {
            text: Arc::<str>::from("AGENTS.md changed"),
        }];
        let next = fingerprint_request(&request_with_items(vec![ConversationItem::User(item)]));
        assert!(diff_fingerprints(&fp, &next).is_break());
    }
}
