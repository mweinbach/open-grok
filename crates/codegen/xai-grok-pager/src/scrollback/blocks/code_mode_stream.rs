//! CodeModeStreamBlock — ephemeral live view of a streaming Code Mode
//! transport call (`exec` / `wait`).
//!
//! While the model is writing a transport call, its payload (raw JavaScript
//! for `exec`, JSON arguments for `wait`) renders here instead of the generic
//! writing-tool spinner. The block is never persisted and never survives to
//! a completed tool card: transport calls are UI-hidden by contract, so the
//! tracker removes the entry once the call lands or the stream moves on.
//!
//! Payload text arrives pre-truncated by the tracker (cap + tail); this block
//! only displays what it is given.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode,
};
use crate::theme::Theme;

/// Which Code Mode transport tool this live view belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeModeStreamTool {
    /// `exec` — raw JavaScript cell source.
    Exec,
    /// `wait` — JSON arguments (`cell_id`, timeouts, …).
    Wait,
}

impl CodeModeStreamTool {
    /// Spinner-style present-progressive header label.
    pub fn header_label(self) -> &'static str {
        match self {
            Self::Exec => "Writing exec cell…",
            Self::Wait => "Writing wait call…",
        }
    }
}

/// Body lines shown in the truncated (default) live view. The full payload
/// stays available via expand; the cap that bounds memory lives in the
/// tracker, while this bound keeps the default render cheap.
const LIVE_TAIL_LINES: usize = 8;

/// Live, ephemeral streaming payload for a Code Mode transport tool call.
#[derive(Debug, Clone)]
pub struct CodeModeStreamBlock {
    tool: CodeModeStreamTool,
    payload: String,
    /// Characters dropped from the head of the payload by the tracker's
    /// cap+tail truncation, so the view can say the body is partial.
    dropped_chars: u64,
}

impl CodeModeStreamBlock {
    pub fn new(tool: CodeModeStreamTool, payload: impl Into<String>, dropped_chars: u64) -> Self {
        Self {
            tool,
            payload: payload.into(),
            dropped_chars,
        }
    }

    /// Replace the displayed payload (tracker sends the full capped buffer).
    pub fn set_payload(&mut self, payload: &str, dropped_chars: u64) {
        self.payload.clear();
        self.payload.push_str(payload);
        self.dropped_chars = dropped_chars;
    }

    /// The retained payload text.
    pub fn payload(&self) -> &str {
        &self.payload
    }

    /// Characters dropped from the head by the tracker's cap+tail truncation.
    pub fn dropped_chars(&self) -> u64 {
        self.dropped_chars
    }

    pub fn tool(&self) -> CodeModeStreamTool {
        self.tool
    }

    fn header_line(&self) -> Line<'static> {
        let theme = Theme::current();
        Line::from(Span::styled(
            self.tool.header_label().to_string(),
            theme.muted().add_modifier(Modifier::BOLD),
        ))
    }

    /// Styled body lines for the retained payload, longest-first truncation
    /// handled by the caller (`render_truncated` shows only the tail lines).
    fn body_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if self.dropped_chars > 0 {
            lines.push(Line::from(Span::styled(
                format!("… +{} chars earlier", self.dropped_chars),
                Theme::current().dim(),
            )));
        }
        for line in self.payload.lines() {
            lines.push(Line::from(line.to_string()));
        }
        if self.payload.ends_with('\n') {
            // Preserve a trailing blank line so growth reads naturally.
            lines.push(Line::from(""));
        }
        lines
    }
}

