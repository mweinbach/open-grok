//! Ephemeral, sanitized previews of nested tools inferred from Code Mode source.
//!
//! Transport names, JavaScript source, arguments, and wait payloads never enter
//! this block. Canonical ACP tool cards replace previews once dispatch begins.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode,
};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeModeStreamTool {
    Exec,
    Wait,
}

const LIVE_TAIL_LINES: usize = 8;
const MAX_INFERRED_NESTED_TOOLS: usize = 16;
const MAX_INFERRED_TOOL_NAME_BYTES: usize = 128;

#[derive(Debug, Clone)]
pub struct CodeModeStreamBlock {
    tool: CodeModeStreamTool,
    payload: String,
    dropped_chars: u64,
}

impl CodeModeStreamBlock {
    pub fn new(tool: CodeModeStreamTool, payload: impl Into<String>, dropped_chars: u64) -> Self {
        let mut block = Self {
            tool,
            payload: String::new(),
            dropped_chars,
        };
        block.set_payload(&payload.into(), dropped_chars);
        block
    }

    pub fn set_payload(&mut self, payload: &str, dropped_chars: u64) {
        self.payload = Self::nested_tool_names(self.tool, payload).join("\n");
        self.dropped_chars = dropped_chars;
    }

    pub fn payload(&self) -> &str {
        &self.payload
    }

    pub fn dropped_chars(&self) -> u64 {
        self.dropped_chars
    }

    pub fn tool(&self) -> CodeModeStreamTool {
        self.tool
    }

    pub fn nested_tool_names(tool: CodeModeStreamTool, payload: &str) -> Vec<String> {
        if tool != CodeModeStreamTool::Exec {
            return Vec::new();
        }

        let decoded_source = serde_json::from_str::<serde_json::Value>(payload)
            .ok()
            .and_then(|value| {
                value
                    .get("source")
                    .and_then(|source| source.as_str())
                    .map(str::to_owned)
            });
        let source = decoded_source.as_deref().unwrap_or(payload);
        let bytes = source.as_bytes();
        let mut names = Vec::new();
        let mut position = 0;

        while position < bytes.len() && names.len() < MAX_INFERRED_NESTED_TOOLS {
            let Some(relative) = source[position..].find("tools") else {
                break;
            };
            let start = position + relative;
            position = start + "tools".len();
            if start > 0 && is_identifier_byte(bytes[start - 1]) {
                continue;
            }
            while position < bytes.len() && bytes[position].is_ascii_whitespace() {
                position += 1;
            }

            let (name_start, name_end) = if bytes.get(position) == Some(&b'.') {
                position += 1;
                while position < bytes.len() && bytes[position].is_ascii_whitespace() {
                    position += 1;
                }
                let name_start = position;
                while position < bytes.len() && is_identifier_byte(bytes[position]) {
                    position += 1;
                }
                (name_start, position)
            } else if bytes.get(position) == Some(&b'[') {
                position += 1;
                while position < bytes.len() && bytes[position].is_ascii_whitespace() {
                    position += 1;
                }
                let Some(quote @ (b'\'' | b'"')) = bytes.get(position).copied() else {
                    continue;
                };
                position += 1;
                let name_start = position;
                while position < bytes.len() && is_identifier_byte(bytes[position]) {
                    position += 1;
                }
                let name_end = position;
                if bytes.get(position) != Some(&quote) {
                    continue;
                }
                position += 1;
                while position < bytes.len() && bytes[position].is_ascii_whitespace() {
                    position += 1;
                }
                if bytes.get(position) != Some(&b']') {
                    continue;
                }
                position += 1;
                (name_start, name_end)
            } else {
                continue;
            };

            if name_start == name_end || name_end - name_start > MAX_INFERRED_TOOL_NAME_BYTES {
                continue;
            }
            while position < bytes.len() && bytes[position].is_ascii_whitespace() {
                position += 1;
            }
            if bytes.get(position) == Some(&b'(') {
                let name = &source[name_start..name_end];
                if !matches!(name, "exec" | "wait") {
                    names.push(name.to_string());
                }
            }
        }

        names
    }

    pub fn primary_tool_name(&self) -> Option<&str> {
        self.payload.lines().next()
    }

    pub fn activity_label(&self) -> String {
        let count = self.payload.lines().count();
        match (count, self.primary_tool_name()) {
            (1, Some(name)) => nested_tool_label(name),
            (count, _) if count > 1 => format!("Preparing {count} tools…"),
            _ => "Preparing tools…".to_string(),
        }
    }

