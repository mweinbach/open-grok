use xai_grok_sampling_types::{ContentPart, ConversationItem, CustomToolOutputContent};

pub(crate) const TOOL_IMAGE_COMPACT_NOTE: &str = "[One or more images from this tool result were removed to keep the request within its size limit and are no longer visible. Do not describe or reason about their contents from memory.]";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageBudgetOutcome {
    pub body_bytes: usize,
    pub body_bytes_after: usize,
    pub inline_images: usize,
    pub needs_image_compaction: bool,
    pub evicted: usize,
}

#[derive(Clone, Debug)]
pub struct BudgetedConversation {
    pub items: Vec<ConversationItem>,
    pub outcome: ImageBudgetOutcome,
}

#[must_use]
pub fn apply_image_budget(items: Vec<ConversationItem>) -> BudgetedConversation {
    apply_image_budget_with_limits(
        items,
        IMAGE_COMPACT_TRIGGER_BYTES,
        IMAGE_COMPACT_RECLAIM_TARGET_BYTES,
    )
}

#[must_use]
pub fn apply_image_budget_with_limits(
    mut items: Vec<ConversationItem>,
    trigger_bytes: usize,
    reclaim_target_bytes: usize,
) -> BudgetedConversation {
    let body_bytes = conversation_body_bytes(&items);
    let inline_images = inline_image_count(&items);
    let needs_image_compaction = body_bytes >= trigger_bytes;
    let (evicted, body_bytes_after) = if needs_image_compaction {
        let outcome = compact_images_to_byte_budget(&mut items, body_bytes, reclaim_target_bytes);
        (outcome.evicted, outcome.body_bytes_after)
    } else {
        (0, body_bytes)
    };
    BudgetedConversation {
        items,
        outcome: ImageBudgetOutcome {
            body_bytes,
            body_bytes_after,
            inline_images,
            needs_image_compaction,
            evicted,
        },
    }
}

/// Replaces an inline image evicted to keep the request body under the proxy's
/// 50 MB limit. Phrased so the model treats the image as gone rather than
/// describing it from memory — a silently-stripped image otherwise induces
/// confident hallucination of its contents.
pub(crate) const IMAGE_COMPACT_PLACEHOLDER: &str = "[An earlier image was removed to keep the request within its size limit and is no longer visible. Do not describe or reason about its contents from memory; ask the user to re-share it if you need to see it again.]";

/// Hard request-body ceiling enforced by the inference proxy
/// (nginx `proxy-body-size`). Bodies larger than this are rejected with HTTP
/// 413 — or a connection reset before the response is written. Inline image
/// `data:` URLs (base64) are the dominant term in this size.
const MAX_REQUEST_BYTES: usize = 50 * 1024 * 1024;

/// Evict old images once the serialized body reaches this size.
///
/// We gate on the exact body (see [`conversation_body_bytes`]) — system prompt,
/// all message text, tool results, and image `data:` URLs are all counted
/// precisely. This sits 3 MB below [`MAX_REQUEST_BYTES`] as headroom for the
/// only parts of the wire request the body measurement does **not** include:
/// - **tool definitions** — sent alongside the conversation but not part of it
///   (tool JSON schemas + MCP tools); this is the bulk of the gap.
/// - the request envelope and sampling params.
/// - the small delta between our internal `ContentPart` JSON and the public-API
///   wire format (the dominant base64 image bytes are identical in both).
///
/// The uncounted remainder is only sub-MB to low-MB in practice, so 3 MB covers
/// it without needlessly sacrificing image capacity. The sampler's reactive 413
/// image-strip is the final backstop if this is ever under-estimated.
///
/// Below this threshold every image stays in place so the KV-cache prefix is
/// byte-stable across turns; eviction rewrites earlier turns and busts the
/// prefix cache, so we only pay that cost when a 413 is actually near.
pub const IMAGE_COMPACT_TRIGGER_BYTES: usize = MAX_REQUEST_BYTES - 3 * 1024 * 1024;