impl BlockContent for CodeModeStreamBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        match ctx.mode {
            DisplayMode::Collapsed => {
                let line = crate::render::line_utils::truncate_line(
                    self.header_line(),
                    ctx.content_width(),
                );
                BlockOutput {
                    lines: vec![BlockLine::separator(line)],
                }
            }
            DisplayMode::Truncated => {
                let mut body = self.body_lines();
                let overflow = body.len().saturating_sub(LIVE_TAIL_LINES);
                if overflow > 0 {
                    body.drain(..overflow);
                    body.insert(0, Line::from(Span::styled("…", Theme::current().dim())));
                }
                let mut lines = Vec::with_capacity(body.len() + 2);
                lines.push(BlockLine::separator(self.header_line()));
                lines.extend(body.into_iter().map(BlockLine::styled));
                BlockOutput { lines }
            }
            DisplayMode::Expanded => {
                let mut lines = Vec::with_capacity(1 + self.payload.lines().count() + 2);
                lines.push(BlockLine::separator(self.header_line()));
                lines.extend(self.body_lines().into_iter().map(BlockLine::styled));
                BlockOutput { lines }
            }
        }
    }

    fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        None
    }

    fn background(&self, _ctx: &BlockContext) -> BlockBackground {
        BlockBackground::None
    }

    fn has_vpad_for(&self, _appearance: &crate::appearance::AppearanceConfig) -> bool {
        false
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Truncated
    }

    fn collapse_mode(&self, is_running: bool) -> DisplayMode {
        if is_running {
            DisplayMode::Truncated
        } else {
            DisplayMode::Collapsed
        }
    }

    fn next_fold_mode(&self, current: DisplayMode, _is_running: bool) -> DisplayMode {
        match current {
            DisplayMode::Collapsed | DisplayMode::Truncated => DisplayMode::Expanded,
            DisplayMode::Expanded => DisplayMode::Collapsed,
        }
    }

    fn finished_display_mode(&self) -> Option<DisplayMode> {
        Some(DisplayMode::Collapsed)
    }

    fn is_groupable(&self) -> bool {
        true
    }

    fn preamble(&self, _ctx: &BlockContext) -> Option<ratatui::text::Text<'static>> {
        Some(ratatui::text::Text::from(self.header_line()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::AppearanceConfig;
    use ratatui::style::Style;

    fn ctx(mode: DisplayMode, width: u16) -> BlockContext {
        BlockContext {
            mode,
            is_running: false,
            width,
            raw: false,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        }
    }

    fn plain_text(out: &BlockOutput) -> String {
        out.lines
            .iter()
            .map(|l| crate::scrollback::types::line_plain_text(&l.content))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn exec_header_and_payload_render_in_truncated_view() {
        let block = CodeModeStreamBlock::new(CodeModeStreamTool::Exec, "let x = 1;", 0);
        let out = block.output(&ctx(DisplayMode::Truncated, 60));
        let text = plain_text(&out);
        assert!(text.contains("Writing exec cell…"), "{text:?}");
        assert!(text.contains("let x = 1;"), "{text:?}");
    }

    #[test]
    fn wait_header_labels_json_args() {
        let block = CodeModeStreamBlock::new(CodeModeStreamTool::Wait, r#"{"cell_id":"c1"}"#, 0);
        let out = block.output(&ctx(DisplayMode::Truncated, 60));
        let text = plain_text(&out);
        assert!(text.contains("Writing wait call…"), "{text:?}");
        assert!(text.contains("\"cell_id\""), "{text:?}");
    }

    #[test]
    fn truncated_view_shows_only_tail_lines_with_ellipsis() {
        let payload = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let block = CodeModeStreamBlock::new(CodeModeStreamTool::Exec, payload, 0);
        let out = block.output(&ctx(DisplayMode::Truncated, 60));
        let text = plain_text(&out);
        assert!(!text.contains("line0"), "head must be elided: {text:?}");
        assert!(text.contains("line29"), "tail must remain: {text:?}");
        assert!(text.contains('…'), "ellipsis marker expected: {text:?}");

        let expanded = plain_text(&block.output(&ctx(DisplayMode::Expanded, 60)));
        assert!(expanded.contains("line0"), "expanded keeps everything");
    }

    #[test]
    fn dropped_head_marker_renders_before_payload() {
        let block = CodeModeStreamBlock::new(CodeModeStreamTool::Exec, "tail", 120_000);
        let out = block.output(&ctx(DisplayMode::Expanded, 60));
        let text = plain_text(&out);
        assert!(
            text.contains("+120000 chars earlier"),
            "dropped-char marker missing: {text:?}"
        );
        assert!(text.find("+120000").unwrap() < text.find("tail").unwrap());
    }

    #[test]
    fn collapsed_view_is_header_only() {
        let block = CodeModeStreamBlock::new(CodeModeStreamTool::Exec, "secret-ish", 0);
        let out = block.output(&ctx(DisplayMode::Collapsed, 60));
        assert_eq!(out.lines.len(), 1);
        let text = plain_text(&out);
        assert!(text.contains("Writing exec cell…"));
        assert!(!text.contains("secret-ish"));
    }

    #[test]
    fn set_payload_replaces_content_and_dropped_count() {
        let mut block = CodeModeStreamBlock::new(CodeModeStreamTool::Wait, "{}", 0);
        block.set_payload("{\"cell_id\":", 5);
        assert_eq!(block.payload(), "{\"cell_id\":");
        let out = block.output(&ctx(DisplayMode::Expanded, 40));
        assert!(plain_text(&out).contains("+5 chars earlier"));

        block.set_payload("{\"cell_id\": \"c1\"}", 0);
        assert_eq!(block.dropped_chars(), 0);
    }

    #[test]
    fn style_is_plain_when_not_selected() {
        let _guard = crate::theme::cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let block = CodeModeStreamBlock::new(CodeModeStreamTool::Exec, "code()", 0);
        let out = block.output(&ctx(DisplayMode::Expanded, 40));
        let last = out.lines.last().expect("body line");
        assert!(
            last.content.spans.iter().all(|s| s.style.fg.is_none()),
            "body spans must inherit the default foreground"
        );
        assert!(Style::default().fg.is_none());
    }
}
