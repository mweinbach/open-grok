//! Popup dialog for creating a new worktree with an optional label.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::app_view::NewWorktreeDialogState;
use crate::theme::Theme;

/// Minimum dialog width (fits title + empty input + hints comfortably).
const MIN_DIALOG_WIDTH: u16 = 50;
const DIALOG_HEIGHT: u16 = 5;
/// Left/right padding inside the border (`inner_x = dialog.x + 2`).
const INNER_PAD: u16 = 4;
const LABEL_PREFIX: &str = "Name (optional): ";

/// Render the new-worktree popup dialog centered on screen.
///
/// The dialog grows with the typed label (up to the available terminal
/// width) so long names stay fully visible. When the terminal itself is
/// too narrow for the full name, the input scrolls to keep the cursor
/// (end of the label) in view, with a leading `…` when scrolled.
pub fn render_new_worktree_dialog(area: Rect, buf: &mut Buffer, state: &NewWorktreeDialogState) {
    let theme = Theme::current();

    let dialog_width = dialog_width_for(area.width, &state.label_input);

    if area.height < DIALOG_HEIGHT || area.width < 20 {
        // Too small to render — draw a minimal "resize" hint so the user
        // knows the dialog is still active and can press Esc to dismiss.
        if area.height >= 1 && area.width >= 16 {
            let hint = Line::from(Span::styled(
                "[Esc] to close",
                Style::default().fg(theme.gray_dim),
            ));
            hint.render(Rect::new(area.x, area.y, area.width.min(16), 1), buf);
        }
        return;
    }

    let [_, dialog_h, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(dialog_width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(area);

    let [_, dialog, _] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(DIALOG_HEIGHT),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(dialog_h);

    // Draw background
    let bg_style = Style::default().bg(theme.bg_dark);
    for y in dialog.y..dialog.y + dialog.height {
        for x in dialog.x..dialog.x + dialog.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_style(bg_style);
            }
        }
    }

    // Draw border
    let border_style = Style::default().fg(theme.gray_dim).bg(theme.bg_dark);
    // Top border
    if let Some(cell) = buf.cell_mut((dialog.x, dialog.y)) {
        cell.set_char('\u{256D}');
        cell.set_style(border_style);
    }
    for x in dialog.x + 1..dialog.x + dialog.width - 1 {
        if let Some(cell) = buf.cell_mut((x, dialog.y)) {
            cell.set_char('\u{2500}');
            cell.set_style(border_style);
        }
    }
    if let Some(cell) = buf.cell_mut((dialog.x + dialog.width - 1, dialog.y)) {
        cell.set_char('\u{256E}');
        cell.set_style(border_style);
    }
    // Bottom border
    let bottom = dialog.y + dialog.height - 1;
    if let Some(cell) = buf.cell_mut((dialog.x, bottom)) {
        cell.set_char('\u{2570}');
        cell.set_style(border_style);
    }
    for x in dialog.x + 1..dialog.x + dialog.width - 1 {
        if let Some(cell) = buf.cell_mut((x, bottom)) {
            cell.set_char('\u{2500}');
            cell.set_style(border_style);
        }
    }
    if let Some(cell) = buf.cell_mut((dialog.x + dialog.width - 1, bottom)) {
        cell.set_char('\u{256F}');
        cell.set_style(border_style);
    }
    // Side borders
    for y in dialog.y + 1..dialog.y + dialog.height - 1 {
        if let Some(cell) = buf.cell_mut((dialog.x, y)) {
            cell.set_char('\u{2502}');
            cell.set_style(border_style);
        }
        if let Some(cell) = buf.cell_mut((dialog.x + dialog.width - 1, y)) {
            cell.set_char('\u{2502}');
            cell.set_style(border_style);
        }
    }

    let inner_x = dialog.x + 2;
    let inner_width = dialog.width.saturating_sub(INNER_PAD);

    // Row 1: Title
    let title = Line::from(Span::styled(
        "New Worktree",
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    ));
    title.render(Rect::new(inner_x, dialog.y + 1, inner_width, 1), buf);

    // Row 2: Label input — grow with content; scroll when still too wide.
    let prefix_w = LABEL_PREFIX.width() as u16;
    let cursor_w = 1u16;
    let input_budget = inner_width
        .saturating_sub(prefix_w)
        .saturating_sub(cursor_w) as usize;
    let visible_input = visible_input_suffix(&state.label_input, input_budget);

    let prefix_span = Span::styled(LABEL_PREFIX, Style::default().fg(theme.gray_bright));
    let input_span = Span::styled(visible_input, Style::default().fg(theme.text_primary));
    let cursor_span = Span::styled("\u{2588}", Style::default().fg(theme.accent_user));
    let input_line = Line::from(vec![prefix_span, input_span, cursor_span]);
    input_line.render(Rect::new(inner_x, dialog.y + 2, inner_width, 1), buf);

    // Row 3: Hints
    let hints = Line::from(vec![
        Span::styled(
            "enter",
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" = create   ", Style::default().fg(theme.gray)),
        Span::styled(
            "esc",
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" = cancel", Style::default().fg(theme.gray)),
    ]);
    hints.render(Rect::new(inner_x, dialog.y + 3, inner_width, 1), buf);
}