/// Low-water mark that eviction reclaims down to once it fires (hysteresis).
///
/// Eviction is **gated** at [`IMAGE_COMPACT_TRIGGER_BYTES`] but **reclaims** to
/// this strictly lower mark. Evicting only enough to clear the trigger means
/// the next image-bearing turn re-crosses it and evicts again — rewriting the
/// prefix and busting the KV cache on essentially every turn once the body sits
/// at the ceiling. Dropping to half the hard limit instead frees ~25 MB of
/// headroom, so the prefix is rewritten once and then stays stable (cache-warm)
/// across many turns until the headroom is consumed again. The oldest images
/// (least useful) are sacrificed in a batch rather than one-per-turn — a
/// high-water trigger paired with a lower reclaim mark (classic hysteresis).
pub const IMAGE_COMPACT_RECLAIM_TARGET_BYTES: usize = MAX_REQUEST_BYTES / 2;

// Hysteresis invariant: eviction is gated at the trigger but reclaims to a
// strictly lower mark, so one batch eviction buys many cache-warm turns rather
// than re-triggering (and re-busting the prompt cache) every turn at the
// ceiling. Enforced at compile time so the two constants can't drift together.
const _: () = assert!(IMAGE_COMPACT_RECLAIM_TARGET_BYTES < IMAGE_COMPACT_TRIGGER_BYTES);

/// An [`std::io::Write`] sink that counts bytes instead of storing them. Lets
/// us measure a `serde_json` encoding's length without allocating the full
/// (potentially tens-of-MB) output buffer.
#[derive(Default)]
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Exact JSON-serialized byte length of any value, measured through a
/// [`ByteCounter`] so no encoded buffer is allocated. JSON quoting and string
/// escaping are captured precisely (not estimated from field lengths).
fn serialized_json_bytes<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    let mut counter = ByteCounter::default();
    if let Err(err) = serde_json::to_writer(&mut counter, value) {
        // Serializing in-memory state to a byte sink is infallible in
        // practice; if it ever fails, fall back to the bytes counted so far
        // (a lower bound) rather than forcing a needless compaction.
        tracing::warn!(%err, "failed to measure serialized size");
    }
    counter.0
}

/// Exact serialized size of an inline image value from its cheap blank-URL
/// frame plus the escaped URL bytes, without scanning base64 data tails.
fn image_value_bytes<T: serde::Serialize + ?Sized>(blank_value: &T, url: &str) -> usize {
    serialized_json_bytes(blank_value) + encoded_url_bytes(url)
}

fn encoded_url_bytes(url: &str) -> usize {
    let Some((header, base64_tail)) = url.split_once(',') else {
        return serialized_json_bytes(url).saturating_sub(2);
    };
    if header.starts_with("data:") && header.ends_with(";base64") {
        serialized_json_bytes(header).saturating_sub(2) + 1 + base64_tail.len()
    } else {
        serialized_json_bytes(url).saturating_sub(2)
    }
}

/// Count of inline images in the conversation — for observability only.
pub(crate) fn inline_image_count(conversation: &[ConversationItem]) -> usize {
    conversation
        .iter()
        .map(|item| match item {
            ConversationItem::User(user) => user
                .content
                .iter()
                .filter(|part| matches!(part, ContentPart::Image { .. }))
                .count(),
            ConversationItem::ToolResult(result) => {
                result
                    .images
                    .iter()
                    .filter(|part| matches!(part, ContentPart::Image { .. }))
                    .count()
                    + result
                        .ordered_content
                        .iter()
                        .filter(|part| matches!(part, CustomToolOutputContent::Image { .. }))
                        .count()
            }
            ConversationItem::CustomToolOutput(output) => output
                .content
                .iter()
                .filter(|part| matches!(part, CustomToolOutputContent::Image { .. }))
                .count(),
            _ => 0,
        })
        .sum()
}

/// Outcome of [`compact_images_to_byte_budget`], surfaced for logging and
/// local verification.
pub(crate) struct ImageEvictionOutcome {
    /// Number of inline images replaced with the placeholder.
    pub evicted: usize,
    /// Estimated serialized body size after eviction (`current_bytes` minus the
    /// net bytes freed) — at or below `target_bytes` once enough images go.
    pub body_bytes_after: usize,
}

