//! Cross-model fork context digest.
//!
//! When a subagent forks from a parent that runs a *different* model, raw
//! conversation items must never cross the provider boundary (opaque
//! reasoning, `CodexRawInput.raw`, encrypted payloads). Instead the parent
//! history is rendered into a plaintext `<forked_context>` digest that seeds
//! the child as its first user message.
//!
//! The digest is a richer sibling of [`crate::context::normalize_forked_context`]:
//! it keeps reasoning *summaries* (never `encrypted_content`), more generous
//! tool-step previews, and works against an explicit character budget derived
//! from the child model's context window. When the deterministic render is
//! over budget, the caller may run one LLM compaction pass over the earlier
//! portion (see [`DigestPlan`]) and re-render with the summary spliced in.

use std::fmt::Write;

use xai_grok_sampling_types::conversation::{BackendToolKind, ContentPart, ConversationItem};
use xai_grok_sampling_types::reasoning_item_text;

use crate::context::{count_complete_turns, render_summary, strip_fork_noise, truncate_str};

/// Per-item caps (in characters) for the deterministic digest render.
const USER_TEXT_CAP: usize = 4_000;
const ASSISTANT_TEXT_CAP: usize = 4_000;
const REASONING_TEXT_CAP: usize = 1_200;
const TOOL_ARGS_CAP: usize = 300;
const TOOL_RESULT_CAP: usize = 600;
const BACKEND_SUMMARY_CAP: usize = 4_000;

/// Fraction of the character budget reserved for the verbatim recent tail
/// when the full history does not fit.
const RECENT_TAIL_FRACTION: f64 = 0.6;

const DIGEST_OPEN: &str = "<forked_context>\n";
const DIGEST_CLOSE: &str = "</forked_context>";
const DIGEST_PREAMBLE: &str = "You are a subagent forked from a parent agent session. The digest \
below summarizes the parent's conversation so far (its instructions, findings, and tool activity). \
Treat it as background context: continue the investigation with your own tools rather than \
assuming these results are complete.\n\n";

/// How the digest should be assembled, decided before any LLM call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DigestPlan {
    /// Number of leading non-system items that fall in the "earlier"
    /// (summarized) section. `0` means the whole history renders verbatim.
    pub earlier_items: usize,
    /// Whether the fully verbatim deterministic render fits the budget.
    /// When `false`, callers may run an LLM compaction pass over the
    /// earlier portion and pass the result to [`render_forked_context_digest`].
    pub fits: bool,
}

/// Decide how to split the parent history for a digest under `max_chars`.
///
/// If the full deterministic render fits, no split happens. Otherwise the
/// most recent complete turns that fit in [`RECENT_TAIL_FRACTION`] of the
/// budget stay verbatim and everything earlier is marked for summarization.
pub fn plan_forked_context_digest(items: &[ConversationItem], max_chars: usize) -> DigestPlan {
    let parent_items = non_system_items(items);
    if parent_items.is_empty() {
        return DigestPlan {
            earlier_items: 0,
            fits: true,
        };
    }

    let mut full = String::new();
    for item in &parent_items {
        render_item_to_digest(&mut full, item);
    }
    let overhead = DIGEST_OPEN.len() + DIGEST_PREAMBLE.len() + DIGEST_CLOSE.len();
    if full.len() + overhead <= max_chars {
        return DigestPlan {
            earlier_items: 0,
            fits: true,
        };
    }

    let tail_budget = ((max_chars.saturating_sub(overhead)) as f64 * RECENT_TAIL_FRACTION) as usize;
    let turns = count_complete_turns(&parent_items);
    // Walk turn boundaries from the end until the verbatim tail would exceed
    // its budget; always keep at least the final turn (or trailing partial).
    let mut split = if turns.is_empty() { 0 } else { turns.len() };
    let mut earlier_items = 0usize;
    while split > 0 {
        let candidate_start = if split == 1 { 0 } else { turns[split - 2] };
        let mut tail = String::new();
        for item in &parent_items[candidate_start..] {
            render_item_to_digest(&mut tail, item);
        }
        if tail.len() > tail_budget && earlier_items != 0 {
            break;
        }
        earlier_items = candidate_start;
        if tail.len() > tail_budget {
            break;
        }
        split -= 1;
    }

    DigestPlan {
        earlier_items,
        fits: false,
    }
}

