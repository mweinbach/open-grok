//! ThinkingBlock - displays agent thinking/reasoning content with markdown support.

use ratatui::style::{Color, Stylize};
use ratatui::text::{Line, Span, Text};

use crate::render::color::blend_line_with_default;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode,
};
use crate::theme::Theme;

use super::markdown_content::MarkdownContent;
use super::quote_bar::QuoteBarStrip;

/// Block displaying agent thinking content with markdown rendering.
///
/// Uses [`MarkdownContent`] for incremental markdown rendering with cached
/// word-wrapping, plus special display modes:
/// - **Collapsed**: Shows "Thought" or "Thought for Xs" if time is set
/// - **Truncated** (default): Shows "…" + last N lines
/// - **Expanded**: Full content
#[derive(Debug, Clone)]
pub struct ThinkingBlock {
    content: MarkdownContent,

    /// Optional elapsed time in milliseconds (from server).
    /// When set, collapsed view shows "Thought for Xs".
    elapsed_time_ms: Option<i64>,
    /// When the thinking block started (local timestamp for live elapsed).
    started_at: Option<std::time::Instant>,
}
impl ThinkingBlock {
    /// Create a new thinking block with complete text.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            content: MarkdownContent::new(text),
            elapsed_time_ms: None,
            started_at: None,
        }
    }

    /// Create an empty block for streaming.
    pub fn streaming() -> Self {
        Self {
            content: MarkdownContent::streaming(),
            elapsed_time_ms: None,
            started_at: Some(std::time::Instant::now()),
        }
    }

    /// Create an empty streaming block for **historical replay**.
    ///
    /// Unlike [`streaming`], this does NOT arm the local `started_at` timer.
    /// Replay re-applies a whole session's persisted chunks back-to-back in
    /// microseconds, so a local wall-clock timer would freeze to ~0ms in
    /// [`finish`] and render a bogus "Thought for 0.0s". With no local timer
    /// `finish` leaves `elapsed_time_ms` unset, so
    /// [`ScrollbackState::finish_running_with_time`] falls back to the
    /// server-reported elapsed (derived from `agentTimestampMs - streamStartMs`),
    /// which is the real duration the user originally experienced.
    pub fn streaming_replay() -> Self {
        Self {
            content: MarkdownContent::streaming(),
            elapsed_time_ms: None,
            started_at: None,
        }
    }

    /// Push a streaming chunk of markdown text.
    pub fn push_chunk(&mut self, chunk: &str) {
        self.content.push_chunk(chunk);
    }

    /// Push a chunk without rendering immediately.
    pub fn push_chunk_deferred(&mut self, chunk: &str) {
        self.content.push_chunk_deferred(chunk);
    }

    /// Finish streaming and do a full re-render for safety.
    ///
    /// Freezes the local elapsed time from `started_at` so the collapsed
    /// view shows the actual wall-clock duration the user experienced,
    /// not the server-reported delta.
    pub fn finish(&mut self) {
        self.content.finish();
        // Freeze local elapsed if no server time has been set.
        // The local timer (started_at → now) captures the full duration
        // from block creation to finish, which is what the user perceives.
        if self.elapsed_time_ms.is_none()
            && let Some(start) = self.started_at
        {
            self.elapsed_time_ms = Some(start.elapsed().as_millis() as i64);
        }
    }

    /// Get the source text.
    pub fn text(&self) -> String {
        self.content.text()
    }

    /// Get the elapsed thinking time in milliseconds.
    ///
    /// Returns server-reported time if available, otherwise live elapsed
    /// from `started_at` (for running thinking blocks).
    pub fn elapsed_time_ms(&self) -> Option<i64> {
        match self.elapsed_time_ms {
            Some(ms) => Some(ms),
            None => self
                .started_at
                .map(|start| start.elapsed().as_millis() as i64),
        }
    }

    /// Set the elapsed time (in milliseconds).
    ///
    /// When set, the collapsed view will show "Thought for Xs".
    pub fn set_elapsed_time_ms(&mut self, time_ms: Option<i64>) {
        self.elapsed_time_ms = time_ms;
    }

    /// Set the raw mode, re-rendering if it changed.
    pub fn set_raw_mode(&mut self, raw: bool) {
        self.content.set_raw_mode(raw);
    }

    /// Access the underlying markdown content (for viewer item building).
    pub fn content(&self) -> &MarkdownContent {
        &self.content
    }

    /// Mutable access to the underlying markdown content.
    pub fn content_mut(&mut self) -> &mut MarkdownContent {
        &mut self.content
    }

    /// Get copyable text for this block.
    ///
    /// When `raw` is true, returns the raw markdown source.
    /// When `raw` is false, returns the rendered text (styles stripped).
    pub fn copy_text(&self, raw: bool) -> String {
        if raw {
            self.content.text()
        } else {
            self.content.rendered_plain_text()
        }
    }

    /// Format elapsed time for display.
    fn format_time(&self) -> Option<String> {
        self.elapsed_time_ms.map(|ms| {
            let secs = ms as f64 / 1000.0;
            if secs < 60.0 {
                format!("{:.1}s", secs)
            } else {
                let mins = (secs / 60.0).floor() as u32;
                let remaining = secs - (mins as f64 * 60.0);
                format!("{}m{:.0}s", mins, remaining)
            }
        })
    }

    /// Build the header line: "Thinking..." (running) or "Thought for Xs" (done).
    ///
    /// Respects muted_collapsed: when collapsed and muting is on, uses muted style.
    /// When the entry is selected, the muted treatment is suppressed and
    /// the label is forced to the bright/primary style so the selected
    /// header reads as undimmed — same rule as the tool-call variants.
    fn header_line(&self, ctx: &BlockContext) -> Line<'static> {
        let theme = Theme::current();
        let tool_cfg = &ctx.appearance.scrollback.blocks.tool;
        let thinking_cfg = &ctx.appearance.scrollback.blocks.thinking;
        let is_collapsed = ctx.mode == DisplayMode::Collapsed;
        let is_muted = is_collapsed && ctx.mute_when_collapsed(tool_cfg.muted_collapsed);

        // Bright on selection or config opt-in, but never while muted
        // — keeps legacy-ConHost collapse uniformly muted.
        let use_bright = !is_muted && (ctx.is_selected || thinking_cfg.header_bright);

        let label_style = if use_bright {
            theme.primary().bold()
        } else {
            theme.muted().bold()
        };

        let detail_style = theme.muted();

        if ctx.is_running {
            Line::from(Span::styled("Thinking…", label_style))
        } else if let Some(time_str) = self.format_time() {
            Line::from(vec![
                Span::styled("Thought", label_style),
                Span::styled(format!(" for {time_str}"), detail_style),
            ])
        } else {
            Line::from(Span::styled("Thought", label_style))
        }
    }

    /// Render the collapsed view: header line only, truncated to fit.
    fn render_collapsed(&self, ctx: &BlockContext) -> BlockOutput {
        let line = self.header_line(ctx);
        let line = crate::render::line_utils::truncate_line(line, ctx.content_width());
        BlockOutput {
            lines: vec![BlockLine::separator(line)],
        }
    }

    /// Prepend header + blank line to output, if header config is enabled.
    fn maybe_prepend_header(&self, mut output: BlockOutput, ctx: &BlockContext) -> BlockOutput {
        if ctx.appearance.scrollback.blocks.thinking.header {
            output.lines.insert(0, BlockLine::separator(Line::from("")));
            output
                .lines
                .insert(0, BlockLine::separator(self.header_line(ctx)));
        }
        output
    }

    /// One wrapped markdown line → selectable, blended [`BlockLine`].
    ///
    /// Quote-bar exclusion must run before blending: blending rewrites span
    /// fg colors, which would defeat the bar-style detection (it preserves
    /// span structure, so the computed span indices stay valid after it).
    fn thinking_body_line(
        line: &Line<'static>,
        joiner: &Option<String>,
        strip: &QuoteBarStrip,
        bg_base: Color,
        fg_default: Color,
        blend_factor: f32,
    ) -> BlockLine {
        let mut content = line.clone();
        let selectable = strip.selectable(&mut content);
        let blended = blend_line_with_default(content, bg_base, fg_default, blend_factor);
        let mut block_line = BlockLine::styled(blended)
            .with_selection_range(Some(0))
            .with_joiner(joiner.clone());
        block_line.selectable = selectable;
        block_line
    }

    /// Render truncated view: optional header + "…" + last N lines.
    fn render_truncated(&self, ctx: &BlockContext) -> BlockOutput {
        let config = &ctx.appearance.scrollback.blocks.thinking;
        let n = config.truncated_lines as usize;
        let width = ctx.width as usize;
        let blend_factor = config.bg_blend;
        let strip = QuoteBarStrip::new(!self.content.is_raw());

        self.content.with_wrapped_lines(width, |wrapped| {
            if wrapped.lines.is_empty() {
                return self.render_empty_placeholder(ctx);
            }

            let theme = Theme::current();
            let bg_base = theme.bg_base;
            let fg_default = theme.text_primary;

            let total = wrapped.lines.len();
            if total <= n {
                // Content fits within N lines, show all (with blending)
                let output = BlockOutput {
                    lines: wrapped
                        .lines
                        .iter()
                        .zip(wrapped.joiners.iter())
                        .map(|(line, joiner)| {
                            Self::thinking_body_line(
                                line,
                                joiner,
                                &strip,
                                bg_base,
                                fg_default,
                                blend_factor,
                            )
                        })
                        .collect(),
                };
                return self.maybe_prepend_header(output, ctx);
            }

            // Build truncated output: "…" + last N lines
            let theme = Theme::current();
            let mut output_lines = Vec::with_capacity(n + 1);

            // Ellipsis line
            let ellipsis = Line::from(Span::styled("…", theme.muted()));
            output_lines.push(ellipsis.into());

            // Last N lines (with blending)
            for i in (total - n)..total {
                output_lines.push(Self::thinking_body_line(
                    &wrapped.lines[i],
                    &wrapped.joiners[i],
                    &strip,
                    bg_base,
                    fg_default,
                    blend_factor,
                ));
            }

            self.maybe_prepend_header(
                BlockOutput {
                    lines: output_lines,
                },
                ctx,
            )
        })
    }

    /// Render expanded view: full content.
    fn render_expanded(&self, ctx: &BlockContext) -> BlockOutput {
        let config = &ctx.appearance.scrollback.blocks.thinking;
        let width = ctx.width as usize;
        let blend_factor = config.bg_blend;
        let strip = QuoteBarStrip::new(!self.content.is_raw());

        self.content.with_wrapped_lines(width, |wrapped| {
            if wrapped.lines.is_empty() {
                return self.render_empty_placeholder(ctx);
            }

            let theme = Theme::current();
            let bg_base = theme.bg_base;
            let fg_default = theme.text_primary;

            let output = BlockOutput {
                lines: wrapped
                    .lines
                    .iter()
                    .zip(wrapped.joiners.iter())
                    .map(|(line, joiner)| {
                        Self::thinking_body_line(
                            line,
                            joiner,
                            &strip,
                            bg_base,
                            fg_default,
                            blend_factor,
                        )
                    })
                    .collect(),
            };
            self.maybe_prepend_header(output, ctx)
        })
    }

    /// Placeholder for empty thinking block — shows the same header
    /// as collapsed mode ("Thinking…" or "Thought for Xs").
    fn render_empty_placeholder(&self, ctx: &BlockContext) -> BlockOutput {
        self.render_collapsed(ctx)
    }
}