/// Exact serialized size of the conversation body — the figure the inference
/// proxy weighs against its 50 MB limit — computed **without** scanning the
/// multi-MB base64 image payloads.
///
/// `serde_json` escape-scans every byte of every string, so encoding the real
/// conversation would walk tens of MB of base64 on every turn. Instead we
/// serialize a copy with image URLs blanked (cheap: only the small non-image
/// content — system prompt, message text, tool results — is scanned, and it is
/// measured *exactly*, escaping included) and add back each URL's encoded length.
/// Base64 tails need no escape scan; remote URLs and caller-supplied data-URI
/// headers retain their exact serialized escaping contribution.
///
/// The blanking copy is cheap: image data lives behind `Arc<str>`, so cloning
/// only bumps refcounts and the blanked clone drops them without copying bytes.
pub(crate) fn conversation_body_bytes(conversation: &[ConversationItem]) -> usize {
    let mut blanked = conversation.to_vec();
    let mut image_url_bytes = 0usize;
    for item in &mut blanked {
        match item {
            ConversationItem::User(user) => {
                for part in &mut user.content {
                    if let ContentPart::Image { url } = part {
                        image_url_bytes += encoded_url_bytes(url);
                        *url = std::sync::Arc::<str>::from("");
                    }
                }
            }
            ConversationItem::ToolResult(result) => {
                for part in &mut result.images {
                    if let ContentPart::Image { url } = part {
                        image_url_bytes += encoded_url_bytes(url);
                        *url = std::sync::Arc::<str>::from("");
                    }
                }
                for part in &mut result.ordered_content {
                    if let CustomToolOutputContent::Image { url, .. } = part {
                        image_url_bytes += encoded_url_bytes(url);
                        *url = std::sync::Arc::<str>::from("");
                    }
                }
            }
            ConversationItem::CustomToolOutput(output) => {
                for part in &mut output.content {
                    if let CustomToolOutputContent::Image { url, .. } = part {
                        image_url_bytes += encoded_url_bytes(url);
                        *url = std::sync::Arc::<str>::from("");
                    }
                }
            }
            _ => {}
        }
    }
    serialized_json_bytes(&blanked) + image_url_bytes
}

#[derive(Clone, Copy)]
enum InlineImageLocation {
    User { item_idx: usize, part_idx: usize },
    ToolResultImage { item_idx: usize },
    ToolResultOrdered { item_idx: usize, part_idx: usize },
    CustomToolOutput { item_idx: usize, part_idx: usize },
}

struct InlineImageCandidate {
    location: InlineImageLocation,
    image_bytes: usize,
    placeholder_bytes: usize,
}