/// Plaintext source text for the LLM compaction pass over the earlier
/// portion of the history (`items[..plan.earlier_items]` of the non-system
/// view). Capped at `cap_chars` to bound the summarization request itself.
pub fn digest_earlier_source_text(
    items: &[ConversationItem],
    earlier_items: usize,
    cap_chars: usize,
) -> String {
    let parent_items = non_system_items(items);
    let end = earlier_items.min(parent_items.len());
    let mut out = String::new();
    for item in &parent_items[..end] {
        render_item_to_digest(&mut out, item);
    }
    if out.len() > cap_chars {
        let keep = truncate_str(&out, cap_chars).len();
        out.truncate(keep);
        out.push_str("\n[...truncated...]\n");
    }
    out
}

/// Render the final `<forked_context>` digest.
///
/// `earlier_summary` is the optional LLM compaction of the earlier portion;
/// when absent and a split is required, a deterministic metadata summary
/// (message counts, tools used) stands in. The output is hard-capped at
/// `max_chars` (closing tag preserved).
pub fn render_forked_context_digest(
    items: &[ConversationItem],
    max_chars: usize,
    plan: &DigestPlan,
    earlier_summary: Option<&str>,
) -> String {
    let parent_items = non_system_items(items);

    let mut out = String::from(DIGEST_OPEN);
    out.push_str(DIGEST_PREAMBLE);

    if plan.fits || plan.earlier_items == 0 {
        for item in &parent_items {
            render_item_to_digest(&mut out, item);
        }
    } else {
        let split = plan.earlier_items.min(parent_items.len());
        out.push_str("=== Earlier context (summarized) ===\n");
        match earlier_summary {
            Some(summary) if !summary.trim().is_empty() => {
                out.push_str(summary.trim());
                out.push('\n');
            }
            _ => render_summary(&mut out, &parent_items[..split]),
        }
        out.push_str("\n=== Recent turns (verbatim) ===\n");
        for item in &parent_items[split..] {
            render_item_to_digest(&mut out, item);
        }
    }
    out.push_str(DIGEST_CLOSE);

    enforce_hard_cap(&mut out, max_chars);
    out
}

/// Hard-cap the digest at `max_chars`, preserving the closing tag so the
/// child never sees an unterminated block.
fn enforce_hard_cap(out: &mut String, max_chars: usize) {
    if out.len() <= max_chars {
        return;
    }
    const TRUNCATION_SUFFIX: &str = "\n[...truncated...]\n</forked_context>";
    let keep_target = max_chars.saturating_sub(TRUNCATION_SUFFIX.len());
    let keep = truncate_str(out, keep_target).len();
    out.truncate(keep);
    out.push_str(TRUNCATION_SUFFIX);
}

fn non_system_items(items: &[ConversationItem]) -> Vec<&ConversationItem> {
    items
        .iter()
        .filter(|i| !matches!(i, ConversationItem::System(_)))
        .collect()
}

fn push_capped(out: &mut String, label: &str, text: &str, cap: usize) {
    if text.is_empty() {
        return;
    }
    if text.len() > cap {
        let _ = writeln!(out, "[{label}]: {}...", truncate_str(text, cap));
    } else {
        let _ = writeln!(out, "[{label}]: {text}");
    }
}