impl BlockContent for ThinkingBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        match ctx.mode {
            DisplayMode::Collapsed => self.render_collapsed(ctx),
            DisplayMode::Truncated => self.render_truncated(ctx),
            DisplayMode::Expanded => self.render_expanded(ctx),
        }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        let cfg = &ctx.appearance.scrollback.blocks.thinking;
        if !cfg.accent_enabled {
            return None;
        }
        // No accent when collapsed — accent is only for expanded/truncated content.
        // TODO: revisit if we want accent in collapsed state with header enabled.
        if ctx.mode == DisplayMode::Collapsed {
            return None;
        }
        if cfg.animate && ctx.is_running {
            Some(AccentStyle::animated(cfg.accent))
        } else {
            Some(AccentStyle::static_color(cfg.accent))
        }
    }

    /// Thinking bullet: default (None) when not running, animated when running.
    /// This means collapsed thinking shows gray bullet, running thinking syncs with accent.
    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        if ctx.is_running {
            self.accent(ctx) // sync bullet with accent animation when running
        } else {
            None // default gray/primary
        }
    }

    fn background(&self, _ctx: &BlockContext) -> BlockBackground {
        BlockBackground::None
    }

    fn accent_background(&self, _ctx: &BlockContext) -> bool {
        false
    }

    fn has_vpad(&self, _ctx: &BlockContext) -> bool {
        false
    }

    fn has_raw_mode(&self) -> bool {
        true
    }

    fn is_foldable(&self) -> bool {
        true
    }

    fn next_fold_mode(&self, current: DisplayMode, is_running: bool) -> DisplayMode {
        if is_running {
            match current {
                DisplayMode::Collapsed | DisplayMode::Truncated => DisplayMode::Expanded,
                DisplayMode::Expanded => DisplayMode::Truncated,
            }
        } else {
            match current {
                DisplayMode::Collapsed => DisplayMode::Expanded,
                DisplayMode::Truncated | DisplayMode::Expanded => DisplayMode::Collapsed,
            }
        }
    }

    fn collapse_mode(&self, is_running: bool) -> DisplayMode {
        if is_running {
            DisplayMode::Truncated
        } else {
            DisplayMode::Collapsed
        }
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Truncated
    }

    fn finished_display_mode(&self) -> Option<DisplayMode> {
        Some(DisplayMode::Collapsed)
    }

    fn has_bullet(&self, ctx: &BlockContext) -> bool {
        let cfg = &ctx.appearance.scrollback.blocks.thinking;
        let has_header_visible = ctx.mode == DisplayMode::Collapsed || cfg.header;
        has_header_visible
            && ctx
                .appearance
                .scrollback
                .blocks
                .tool
                .bullet
                .char()
                .is_some()
    }

    fn is_groupable(&self) -> bool {
        true
    }

    fn preamble(&self, ctx: &BlockContext) -> Option<Text<'static>> {
        // Use expanded (bright) styling — not muted collapsed
        let bright_ctx = BlockContext {
            mode: DisplayMode::Expanded,
            ..ctx.clone()
        };
        Some(Text::from(self.header_line(&bright_ctx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::AppearanceConfig;
    use crate::scrollback::types::Selectable;

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

    #[test]
    fn collapsed_thinking_header_is_non_selectable() {
        let block = ThinkingBlock::new("hello world");
        let out = block.output(&ctx(DisplayMode::Collapsed, 40));
        assert_eq!(out.lines.len(), 1);
        assert!(matches!(out.lines[0].selectable, Selectable::None));
        assert_eq!(out.lines[0].selection_range, None);
    }

    #[test]
    fn prepended_thinking_header_is_non_selectable() {
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.thinking.header = true;
        let ctx = BlockContext {
            appearance,
            ..ctx(DisplayMode::Expanded, 40)
        };
        let block = ThinkingBlock::new("hello world");
        let out = block.output(&ctx);

        assert!(out.lines.len() >= 3);
        assert!(matches!(out.lines[0].selectable, Selectable::None));
        assert!(matches!(out.lines[1].selectable, Selectable::None));
        assert!(
            out.lines
                .iter()
                .skip(2)
                .all(|line| line.selection_range == Some(0))
        );
    }

    #[test]
    fn thinking_body_lines_keep_markdown_range_ids() {
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.thinking.header = false;
        let ctx = BlockContext {
            appearance,
            ..ctx(DisplayMode::Expanded, 10)
        };
        let block = ThinkingBlock::new("hello world this should wrap across lines");
        let out = block.output(&ctx);
        assert!(out.lines.len() > 1);
        assert!(out.lines.iter().all(|line| line.selection_range == Some(0)));
    }

    #[test]
    fn thinking_quote_line_selection_excludes_bar_prefix() {
        use crate::scrollback::types::{derive_selection_text, line_plain_text};

        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.thinking.header = false;
        let ctx = BlockContext {
            appearance,
            ..ctx(DisplayMode::Expanded, 40)
        };
        let block = ThinkingBlock::new("> QUOTE alpha");
        let out = block.output(&ctx);

        let line = out
            .lines
            .iter()
            .find(|l| line_plain_text(&l.content).contains("QUOTE"))
            .expect("quote line rendered");
        assert!(line_plain_text(&line.content).starts_with("│ "));
        assert!(matches!(line.selectable, Selectable::Spans(_)));
        assert_eq!(derive_selection_text(line), "QUOTE alpha");
    }
}