/// Replace the oldest inline images with [`IMAGE_COMPACT_PLACEHOLDER`] until
/// the serialized request body drops back to `target_bytes`, keeping the
/// newest images. `current_bytes` is the already-measured whole-body size (see
/// [`conversation_body_bytes`]); each eviction drops `running` by the image
/// part's exact serialized size minus the placeholder that replaces it, so it
/// tracks the true body byte-for-byte as images are removed.
///
/// Operates on a mutable slice — intended for the request *copy* so the stored
/// conversation is never modified.
///
/// ## Cache behavior
///
/// Eviction is **oldest-first**, which is sticky by construction: because we
/// always retain the newest images, an image only transitions image →
/// placeholder as *newer/larger* payloads push the body past the limit, never
/// placeholder → image within a stable prefix. (Token compaction removes old
/// turns wholesale and can free room to restore a previously-evicted image,
/// but that already rewrites the prefix and invalidates the server-side prompt
/// cache, so the restore is free.)
///
/// The caller gates eviction at [`IMAGE_COMPACT_TRIGGER_BYTES`] but passes the
/// lower [`IMAGE_COMPACT_RECLAIM_TARGET_BYTES`] as `target_bytes`, so one
/// eviction reclaims a batch of the oldest images and frees headroom for many
/// later image turns. This turns "rewrite the prefix on essentially every turn
/// once at the ceiling" into one larger, rare rewrite followed by a long
/// cache-warm stretch — the prefix-cache cost of dropping the oldest (least
/// useful) image is paid infrequently instead of per turn.
///
/// This replaces the previous policy — strip every image older than the most
/// recent user turn on *every* request — which (a) busted the prompt-cache
/// prefix on the turn after any image, and (b) dropped images the model still
/// needed one turn later, causing it to hallucinate their contents.
pub(crate) fn compact_images_to_byte_budget(
    conversation: &mut [ConversationItem],
    current_bytes: usize,
    target_bytes: usize,
) -> ImageEvictionOutcome {
    if current_bytes <= target_bytes {
        return ImageEvictionOutcome {
            evicted: 0,
            body_bytes_after: current_bytes,
        };
    }

    // Each content family has its own serialized frame. Measure both exact
    // placeholder forms once, then retain the image frame's detail metadata
    // while measuring each candidate.
    let content_placeholder = ContentPart::Text {
        text: std::sync::Arc::<str>::from(IMAGE_COMPACT_PLACEHOLDER),
    };
    let content_placeholder_bytes = serialized_json_bytes(&content_placeholder);
    let ordered_placeholder = CustomToolOutputContent::text(IMAGE_COMPACT_PLACEHOLDER);
    let ordered_placeholder_bytes = serialized_json_bytes(&ordered_placeholder);

    // Every inline image, oldest-first across all conversation output forms.
    let mut images = Vec::new();
    for (item_index, item) in conversation.iter().enumerate() {
        match item {
            ConversationItem::User(user) => {
                for (part_index, part) in user.content.iter().enumerate() {
                    if let ContentPart::Image { url } = part {
                        let blank = ContentPart::Image {
                            url: std::sync::Arc::<str>::from(""),
                        };
                        images.push(InlineImageCandidate {
                            location: InlineImageLocation::User {
                                item_idx: item_index,
                                part_idx: part_index,
                            },
                            image_bytes: image_value_bytes(&blank, url),
                            placeholder_bytes: content_placeholder_bytes,
                        });
                    }
                }
            }
            ConversationItem::ToolResult(result) => {
                for part in &result.images {
                    if let ContentPart::Image { url } = part {
                        let blank = ContentPart::Image {
                            url: std::sync::Arc::<str>::from(""),
                        };
                        images.push(InlineImageCandidate {
                            location: InlineImageLocation::ToolResultImage {
                                item_idx: item_index,
                            },
                            image_bytes: image_value_bytes(&blank, url),
                            placeholder_bytes: content_placeholder_bytes,
                        });
                    }
                }
                for (part_index, part) in result.ordered_content.iter().enumerate() {
                    if let CustomToolOutputContent::Image { url, detail } = part {
                        let blank = CustomToolOutputContent::Image {
                            url: std::sync::Arc::<str>::from(""),
                            detail: *detail,
                        };
                        images.push(InlineImageCandidate {
                            location: InlineImageLocation::ToolResultOrdered {
                                item_idx: item_index,
                                part_idx: part_index,
                            },
                            image_bytes: image_value_bytes(&blank, url),
                            placeholder_bytes: ordered_placeholder_bytes,
                        });
                    }
                }
            }
            ConversationItem::CustomToolOutput(output) => {
                for (part_index, part) in output.content.iter().enumerate() {
                    if let CustomToolOutputContent::Image { url, detail } = part {
                        let blank = CustomToolOutputContent::Image {
                            url: std::sync::Arc::<str>::from(""),
                            detail: *detail,
                        };
                        images.push(InlineImageCandidate {
                            location: InlineImageLocation::CustomToolOutput {
                                item_idx: item_index,
                                part_idx: part_index,
                            },
                            image_bytes: image_value_bytes(&blank, url),
                            placeholder_bytes: ordered_placeholder_bytes,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // Evict oldest-first until the body fits again, keeping the newest images.
    let mut running = current_bytes;
    let mut evicted = 0usize;
    for image in images {
        if running <= target_bytes {
            break;
        }
        let mut image_bytes = image.image_bytes;
        let mut placeholder_bytes = image.placeholder_bytes;
        let replaced = match image.location {
            InlineImageLocation::User { item_idx, part_idx } => {
                if let ConversationItem::User(user) = &mut conversation[item_idx]
                    && let Some(part) = user.content.get_mut(part_idx)
                {
                    *part = content_placeholder.clone();
                    true
                } else {
                    false
                }
            }
            InlineImageLocation::ToolResultImage { item_idx } => {
                image_bytes = conversation_body_bytes(&conversation[item_idx..=item_idx]);
                if let ConversationItem::ToolResult(result) = &mut conversation[item_idx]
                    && let Some(image_index) = result
                        .images
                        .iter()
                        .position(|part| matches!(part, ContentPart::Image { .. }))
                {
                    result.images.remove(image_index);
                    if !result.content.contains(TOOL_IMAGE_COMPACT_NOTE) {
                        result.content =
                            format!("{}\n\n{TOOL_IMAGE_COMPACT_NOTE}", result.content).into();
                    }
                    placeholder_bytes = conversation_body_bytes(&conversation[item_idx..=item_idx]);
                    true
                } else {
                    false
                }
            }
            InlineImageLocation::ToolResultOrdered { item_idx, part_idx } => {
                if let ConversationItem::ToolResult(result) = &mut conversation[item_idx]
                    && let Some(part) = result.ordered_content.get_mut(part_idx)
                {
                    *part = ordered_placeholder.clone();
                    true
                } else {
                    false
                }
            }
            InlineImageLocation::CustomToolOutput { item_idx, part_idx } => {
                if let ConversationItem::CustomToolOutput(output) = &mut conversation[item_idx]
                    && let Some(part) = output.content.get_mut(part_idx)
                {
                    *part = ordered_placeholder.clone();
                    true
                } else {
                    false
                }
            }
        };
        if replaced {
            if image_bytes >= placeholder_bytes {
                running -= image_bytes - placeholder_bytes;
            } else {
                running += placeholder_bytes - image_bytes;
            }
            evicted += 1;
        }
    }

    ImageEvictionOutcome {
        evicted,
        body_bytes_after: running,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::ToolCall;

    #[test]
    fn native_image_outputs_preserve_identity_and_opaque_siblings() {
        use xai_grok_sampling_types::{CustomToolOutputImageDetail, CustomToolOutputItem};

        let image_part = CustomToolOutputContent::Image {
            url: format!("data:image/png;base64,{}", "A".repeat(2_000)).into(),
            detail: CustomToolOutputImageDetail::Original,
        };
        let mut custom = CustomToolOutputItem::new("custom-call", [image_part.clone()]);
        custom.item_id = Some("native-item".into());
        custom.name = Some("exec".into());
        let mut reasoning = xai_grok_sampling_types::synthesized_reasoning_item("reasoning");
        reasoning.encrypted_content = Some("opaque-encrypted-content".into());
        let history = vec![
            ConversationItem::tool_result_with_ordered_content("ordered-call", vec![image_part]),
            ConversationItem::custom_tool_output(custom),
            ConversationItem::Reasoning(reasoning),
        ];
        let canonical = serde_json::to_vec(&history).unwrap();
        let budgeted = apply_image_budget_with_limits(history.clone(), 1, 0);
        assert_eq!(budgeted.outcome.inline_images, 2);
        assert_eq!(budgeted.outcome.evicted, 2);
        assert_eq!(budgeted.outcome.body_bytes, canonical.len());
        assert_eq!(
            budgeted.outcome.body_bytes_after,
            serde_json::to_vec(&budgeted.items).unwrap().len()
        );
        assert_eq!(serde_json::to_vec(&history).unwrap(), canonical);
        assert_eq!(
            serde_json::to_value(&budgeted.items[2]).unwrap(),
            serde_json::to_value(&history[2]).unwrap()
        );
        assert!(
            matches!(&budgeted.items[0], ConversationItem::ToolResult(result)
            if result.tool_call_id == "ordered-call"
                && matches!(result.ordered_content.as_slice(), [CustomToolOutputContent::Text { text }] if text.as_ref() == IMAGE_COMPACT_PLACEHOLDER))
        );
        assert!(
            matches!(&budgeted.items[1], ConversationItem::CustomToolOutput(output)
            if output.call_id == "custom-call"
                && output.item_id.as_deref() == Some("native-item")
                && output.name.as_deref() == Some("exec")
                && matches!(output.content.as_slice(), [CustomToolOutputContent::Text { text }] if text.as_ref() == IMAGE_COMPACT_PLACEHOLDER))
        );
    }

    fn image(url: impl Into<String>) -> ContentPart {
        ContentPart::Image {
            url: url.into().into(),
        }
    }

    fn data_image(byte: char, bytes: usize) -> ContentPart {
        let prefix = "data:image/png;base64,";
        image(format!(
            "{prefix}{}",
            byte.to_string().repeat(bytes - prefix.len())
        ))
    }

    fn mixed_history() -> Vec<ConversationItem> {
        vec![
            ConversationItem::user_with_parts(vec![
                ContentPart::Text {
                    text: "old user".into(),
                },
                data_image('U', 500),
            ]),
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: r#"{"target_file":"image.png"}"#.into(),
            }]),
            ConversationItem::tool_result_with_images(
                "call-1",
                "tool text",
                vec![
                    ContentPart::Text {
                        text: "metadata".into(),
                    },
                    data_image('A', 500),
                    data_image('B', 500),
                    data_image('C', 500),
                ],
            ),
            ConversationItem::user_with_parts(vec![data_image('N', 500)]),
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "call-2".into(),
                name: "read_file".into(),
                arguments: r#"{"target_file":"new.png"}"#.into(),
            }]),
            ConversationItem::tool_result_with_images(
                "call-2",
                "new tool text",
                vec![data_image('Z', 500)],
            ),
        ]
    }

    fn expected_after_three_evictions() -> Vec<ConversationItem> {
        let mut expected = mixed_history();
        let ConversationItem::User(user) = &mut expected[0] else {
            unreachable!()
        };
        user.content[1] = ContentPart::Text {
            text: IMAGE_COMPACT_PLACEHOLDER.into(),
        };
        let ConversationItem::ToolResult(tool_result) = &mut expected[2] else {
            unreachable!()
        };
        tool_result.images.remove(1);
        tool_result.images.remove(1);
        tool_result.content = format!("tool text\n\n{TOOL_IMAGE_COMPACT_NOTE}").into();
        expected
    }

    #[test]
    fn mixed_escaped_urls_measure_exactly() {
        let history = vec![
            ConversationItem::user_with_parts(vec![image("https://example.com/a\"b\\c\n")]),
            ConversationItem::tool_result_with_images(
                "call-1",
                "tool",
                vec![
                    image("https://example.com/d\"e\\f\t"),
                    image("data:image/\"png\\escaped;base64,AAAA"),
                ],
            ),
        ];
        assert_eq!(
            conversation_body_bytes(&history),
            serde_json::to_vec(&history).unwrap().len()
        );
    }

    #[test]
    fn below_trigger_preserves_complete_history() {
        let history = mixed_history();
        let expected = serde_json::to_value(&history).unwrap();
        let budgeted = apply_image_budget(history);
        assert_eq!(
            budgeted.outcome,
            ImageBudgetOutcome {
                body_bytes: budgeted.outcome.body_bytes,
                body_bytes_after: budgeted.outcome.body_bytes,
                inline_images: 6,
                needs_image_compaction: false,
                evicted: 0,
            }
        );
        assert_eq!(serde_json::to_value(budgeted.items).unwrap(), expected);
        assert_eq!(IMAGE_COMPACT_TRIGGER_BYTES, 47 * 1024 * 1024);
        assert_eq!(IMAGE_COMPACT_RECLAIM_TARGET_BYTES, 25 * 1024 * 1024);
    }

    #[test]
    fn partial_tool_result_eviction_is_exact_and_ordered() {
        let expected = expected_after_three_evictions();
        let target = conversation_body_bytes(&expected);
        let budgeted = apply_image_budget_with_limits(mixed_history(), 1, target);
        assert_eq!(
            serde_json::to_value(&budgeted.items).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(
            budgeted.outcome,
            ImageBudgetOutcome {
                body_bytes: conversation_body_bytes(&mixed_history()),
                body_bytes_after: target,
                inline_images: 6,
                needs_image_compaction: true,
                evicted: 3,
            }
        );
        assert_eq!(
            budgeted.outcome.body_bytes_after,
            serde_json::to_vec(&budgeted.items).unwrap().len()
        );
        let ConversationItem::ToolResult(tool_result) = &budgeted.items[2] else {
            unreachable!()
        };
        assert_eq!(tool_result.tool_call_id, "call-1");
        assert_eq!(
            tool_result.content.as_ref(),
            format!("tool text\n\n{TOOL_IMAGE_COMPACT_NOTE}")
        );
        assert_eq!(
            tool_result.content.matches(TOOL_IMAGE_COMPACT_NOTE).count(),
            1
        );
        assert!(
            matches!(&tool_result.images[..], [ContentPart::Text { text }, ContentPart::Image { url }] if text.as_ref() == "metadata" && url.contains('C'))
        );
        assert!(
            matches!(&budgeted.items[3], ConversationItem::User(user) if matches!(user.content.first(), Some(ContentPart::Image { url }) if url.contains('N')))
        );
        assert!(
            matches!(&budgeted.items[5], ConversationItem::ToolResult(result) if matches!(result.images.first(), Some(ContentPart::Image { url }) if url.contains('Z')))
        );
    }

    #[test]
    fn final_tool_image_removal_reports_exact_omitted_field_size() {
        let history = vec![ConversationItem::tool_result_with_images(
            "call-1",
            "tool",
            vec![data_image('A', 500)],
        )];
        let expected = vec![ConversationItem::tool_result(
            "call-1",
            format!("tool\n\n{TOOL_IMAGE_COMPACT_NOTE}"),
        )];
        let target = conversation_body_bytes(&expected);
        let budgeted = apply_image_budget_with_limits(history, 1, target);
        assert_eq!(
            serde_json::to_value(&budgeted.items).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(budgeted.outcome.evicted, 1);
        assert_eq!(budgeted.outcome.body_bytes_after, target);
        assert_eq!(target, serde_json::to_vec(&budgeted.items).unwrap().len());
    }

    #[test]
    fn production_threshold_reclaims_a_batch_to_low_water_mark() {
        let image_bytes = 2 * 1024 * 1024;
        let image_count = IMAGE_COMPACT_TRIGGER_BYTES / image_bytes + 2;
        let history = (0..image_count)
            .map(|index| {
                ConversationItem::user_with_parts(vec![data_image(
                    char::from(b'A' + (index % 26) as u8),
                    image_bytes,
                )])
            })
            .collect();
        let budgeted = apply_image_budget(history);

        assert!(budgeted.outcome.body_bytes >= IMAGE_COMPACT_TRIGGER_BYTES);
        assert!(budgeted.outcome.body_bytes_after <= IMAGE_COMPACT_RECLAIM_TARGET_BYTES);
        assert!(budgeted.outcome.evicted > image_count / 4);
        assert!(matches!(
            budgeted.items.last(),
            Some(ConversationItem::User(user))
                if matches!(user.content.first(), Some(ContentPart::Image { .. }))
        ));
        assert_eq!(
            budgeted.outcome.body_bytes_after,
            serde_json::to_vec(&budgeted.items).unwrap().len()
        );
    }

    #[test]
    fn trigger_and_reclaim_boundaries_are_inclusive() {
        let history = vec![ConversationItem::user_with_parts(vec![data_image(
            'A', 500,
        )])];
        let bytes = conversation_body_bytes(&history);

        let at_trigger = apply_image_budget_with_limits(history.clone(), bytes, bytes);
        assert!(at_trigger.outcome.needs_image_compaction);
        assert_eq!(at_trigger.outcome.evicted, 0);
        assert_eq!(
            serde_json::to_value(at_trigger.items).unwrap(),
            serde_json::to_value(&history).unwrap()
        );

        let below_trigger = apply_image_budget_with_limits(history.clone(), bytes + 1, 0);
        assert!(!below_trigger.outcome.needs_image_compaction);
        assert_eq!(below_trigger.outcome.evicted, 0);
        assert_eq!(
            serde_json::to_value(below_trigger.items).unwrap(),
            serde_json::to_value(history).unwrap()
        );
    }
}