/// Render a single conversation item into the digest.
///
/// Safety guards (provider isolation): reasoning is rendered from
/// [`reasoning_item_text`] only (summary/plaintext content — never
/// `encrypted_content`), and backend tool calls render their plaintext
/// `text_summary()` (for `CodexRawInput` that is the cross-provider fallback
/// or a placeholder — never `raw`).
fn render_item_to_digest(out: &mut String, item: &ConversationItem) {
    match item {
        ConversationItem::System(_) => {}
        ConversationItem::User(u) => {
            let text: String = u
                .content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = strip_fork_noise(&text);
            push_capped(out, "User", &text, USER_TEXT_CAP);
        }
        ConversationItem::Reasoning(r) => {
            let text = reasoning_item_text(r);
            push_capped(out, "Thinking", text.trim(), REASONING_TEXT_CAP);
        }
        ConversationItem::Assistant(a) => {
            push_capped(out, "Assistant", &a.content, ASSISTANT_TEXT_CAP);
            for tc in &a.tool_calls {
                let args: &str =
                    if xai_grok_sampling_types::conversation::codex_private_function_arguments(
                        &tc.id, &tc.name,
                    ) {
                        "[provider-private tool arguments omitted]"
                    } else {
                        tc.arguments.as_ref()
                    };
                if args.len() > TOOL_ARGS_CAP {
                    let _ = writeln!(
                        out,
                        "[Tool Call]: {} ({}...)",
                        tc.name,
                        truncate_str(args, TOOL_ARGS_CAP)
                    );
                } else {
                    let _ = writeln!(out, "[Tool Call]: {} ({args})", tc.name);
                }
            }
        }
        ConversationItem::ToolResult(tr) => {
            push_capped(out, "Tool Result", &tr.content, TOOL_RESULT_CAP);
        }
        ConversationItem::CustomToolOutput(output) => {
            push_capped(out, "Tool Result", &output.text_content(), TOOL_RESULT_CAP);
        }
        ConversationItem::BackendToolCall(b) => {
            // CodexRawInput without a cross-provider fallback has no safe
            // plaintext beyond a placeholder; skip pure placeholders to keep
            // the digest signal-dense but keep real fallback/compaction text.
            if let BackendToolKind::CodexRawInput(raw) = &b.kind
                && raw.cross_provider_fallback.is_none()
            {
                return;
            }
            push_capped(out, "Prior Context", &b.text_summary(), BACKEND_SUMMARY_CAP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use xai_grok_sampling_types::conversation::{
        BackendToolCallItem, CodexRawInputItem, ToolResultItem,
    };

    fn user(text: &str) -> ConversationItem {
        ConversationItem::user(text)
    }

    fn assistant(text: &str) -> ConversationItem {
        ConversationItem::assistant(text)
    }

    fn reasoning(text: &str) -> ConversationItem {
        ConversationItem::Reasoning(xai_grok_sampling_types::synthesized_reasoning_item(text))
    }

    fn tool_result(content: &str) -> ConversationItem {
        ConversationItem::ToolResult(ToolResultItem {
            tool_call_id: "tc-1".to_string(),
            content: content.into(),
            images: Vec::new(),
            ordered_content: Vec::new(),
        })
    }

    fn codex_raw(fallback: Option<&str>) -> ConversationItem {
        ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::CodexRawInput(CodexRawInputItem {
                id: "raw-1".to_string(),
                raw: json!({
                    "type": "compaction",
                    "encrypted_content": "SECRET_ENCRYPTED_BLOB"
                }),
                cross_provider_fallback: fallback.map(str::to_owned),
            }),
        })
    }

    #[test]
    fn small_history_renders_fully_within_budget() {
        let items = vec![
            ConversationItem::system("sys"),
            user("Find the bug in parser.rs"),
            reasoning("The parser likely mishandles escapes"),
            assistant("I found the issue in unescape()"),
        ];
        let plan = plan_forked_context_digest(&items, 10_000);
        assert!(plan.fits);
        assert_eq!(plan.earlier_items, 0);
        let digest = render_forked_context_digest(&items, 10_000, &plan, None);
        assert!(digest.starts_with("<forked_context>"));
        assert!(digest.ends_with("</forked_context>"));
        assert!(digest.contains("[User]: Find the bug in parser.rs"));
        assert!(digest.contains("[Thinking]: The parser likely mishandles escapes"));
        assert!(digest.contains("[Assistant]: I found the issue in unescape()"));
    }

    #[test]
    fn reasoning_summary_included_but_capped() {
        let long = "R".repeat(5_000);
        let items = vec![user("q"), reasoning(&long), assistant("a")];
        let plan = plan_forked_context_digest(&items, 100_000);
        let digest = render_forked_context_digest(&items, 100_000, &plan, None);
        assert!(digest.contains(&"R".repeat(REASONING_TEXT_CAP)));
        assert!(!digest.contains(&"R".repeat(REASONING_TEXT_CAP + 1)));
    }

    #[test]
    fn codex_raw_never_leaks_encrypted_content() {
        let items = vec![
            user("q"),
            codex_raw(Some("Earlier compacted summary text")),
            codex_raw(None),
            assistant("a"),
        ];
        let plan = plan_forked_context_digest(&items, 100_000);
        let digest = render_forked_context_digest(&items, 100_000, &plan, None);
        assert!(!digest.contains("SECRET_ENCRYPTED_BLOB"));
        assert!(digest.contains("Earlier compacted summary text"));
    }

    #[test]
    fn over_budget_splits_and_uses_metadata_summary() {
        let big = "x".repeat(3_000);
        let mut items = vec![ConversationItem::system("sys")];
        for i in 0..6 {
            items.push(user(&format!("question {i} {big}")));
            items.push(assistant(&format!("answer {i} {big}")));
        }
        let plan = plan_forked_context_digest(&items, 8_000);
        assert!(!plan.fits);
        assert!(plan.earlier_items > 0);
        let digest = render_forked_context_digest(&items, 8_000, &plan, None);
        assert!(digest.len() <= 8_000);
        assert!(digest.contains("=== Earlier context (summarized) ==="));
        assert!(digest.contains("=== Recent turns (verbatim) ==="));
        assert!(digest.contains("Messages:"));
        assert!(digest.ends_with("</forked_context>"));
    }

    #[test]
    fn over_budget_with_llm_summary_splices_it_in() {
        let big = "y".repeat(3_000);
        let mut items = Vec::new();
        for i in 0..6 {
            items.push(user(&format!("question {i} {big}")));
            items.push(assistant(&format!("answer {i} {big}")));
        }
        let plan = plan_forked_context_digest(&items, 8_000);
        assert!(!plan.fits);
        let digest =
            render_forked_context_digest(&items, 8_000, &plan, Some("LLM_SUMMARY_OF_EARLIER"));
        assert!(digest.contains("LLM_SUMMARY_OF_EARLIER"));
        assert!(!digest.contains("Messages:"));
    }

    #[test]
    fn hard_cap_preserves_closing_tag() {
        let big = "z".repeat(2_000);
        let items = vec![user(&big), assistant(&big)];
        let plan = plan_forked_context_digest(&items, 1_000);
        let digest = render_forked_context_digest(&items, 1_000, &plan, None);
        assert!(digest.len() <= 1_000);
        assert!(digest.ends_with("</forked_context>"));
        assert!(digest.contains("[...truncated...]"));
    }

    #[test]
    fn earlier_source_text_covers_split_and_caps() {
        let big = "w".repeat(3_000);
        let mut items = Vec::new();
        for i in 0..6 {
            items.push(user(&format!("question {i} {big}")));
            items.push(assistant(&format!("answer {i} {big}")));
        }
        let plan = plan_forked_context_digest(&items, 8_000);
        assert!(plan.earlier_items > 0);
        let source = digest_earlier_source_text(&items, plan.earlier_items, 2_000);
        assert!(source.len() <= 2_000 + "\n[...truncated...]\n".len());
        assert!(source.contains("question 0"));
    }

    #[test]
    fn tool_steps_render_condensed() {
        use xai_grok_sampling_types::conversation::ToolCall;
        let mut a = ConversationItem::assistant("running a command");
        if let ConversationItem::Assistant(ref mut item) = a {
            item.tool_calls = vec![ToolCall {
                id: "tc-1".into(),
                name: "bash".to_string(),
                arguments: json!({"command": "ls -la"}).to_string().into(),
            }];
        }
        let items = vec![user("list files"), a, tool_result("total 42\nfile.txt")];
        let plan = plan_forked_context_digest(&items, 100_000);
        let digest = render_forked_context_digest(&items, 100_000, &plan, None);
        assert!(digest.contains("[Tool Call]: bash"));
        assert!(digest.contains("ls -la"));
        assert!(digest.contains("[Tool Result]: total 42"));
    }
}
