//! ConversationRequest assembly — image compaction, repair, memory injection.

#[cfg(test)]
use xai_grok_sampling_types::{ContentPart, CustomToolOutputContent};
use xai_grok_sampling_types::{ConversationItem, ConversationRequest, ToolSpec, TraceContext};

use super::ChatStateActor;
use crate::events::ChatStateEvent;
#[cfg(test)]
pub(crate) use crate::image_budget::{
    IMAGE_COMPACT_PLACEHOLDER, IMAGE_COMPACT_RECLAIM_TARGET_BYTES, IMAGE_COMPACT_TRIGGER_BYTES,
    compact_images_to_byte_budget, conversation_body_bytes, inline_image_count,
};
use crate::image_budget::{ImageBudgetOutcome, apply_image_budget};
use crate::types::PruningConfig;

/// Placeholder inserted when a tool result is hard-cleared.
///
/// `pub(super)` so that `mutations.rs` can use the same string when it
/// hard-clears tool results in the retained in-memory conversation.
pub(super) const HARD_CLEAR_PLACEHOLDER: &str = "[Tool result omitted — too old]";

/// Separator inserted between head and tail in soft-trimmed results.
const SOFT_TRIM_SEPARATOR: &str = "\n\n[…trimmed…]\n\n";

impl ChatStateActor {
    /// Build a `ConversationRequest` from the current actor state.
    ///
    /// 1. Evict oldest inline images when the inline-image bytes near 50 MB
    /// 2. Optionally persist the memory reminder into actor state
    /// 3. Inject memory reminder into the request clone (if needed)
    /// 4. Assemble and return the `ConversationRequest`
    ///
    /// Tool-result soft-trim / hard-clear is **not** applied here. Rewriting
    /// already-sent items on the hot request path busts the prompt-cache
    /// prefix and costs more than it saves. Call
    /// [`crate::ChatStateHandle::prune_for_fresh_input`] (or
    /// [`prune_conversation`] on a compact-model clone) only when the
    /// next model request is already a cold prefix — compaction or a
    /// model swap.
    ///
    /// # Repair invariant
    ///
    /// The `BuildConversationRequest` command handler calls
    /// `ensure_conversation_integrity()` on the actor's own conversation
    /// **before** this function runs. The clone therefore starts from an
    /// already-repaired state, so there is no need to run
    /// `dedup_duplicate_tool_results` / `repair_dangling_tool_calls` on the
    /// clone — those would be O(n) no-ops.
    pub(super) fn build_conversation_request(
        &mut self,
        tool_definitions: Vec<ToolSpec>,
        memory_reminder: Option<String>,
        persist_memory_reminder: bool,
        trace: Option<Box<dyn TraceContext>>,
        conv_id: String,
        req_id: String,
    ) -> ConversationRequest {
        let mut memory_reminder = memory_reminder;
        if let Some(reminder) = memory_reminder.as_deref()
            && persist_memory_reminder
        {
            // A live in-place inject can prepend a `System` item, shifting indices
            // under an active capture; snapshot + rebase like the other mutators.
            self.snapshot_turn_slice();
            let injected = inject_memory_reminder(&mut self.state.conversation, reminder);
            if injected {
                self.persistence.replace_history(&self.state.conversation);
                memory_reminder = None;
            }
            self.rebase_turn_capture_offset();
        }
        let budgeted = apply_image_budget(self.state.conversation.clone());
        let ImageBudgetOutcome {
            body_bytes,
            body_bytes_after,
            inline_images,
            needs_image_compaction,
            evicted,
        } = budgeted.outcome;
        let mut items = budgeted.items;
        if inline_images > 0 {
            self.send_event(ChatStateEvent::ImageBudget {
                body_bytes,
                trigger_bytes: crate::image_budget::IMAGE_COMPACT_TRIGGER_BYTES,
                reclaim_target_bytes: crate::image_budget::IMAGE_COMPACT_RECLAIM_TARGET_BYTES,
                inline_images,
                needs_image_compaction,
                evicted,
                body_bytes_after,
            });
        }
        if let Some(reminder) = memory_reminder {
            inject_memory_reminder(&mut items, &reminder);
        }
        let items = crate::compaction_utils::ModelRequestHistory::from_raw(items).into_items();

        // Step 3: Assemble request
        ConversationRequest {
            items,
            tools: tool_definitions,
            hosted_tools: vec![],
            tool_choice: None,
            model: Some(self.state.sampling_config.model.clone()),
            temperature: self.state.sampling_config.temperature,
            max_output_tokens: self.state.sampling_config.max_completion_tokens,
            top_p: self.state.sampling_config.top_p,
            x_grok_conv_id: Some(conv_id),
            x_grok_req_id: Some(req_id),
            x_grok_session_id: None,
            x_grok_turn_idx: None,
            x_grok_transient_retry: None,
            x_grok_agent_id: None,
            x_grok_deployment_id: None,
            x_grok_user_id: None,
            x_grok_cache_affinity_id: None,
            trace,
            prompt_cache_key: None,
            reasoning_effort: self.state.sampling_config.reasoning_effort,
            service_tier: self.state.sampling_config.service_tier.clone(),
            json_schema: None,
            length_policy: xai_grok_sampling_types::LengthPolicy::CompleteToolCalls,
        }
    }
}