/// Dialog width that fits the typed label, clamped to the available area.
fn dialog_width_for(area_width: u16, label: &str) -> u16 {
    let max_width = area_width.saturating_sub(4);
    // prefix + label + block cursor + inner pad
    let needed = (LABEL_PREFIX.width() + label.width() + 1 + INNER_PAD as usize) as u16;
    needed.max(MIN_DIALOG_WIDTH).min(max_width)
}

/// Return the visible portion of `label` for an end-anchored input field.
///
/// When `label` fits in `budget` columns, returns it unchanged. Otherwise
/// returns a leading `…` plus the suffix that fits, so the cursor at the
/// end of the label stays visible while typing a long name.
///
/// Walks Unicode grapheme clusters (not scalar values) so combining marks
/// and ZWJ sequences are never split across the scroll boundary.
fn visible_input_suffix(label: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if label.width() <= budget {
        return label.to_string();
    }
    if budget == 1 {
        return "…".to_string();
    }

    let suffix_budget = budget - 1; // reserve one column for leading …
    let mut width = 0usize;
    let mut start = label.len();
    let graphemes: Vec<(usize, &str)> = label.grapheme_indices(true).collect();
    for &(i, g) in graphemes.iter().rev() {
        let cw = UnicodeWidthStr::width(g);
        if width + cw > suffix_budget {
            break;
        }
        width += cw;
        start = i;
    }
    format!("…{}", &label[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn render_to_text(area: Rect, label: &str) -> String {
        let mut buf = Buffer::empty(area);
        let state = NewWorktreeDialogState {
            label_input: label.to_string(),
        };
        render_new_worktree_dialog(area, &mut buf, &state);
        let mut lines = Vec::new();
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            lines.push(row);
        }
        lines.join("\n")
    }

    #[test]
    fn empty_dialog_uses_minimum_width() {
        assert_eq!(dialog_width_for(120, ""), MIN_DIALOG_WIDTH);
        assert_eq!(dialog_width_for(40, ""), 36); // area.width - 4
    }

    #[test]
    fn dialog_grows_with_long_label() {
        let label = "a-very-long-worktree-name-that-exceeds-fifty";
        let width = dialog_width_for(120, label);
        assert!(
            width > MIN_DIALOG_WIDTH,
            "expected dialog wider than min for long label, got {width}"
        );
        // Full label + chrome must fit inside the grown dialog.
        let inner = width.saturating_sub(INNER_PAD) as usize;
        let needed = LABEL_PREFIX.width() + label.width() + 1;
        assert!(
            needed <= inner,
            "grown dialog inner={inner} should fit needed={needed}"
        );
    }

    #[test]
    fn dialog_clamps_to_terminal_width() {
        let label = "x".repeat(100);
        let width = dialog_width_for(60, &label);
        assert_eq!(width, 56); // 60 - 4
    }

    #[test]
    fn visible_suffix_keeps_end_when_scrolled() {
        let label = "abcdefghijklmnopqrstuvwxyz0123456789";
        let visible = visible_input_suffix(label, 10);
        assert!(
            visible.starts_with('…'),
            "expected leading ellipsis: {visible}"
        );
        assert!(
            visible.ends_with("0123456789") || visible.ends_with("123456789"),
            "expected end of label visible: {visible}"
        );
        assert_eq!(visible.width(), 10);
    }

    #[test]
    fn visible_suffix_unchanged_when_fits() {
        assert_eq!(visible_input_suffix("short", 20), "short");
    }

    #[test]
    fn visible_suffix_does_not_split_grapheme_clusters() {
        // "e" + combining acute (U+0301) is one grapheme; pad so we must scroll.
        let cluster = "e\u{0301}";
        let label = format!("{}{}", "x".repeat(20), cluster);
        let visible = visible_input_suffix(&label, 8);
        assert!(
            visible.starts_with('…'),
            "expected leading ellipsis: {visible}"
        );
        // Either the full cluster is present, or it was dropped as a unit —
        // never a lone combining mark after the ellipsis.
        let after_ellipsis = &visible[visible.char_indices().nth(1).map(|(i, _)| i).unwrap_or(0)..];
        assert!(
            !after_ellipsis.starts_with('\u{0301}'),
            "must not start scrolled suffix on a combining mark: {visible:?}"
        );
        if after_ellipsis.contains('e') {
            assert!(
                after_ellipsis.contains(cluster),
                "base 'e' must keep its combining mark: {visible:?}"
            );
        }
        assert!(visible.width() <= 8, "width overflow: {visible:?}");
    }

    #[test]
    fn long_name_fully_visible_on_wide_terminal() {
        let area = Rect::new(0, 0, 100, 20);
        let label = "biscuit-worktree-popup-long-name-fix";
        let text = render_to_text(area, label);
        assert!(
            text.contains(label),
            "full long name must be visible on a wide terminal:\n{text}"
        );
        assert!(text.contains("New Worktree"), "title missing:\n{text}");
    }

    #[test]
    fn long_name_end_visible_on_narrow_terminal() {
        // Terminal narrower than the full label — end (cursor side) must show.
        let area = Rect::new(0, 0, 40, 12);
        let label = "super-long-worktree-name-that-will-not-fit";
        let text = render_to_text(area, label);
        let tail = &label[label.len().saturating_sub(8)..];
        assert!(
            text.contains(tail),
            "end of long name must remain visible when scrolled:\n{text}"
        );
        assert!(
            text.contains('…') || text.contains(tail),
            "expected scrolled indicator or tail:\n{text}"
        );
    }
}
