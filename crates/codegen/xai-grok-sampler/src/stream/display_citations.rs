//! Strip OpenAI Responses display-citation markers from assistant text.
//!
//! Hosted tools (web search, file search, etc.) train models to emit inline
//! citation widgets using Private Use Area sentinels:
//!
//! ```text
//! \u{e200}cite\u{e202}turn1search0\u{e202}turn7search0\u{e201}
//! ```
//!
//! These are intended for clients that render footnotes/links. Terminal UIs
//! should hide them. See
//! <https://developers.openai.com/api/docs/guides/citation-formatting>.

/// Opens a display citation widget.
pub const CITATION_START: char = '\u{e200}';
/// Closes a display citation widget.
pub const CITATION_STOP: char = '\u{e201}';
/// Separates fields inside a display citation widget.
pub const CITATION_DELIMITER: char = '\u{e202}';

/// Streaming filter that drops complete and in-progress display citation
/// widgets while passing through ordinary text unchanged.
///
/// Chunk boundaries are handled by buffering from [`CITATION_START`] until
/// [`CITATION_STOP`] (or end of stream). Unterminated citations at EOF are
/// dropped so raw PUA markers never leak into the UI.
#[derive(Debug, Default, Clone)]
pub struct DisplayCitationFilter {
    /// `true` while buffering inside a citation that started with
    /// [`CITATION_START`] and has not yet seen [`CITATION_STOP`].
    in_citation: bool,
}

impl DisplayCitationFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a text delta and return the visible remainder (citations removed).
    pub fn push(&mut self, chunk: &str) -> String {
        if chunk.is_empty() {
            return String::new();
        }

        // Fast path: no citation start and not mid-widget.
        if !self.in_citation && !chunk.contains(CITATION_START) {
            // Also drop stray stop/delimiter PUA if they appear alone — rare
            // but meaningless without a start. Keep them only if we are not
            // filtering broadly; OpenAI never emits them outside widgets.
            if !chunk.contains(CITATION_STOP) && !chunk.contains(CITATION_DELIMITER) {
                return chunk.to_string();
            }
        }

        let mut out = String::with_capacity(chunk.len());
        for ch in chunk.chars() {
            if self.in_citation {
                if ch == CITATION_STOP {
                    self.in_citation = false;
                }
                // Drop every char inside the widget, including STOP.
                continue;
            }
            if ch == CITATION_START {
                self.in_citation = true;
                continue;
            }
            // Orphan delimiter/stop characters are not meaningful outside a
            // widget; hide them so partial multi-chunk widgets that lost their
            // start still do not paint tofu glyphs.
            if ch == CITATION_STOP || ch == CITATION_DELIMITER {
                continue;
            }
            out.push(ch);
        }
        out
    }

    /// Finish the stream. Drops any unterminated citation buffer.
    pub fn finish(&mut self) {
        self.in_citation = false;
    }

    /// Whether the filter is currently buffering a citation.
    #[cfg(test)]
    pub fn is_in_citation(&self) -> bool {
        self.in_citation
    }
}

/// Remove all display citation widgets from a complete string.
pub fn strip_display_citations(text: &str) -> String {
    let mut filter = DisplayCitationFilter::new();
    let out = filter.push(text);
    // Unterminated widgets are already withheld while `in_citation`; finish
    // only clears the flag so the next independent strip starts clean.
    filter.finish();
    out
}

/// Strip display citations from assistant message text in conversation items.
pub fn strip_display_citations_in_items(items: &mut [xai_grok_sampling_types::ConversationItem]) {
    use xai_grok_sampling_types::ConversationItem;

    for item in items {
        if let ConversationItem::Assistant(assistant) = item {
            let cleaned = strip_display_citations(assistant.content.as_ref());
            if cleaned.as_str() != assistant.content.as_ref() {
                assistant.content = cleaned.into();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cite(body: &str) -> String {
        format!("{CITATION_START}cite{CITATION_DELIMITER}{body}{CITATION_STOP}")
    }

    #[test]
    fn strips_single_web_search_citation() {
        let raw = format!(
            "Apple collects crash reports. {} More text.",
            cite("turn1search0")
        );
        assert_eq!(
            strip_display_citations(&raw),
            "Apple collects crash reports.  More text."
        );
    }

    #[test]
    fn strips_multi_source_citation_from_session_log() {
        // Exact shape from the Workouts-iOS session log.
        let raw = format!(
            "reports. {CITATION_START}cite{CITATION_DELIMITER}turn1search0\
             {CITATION_DELIMITER}turn7search0{CITATION_STOP}\n\n**Validation**"
        );
        assert_eq!(strip_display_citations(&raw), "reports. \n\n**Validation**");
    }

    #[test]
    fn leaves_plain_text_unchanged() {
        let s = "Hello world — no citations here.";
        assert_eq!(strip_display_citations(s), s);
    }

    #[test]
    fn streaming_across_chunk_boundaries() {
        let mut f = DisplayCitationFilter::new();
        let a = f.push("Hello ");
        assert_eq!(a, "Hello ");

        let marker = cite("turn1search0");
        let mid = marker.len() / 2;
        let b = f.push(&marker[..mid]);
        assert_eq!(b, "");
        assert!(f.is_in_citation());

        let c = f.push(&marker[mid..]);
        assert_eq!(c, "");
        assert!(!f.is_in_citation());

        let d = f.push(" world");
        assert_eq!(d, " world");
        f.finish();
    }

    #[test]
    fn streaming_char_by_char() {
        let mut f = DisplayCitationFilter::new();
        let input = format!("x{}y", cite("turn0file0"));
        let mut out = String::new();
        for ch in input.chars() {
            out.push_str(&f.push(&ch.to_string()));
        }
        f.finish();
        assert_eq!(out, "xy");
    }

    #[test]
    fn drops_unterminated_citation_at_eof() {
        let mut f = DisplayCitationFilter::new();
        let visible = f.push(&format!(
            "keep {CITATION_START}cite{CITATION_DELIMITER}turn0"
        ));
        assert_eq!(visible, "keep ");
        assert!(f.is_in_citation());
        f.finish();
        assert!(!f.is_in_citation());
    }

    #[test]
    fn strips_file_and_line_locator_forms() {
        let with_locator = format!(
            "see {CITATION_START}cite{CITATION_DELIMITER}turn0file0\
             {CITATION_DELIMITER}L8-L13{CITATION_STOP} here"
        );
        assert_eq!(strip_display_citations(&with_locator), "see  here");
    }

    #[test]
    fn strip_in_items_mutates_assistant_only() {
        use xai_grok_sampling_types::ConversationItem;

        let mut items = vec![
            ConversationItem::user("hi"),
            ConversationItem::assistant(format!("answer {}", cite("turn1search0"))),
        ];
        strip_display_citations_in_items(&mut items);
        match &items[1] {
            ConversationItem::Assistant(a) => assert_eq!(a.content.as_ref(), "answer "),
            other => panic!("expected assistant, got {other:?}"),
        }
        match &items[0] {
            ConversationItem::User(u) => match &u.content[0] {
                xai_grok_sampling_types::ContentPart::Text { text } => {
                    assert_eq!(text.as_ref(), "hi");
                }
                other => panic!("expected text part, got {other:?}"),
            },
            other => panic!("expected user, got {other:?}"),
        }
    }
}