// ============================================================================
// Pruning (standalone functions, no actor state needed)
// ============================================================================

/// `User` items that are not real prompt turns (interjections, reminders).
///
/// `prompt_index` counts real turns. Extra `User` items would otherwise make
/// old tool results look older than they are and get trimmed a turn early.
pub(crate) fn count_synthetic_user_items(
    conversation: &[ConversationItem],
    prompt_index: usize,
) -> usize {
    let total_user_items = conversation
        .iter()
        .filter(|i| matches!(i, ConversationItem::User(_)))
        .count();
    total_user_items.saturating_sub(prompt_index)
}

/// Prune old, large tool results from the conversation in place.
///
/// Turn age is estimated by walking backward through the conversation and
/// counting `User` items to determine which "turn" each tool result belongs to.
/// `prompt_index` raises the keep/hard-clear thresholds by the number of
/// synthetic `User` items so interjections do not age tool results early.
///
/// This rewrites already-sent items and must only run when the next model
/// request is already a cold prefix (compaction input or a model swap).
/// Returns the number of tool results that changed.
pub fn prune_conversation(
    conversation: &mut [ConversationItem],
    config: &PruningConfig,
    prompt_index: usize,
) -> usize {
    if !config.enabled {
        return 0;
    }

    let synthetic_count = count_synthetic_user_items(conversation, prompt_index);
    let keep_last_n_turns = config.keep_last_n_turns.saturating_add(synthetic_count);
    let hard_clear_age_turns = config.hard_clear_age_turns.saturating_add(synthetic_count);

    let mut turn_from_end: usize = 0;
    let mut seen_first_user = false;
    let mut changed = 0usize;

    for i in (0..conversation.len()).rev() {
        if matches!(&conversation[i], ConversationItem::User(_)) {
            if seen_first_user {
                turn_from_end += 1;
            }
            seen_first_user = true;
            continue;
        }

        // Never prune recent turns.
        if turn_from_end < keep_last_n_turns {
            continue;
        }

        match &mut conversation[i] {
            ConversationItem::ToolResult(tool_result) => {
                // Hard clear: very old tool results → replace entirely.
                if turn_from_end >= hard_clear_age_turns {
                    let already_cleared = tool_result.content.as_ref() == HARD_CLEAR_PLACEHOLDER
                        && tool_result.images.is_empty()
                        && tool_result.ordered_content.is_empty();
                    if !already_cleared {
                        tool_result.content = std::sync::Arc::<str>::from(HARD_CLEAR_PLACEHOLDER);
                        tool_result.images.clear();
                        tool_result.ordered_content.clear();
                        changed += 1;
                    }
                    continue;
                }

                let source = if tool_result.ordered_content.is_empty() {
                    tool_result.content.as_ref().to_owned()
                } else {
                    tool_result
                        .ordered_content
                        .iter()
                        .filter_map(|part| match part {
                            xai_grok_sampling_types::CustomToolOutputContent::Text { text } => {
                                Some(text.as_ref())
                            }
                            xai_grok_sampling_types::CustomToolOutputContent::Image { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("")
                };
                if source.chars().count() > config.soft_trim_threshold {
                    let head = safe_char_slice(&source, 0, config.soft_trim_head);
                    let tail = safe_char_slice_tail(&source, config.soft_trim_tail);
                    let trimmed = format!("{head}{SOFT_TRIM_SEPARATOR}{tail}");
                    tool_result.content = std::sync::Arc::<str>::from(trimmed.clone());
                    tool_result.images.clear();
                    tool_result.ordered_content =
                        vec![xai_grok_sampling_types::CustomToolOutputContent::text(
                            trimmed,
                        )];
                    changed += 1;
                }
            }
            ConversationItem::CustomToolOutput(output) => {
                if turn_from_end >= hard_clear_age_turns {
                    let already_cleared = output.text_content() == HARD_CLEAR_PLACEHOLDER
                        && output.content.len() == 1;
                    if !already_cleared {
                        output.content =
                            vec![xai_grok_sampling_types::CustomToolOutputContent::text(
                                HARD_CLEAR_PLACEHOLDER,
                            )];
                        changed += 1;
                    }
                    continue;
                }
                let source = output.text_content();
                if source.chars().count() > config.soft_trim_threshold {
                    let head = safe_char_slice(&source, 0, config.soft_trim_head);
                    let tail = safe_char_slice_tail(&source, config.soft_trim_tail);
                    output.content = vec![xai_grok_sampling_types::CustomToolOutputContent::text(
                        format!("{head}{SOFT_TRIM_SEPARATOR}{tail}"),
                    )];
                    changed += 1;
                }
            }
            _ => {}
        }
    }
    changed
}

// ============================================================================
// Image size-gated compaction (request-copy only)
// ============================================================================

// ============================================================================
// Memory reminder injection
// ============================================================================

use crate::types::MEMORY_CONTEXT_OPEN_TAG;

/// Upsert a memory reminder into the conversation's system message.
///
/// If the first item is a `System` message, any previously injected memory
/// reminder section is replaced in-place; otherwise the reminder is appended.
/// If no system message exists, a new `System` item is prepended.
///
/// Returns `true` when the conversation was changed.
pub(super) fn inject_memory_reminder(items: &mut Vec<ConversationItem>, reminder: &str) -> bool {
    let reminder = reminder.trim();
    if reminder.is_empty() {
        return false;
    }

    if let Some(ConversationItem::System(sys)) = items.first_mut() {
        upsert_memory_reminder_text(&mut sys.content, reminder)
    } else {
        items.insert(0, ConversationItem::system(reminder));
        true
    }
}

fn upsert_memory_reminder_text(system_prompt: &mut std::sync::Arc<str>, reminder: &str) -> bool {
    let existing_start = system_prompt
        .find(MEMORY_CONTEXT_OPEN_TAG)
        .map(|idx| system_prompt[..idx].trim_end_matches('\n').len());

    let updated: String = if let Some(prefix_len) = existing_start {
        let prefix = system_prompt[..prefix_len].trim_end_matches('\n');
        if prefix.is_empty() {
            reminder.to_string()
        } else {
            format!("{prefix}\n\n{reminder}")
        }
    } else if system_prompt.trim_end() == reminder {
        system_prompt.as_ref().to_owned()
    } else if system_prompt.is_empty() {
        reminder.to_string()
    } else {
        format!("{}\n\n{reminder}", system_prompt.trim_end_matches('\n'))
    };

    if system_prompt.as_ref() == updated.as_str() {
        false
    } else {
        *system_prompt = std::sync::Arc::<str>::from(updated);
        true
    }
}

// ============================================================================
// String helpers
// ============================================================================

fn safe_char_slice(s: &str, start: usize, count: usize) -> String {
    s.chars().skip(start).take(count).collect()
}

fn safe_char_slice_tail(s: &str, count: usize) -> String {
    let total = s.chars().count();
    if count >= total {
        return s.to_string();
    }
    s.chars().skip(total - count).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_disabled_is_noop() {
        let mut conv = vec![ConversationItem::tool_result("c1", "x".repeat(10_000))];
        let config = PruningConfig {
            enabled: false,
            ..Default::default()
        };
        prune_conversation(&mut conv, &config, 0);
        if let ConversationItem::ToolResult(ref tr) = conv[0] {
            assert_eq!(tr.content.len(), 10_000);
        }
    }

    #[test]
    fn prune_conversation_does_not_age_from_synthetic_users() {
        // keep_last_n=1 would trim the tool after one extra User item.
        // A synthetic interjection must not count as that extra turn.
        let mut conv = vec![
            ConversationItem::user("real q1"),
            ConversationItem::tool_result("c1", "x".repeat(8_000)),
            ConversationItem::interjection("peer ping"),
            ConversationItem::user("real q2"),
        ];
        let config = PruningConfig {
            keep_last_n_turns: 1,
            ..Default::default()
        };
        // Two real turns (prompt_index=2) + one synthetic User.
        prune_conversation(&mut conv, &config, 2);
        let ConversationItem::ToolResult(ref tr) = conv[1] else {
            panic!("expected tool result");
        };
        assert!(
            !tr.content.contains(SOFT_TRIM_SEPARATOR),
            "synthetic User items must not age a tool result into the trim window"
        );
    }

    #[test]
    fn prune_conversation_still_trims_after_enough_real_turns() {
        let mut conv = vec![
            ConversationItem::user("real q1"),
            ConversationItem::tool_result("c1", "x".repeat(8_000)),
            ConversationItem::user("real q2"),
            ConversationItem::user("real q3"),
        ];
        let config = PruningConfig {
            keep_last_n_turns: 1,
            ..Default::default()
        };
        prune_conversation(&mut conv, &config, 3);
        let ConversationItem::ToolResult(ref tr) = conv[1] else {
            panic!("expected tool result");
        };
        assert!(
            tr.content.contains(SOFT_TRIM_SEPARATOR),
            "a tool result older than keep_last_n real turns must still soft-trim"
        );
    }

    #[test]
    fn inject_memory_into_existing_system() {
        let mut items = vec![
            ConversationItem::system("You are helpful."),
            ConversationItem::user("hi"),
        ];
        inject_memory_reminder(&mut items, "Remember: user likes rust");
        if let ConversationItem::System(ref sys) = items[0] {
            assert!(sys.content.contains("Remember: user likes rust"));
            assert!(sys.content.starts_with("You are helpful."));
        }
        assert_eq!(items.len(), 2); // no new item added
    }

    #[test]
    fn inject_memory_prepends_when_no_system() {
        let mut items = vec![ConversationItem::user("hi")];
        inject_memory_reminder(&mut items, "Remember: user likes rust");
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], ConversationItem::System(_)));
    }

    // -- image size-gated compaction tests --

    /// A user message with a small fixed inline image.
    fn user_with_image(text: &str) -> ConversationItem {
        let mut item = ConversationItem::user(text);
        item.add_image("data:image/png;base64,iVBORw0KGgo=");
        item
    }

    /// A user message carrying an inline image whose `data:` URL is exactly
    /// `url_bytes` long (must be >= the data-URL prefix length).
    fn user_with_image_of_bytes(text: &str, url_bytes: usize) -> ConversationItem {
        const PREFIX: &str = "data:image/png;base64,";
        let pad = url_bytes.saturating_sub(PREFIX.len());
        let mut item = ConversationItem::user(text);
        item.add_image(format!("{PREFIX}{}", "A".repeat(pad)));
        item
    }

    fn has_image(item: &ConversationItem) -> bool {
        matches!(
            item,
            ConversationItem::User(u)
                if u.content.iter().any(|p| matches!(p, ContentPart::Image { .. }))
        )
    }

    fn has_placeholder(item: &ConversationItem) -> bool {
        matches!(
            item,
            ConversationItem::User(u) if u.content.iter().any(|p| matches!(
                p,
                ContentPart::Text { text } if text.as_ref() == IMAGE_COMPACT_PLACEHOLDER
            ))
        )
    }

    // Images are sized ~100 KB so the ~235 B placeholder that replaces an
    // evicted image is negligible: each eviction frees ~one image's bytes.
    const TEST_IMG_BYTES: usize = 100_000;

    #[test]
    fn no_eviction_when_at_or_below_target() {
        // Multiple old image turns are *retained* when the body already fits —
        // the key behavior change from the old "strip everything but newest".
        let mut conv = vec![
            ConversationItem::system("sys"),
            user_with_image_of_bytes("first", TEST_IMG_BYTES),
            ConversationItem::assistant("a"),
            user_with_image_of_bytes("second", TEST_IMG_BYTES),
            user_with_image_of_bytes("third", TEST_IMG_BYTES),
        ];
        // current < target: nothing to do.
        compact_images_to_byte_budget(&mut conv, 300_000, 400_000);
        assert_eq!(conv.iter().filter(|i| has_image(i)).count(), 3);
    }

    #[test]
    fn evicts_oldest_until_under_target() {
        let mut conv = vec![
            user_with_image_of_bytes("oldest", TEST_IMG_BYTES),
            user_with_image_of_bytes("middle", TEST_IMG_BYTES),
            user_with_image_of_bytes("newest", TEST_IMG_BYTES),
        ];
        // current 300k, target 250k: evicting the oldest (~100 KB) fits.
        compact_images_to_byte_budget(&mut conv, 300_000, 250_000);
        assert!(has_placeholder(&conv[0]), "oldest evicted");
        assert!(has_image(&conv[1]), "middle kept");
        assert!(has_image(&conv[2]), "newest kept");
    }

    #[test]
    fn evicts_more_oldest_for_lower_target() {
        let mut conv = vec![
            user_with_image_of_bytes("oldest", TEST_IMG_BYTES),
            user_with_image_of_bytes("middle", TEST_IMG_BYTES),
            user_with_image_of_bytes("newest", TEST_IMG_BYTES),
        ];
        // current 300k, target 150k: must drop the two oldest to fit.
        compact_images_to_byte_budget(&mut conv, 300_000, 150_000);
        assert!(has_placeholder(&conv[0]));
        assert!(has_placeholder(&conv[1]));
        assert!(has_image(&conv[2]), "newest kept");
    }

    #[test]
    fn eviction_reclaims_batch_to_low_water_mark() {
        // Mirror production: a body sitting just over the trigger, made of many
        // equal images, is reclaimed in one pass down to the low-water mark —
        // dropping a *batch* of the oldest, not just the one image needed to
        // clear the trigger. This is the hysteresis that keeps the prefix
        // cache-warm for the following turns.
        let img_bytes = 1_000_000usize; // ~1 MB url each
        let n = (IMAGE_COMPACT_TRIGGER_BYTES / img_bytes) + 2; // body just over trigger
        let mut conv: Vec<ConversationItem> = (0..n)
            .map(|i| user_with_image_of_bytes(&format!("i{i}"), img_bytes))
            .collect();
        let current = n * img_bytes;
        assert!(current > IMAGE_COMPACT_TRIGGER_BYTES);

        compact_images_to_byte_budget(&mut conv, current, IMAGE_COMPACT_RECLAIM_TARGET_BYTES);

        let kept = conv.iter().filter(|i| has_image(i)).count();
        let evicted = conv.iter().filter(|i| has_placeholder(i)).count();

        // Clearing only the trigger would evict ~3 images; reclaiming to the
        // low-water mark (~half the ceiling) must evict far more.
        assert!(
            evicted > n / 4,
            "expected batch eviction to the low-water mark, only {evicted}/{n} evicted"
        );
        // Oldest-first stops at the mark, so the most recent image survives.
        assert!(kept > 0);
        assert!(
            has_image(conv.last().unwrap()),
            "most recent image must be retained"
        );
    }

    #[test]
    fn evicts_all_when_target_below_one_image() {
        let mut conv = vec![
            user_with_image_of_bytes("a", TEST_IMG_BYTES),
            user_with_image_of_bytes("b", TEST_IMG_BYTES),
        ];
        compact_images_to_byte_budget(&mut conv, 200_000, 50_000);
        assert!(has_placeholder(&conv[0]));
        assert!(has_placeholder(&conv[1]));
    }

    #[test]
    fn eviction_keeps_newest_and_is_idempotent() {
        let mut conv = vec![
            user_with_image_of_bytes("i0", TEST_IMG_BYTES),
            user_with_image_of_bytes("i1", TEST_IMG_BYTES),
            user_with_image_of_bytes("i2", TEST_IMG_BYTES),
            user_with_image_of_bytes("i3", TEST_IMG_BYTES),
        ];
        // current 400k, target 250k: drop the two oldest, keep the newest two.
        compact_images_to_byte_budget(&mut conv, 400_000, 250_000);
        assert!(has_placeholder(&conv[0]) && has_placeholder(&conv[1]));
        assert!(has_image(&conv[2]) && has_image(&conv[3]));

        // Re-running with the now-smaller body is a no-op (sticky): the two
        // surviving images already fit.
        compact_images_to_byte_budget(&mut conv, 200_000, 250_000);
        assert!(has_placeholder(&conv[0]) && has_placeholder(&conv[1]));
        assert!(has_image(&conv[2]) && has_image(&conv[3]));
    }

    #[test]
    fn evicted_image_uses_honest_placeholder() {
        let mut conv = vec![user_with_image_of_bytes("x", TEST_IMG_BYTES)];
        compact_images_to_byte_budget(&mut conv, 100_000, 10);
        assert!(has_placeholder(&conv[0]));
    }

    // -- conversation_body_bytes tests --

    #[test]
    fn conversation_body_bytes_empty_is_json_array() {
        // serde encodes an empty slice as "[]" (2 bytes).
        assert_eq!(conversation_body_bytes(&[]), 2);
    }

    #[test]
    fn conversation_body_bytes_matches_serde_json_exactly() {
        // The blank-and-add-URLs measurement must equal a full serde_json
        // encode byte-for-byte — including non-image content and string
        // escaping. The `"` in the system text is escaped by serde; the
        // measurement must account for it.
        let conv = vec![
            ConversationItem::system("system \"quoted\" prompt"),
            user_with_image("look"),
            ConversationItem::assistant("a longer assistant reply with text"),
            ConversationItem::user("plain follow-up turn"),
        ];
        let expected = serde_json::to_vec(&conv).unwrap().len();
        assert_eq!(conversation_body_bytes(&conv), expected);
    }

    #[test]
    fn conversation_body_bytes_matches_serde_json_with_large_image() {
        // Exact even for a multi-KB base64 payload — the scan we deliberately
        // skip still lands on the same byte count.
        let conv = vec![user_with_image_of_bytes("big", 50_000)];
        let expected = serde_json::to_vec(&conv).unwrap().len();
        assert_eq!(conversation_body_bytes(&conv), expected);
    }

    #[test]
    fn conversation_body_bytes_small_image_is_below_trigger() {
        // A normal small inline image must not trip the 50 MB gate — the case
        // the cache-miss fix preserves.
        let conv = vec![
            user_with_image("old"),
            ConversationItem::assistant("reply"),
            ConversationItem::user("current"),
        ];
        assert!(conversation_body_bytes(&conv) < IMAGE_COMPACT_TRIGGER_BYTES);
    }

    #[test]
    fn conversation_body_bytes_large_image_reaches_trigger() {
        let conv = vec![user_with_image_of_bytes("big", IMAGE_COMPACT_TRIGGER_BYTES)];
        assert!(conversation_body_bytes(&conv) >= IMAGE_COMPACT_TRIGGER_BYTES);
    }

    // -- edge cases: exactness, boundaries, ordering --

    #[test]
    fn body_bytes_parity_multi_image_unicode_escaping() {
        // The gate is only as correct as this equality. Exercise multiple
        // images in one turn, multibyte unicode (passed through, not escaped),
        // and chars serde *does* escape (`"`, `\`, control).
        let mut turn = ConversationItem::user("two pics 🚀 with \"quotes\" and \\ slash");
        turn.add_image("data:image/png;base64,AAAA");
        turn.add_image("data:image/png;base64,BBBBBB");
        let conv = vec![
            ConversationItem::system("sys 日本語 \t control"),
            turn,
            ConversationItem::assistant("reply"),
            ConversationItem::user("plain follow-up"),
        ];
        assert_eq!(
            conversation_body_bytes(&conv),
            serde_json::to_vec(&conv).unwrap().len()
        );
    }

    #[test]
    fn no_eviction_when_exactly_at_target() {
        // The no-op guard is `current <= target`; pin the inclusive boundary.
        let mut conv = vec![user_with_image_of_bytes("a", TEST_IMG_BYTES)];
        compact_images_to_byte_budget(&mut conv, 250_000, 250_000);
        assert!(has_image(&conv[0]), "exactly at target must not evict");
    }

    #[test]
    fn terminates_when_placeholder_exceeds_image() {
        // Tiny images: each "saving" saturates to 0, but the loop must still
        // terminate and replace every image when the target is unreachable.
        let mut conv = vec![
            user_with_image_of_bytes("a", 40),
            user_with_image_of_bytes("b", 40),
        ];
        compact_images_to_byte_budget(&mut conv, 1_000, 10);
        assert!(has_placeholder(&conv[0]) && has_placeholder(&conv[1]));
    }

    #[test]
    fn evicts_oldest_image_parts_first() {
        // `has_image`/`has_placeholder` are per-item, so count actual image
        // parts to verify oldest-first ordering across parts within a turn.
        fn image_parts(conv: &[ConversationItem]) -> usize {
            conv.iter()
                .filter_map(|i| match i {
                    ConversationItem::User(u) => Some(u),
                    _ => None,
                })
                .flat_map(|u| u.content.iter())
                .filter(|p| matches!(p, ContentPart::Image { .. }))
                .count()
        }
        let mut newest = ConversationItem::user("newest turn");
        newest.add_image(format!(
            "data:image/png;base64,{}",
            "A".repeat(TEST_IMG_BYTES)
        ));
        newest.add_image(format!(
            "data:image/png;base64,{}",
            "B".repeat(TEST_IMG_BYTES)
        ));
        let mut conv = vec![user_with_image_of_bytes("oldest", TEST_IMG_BYTES), newest];
        assert_eq!(image_parts(&conv), 3);

        // ~300k body, reclaim to 150k: drop the two oldest, keep the newest.
        compact_images_to_byte_budget(&mut conv, 300_000, 150_000);
        assert_eq!(image_parts(&conv), 1, "newest image survives");
        assert!(has_placeholder(&conv[0]), "oldest turn evicted");
        assert!(has_image(&conv[1]), "newest turn keeps an image");
    }

    #[test]
    fn image_budget_covers_user_and_tool_output_images() {
        use xai_grok_sampling_types::{CustomToolOutputImageDetail, CustomToolOutputItem};

        let image_url = |fill: char| {
            std::sync::Arc::<str>::from(format!(
                "data:image/png;base64,{}",
                fill.to_string().repeat(TEST_IMG_BYTES)
            ))
        };
        let mut conv = vec![
            user_with_image_of_bytes("user", TEST_IMG_BYTES),
            ConversationItem::tool_result_with_images(
                "legacy",
                "legacy image",
                vec![ContentPart::Image {
                    url: image_url('B'),
                }],
            ),
            ConversationItem::tool_result_with_ordered_content(
                "ordered",
                vec![CustomToolOutputContent::Image {
                    url: image_url('C'),
                    detail: CustomToolOutputImageDetail::High,
                }],
            ),
            ConversationItem::custom_tool_output(CustomToolOutputItem::new(
                "custom",
                [CustomToolOutputContent::Image {
                    url: image_url('D'),
                    detail: CustomToolOutputImageDetail::Original,
                }],
            )),
        ];

        assert_eq!(inline_image_count(&conv), 4);
        let current = conversation_body_bytes(&conv);
        assert_eq!(current, serde_json::to_vec(&conv).unwrap().len());

        let outcome = compact_images_to_byte_budget(&mut conv, current, 0);

        assert_eq!(outcome.evicted, 4);
        assert_eq!(inline_image_count(&conv), 0);
        assert_eq!(
            outcome.body_bytes_after,
            conversation_body_bytes(&conv),
            "incremental accounting must match the compacted wire body"
        );
        assert!(matches!(
            &conv[1],
            ConversationItem::ToolResult(result)
                if result.images.is_empty()
                    && result.content.starts_with("legacy image\n\n")
                    && result.content.contains(crate::image_budget::TOOL_IMAGE_COMPACT_NOTE)
        ));
        assert!(matches!(
            &conv[2],
            ConversationItem::ToolResult(result)
                if matches!(
                    result.ordered_content.as_slice(),
                    [CustomToolOutputContent::Text { text }]
                        if text.as_ref() == IMAGE_COMPACT_PLACEHOLDER
                )
        ));
        assert!(matches!(
            &conv[3],
            ConversationItem::CustomToolOutput(output)
                if matches!(
                    output.content.as_slice(),
                    [CustomToolOutputContent::Text { text }]
                        if text.as_ref() == IMAGE_COMPACT_PLACEHOLDER
                )
        ));
    }

    #[test]
    fn escaped_remote_url_is_measured_exactly() {
        let mut item = ConversationItem::user("");
        item.add_image(r#"https://example.com/a"b"#);
        let conv = vec![item];
        assert_eq!(
            conversation_body_bytes(&conv),
            serde_json::to_vec(&conv).unwrap().len()
        );
    }
}