    fn header_line(&self) -> Line<'static> {
        let theme = Theme::current();
        Line::from(Span::styled(
            self.activity_label(),
            theme.muted().add_modifier(Modifier::BOLD),
        ))
    }

    fn body_lines(&self) -> Vec<Line<'static>> {
        if self.payload.lines().count() <= 1 {
            return Vec::new();
        }
        self.payload
            .lines()
            .map(|name| Line::from(nested_tool_label(name)))
            .collect()
    }
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn nested_tool_label(name: &str) -> String {
    crate::acp::tracker::WritingToolCall {
        tool_name: Some(name.to_string()),
        ordinal: std::num::NonZeroU32::MIN,
    }
    .label()
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
                let mut lines = Vec::with_capacity(1 + self.payload.lines().count());
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
    fn screenshot_exec_source_renders_only_inferred_nested_tool() {
        let source = "const results = await Promise.all([tools.run_terminal_command({command: 'secret command', description: 'Inspect repository'})]); text(JSON.stringify(results));";
        let block = CodeModeStreamBlock::new(CodeModeStreamTool::Exec, source, 0);

        for mode in [
            DisplayMode::Collapsed,
            DisplayMode::Truncated,
            DisplayMode::Expanded,
        ] {
            let text = plain_text(&block.output(&ctx(mode, 120)));
            assert!(text.contains("Writing command…"), "{text:?}");
            assert!(!text.contains("exec"), "{text:?}");
            assert!(!text.contains("Promise.all"), "{text:?}");
            assert!(!text.contains("secret command"), "{text:?}");
            assert!(!text.contains("JSON.stringify"), "{text:?}");
        }
    }

    #[test]
    fn wait_payload_never_enters_any_display_mode() {
        let block = CodeModeStreamBlock::new(CodeModeStreamTool::Wait, r#"{"cell_id":"c1"}"#, 0);
        assert!(block.payload().is_empty());
        for mode in [
            DisplayMode::Collapsed,
            DisplayMode::Truncated,
            DisplayMode::Expanded,
        ] {
            let text = plain_text(&block.output(&ctx(mode, 60)));
            assert!(!text.contains("wait"), "{text:?}");
            assert!(!text.contains("cell_id"), "{text:?}");
            assert!(!text.contains("c1"), "{text:?}");
        }
    }

    #[test]
    fn promise_all_renders_simultaneous_nested_tools_without_source() {
        let source = "await Promise.all([tools.read_file({file_path: '/secret'}), tools.grep_files({pattern: 'token'}), tools.mcp__ologs__get_profile({id: 'private'})]);";
        let block = CodeModeStreamBlock::new(CodeModeStreamTool::Exec, source, 0);
        let text = plain_text(&block.output(&ctx(DisplayMode::Expanded, 120)));

        assert!(text.contains("Preparing 3 tools…"), "{text:?}");
        assert!(text.contains("read_file"), "{text:?}");
        assert!(text.contains("grep_files"), "{text:?}");
        assert!(text.contains("ologs"), "{text:?}");
        assert!(!text.contains("Promise.all"), "{text:?}");
        assert!(!text.contains("/secret"), "{text:?}");
        assert!(!text.contains("token"), "{text:?}");
        assert!(!text.contains("private"), "{text:?}");
    }

    #[test]
    fn nested_tool_inference_normalizes_supported_property_access() {
        let source = r#"await tools . read_file ({}); tools['grep_files']({}); tools["mcp__server__run"]({}); nottools.fake({}); tools.exec({}); tools.wait({}); tools.partial"#;
        assert_eq!(
            CodeModeStreamBlock::nested_tool_names(CodeModeStreamTool::Exec, source),
            ["read_file", "grep_files", "mcp__server__run"]
        );
    }

    #[test]
    fn function_transport_envelope_infers_source_without_displaying_it() {
        let payload =
            serde_json::json!({"source": "await tools.apply_patch('sensitive patch')"}).to_string();
        let block = CodeModeStreamBlock::new(CodeModeStreamTool::Exec, payload, 0);
        let text = plain_text(&block.output(&ctx(DisplayMode::Expanded, 80)));
        assert!(text.contains("edit"), "{text:?}");
        assert!(!text.contains("source"), "{text:?}");
        assert!(!text.contains("sensitive patch"), "{text:?}");
    }

    #[test]
    fn inferred_nested_tools_and_visible_rows_are_bounded() {
        let source = (0..30)
            .map(|index| format!("tools.tool_{index}({{secret: '{index}'}})"))
            .collect::<Vec<_>>()
            .join(",");
        let block = CodeModeStreamBlock::new(CodeModeStreamTool::Exec, source, 120_000);
        assert_eq!(block.payload().lines().count(), MAX_INFERRED_NESTED_TOOLS);
        let out = block.output(&ctx(DisplayMode::Truncated, 60));
        let text = plain_text(&out);
        assert!(out.lines.len() <= LIVE_TAIL_LINES + 2, "{text:?}");
        assert!(text.contains("tool_15"), "{text:?}");
        assert!(!text.contains("120000"), "{text:?}");
        assert!(!text.contains("secret"), "{text:?}");
    }

    #[test]
    fn collapsed_view_is_header_only() {
        let block = CodeModeStreamBlock::new(
            CodeModeStreamTool::Exec,
            "tools.read_file({file_path: 'secret-ish'}); tools.grep_files({})",
            0,
        );
        let out = block.output(&ctx(DisplayMode::Collapsed, 60));
        assert_eq!(out.lines.len(), 1);
        let text = plain_text(&out);
        assert!(text.contains("Preparing 2 tools…"));
        assert!(!text.contains("secret-ish"));
    }

    #[test]
    fn set_payload_replaces_sanitized_nested_tools_and_dropped_count() {
        let mut block =
            CodeModeStreamBlock::new(CodeModeStreamTool::Exec, "tools.read_file({})", 0);
        block.set_payload("tools.grep_files({pattern: 'secret'})", 5);
        assert_eq!(block.payload(), "grep_files");
        assert_eq!(block.dropped_chars(), 5);
        assert!(!plain_text(&block.output(&ctx(DisplayMode::Expanded, 60))).contains("secret"));

        block.set_payload("tools.apply_patch('private')", 0);
        assert_eq!(block.payload(), "apply_patch");
        assert_eq!(block.dropped_chars(), 0);
    }

    #[test]
    fn partial_tool_call_is_not_inferred_until_opening_parenthesis() {
        let mut block = CodeModeStreamBlock::new(CodeModeStreamTool::Exec, "await tools.read_f", 0);
        assert!(block.payload().is_empty());
        block.set_payload("await tools.read_file", 0);
        assert!(block.payload().is_empty());
        block.set_payload("await tools.read_file({file_path: 'private'})", 0);
        assert_eq!(block.payload(), "read_file");
    }
}
