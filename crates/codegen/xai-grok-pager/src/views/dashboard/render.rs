use indexmap::IndexMap;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::layout::{MIN_DASHBOARD_WIDTH, compute_layout};
use super::row::{DashboardRow, RowBadge, build_rows_with_roster, build_rows_with_workspace};
use super::state::{
    DashboardRowId, DashboardState, Filter, Focusable, Grouping, LocationPickerState, RenameDraft,
    RowState, SectionKey,
};
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::render::line_utils::{truncate_line, truncate_str};
use crate::theme::Theme;
use crate::util::format_time_ago;

/// Show each spinner frame for this many animation ticks.
/// The frames come from [`crate::glyphs::dot_spinner_frames`] so they degrade to an ASCII pulse on legacy Windows consoles.
const SPINNER_DIVISOR: u64 = 4;
/// How many ticks each phase of the `NeedsInput` bullet blink lasts.
/// At the ~30 Hz dashboard tick this toggles roughly every 0.33 s, about a 1.5 Hz blink.
const NEEDS_INPUT_BLINK_DIVISOR: u64 = 10;

fn ensure_peek_viewport_lifecycle(
    state: &mut DashboardState,
    agents: &mut IndexMap<AgentId, AgentView>,
) {
    if state.attached_agent.is_some() {
        return;
    }

    let Some(row) = state.peek.as_ref().map(|p| p.row.clone()) else {
        state.restore_peek_viewport(agents);
        return;
    };
    if state
        .peek_viewport
        .as_ref()
        .is_some_and(|lease| lease.row == row)
    {
        return;
    }
    if super::state::scrollback_available_for_row(&row, agents) {
        state.begin_peek_viewport(row, agents);
    } else {
        state.restore_peek_viewport(agents);
    }
}

/// Per-row visual height in cells: a title row, a secondary row, and a one-cell breathing gap.
const ROW_HEIGHT: u16 = 3;
/// Per-group-header visual height in cells: a label row and a one-cell breathing gap.
const GROUP_HEADER_HEIGHT: u16 = 2;

/// Promo upgrade CTA for the dashboard header, resolved through the shared slot gate by the producer (`app_view`).
#[derive(Clone, Copy)]
pub struct HeaderUpgradeCta<'a> {
    /// The `[label]` button text.
    pub label: &'a str,
    /// True when the promo is non-dismissible, so the `Ctrl+O` override applies.
    pub pinned: bool,
    /// The promo's trimmed `cta.caption` value; painted only when `pinned` is set.
    pub caption: Option<&'a str>,
}

/// Render the dashboard. Like `agent_view::draw`, returns the cursor position to place after the frame is committed.
///
/// When a permission request fires on an agent while the dashboard is open, this renderer DOES NOT auto-popup the `PermissionView` modal.
/// Instead, the row's state flips to `NeedsInput` (blinking yellow bullet and a yellow `Pending:` subtitle).
/// The user must press Space to peek (which routes the permission question and options into the peek panel) and then a number key to answer.
/// The dashboard is the top-level view, and random subagent permission requests should not obscure it.
#[allow(clippy::too_many_arguments)]
pub fn render_dashboard(
    buf: &mut Buffer,
    area: Rect,
    state: &mut DashboardState,
    agents: &mut IndexMap<AgentId, AgentView>,
    registry: &crate::actions::ActionRegistry,

    pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
    // Leader-mode session roster (FleetView)
    roster: &[crate::app::roster::RosterEntry],

    workspace_dashboard_enabled: bool,
    workspace_snapshot: Option<&xai_grok_dashboard_store::WorkspaceSnapshot>,

    dashboard_sessions_loading: bool,

    upgrade_cta: Option<HeaderUpgradeCta<'_>>,
) -> Option<(u16, u16)> {
    state.pinned_upgrade_cta_live = upgrade_cta.is_some_and(|cta| cta.pinned);

    let theme = Theme::current();

    state.last_area = area;

    buf.set_style(area, ratatui::style::Style::default().bg(theme.bg_base));

    let home = cached_home();
    let rows = if workspace_dashboard_enabled {
        workspace_snapshot
            .map(|snapshot| build_rows_with_workspace(agents, snapshot, home))
            .unwrap_or_default()
    } else {
        build_rows_with_roster(
            agents,
            &state.pinned,
            &state.reorder,
            state.grouping,
            &state.filter,
            home,
            roster,
        )
    };

    state.conversation_row_ids = if workspace_dashboard_enabled {
        Default::default()
    } else {
        roster
            .iter()
            .filter(|e| e.origin.kind == "conversation")
            .map(|e| e.session_id.clone())
            .collect()
    };
    state.reanchor_selection(&rows);

    // DO NOT GC pinned/reorder at render time

    //

    if state.attached_agent.is_some() {
        state.peek_close_rect = None;
        state.slash_dropdown_items_area = None;
        state.slash_dropdown_hit = Default::default();
        state.file_search_dropdown_items_area = None;
        state.dispatch_rect = None;
        let popup = popup_rect(area);
        let banner_h = popup.y.saturating_sub(area.y);
        let banner_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: banner_h,
        };
        render_dashboard_banner(buf, banner_area, &theme, &rows, state);

        return None;
    }

    let mut layout = compute_layout(area, false);
    let fixed = super::layout::chrome_overhead(area);
    let reply_text_w = layout.dispatch.width.saturating_sub(6);

    match state.selected.clone() {
        Some(sel) => match super::peek::compute_peek_fields(&sel, agents) {
            Some(fields) => {
                let question = fields.question.is_some();
                let peek_min = if question {
                    super::layout::PEEK_MIN_BOX_QUESTION
                } else {
                    super::layout::PEEK_MIN_BOX_LIVE_TAIL
                };
                let content_rows = if question {
                    1 + fields.options.len().min(9) as u16
                } else {
                    let reply_rows = super::peek::reply_row_count(
                        &state.peek_reply,
                        reply_text_w,
                        super::peek::MAX_REPLY_ROWS,
                    );
                    let max_content = super::layout::max_peek_content_rows(area);

                    let middle_w = layout.dispatch.width.saturating_sub(4);
                    let (body_measured, pin_user) =
                        super::state::scrollback_mut_for_row(&sel, agents)
                            .map(|sb| {
                                (
                                    super::peek_tail::densified_body_line_count(sb, middle_w),
                                    super::peek_tail::scrollback_has_last_user(sb),
                                )
                            })
                            .unwrap_or((0, false));
                    super::layout::peek_live_tail_desired_content(
                        max_content,
                        reply_rows,
                        body_measured,
                        pin_user,
                    )
                    .content_rows
                };
                let alloc =
                    super::layout::allocate_peek(area.height, fixed, content_rows, peek_min);
                if alloc.show_peek {
                    state.set_peek_reply_target_cwd(peeked_agent_cwd(&sel, agents));
                    let badge = super::peek::peek_model_and_mode(&sel, agents);
                    match state.peek.as_mut() {
                        Some(p) => {
                            if p.apply_fields(sel, fields) {
                                state.clear_peek_reply();
                            }
                        }
                        None => state.set_peek(Some(super::peek::PeekPanelState::new(sel, fields))),
                    }
                    if let Some(p) = state.peek.as_mut() {
                        p.model_name = badge.model;
                        p.auto_approve = badge.yolo;
                        p.auto = badge.auto;
                        p.plan_mode = badge.plan;
                    }
                    layout = super::layout::compute_layout_with_peek_box(area, alloc.peek_box_h);
                } else {
                    state.set_peek_reply_target_cwd(None);
                    state.set_peek(None);
                }
            }
            None => {
                state.set_peek_reply_target_cwd(None);
                state.set_peek(None);
            }
        },
        None => {
            state.set_peek_reply_target_cwd(None);
            state.set_peek(None);
        }
    }

    ensure_peek_viewport_lifecycle(state, agents);

    if state.peek.is_none() && area.height > 8 && !state.dispatch.text().is_empty() {
        let rows = dispatch_text_rows(state, layout.dispatch.width, area.height);
        if rows > 1 {
            layout = super::layout::compute_layout_with_dispatch(area, false, rows);
        }
    }

    // Header.
    render_header(buf, layout.header, &theme, &rows, state, upgrade_cta);

    if rows.is_empty() {
        if state.filter.is_active() {
            render_no_match(buf, layout.list, &theme, &state.filter);
        } else {
            render_empty_state(buf, layout.list, &theme, dashboard_sessions_loading);
        }
    } else if area.width < MIN_DASHBOARD_WIDTH {
        render_narrow_rows(buf, layout.list, &theme, &rows, state);
    } else {
        render_rows(buf, layout.list, &theme, &rows, state);
    }

    let selected_state = state
        .selected
        .as_ref()
        .and_then(|sel| rows.iter().find(|r| r.id == *sel).map(|r| r.state));
    let peek_active = state.peek.is_some();

    let dispatch_cursor = if peek_active {
        // Peek has no in-box close button

        state.peek_close_rect = None;

        let voice_listening = state.voice_listening;
        let voice_interim = state.voice_interim.clone();
        let multiline = state.multiline_mode;
        let peeked_row = state.peek.as_ref().map(|p| p.row.clone());
        let question_pending = state.peek.as_ref().is_some_and(|p| p.question.is_some());
        let (empty_hint, has_scrollback) = match peeked_row.as_ref() {
            Some(DashboardRowId::Subagent {
                parent,
                child_session_id,
            }) => {
                let parent_ok = agents
                    .get(parent)
                    .is_some_and(|p| p.subagent_sessions.contains_key(child_session_id));
                let loaded = agents
                    .get(parent)
                    .is_some_and(|p| p.subagent_views.contains_key(child_session_id));
                if parent_ok && !loaded {
                    (Some("Subagent not loaded"), false)
                } else {
                    (None, loaded)
                }
            }
            Some(row) => (
                None,
                super::state::scrollback_available_for_row(row, agents),
            ),
            None => (None, false),
        };
        let render = if let Some(panel) = state.peek.as_ref() {
            let live_tail = if !question_pending && has_scrollback {
                peeked_row
                    .as_ref()
                    .and_then(|row| super::state::scrollback_mut_for_row(row, agents))
                    .map(|scrollback| super::peek::PeekLiveTailArgs { scrollback })
            } else {
                None
            };
            super::peek::render_peek_panel(
                buf,
                layout.dispatch,
                panel,
                &mut state.peek_reply,
                &theme,
                voice_listening,
                voice_interim.as_deref(),
                multiline,
                Some(layout.list).filter(|r| r.area() > 0),
                live_tail,
                empty_hint,
            )
        } else {
            Default::default()
        };
        state.peek_reply_rect = render.reply_rect;
        let cursor = render.caret;

        state.slash_dropdown_items_area = None;
        state.slash_dropdown_hit = Default::default();
        if state.peek_reply.file_search_visible() {
            state.file_search_dropdown_items_area = render_file_search_dropdown_for(
                buf,
                area,
                layout.dispatch,
                &theme,
                &mut state.peek_reply.file_search,
            );
        } else {
            state.file_search_dropdown_items_area = None;
        }

        state.dispatch_rect = None;
        cursor
    } else {
        state.peek_close_rect = None;
        state.peek_reply_rect = None;

        state.dispatch_rect = Some(layout.dispatch);
        let cursor = render_dispatch(
            buf,
            layout.dispatch,
            &theme,
            state,
            Some(layout.list).filter(|r| r.area() > 0),
        );
        // Completion dropdowns paint ABOVE the dispatch box

        if state.dispatch.file_search_visible() {
            render_file_search_dropdown(buf, area, layout.dispatch, &theme, state);
            state.slash_dropdown_items_area = None;
            state.slash_dropdown_hit = Default::default();
        } else {
            render_slash_dropdown(buf, area, layout.dispatch, &theme, state);
            state.file_search_dropdown_items_area = None;
        }
        cursor
    };

    // Footer.
    render_footer(
        buf,
        layout.footer,
        &theme,
        state,
        registry,
        selected_state,
        peek_active,
        pending_hint,
    );

    if let Some(modal) = state.shortcuts_modal.as_mut() {
        crate::views::shortcuts_help::render_modal(
            buf,
            area,
            &modal.entries,
            &mut modal.state,
            &mut modal.window,
            modal.filter_active,
            &modal.collapsed_sections,
            &modal.expanded_ids,
            &modal.mode,
            &theme,
            /* compact */ false,
        );
        return None;
    }

    if let Some(modal) = state.location_picker.as_mut() {
        render_location_picker(buf, area, &theme, modal);
        return None;
    }

    if let Some(dialog) = state.worktree_dialog.as_ref() {
        crate::views::new_worktree_dialog::render_new_worktree_dialog(area, buf, dialog);
        return None;
    }

    // An active rename replaces the dispatch caret with its row-local editor caret.
    if let Some(pos) = rename_cursor_pos(state, &rows) {
        return Some(pos);
    }
    dispatch_cursor
}

const RENAME_PREFIX: &str = "rename: ";

fn rename_editor_view(draft: &RenameDraft, width: u16) -> (&str, u16) {
    let prefix_width = UnicodeWidthStr::width(RENAME_PREFIX) as u16;
    let editor_width = width.saturating_sub(prefix_width);
    let viewport = draft.viewport(editor_width as usize);
    let visible = &draft.text()[viewport.visible_byte_range];
    let cursor_offset = prefix_width
        .saturating_add(viewport.cursor_display_column as u16)
        .min(width.saturating_sub(1));
    (visible, cursor_offset)
}

fn render_rename_editor(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    style: Style,
    draft: &RenameDraft,
) {
    if width == 0 {
        return;
    }
    let prefix_width = UnicodeWidthStr::width(RENAME_PREFIX) as u16;
    buf.set_span(
        x,
        y,
        &Span::styled(RENAME_PREFIX, style),
        prefix_width.min(width),
    );
    let (visible, _) = rename_editor_view(draft, width);
    if !visible.is_empty() && prefix_width < width {
        buf.set_span(
            x + prefix_width,
            y,
            &Span::styled(visible, style),
            width - prefix_width,
        );
    }
}

/// Return the active rename's caret when its row is visible.
fn rename_cursor_pos(state: &DashboardState, rows: &[DashboardRow]) -> Option<(u16, u16)> {
    let rn = state.rename.as_ref()?;
    let (_, rect) = state.row_rects.iter().find(|(id, _)| *id == rn.row)?;
    let row = rows.iter().find(|r| r.id == rn.row);
    let (marker_width, indent_width, icon_width) = row
        .map(|r| {
            (
                UnicodeWidthStr::width(crate::glyphs::selection_bar()) as u16,
                (r.indent as u16) * 2,
                UnicodeWidthStr::width(state_icon(r.state, state.spinner_tick)) as u16,
            )
        })
        .unwrap_or((1, 0, 1));
    let chrome_width = marker_width + 1 + indent_width + icon_width + 1;
    let content_x = rect.x.saturating_add(chrome_width);
    let content_width = rect.x.saturating_add(rect.width).saturating_sub(content_x);
    let (_, cursor_offset) = rename_editor_view(rn, content_width);
    let cursor_x = content_x
        .saturating_add(cursor_offset)
        .min(rect.x.saturating_add(rect.width.saturating_sub(1)));

    let title_y = rect.y + row.map_or(0, |r| row_content_offset(rect.height, r));
    Some((cursor_x, title_y))
}

/// Render the compact dashboard "banner" used when an agent is attached as a popup.
/// The banner is a bordered panel containing the row list summary.
/// The popup renders directly below it, carrying the focused agent's full view (scrollback, prompt, shortcuts).
/// The agent's prompt is then the only input bar on screen.
///
/// Visual:
///
/// ```text
/// ╭──── Dashboard · 3 agents · 1 working ─────────────────────╮
/// │ Working (1)                                                │
/// │   ⸬ session A      Responding                       4.4s   │
/// │ Idle (2)                                                   │
/// │   ○ session B                                       1m12s  │
/// │   ○ session C                                       2m30s  │
/// ╰────────────────────────────────────────────────────────────╯
/// ```
///
/// The border and title are drawn via ratatui's `Block` widget so the chrome matches other bordered panels (subagent fullscreen, popup overlays).
/// Rows are clipped to fit inside the bordered area; the user can see additional rows by closing the popup (Esc) to reach the full dashboard.
fn render_dashboard_banner(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    rows: &[DashboardRow],
    state: &mut DashboardState,
) {
    use ratatui::text::Line;
    use ratatui::widgets::{Block, Borders, Widget};

    state.row_rects.clear();
    state.row_delete_rects.clear();
    state.section_rects.clear();
    if area.area() == 0 || area.height < 3 {
        return;
    }

    // Build the title chip: `Dashboard · N agents · M working`.
    let mut total = 0usize;
    let mut working = 0usize;
    let mut needs_input = 0usize;
    for r in rows.iter().filter(|r| r.indent == 0) {
        total += 1;
        if r.state == RowState::Working {
            working += 1;
        }
        if r.state == RowState::NeedsInput {
            needs_input += 1;
        }
    }
    let agent_word = if total == 1 { "agent" } else { "agents" };
    let mut title_parts: Vec<String> = vec!["Dashboard".to_string()];
    title_parts.push(format!("{total} {agent_word}"));
    if working > 0 {
        title_parts.push(format!("{working} working"));
    }
    if needs_input > 0 {
        title_parts.push(format!("{needs_input} awaiting"));
    }
    let title = format!(" {} ", title_parts.join(" · "));

    // Draw the bordered frame with the title centred on the top edge.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(theme.selection_border)
                .bg(theme.bg_base),
        )
        .title(
            Line::from(title).style(
                Style::default()
                    .fg(theme.text_primary)
                    .bg(theme.bg_base)
                    .add_modifier(Modifier::BOLD),
            ),
        );
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.area() == 0 {
        return;
    }
    if rows.is_empty() {
        let hint = " No sessions yet. Esc to dispatch one. ";
        let trunc = truncate_str(hint, inner.width as usize);
        buf.set_string(
            inner.x,
            inner.y,
            trunc,
            Style::default().fg(theme.gray_dim).bg(theme.bg_base),
        );
        return;
    }

    if inner.width < MIN_DASHBOARD_WIDTH {
        render_narrow_rows(buf, inner, theme, rows, state);
    } else {
        render_rows(buf, inner, theme, rows, state);
    }
}

/// Render the dashboard header row:
///
/// ```text
///   main worktree ~/wt/wt1 (worktree of ~/proj)   ◆ 2 awaiting · ⋮ 3 working · ◇ 1 idle │ [+ New Agent]
/// ```
///
/// Left: the current location (git branch, worktree badge, cwd), the same line the welcome top bar paints (see `views::welcome::location_line`).
/// It is rendered the same way as the session status bar, so the dashboard shows where a dispatched session will run.
/// Truncated with `…` against the chips.
/// Right: state-count chips with state-coloured diamond/spinner glyphs, so the glyph that marks a working row also marks the "working" chip.
/// Counts are top-level rows only; subagents inherit their parent's group and would inflate the tallies if counted directly.
/// Chips with zero count are suppressed.
///
/// A `[+ New Agent]` button sits on the right edge of the header.
/// The button is the default cursor target whenever no row is selected.
/// Up-arrow from the first row, Esc deselect, and opening the dashboard with no prior agent all land here.
/// While focused, Enter on an empty prompt creates a session and opens detail view; click does the same.
/// The focused colour bumps to the green `accent_success` (the "create" colour) so the cursor's location is obvious without a separate marker.
fn render_header(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    rows: &[DashboardRow],
    state: &mut DashboardState,
    upgrade_cta: Option<HeaderUpgradeCta<'_>>,
) {
    use ratatui::text::{Line, Span};

    use crate::views::agent_status::AgentStatusBar;

    state.new_agent_button_hit.set(None);
    state.location_hit.set(None);
    state.upgrade_cta_hit.set(None);

    if area.area() == 0 {
        return;
    }
    buf.set_style(area, Style::default().bg(theme.bg_base));

    let button_label: &str = if state.dispatch_worktree && state.cwd_has_git_ancestor {
        "[+ New Worktree]"
    } else {
        "[+ New Agent]"
    };
    let button_w = UnicodeWidthStr::width(button_label) as u16;
    let right_margin: u16 = 1;
    let mut chip_area = area;
    if area.width >= button_w + right_margin {
        let button_x = area.x + area.width - right_margin - button_w;

        // Otherwise dim gray.
        let hovered = state.new_agent_button_hit.hovered;
        let fg = if state.new_agent_button_focused {
            theme.accent_success
        } else if hovered {
            theme.text_primary
        } else {
            theme.gray
        };
        let style = Style::default()
            .fg(fg)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD);
        buf.set_string(button_x, area.y, button_label, style);
        state.new_agent_button_hit.set(Some(Rect {
            x: button_x,
            y: area.y,
            width: button_w,
            height: 1,
        }));

        let chip_budget = area.width.saturating_sub(button_w + right_margin + 1);
        chip_area = Rect {
            x: area.x,
            y: area.y,
            width: chip_budget,
            height: area.height,
        };
    }

    let mut awaiting = 0usize;
    let mut working = 0usize;
    let mut idle = 0usize;
    let mut done = 0usize;
    let mut failed = 0usize;
    for r in rows.iter().filter(|r| r.indent == 0) {
        match r.state {
            RowState::NeedsInput => awaiting += 1,
            RowState::Working => working += 1,
            RowState::Idle => idle += 1,

            RowState::Inactive => {}
            RowState::Completed => done += 1,
            RowState::Failed => failed += 1,
        }
    }

    // Build right-aligned chips

    let mut status = AgentStatusBar::new(theme);
    let chip = |glyph: &str, color: Color, count: usize, label: &'static str| {
        Line::from(vec![
            Span::styled(
                glyph.to_string(),
                Style::default().fg(color).bg(theme.bg_base),
            ),
            Span::styled(
                format!(" {count} {label}"),
                Style::default().fg(theme.gray).bg(theme.bg_base),
            ),
        ])
    };
    if awaiting > 0 {
        status.push(
            "awaiting",
            chip(
                crate::glyphs::diamond_filled(),
                theme.warning,
                awaiting,
                "awaiting",
            ),
        );
    }
    if working > 0 {
        let frames = crate::glyphs::dot_spinner_frames();
        let spin = frames[(state.spinner_tick / SPINNER_DIVISOR) as usize % frames.len()];
        status.push(
            "working",
            chip(spin, theme.accent_running, working, "working"),
        );
    }
    if idle > 0 {
        status.push(
            "idle",
            chip(
                crate::glyphs::diamond_hollow(),
                theme.gray_dim,
                idle,
                "idle",
            ),
        );
    }
    if done > 0 {
        status.push(
            "done",
            chip(
                crate::glyphs::diamond_filled(),
                theme.accent_success,
                done,
                "done",
            ),
        );
    }
    if failed > 0 {
        status.push(
            "failed",
            chip(
                crate::glyphs::diamond_filled(),
                theme.accent_error,
                failed,
                "failed",
            ),
        );
    }

    let chip_rects = status.render(buf, chip_area);

    //

    let full_label_budget = chip_rects
        .values()
        .map(|r| r.x)
        .min()
        .map(|min_x| min_x.saturating_sub(3).saturating_sub(area.x))
        .unwrap_or(chip_area.width) as usize;

    let upgrade_caption = upgrade_cta.and_then(|cta| cta.pinned.then_some(cta.caption).flatten());
    let upgrade_reserve = upgrade_cta.map_or(0usize, |cta| {
        1 + crate::views::announcements::upgrade_cta_reserve(cta.label, upgrade_caption) as usize
    });
    let label_budget = full_label_budget.saturating_sub(upgrade_reserve);
    let mut location = crate::views::welcome::location_line_at(theme, &state.cwd);
    // 1-cell left inset
    location
        .spans
        .insert(0, Span::styled(" ", Style::default().bg(theme.bg_base)));
    let mut location = truncate_line(location, label_budget);

    if state.location_hit.hovered {
        let icon = crate::git_info::branch_icon();
        location.spans = underline_location_on_hover(std::mem::take(&mut location.spans), icon);
    }
    let location_w = location.width() as u16;
    buf.set_line(area.x, area.y, &location, location_w);

    let hit_w = location_w.min(label_budget as u16);
    if hit_w > 0 {
        state.location_hit.set(Some(Rect {
            x: area.x,
            y: area.y,
            width: hit_w,
            height: 1,
        }));
    }

    if let Some(HeaderUpgradeCta { label, .. }) = upgrade_cta {
        let avail = full_label_budget.saturating_sub(location_w as usize);
        if avail > 1 {
            let cta_x = area.x + location_w;
            buf.set_span(
                cta_x,
                area.y,
                &Span::styled(" ", Style::default().bg(theme.bg_base)),
                1,
            );
            let painted = crate::views::announcements::render_cta_button(
                buf,
                theme,
                cta_x + 1,
                area.y,
                (avail - 1) as u16,
                label,
                upgrade_caption,
                state.upgrade_cta_hit.hovered,
            );
            state.upgrade_cta_hit.set(painted);
        }
    }
}

/// Apply the header location label's hover underline: underline only the visible text.
/// Whitespace-only spans (the leading inset, the separator between the git and path parts) stay bare.
/// The git span (`{icon} {branch}`) keeps the branch `icon` plus the space after it un-underlined; only the branch name is underlined.
/// Total width is preserved (the git span is split, not resized).
fn underline_location_on_hover(spans: Vec<Span<'static>>, icon: &str) -> Vec<Span<'static>> {
    let underline = |content: &str, style: Style| -> Style {
        if content.chars().any(|c| !c.is_whitespace()) {
            style.add_modifier(Modifier::UNDERLINED)
        } else {
            style
        }
    };
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len() + 1);
    for span in spans {
        let style = span.style;
        let content = span.content.into_owned();

        if content.starts_with(icon)
            && let Some(space) = content.find(' ')
        {
            let (icon_part, branch) = content.split_at(space + 1);
            out.push(Span::styled(icon_part.to_owned(), style));
            if !branch.is_empty() {
                let s = underline(branch, style);
                out.push(Span::styled(branch.to_owned(), s));
            }
            continue;
        }
        let s = underline(&content, style);
        out.push(Span::styled(content, s));
    }
    out
}

/// Render the location picker modal over the dashboard.
/// Content-row hit areas from the render are stashed on the modal for the mouse handler.
fn render_location_picker(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    modal: &mut LocationPickerState,
) {
    use crate::views::modal_window::{
        ModalSizing, ModalWindowConfig, Shortcut, push_vim_nav_search_hint, render_modal_window,
    };
    use crate::views::picker::{
        PickerEntry, PickerRow, render_divider, render_picker_content,
        render_picker_search_bar_with_label,
    };

    let mut shortcuts = vec![
        Shortcut {
            label: "\u{2191}\u{2193} nav",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Tab complete",
            clickable: false,
            id: 1,
        },
        Shortcut {
            label: "Enter select",
            clickable: false,
            id: 2,
        },
        Shortcut {
            label: "Esc close",
            clickable: false,
            id: 3,
        },
    ];

    push_vim_nav_search_hint(&mut shortcuts, modal.picker.search_active);
    let config = ModalWindowConfig {
        title: "Change directory",
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing::medium(),
        fold_info: None,
    };
    let Some(content) = render_modal_window(buf, area, &mut modal.window, &config, theme) else {
        modal.content_hits = None;
        return;
    };

    let mut content_area = content.content;

    let visible = modal.visible_candidates();
    // Worktrees require a git repo

    let target_dir = visible
        .get(modal.picker.selected)
        .map(|c| c.path.clone())
        .unwrap_or_else(|| modal.base_cwd.clone());
    let show_worktree = modal.target_is_repo(&target_dir);

    modal.worktree_hit.set(None);
    if content_area.height >= 2 {
        let wt_text = if modal.worktree_mode {
            "[worktree:on]"
        } else {
            "[worktree:off]"
        };
        let wt_w = wt_text.len() as u16;
        const WT_GAP: u16 = 1;
        const MIN_PATH_W: u16 = 16;
        let (path_w, wt_rect) = if show_worktree && content_area.width >= wt_w + WT_GAP + MIN_PATH_W
        {
            (
                content_area.width - wt_w - WT_GAP,
                Some(Rect {
                    x: content_area.x + content_area.width - wt_w,
                    y: content_area.y,
                    width: wt_w,
                    height: 1,
                }),
            )
        } else {
            (content_area.width, None)
        };
        render_picker_search_bar_with_label(
            buf,
            content_area.x,
            content_area.y,
            path_w,
            theme,
            " path: ",
            &modal.picker,
            /* active */ false,
            /* show_hint */ false,
            Some(theme.bg_base),
        );
        modal.worktree_hit.set(wt_rect);
        if let Some(r) = wt_rect {
            let label_fg = if modal.worktree_hit.hovered {
                theme.text_primary
            } else {
                theme.gray
            };
            buf.set_string(
                r.x,
                r.y,
                wt_text,
                Style::default().fg(label_fg).bg(theme.bg_base),
            );
            if modal.worktree_mode
                && let Some(on_at) = wt_text.find("on")
            {
                buf.set_string(
                    r.x + on_at as u16,
                    r.y,
                    "on",
                    Style::default().fg(theme.accent_success).bg(theme.bg_base),
                );
            }
        }

        if let Some(err) = modal.error.as_deref() {
            let err_line = truncate_str(err, content_area.width as usize);
            buf.set_string(
                content_area.x,
                content_area.y + 1,
                &err_line,
                Style::default().fg(theme.accent_error).bg(theme.bg_base),
            );
        } else {
            render_divider(
                buf,
                content_area.x,
                content_area.y + 1,
                content_area.width,
                theme,
                Some(theme.bg_base),
            );
        }
        content_area.y += 2;
        content_area.height = content_area.height.saturating_sub(2);
    }

    // Build entries from the effective list (`visible`, computed above)

    let badges: Vec<String> = visible
        .iter()
        .map(|c| match &c.worktree {
            Some(name) if name == &c.label => "worktree".to_string(),
            Some(name) => format!("worktree: {name}"),
            None => String::new(),
        })
        .collect();

    let details: Vec<String> = {
        const PREFIX: u16 = 2;
        const GAP: u16 = 2;
        const TRAILING: u16 = 1;
        let row_w = content_area.width.saturating_sub(1);
        visible
            .iter()
            .zip(&badges)
            .map(|(c, badge)| {
                let badge_w = if badge.is_empty() {
                    0
                } else {
                    badge.width() as u16 + 1
                };
                let reserved = PREFIX + c.label.width() as u16 + badge_w + GAP + TRAILING;
                let budget = row_w.saturating_sub(reserved) as usize;
                truncate_str(&c.detail, budget)
            })
            .collect()
    };
    let entries: Vec<PickerEntry<'_>> = visible
        .iter()
        .zip(&badges)
        .zip(&details)
        .enumerate()
        .map(|(vis, ((c, badge), detail))| {
            PickerEntry::Row(PickerRow {
                label: c.label.as_str(),
                right_label: detail.as_str(),
                selected: vis == modal.picker.selected,
                expanded: false,
                fields: &[],
                description_lines: &[],
                summary_lines: &[],
                dimmed: false,
                indent: 0,
                badge: badge.as_str(),
                badge_color: (!badge.is_empty()).then_some(theme.accent_user),
                collapsible: false,
                underline_last_desc: false,
            })
        })
        .collect();

    let hits = render_picker_content(
        buf,
        content_area,
        theme,
        &mut modal.picker,
        &entries,
        /* non_selectable */ &[],
        /* non_selectable_clickable */ &[],
        Some(theme.bg_base),
        /* loading */ false,
    );
    modal.content_hits = Some(hits);
}

/// One line in the dashboard's vertical stack: either a state-group header or a content row.
///
/// Explicit state group headers (`── Needs input (2) ──`) mark the group boundaries.
/// The per-row dot and state colour alone don't show at a glance how many sessions are awaiting input, working, idle, or done.
/// Headers are emitted on every top-level state transition when `grouping == Grouping::State` and the filter isn't already pinned to a single state.
/// Subagent rows inherit their parent's group and never trigger a header.
///
/// Headers do NOT register `row_rects`; they're not selectable, hoverable, or clickable.
enum DashboardLine<'a> {
    /// Cross-cutting "Pinned" section header (with count), emitted above the pinned block when grouping is ON.
    PinnedHeader {
        count: usize,
    },
    /// A textless horizontal rule, used when grouping is OFF to separate the pinned block from the rest without a labelled header.
    Divider,
    Header {
        state: RowState,
        count: usize,
    },
    Row(&'a DashboardRow),
    /// The Idle group's "N more" overflow toggle row, emitted at the bottom of a capped Idle group.
    /// `hidden` is the number of folded agents.
    /// `expanded` reflects [`super::state::DashboardState::idle_show_all`] so the label can flip to "show fewer".
    IdleOverflow {
        hidden: usize,
        expanded: bool,
    },
}

/// Walk `rows` and intersperse `DashboardLine::Header` entries at every top-level state transition.
/// Returns a flat sequence that the renderer iterates over directly.
/// Viewport clamping operates on THIS sequence (not on `rows`) so the header rows count toward the visible window's height budget.
///
/// Headers are suppressed when:
///   - `grouping == Grouping::Directory`: the cwd drives the grouping in that mode, and state headers would cut across the cwd sections.
///   - `filter == Filter::State(_)`: the filtered view already contains only a single state, so the header is redundant chrome.
fn build_dashboard_lines<'a>(
    rows: &'a [DashboardRow],
    grouping: Grouping,
    filter: &Filter,
    collapsed: &std::collections::HashSet<SectionKey>,
    idle_show_all: bool,
    search_active: bool,
) -> Vec<DashboardLine<'a>> {
    let groups_on = matches!(grouping, Grouping::State);
    let emit_state_headers = groups_on && !matches!(filter, Filter::State(_));

    let mut pinned_end = 0usize;
    let mut pinned_count = 0usize;
    {
        let mut i = 0usize;
        while i < rows.len() && rows[i].indent == 0 && rows[i].pinned {
            pinned_count += 1;
            i += 1;
            // Glue the pinned parent's subagents into the section.
            while i < rows.len() && rows[i].indent != 0 {
                i += 1;
            }
            pinned_end = i;
        }
    }

    let mut out: Vec<DashboardLine<'a>> = Vec::with_capacity(rows.len() + 6);
    if pinned_count > 0 {
        if groups_on {
            out.push(DashboardLine::PinnedHeader {
                count: pinned_count,
            });
        }

        let pinned_collapsed = groups_on && collapsed.contains(&SectionKey::Pinned);
        if !pinned_collapsed {
            out.extend(rows[..pinned_end].iter().map(DashboardLine::Row));
        }
        if !groups_on && pinned_end < rows.len() {
            out.push(DashboardLine::Divider);
        }
    }

    let rest = &rows[pinned_end..];
    if !emit_state_headers {
        out.extend(rest.iter().map(DashboardLine::Row));
        return out;
    }
    let mut last_top_state: Option<RowState> = None;

    let mut current_collapsed = false;
    // Idle-overflow cap bookkeeping for the group currently being emitted.

    let idle_cap_active = matches!(filter, Filter::None) && !search_active;
    let now = std::time::SystemTime::now();
    let mut idle_limit: Option<usize> = None;
    let mut idle_top_seen = 0usize;
    let mut idle_capping = false;
    let mut pending_overflow: Option<(usize, bool)> = None;
    for (i, row) in rest.iter().enumerate() {
        if row.indent == 0 && Some(row.state) != last_top_state {
            if let Some((hidden, expanded)) = pending_overflow.take() {
                out.push(DashboardLine::IdleOverflow { hidden, expanded });
            }

            let mut count = 0usize;
            let mut recent = 0usize;
            for r in &rest[i..] {
                if r.indent != 0 {
                    continue;
                }
                if r.state == row.state {
                    count += 1;
                    if idle_row_is_recent(r, now) {
                        recent += 1;
                    }
                } else {
                    break;
                }
            }
            out.push(DashboardLine::Header {
                state: row.state,
                count,
            });
            last_top_state = Some(row.state);
            current_collapsed = collapsed.contains(&SectionKey::State(row.state));

            idle_limit = None;
            idle_top_seen = 0;
            idle_capping = false;
            if row.state == RowState::Idle && idle_cap_active && !current_collapsed {
                let base_limit = MAX_VISIBLE_IDLE.max(recent).min(count);
                let base_hidden = count - base_limit;
                if base_hidden >= MIN_IDLE_FOLD {
                    idle_limit = Some(if idle_show_all { count } else { base_limit });
                    pending_overflow = Some((base_hidden, idle_show_all));
                }
            }
        }
        if current_collapsed {
            continue;
        }

        if let Some(limit) = idle_limit {
            if row.indent == 0 {
                idle_top_seen += 1;
                idle_capping = idle_top_seen > limit;
            }
            if idle_capping {
                continue;
            }
        }
        out.push(DashboardLine::Row(row));
    }
    if let Some((hidden, expanded)) = pending_overflow.take() {
        out.push(DashboardLine::IdleOverflow { hidden, expanded });
    }
    out
}

/// Maximum number of top-level Idle agents shown before the rest fold into the "N more" overflow row.
/// The Idle group is sorted most-recent-first, so the folded tail is always the oldest.
pub const MAX_VISIBLE_IDLE: usize = 8;

/// Idle agents last active within this window are never folded, even beyond [`MAX_VISIBLE_IDLE`].
/// A burst of fresh sessions stays visible; the count cap only hides genuinely *old* idle agents.
const IDLE_FRESHNESS: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Don't fold fewer than this many rows: a "1 older" overflow row costs the same vertical space as the single row it would hide.
const MIN_IDLE_FOLD: usize = 2;

/// Whether an Idle `row` was last active within [`IDLE_FRESHNESS`] of `now`.
/// Future timestamps (clock skew across pager processes / roster) count as recent.
fn idle_row_is_recent(row: &DashboardRow, now: std::time::SystemTime) -> bool {
    now.duration_since(row.last_change_at)
        .map(|age| age < IDLE_FRESHNESS)
        .unwrap_or(true)
}

/// Display-order list of keyboard cursor targets: section headers and visible rows.
/// The list is derived from the same [`build_dashboard_lines`] the renderer paints, so navigation and rendering never disagree on what's on screen.
/// Collapsed sections contribute only their header (their rows are skipped by the line builder).
/// `… N more` placeholders are excluded (not selectable).
pub(crate) fn focusables(
    rows: &[DashboardRow],
    grouping: Grouping,
    filter: &Filter,
    collapsed: &std::collections::HashSet<SectionKey>,
    idle_show_all: bool,
    search_active: bool,
) -> Vec<Focusable> {
    build_dashboard_lines(
        rows,
        grouping,
        filter,
        collapsed,
        idle_show_all,
        search_active,
    )
    .into_iter()
    .filter_map(|line| match line {
        DashboardLine::PinnedHeader { .. } => Some(Focusable::Section(SectionKey::Pinned)),
        DashboardLine::Header { state, .. } => Some(Focusable::Section(SectionKey::State(state))),
        DashboardLine::Row(row) if !row.is_more_placeholder => Some(Focusable::Row(row.id.clone())),
        DashboardLine::IdleOverflow { .. } => Some(Focusable::IdleOverflow),
        _ => None,
    })
    .collect()
}

/// The section header that owns `row_id` under the current grouping and filter.
/// `None` when no headers are emitted (directory grouping, `s:state` filter) or the row isn't present in `rows` at all.
/// Walks the same [`build_dashboard_lines`] the renderer paints but with an EMPTY collapsed set.
/// A row hidden inside a collapsed section therefore still resolves to its owning header.
/// That lets `reanchor_selection` move a cursor stranded on a collapse-hidden row onto the header that hid it.
/// Subagent rows resolve to their parent's section (they're emitted under the parent's header).
pub(crate) fn section_of_row(
    rows: &[DashboardRow],
    grouping: Grouping,
    filter: &Filter,
    row_id: &super::DashboardRowId,
) -> Option<SectionKey> {
    let none_collapsed = std::collections::HashSet::new();
    let mut current: Option<SectionKey> = None;

    for line in build_dashboard_lines(rows, grouping, filter, &none_collapsed, true, false) {
        match line {
            DashboardLine::PinnedHeader { .. } => current = Some(SectionKey::Pinned),
            DashboardLine::Header { state, .. } => current = Some(SectionKey::State(state)),
            DashboardLine::Row(row) if row.id == *row_id => return current,
            _ => {}
        }
    }
    None
}

fn render_rows(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    rows: &[DashboardRow],
    state: &mut DashboardState,
) {
    state.row_rects.clear();
    state.row_delete_rects.clear();
    state.section_rects.clear();
    state.idle_overflow_rect = None;
    if area.area() == 0 {
        return;
    }

    let lines = build_dashboard_lines(
        rows,
        state.grouping,
        &state.filter,
        &state.collapsed_sections,
        state.idle_show_all,
        state.search_mode,
    );
    let heights: Vec<u16> = lines
        .iter()
        .map(|l| match l {
            DashboardLine::Row(_) => ROW_HEIGHT,
            DashboardLine::Header { .. }
            | DashboardLine::PinnedHeader { .. }
            | DashboardLine::IdleOverflow { .. }
            | DashboardLine::Divider => GROUP_HEADER_HEIGHT,
        })
        .collect();
    let total_cells: usize = heights.iter().map(|h| *h as usize).sum();

    // Compute the selected row's `(top_cell, height)`

    let mut selected_cell: Option<(usize, u16)> = None;
    {
        let mut cum = 0usize;
        for (line, &h) in lines.iter().zip(heights.iter()) {
            let is_cursor = match line {
                DashboardLine::Row(r) => state.selected.as_ref().is_some_and(|s| *s == r.id),
                DashboardLine::PinnedHeader { .. } => {
                    state.selected_section == Some(SectionKey::Pinned)
                }
                DashboardLine::Header { state: rs, .. } => {
                    state.selected_section == Some(SectionKey::State(*rs))
                }
                DashboardLine::IdleOverflow { .. } => state.selected_idle_overflow,
                DashboardLine::Divider => false,
            };
            if is_cursor {
                selected_cell = Some((cum, h));
                break;
            }
            cum += h as usize;
        }
    }
    let viewport_h = area.height as usize;
    let snap_target = selected_cell.map(|(top, h)| top + (h as usize).saturating_sub(1));
    let offset = state.clamp_viewport(snap_target, viewport_h, total_cells);

    //

    let offset = if !state.manual_scroll_active
        && let Some((sel_top, _)) = selected_cell
        && sel_top < offset
    {
        state.viewport_offset = sel_top;
        sel_top
    } else {
        offset
    };

    // Snap the offset DOWN to the nearest line boundary

    let offset = snap_offset_to_line_boundary(offset, &heights);
    state.viewport_offset = offset;

    let needs_scrollbar = total_cells > viewport_h && area.width >= 4;

    let body_width = area.width;
    let max_y = area.y + area.height;

    let mut line_bg: Vec<Option<Color>> = vec![None; viewport_h];

    let mut cell_y: usize = 0;
    for (line, &h) in lines.iter().zip(heights.iter()) {
        let next_cell_y = cell_y + h as usize;
        // Skip items entirely above the viewport.
        if next_cell_y <= offset {
            cell_y = next_cell_y;
            continue;
        }
        // Stop once we've painted past the visible window.
        if cell_y >= offset + viewport_h {
            break;
        }

        let visible_top = cell_y.max(offset);
        let y = area.y + (visible_top - offset) as u16;
        if y >= max_y {
            break;
        }
        let item_height = (next_cell_y - visible_top) as u16;
        let render_h = item_height.min(max_y - y);
        let line_rect = Rect {
            x: area.x,
            y,
            width: body_width,
            height: render_h,
        };

        let mark = |line_bg: &mut Vec<Option<Color>>, dy: u16, bg: Color| {
            let idx = (y - area.y + dy) as usize;
            if let Some(slot) = line_bg.get_mut(idx) {
                *slot = Some(bg);
            }
        };
        match line {
            DashboardLine::PinnedHeader { count } => {
                let key = SectionKey::Pinned;
                let collapsed = state.is_section_collapsed(key);
                let selected = state.selected_section == Some(key);
                let hovered = state.hovered_section == Some(key);
                render_group_header(
                    buf, line_rect, theme, "Pinned", *count, collapsed, selected, hovered,
                );
                mark(&mut line_bg, 0, theme.bg_base);

                state
                    .section_rects
                    .push((key, Rect::new(area.x, y, body_width, render_h)));
            }
            DashboardLine::Divider => {
                render_divider(buf, line_rect, theme);
                mark(&mut line_bg, 0, theme.bg_base);
            }
            DashboardLine::Header { state: rs, count } => {
                let key = SectionKey::State(*rs);
                let collapsed = state.is_section_collapsed(key);
                let selected = state.selected_section == Some(key);
                let hovered = state.hovered_section == Some(key);
                render_group_header(
                    buf,
                    line_rect,
                    theme,
                    rs.group_label(),
                    *count,
                    collapsed,
                    selected,
                    hovered,
                );
                mark(&mut line_bg, 0, theme.bg_base);

                state
                    .section_rects
                    .push((key, Rect::new(area.x, y, body_width, render_h)));
            }
            DashboardLine::Row(row) => {
                render_row(buf, line_rect, theme, row, state);
                let bg = row_bg(theme, state, row);
                let content_top = row_content_offset(render_h, row);
                let content_h = row_content_height(row).min(render_h);
                for dy in content_top..(content_top + content_h).min(render_h) {
                    mark(&mut line_bg, dy, bg);
                }
                if !row.is_more_placeholder {
                    let hit = Rect {
                        x: area.x,
                        y,
                        width: body_width,
                        height: render_h,
                    };
                    state.row_rects.push((row.id.clone(), hit));
                }
            }
            DashboardLine::IdleOverflow { hidden, expanded } => {
                render_idle_overflow(
                    buf,
                    line_rect,
                    theme,
                    *hidden,
                    *expanded,
                    state.selected_idle_overflow,
                    state.hovered_idle_overflow,
                );
                mark(&mut line_bg, 0, theme.bg_base);

                state.idle_overflow_rect = Some(Rect::new(area.x, y, body_width, render_h));
            }
        }
        cell_y = next_cell_y;
    }

    render_spacer_halos(buf, area, body_width, &line_bg, theme.bg_base);

    if needs_scrollbar {
        render_scrollbar(buf, area, offset, viewport_h, total_cells, theme);
    }
}

/// Paint the spacer lines between items as half-cell "halos" so a highlighted row reads as vertically centered.
/// The spacer below a highlighted block shows the highlight in its TOP half, and the spacer above shows it in its BOTTOM half.
/// Implemented with the upper-half-block glyph (`▀`, CP437 `0xDF`, safe on legacy consoles).
/// The fg paints the top half with the colour of the content line above; the bg paints the bottom half with the colour of the content line below.
/// Spacers between two `bg_base` neighbours are left untouched.
fn render_spacer_halos(
    buf: &mut Buffer,
    area: Rect,
    body_width: u16,
    line_bg: &[Option<Color>],
    base: Color,
) {
    for (i, slot) in line_bg.iter().enumerate() {
        if slot.is_some() {
            continue;
        }
        let above = if i > 0 {
            line_bg[i - 1].unwrap_or(base)
        } else {
            base
        };
        let below = line_bg.get(i + 1).copied().flatten().unwrap_or(base);
        if above == base && below == base {
            continue;
        }
        let y = area.y + i as u16;
        if above == below {
            let fill = " ".repeat(body_width as usize);
            buf.set_string(area.x, y, &fill, Style::default().bg(above));
        } else {
            let fill = "\u{2580}".repeat(body_width as usize);
            buf.set_string(area.x, y, &fill, Style::default().fg(above).bg(below));
        }
    }
}

/// Wide-mode group header reads:
///
/// ```text
/// Working 3 ──────────────────────────────────────────────────
/// ```
///
/// Bold flush-left label (within the padded list area), dim count two cells later, faint horizontal rule filling the remainder of the row.
/// The dim rule visually anchors the boundary the way gh-dash / lazygit / k9s do.
/// Section titles are left-aligned within the list; row text is indented after the marker and icon columns.
#[allow(clippy::too_many_arguments)]
fn render_group_header(
    buf: &mut Buffer,
    rect: Rect,
    theme: &Theme,
    label: &str,
    count: usize,
    collapsed: bool,
    selected: bool,
    hovered: bool,
) {
    let bg = Style::default().bg(theme.bg_base);
    let fill = " ".repeat(rect.width as usize);
    buf.set_string(rect.x, rect.y, fill, bg);
    if rect.width == 0 {
        return;
    }

    let label_fg = if selected {
        theme.accent_user
    } else if hovered {
        theme.text_primary
    } else {
        theme.gray
    };
    let count_str = format!(" {count}");
    let label_style = Style::default()
        .fg(label_fg)
        .bg(theme.bg_base)
        .add_modifier(Modifier::BOLD);
    let count_style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
    let rule_style = Style::default()
        .fg(theme.selection_border)
        .bg(theme.bg_base);

    // Disclosure indicator: ▾ expanded, ▸ collapsed.
    let glyph_str = format!(
        "{} ",
        if collapsed {
            crate::glyphs::disclosure_closed()
        } else {
            crate::glyphs::disclosure_open()
        }
    );
    let glyph_w = UnicodeWidthStr::width(glyph_str.as_str()) as u16;
    let label_w = UnicodeWidthStr::width(label) as u16;
    let count_w = UnicodeWidthStr::width(count_str.as_str()) as u16;

    let mut cx = rect.x;
    if glyph_w >= rect.width {
        return;
    }
    buf.set_string(cx, rect.y, &glyph_str, label_style);
    cx += glyph_w;

    let label_avail = (rect.x + rect.width).saturating_sub(cx);
    if label_w >= label_avail {
        let trunc = truncate_str(label, label_avail as usize);
        buf.set_string(cx, rect.y, trunc, label_style);
        return;
    }
    buf.set_string(cx, rect.y, label, label_style);
    cx += label_w;

    if cx + count_w >= rect.x + rect.width {
        return;
    }
    buf.set_string(cx, rect.y, &count_str, count_style);
    cx += count_w;

    let pad = 1u16;
    if cx + pad >= rect.x + rect.width {
        return;
    }
    cx += pad;

    let rule_w = (rect.x + rect.width).saturating_sub(cx);
    if rule_w == 0 {
        return;
    }
    let rule: String = "\u{2500}".repeat(rule_w as usize);
    buf.set_string(cx, rect.y, &rule, rule_style);
}

/// Textless divider: a full-width horizontal rule in the section-header rule style.
/// It is used when grouping is OFF to separate the pinned block from the rest without a labelled header.
/// Paints only the first cell; any trailing cell of its rect stays at `bg_base` (the breathing gap).
fn render_divider(buf: &mut Buffer, rect: Rect, theme: &Theme) {
    let bg = Style::default().bg(theme.bg_base);
    let fill = " ".repeat(rect.width as usize);
    buf.set_string(rect.x, rect.y, fill, bg);
    if rect.width == 0 {
        return;
    }
    let rule_style = Style::default()
        .fg(theme.selection_border)
        .bg(theme.bg_base);
    let rule: String = "\u{2500}".repeat(rect.width as usize);
    buf.set_string(rect.x, rect.y, &rule, rule_style);
}

/// The Idle group's "N more" overflow toggle row: a single dim line painted at the bottom of a capped Idle group.
/// It carries a `+` / `-` expand indicator in the icon column (`+` collapsed, `-` expanded).
/// The label sits in the agent-name column, aligned with the rows above.
/// The cursor (keyboard) paints it in `accent_user`; mouse hover brightens it to `text_primary`.
/// `expanded` flips the label to "show fewer" so the same row re-folds the list.
/// Shared by the wide and narrow paths (the row is single-line in both).
fn render_idle_overflow(
    buf: &mut Buffer,
    rect: Rect,
    theme: &Theme,
    hidden: usize,
    expanded: bool,
    selected: bool,
    hovered: bool,
) {
    let bg = Style::default().bg(theme.bg_base);
    let fill = " ".repeat(rect.width as usize);
    buf.set_string(rect.x, rect.y, fill, bg);
    if rect.width == 0 {
        return;
    }
    let fg = if selected {
        theme.accent_user
    } else if hovered {
        theme.text_primary
    } else {
        theme.gray_dim
    };
    let style = Style::default().fg(fg).bg(theme.bg_base);
    let label = if expanded {
        "show fewer".to_string()
    } else {
        format!("{hidden} more")
    };

    let indicator = if expanded { "-" } else { "+" };
    let icon_w = unicode_width::UnicodeWidthStr::width(state_icon(RowState::Idle, 0)) as u16;
    let indicator_x = rect.x.saturating_add(2);
    let name_x = indicator_x.saturating_add(icon_w + 1);
    if indicator_x < rect.x + rect.width {
        buf.set_string(indicator_x, rect.y, indicator, style);
    }
    if name_x >= rect.x + rect.width {
        return;
    }
    let avail = (rect.x + rect.width).saturating_sub(name_x);
    let trunc = truncate_str(&label, avail as usize);
    buf.set_string(name_x, rect.y, trunc, style);
}

/// Narrow-mode group header. Compact `Done 12` form, with no trailing rule because the narrow layout doesn't have the width budget.
#[allow(clippy::too_many_arguments)]
fn render_group_header_narrow(
    buf: &mut Buffer,
    rect: Rect,
    theme: &Theme,
    label: &str,
    count: usize,
    collapsed: bool,
    selected: bool,
    hovered: bool,
) {
    let bg_style = Style::default().bg(theme.bg_base);
    let label_fg = if selected {
        theme.accent_user
    } else if hovered {
        theme.text_primary
    } else {
        theme.gray
    };
    let label_style = Style::default()
        .fg(label_fg)
        .bg(theme.bg_base)
        .add_modifier(Modifier::BOLD);
    let fill = " ".repeat(rect.width as usize);
    buf.set_string(rect.x, rect.y, fill, bg_style);
    let glyph = if collapsed {
        crate::glyphs::disclosure_closed()
    } else {
        crate::glyphs::disclosure_open()
    };
    let line = format!("{glyph} {label} {count}");
    let trunc = truncate_str(&line, rect.width as usize);
    buf.set_string(rect.x, rect.y, trunc, label_style);
}

/// Render the list scrollbar in the right gutter, stuck to the window's rightmost column, in the exact scrollback style.
/// That style is a `█` thumb in `scrollbar_fg` over a `scrollbar_bg` track (no `│` line).
/// Reuses the shared `render_scrollbar_styled` so the thumb sizing and colour match the scrollback pixel-for-pixel.
///
/// The list content is inset by `LIST_OUTER_HPAD`; the scrollbar lives in that right margin so it never reserves width from the rows.
/// The position is clamped to the buffer edge so isolated renders (where `area` spans the whole buffer, e.g. tests) still paint in-bounds.
fn render_scrollbar(
    buf: &mut Buffer,
    area: Rect,
    offset: usize,
    visible: usize,
    total: usize,
    theme: &Theme,
) {
    use crate::render::scrollbar::render_scrollbar_styled;

    let x = (area.x + area.width - 1 + super::layout::LIST_OUTER_HPAD)
        .min(buf.area.right().saturating_sub(1));
    let scrollbar_area = Rect {
        x,
        y: area.y,
        width: 1,
        height: area.height,
    };
    let track_style = Style::default().bg(theme.scrollbar_bg);
    let thumb_style = Style::default()
        .fg(theme.scrollbar_fg)
        .bg(theme.scrollbar_bg);
    let cap = |v: usize| v.min(u16::MAX as usize) as u16;
    render_scrollbar_styled(
        buf,
        Some(scrollbar_area),
        cap(total),
        cap(visible),
        cap(offset),
        track_style,
        thumb_style,
    );
}

/// Snap a cell-granular viewport offset DOWN to the nearest item-start boundary in `heights`.
/// Each entry in `heights` is the cell-height of one `DashboardLine` (rows 3, headers 2).
/// The returned offset is the cumulative start of the item that currently overlaps `offset`.
/// Offsets that already land on a boundary are returned unchanged.
///
/// This exists because `render_row` paints its content from `rect.y` (title, then secondary) and has no notion of a partial top clip.
/// Without snapping, a trackpad scrolling by 1 cell would paint the row's title at the viewport top where the row's gap-cell should be.
/// Visually the row "sticks" instead of scrolling away.
/// Snapping forces whole-row alignment so the topmost row always starts at its first cell.
fn snap_offset_to_line_boundary(offset: usize, heights: &[u16]) -> usize {
    let mut cum = 0usize;
    let mut snapped = 0usize;
    for &h in heights {
        if cum > offset {
            break;
        }
        snapped = cum;
        cum = cum.saturating_add(h as usize);
    }
    snapped
}

/// Number of content lines a row renders: the title plus an optional secondary line.
fn row_content_height(row: &DashboardRow) -> u16 {
    if row.secondary_line.as_deref().is_some_and(|s| !s.is_empty()) {
        2
    } else {
        1
    }
}

/// Vertical offset of a row's content block within its rect.
/// The 1- or 2-line content is centered at cell granularity.
/// A title-only row in a 3-cell rect gets one padding line above and below, while a row with a secondary line stays top-aligned ((3 - 2) / 2 = 0).
fn row_content_offset(height: u16, row: &DashboardRow) -> u16 {
    height.saturating_sub(row_content_height(row)) / 2
}

/// The row's background: keyboard selection wins over mouse hover.
fn row_bg(theme: &Theme, state: &DashboardState, row: &DashboardRow) -> Color {
    if state.selected.as_ref().is_some_and(|s| *s == row.id) {
        theme.bg_highlight
    } else if state.hovered_row.as_ref().is_some_and(|h| *h == row.id) {
        theme.bg_hover
    } else {
        theme.bg_base
    }
}

/// Render a row as a 2-line block plus a trailing padding line.
/// `rect.height` is expected to be `>= 2`.
/// The caller (`render_rows`) sizes the rect to either 2 or 3 lines depending on whether the padding is in budget.
///
/// Visual:
///
/// ```text
///   ◆ Add responsiveness to /context · xai my-branch-2 worktree       4 mins
///     Pending: plan approval plan.md
/// ```
///
/// Line 1 (title): selection marker, icon, label, subtitle, age.
/// Line 2 (secondary): dim text aligned under the label.
/// It shows the last tool call, the last assistant message, or a `Pending: …` preview of the front-most permission request.
///
/// The content block is vertically centered within the rect (see [`row_content_offset`]).
/// A title-only row in a 3-cell rect renders as padding, title, padding.
///
/// Selection / hover backgrounds fill the CONTENT lines.
/// The spacer lines around them are painted afterwards by `render_rows`'s half-block pass (see `render_spacer_halos`).
/// That pass extends the highlight half a cell above and below so it reads as centered on the text.
fn render_row(
    buf: &mut Buffer,
    rect: Rect,
    theme: &Theme,
    row: &DashboardRow,
    state: &mut DashboardState,
) {
    if rect.area() == 0 {
        return;
    }
    let selected = state.selected.as_ref().is_some_and(|s| *s == row.id);
    let renaming = state.rename.as_ref().is_some_and(|r| r.row == row.id);
    let bg = row_bg(theme, state, row);

    // Paint the content lines with the row background

    let content_top = row_content_offset(rect.height, row);
    let content_h = row_content_height(row).min(rect.height);
    let fill = " ".repeat(rect.width as usize);
    for dy in content_top..(content_top + content_h).min(rect.height) {
        buf.set_string(rect.x, rect.y + dy, &fill, Style::default().bg(bg));
    }

    // Layout columns: marker (1) | gap (1) | indent (2*n) | icon (1-2)
    //                | gap (1) | label/secondary start.
    let marker = if selected {
        crate::glyphs::selection_bar()
    } else {
        " "
    };
    let marker_w = UnicodeWidthStr::width(marker) as u16;
    let indent_w = (row.indent as u16) * 2;
    let icon = state_icon(row.state, state.spinner_tick);

    let icon_color = if row.state == RowState::NeedsInput {
        needs_input_bullet_color(state.spinner_tick, theme)
    } else {
        state_color(row.state, theme)
    };
    let icon_w = UnicodeWidthStr::width(icon) as u16;
    // Title-row paint cursor

    let title_y = rect.y + row_content_offset(rect.height, row);
    let content_start_x = rect.x + marker_w + 1 + indent_w + icon_w + 1;

    if renaming && let Some(rn) = state.rename.as_ref() {
        buf.set_string(
            rect.x,
            title_y,
            marker,
            Style::default()
                .bg(bg)
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        );

        if selected {
            let bar_style = Style::default()
                .bg(bg)
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD);
            for dy in content_top..(content_top + content_h).min(rect.height) {
                buf.set_string(
                    rect.x,
                    rect.y + dy,
                    crate::glyphs::selection_bar(),
                    bar_style,
                );
            }
        }

        buf.set_string(
            rect.x + marker_w + 1 + indent_w,
            title_y,
            icon,
            Style::default().fg(icon_color).bg(bg),
        );
        let available = (rect.x + rect.width).saturating_sub(content_start_x);
        render_rename_editor(
            buf,
            content_start_x,
            title_y,
            available,
            Style::default()
                .fg(theme.accent_user)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
            rn,
        );
        return;
    }

    buf.set_string(
        rect.x,
        title_y,
        marker,
        Style::default()
            .bg(bg)
            .fg(theme.accent_user)
            .add_modifier(Modifier::BOLD),
    );

    if selected {
        let bar_style = Style::default()
            .bg(bg)
            .fg(theme.accent_user)
            .add_modifier(Modifier::BOLD);
        for dy in content_top..(content_top + content_h).min(rect.height) {
            buf.set_string(
                rect.x,
                rect.y + dy,
                crate::glyphs::selection_bar(),
                bar_style,
            );
        }
    }

    let icon_x = rect.x + marker_w + 1 + indent_w;
    buf.set_string(
        icon_x,
        title_y,
        icon,
        Style::default().bg(bg).fg(icon_color),
    );

    let armed_delete = state.armed_delete_row_ref();
    let show_delete = !row.is_more_placeholder
        && !row.id.is_subagent()
        && !row.id.is_workspace()
        && row.state.allows_delete()
        && !state.row_is_conversation(&row.id)
        && (state.hovered_row.as_ref() == Some(&row.id) || armed_delete == Some(&row.id));
    let delete_label = crate::glyphs::ballot_x_button();
    let delete_w = UnicodeWidthStr::width(delete_label) as u16;
    let age = format_time_ago(row.last_change_at.elapsed().unwrap_or_default());
    let age_str = format!("{age:>6}");
    let age_w = UnicodeWidthStr::width(age_str.as_str()) as u16;
    let right_w = if show_delete { delete_w } else { age_w };
    let right_x = rect.x + rect.width.saturating_sub(right_w + 1);
    if right_x > content_start_x {
        if show_delete {
            let fg = if state.hovered_delete.as_ref() == Some(&row.id)
                || armed_delete == Some(&row.id)
            {
                theme.accent_error
            } else {
                theme.text_secondary
            };
            buf.set_string(
                right_x,
                title_y,
                delete_label,
                Style::default().bg(bg).fg(fg),
            );
            state.row_delete_rects.push((
                row.id.clone(),
                Rect {
                    x: right_x,
                    y: title_y,
                    width: delete_w,
                    height: 1,
                },
            ));
        } else {
            buf.set_string(
                right_x,
                title_y,
                &age_str,
                Style::default().bg(bg).fg(theme.gray),
            );
        }
    }

    let title_avail = right_x.saturating_sub(content_start_x).saturating_sub(2);
    let mut cx = content_start_x;
    if title_avail > 0 {
        let label_style = Style::default().bg(bg).fg(if row.is_more_placeholder {
            theme.gray_dim
        } else {
            theme.text_primary
        });

        let dim_suffix = (!row.is_more_placeholder)
            .then(|| row.label.strip_prefix(super::row::NEW_SESSION_LABEL))
            .flatten()
            .filter(|rest| rest.starts_with(" #"));
        if let Some(suffix) = dim_suffix {
            let head_trunc = truncate_str(super::row::NEW_SESSION_LABEL, title_avail as usize);
            let head_w = UnicodeWidthStr::width(&head_trunc[..]) as u16;
            buf.set_string(cx, title_y, &head_trunc, label_style);
            cx += head_w;
            let remaining = title_avail.saturating_sub(head_w) as usize;
            if remaining > 0 {
                let suffix_trunc = truncate_str(suffix, remaining);
                let suffix_w = UnicodeWidthStr::width(&suffix_trunc[..]) as u16;
                buf.set_string(
                    cx,
                    title_y,
                    &suffix_trunc,
                    Style::default().bg(bg).fg(theme.gray_dim),
                );
                cx += suffix_w;
            }
        } else {
            let label_trunc = truncate_str(&row.label, title_avail as usize);
            let label_w = UnicodeWidthStr::width(&label_trunc[..]) as u16;
            buf.set_string(cx, title_y, label_trunc, label_style);
            cx += label_w;
        }

        // Subtitle: ` · xai my-branch-2 worktree`.
        if let Some(sub) = row.subtitle.as_deref()
            && cx + 4 < right_x
        {
            let remaining = right_x.saturating_sub(cx).saturating_sub(2) as usize;
            let sub_str = format!(" \u{00B7} {sub}");
            let sub_trunc = truncate_str(&sub_str, remaining);
            let sub_w = UnicodeWidthStr::width(&sub_trunc[..]) as u16;
            buf.set_string(
                cx,
                title_y,
                sub_trunc,
                Style::default().bg(bg).fg(theme.gray_dim),
            );
            cx += sub_w;
        }

        for badge in &row.badges {
            if matches!(
                badge,
                RowBadge::Worktree | RowBadge::NeedsInput | RowBadge::Pinned
            ) {
                continue;
            }
            let label = match badge {
                RowBadge::NeedsInput | RowBadge::Worktree | RowBadge::Pinned => continue,
                RowBadge::Failed => "failed",
                RowBadge::BgTask => "bg",
            };
            let chip = format!(" [{label}]");
            let cw = UnicodeWidthStr::width(chip.as_str()) as u16;
            if cx + cw + 1 < right_x {
                buf.set_string(
                    cx,
                    title_y,
                    &chip,
                    Style::default().bg(bg).fg(badge_color(*badge, theme)),
                );
                cx += cw;
            }
        }
    }

    // Secondary row

    if rect.height >= 2
        && let Some(secondary) = row.secondary_line.as_deref()
        && !secondary.is_empty()
    {
        let sec_y = title_y + 1;
        let avail = rect
            .width
            .saturating_sub(content_start_x - rect.x)
            .saturating_sub(1);
        if avail > 0 {
            let trunc = truncate_str(secondary, avail as usize);
            let secondary_fg = if selected {
                theme.text_secondary
            } else {
                theme.gray_dim
            };
            // The awaiting-input subtitle is `Pending: …`

            const PENDING_PREFIX: &str = "Pending:";
            if let Some(rest) = trunc.strip_prefix(PENDING_PREFIX) {
                buf.set_string(
                    content_start_x,
                    sec_y,
                    PENDING_PREFIX,
                    Style::default().bg(bg).fg(theme.warning),
                );
                let prefix_w = UnicodeWidthStr::width(PENDING_PREFIX) as u16;
                buf.set_string(
                    content_start_x + prefix_w,
                    sec_y,
                    rest,
                    Style::default().bg(bg).fg(secondary_fg),
                );
            } else {
                buf.set_string(
                    content_start_x,
                    sec_y,
                    trunc,
                    Style::default().bg(bg).fg(secondary_fg),
                );
            }
        }
    }
}

fn render_narrow_rows(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    rows: &[DashboardRow],
    state: &mut DashboardState,
) {
    state.row_rects.clear();
    state.row_delete_rects.clear();
    state.section_rects.clear();
    state.idle_overflow_rect = None;
    if area.area() == 0 {
        return;
    }

    let lines = build_dashboard_lines(
        rows,
        state.grouping,
        &state.filter,
        &state.collapsed_sections,
        state.idle_show_all,
        state.search_mode,
    );
    let viewport_h = area.height as usize;

    let selected_line_idx = lines.iter().position(|l| match l {
        DashboardLine::Row(r) => state.selected.as_ref().is_some_and(|s| r.id == *s),
        DashboardLine::PinnedHeader { .. } => state.selected_section == Some(SectionKey::Pinned),
        DashboardLine::Header { state: rs, .. } => {
            state.selected_section == Some(SectionKey::State(*rs))
        }
        DashboardLine::IdleOverflow { .. } => state.selected_idle_overflow,
        DashboardLine::Divider => false,
    });
    let offset = state.clamp_viewport(selected_line_idx, viewport_h, lines.len());

    let needs_scrollbar = lines.len() > viewport_h && area.width >= 4;

    let body_width = area.width;

    let mut y = area.y;
    for line in lines.iter().skip(offset).take(viewport_h) {
        if y >= area.y + area.height {
            break;
        }
        let line_rect = Rect {
            x: area.x,
            y,
            width: body_width,
            height: 1,
        };
        let row = match line {
            DashboardLine::PinnedHeader { count } => {
                let key = SectionKey::Pinned;
                let collapsed = state.is_section_collapsed(key);
                let selected = state.selected_section == Some(key);
                let hovered = state.hovered_section == Some(key);
                render_group_header_narrow(
                    buf, line_rect, theme, "Pinned", *count, collapsed, selected, hovered,
                );
                state
                    .section_rects
                    .push((key, Rect::new(area.x, y, body_width, 1)));
                y += 1;
                continue;
            }
            DashboardLine::Header { state: rs, count } => {
                let key = SectionKey::State(*rs);
                let collapsed = state.is_section_collapsed(key);
                let selected = state.selected_section == Some(key);
                let hovered = state.hovered_section == Some(key);
                render_group_header_narrow(
                    buf,
                    line_rect,
                    theme,
                    rs.group_label(),
                    *count,
                    collapsed,
                    selected,
                    hovered,
                );
                state
                    .section_rects
                    .push((key, Rect::new(area.x, y, body_width, 1)));
                y += 1;
                continue;
            }
            DashboardLine::Divider => {
                render_divider(buf, line_rect, theme);
                y += 1;
                continue;
            }
            DashboardLine::IdleOverflow { hidden, expanded } => {
                render_idle_overflow(
                    buf,
                    line_rect,
                    theme,
                    *hidden,
                    *expanded,
                    state.selected_idle_overflow,
                    state.hovered_idle_overflow,
                );
                state.idle_overflow_rect = Some(Rect::new(area.x, y, body_width, 1));
                y += 1;
                continue;
            }
            DashboardLine::Row(row) => row,
        };

        let selected = state.selected.as_ref().is_some_and(|s| *s == row.id);
        let hovered = state.hovered_row.as_ref().is_some_and(|h| *h == row.id);
        let renaming = state.rename.as_ref().is_some_and(|r| r.row == row.id);
        let bg = if selected {
            theme.bg_highlight
        } else if hovered {
            theme.bg_hover
        } else {
            theme.bg_base
        };

        if renaming && let Some(rn) = state.rename.as_ref() {
            let marker = if selected {
                crate::glyphs::selection_bar()
            } else {
                " "
            };
            let icon = state_icon(row.state, state.spinner_tick);
            let indent = "  ".repeat(row.indent as usize);
            let chrome = format!("{marker} {indent}{icon} ");
            let chrome_w = UnicodeWidthStr::width(chrome.as_str()) as u16;
            buf.set_string(
                area.x,
                y,
                &chrome,
                Style::default().fg(theme.text_primary).bg(bg),
            );
            render_rename_editor(
                buf,
                area.x + chrome_w,
                y,
                body_width.saturating_sub(chrome_w),
                Style::default()
                    .fg(theme.accent_user)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
                rn,
            );
        } else {
            let marker = if selected {
                crate::glyphs::selection_bar()
            } else {
                " "
            };
            let marker_w = UnicodeWidthStr::width(marker) as u16;
            let icon = state_icon(row.state, state.spinner_tick);
            let icon_w = UnicodeWidthStr::width(icon) as u16;
            let indent = "  ".repeat(row.indent as usize);
            let indent_w = UnicodeWidthStr::width(indent.as_str()) as u16;
            let gap_after_marker = 1u16;
            let chrome = marker_w + gap_after_marker + indent_w + icon_w + 1;
            let armed_here = state.armed_delete_row_ref() == Some(&row.id);
            let show_delete = !row.is_more_placeholder
                && !row.id.is_subagent()
                && !row.id.is_workspace()
                && row.state.allows_delete()
                && !state.row_is_conversation(&row.id)
                && (hovered || armed_here);
            let delete_label = crate::glyphs::ballot_x_button();
            let delete_w = UnicodeWidthStr::width(delete_label) as u16;
            let label_budget = body_width
                .saturating_sub(chrome)
                .saturating_sub(if show_delete { delete_w + 1 } else { 0 });
            let label = truncate_str(&row.label, label_budget as usize);
            let line = format!("{marker} {indent}{icon} {label}");
            buf.set_string(
                area.x,
                y,
                line,
                Style::default().fg(theme.text_primary).bg(bg),
            );
            if show_delete && body_width > chrome + delete_w {
                let dx = area.x + body_width.saturating_sub(delete_w);
                let fg = if state.hovered_delete.as_ref() == Some(&row.id) || armed_here {
                    theme.accent_error
                } else {
                    theme.text_secondary
                };
                buf.set_string(dx, y, delete_label, Style::default().fg(fg).bg(bg));
                state
                    .row_delete_rects
                    .push((row.id.clone(), Rect::new(dx, y, delete_w, 1)));
            }
        }
        if !row.is_more_placeholder {
            state.row_rects.push((row.id.clone(), line_rect));
        }
        y += 1;
    }

    if needs_scrollbar {
        render_scrollbar(buf, area, offset, viewport_h, lines.len(), theme);
    }
}

/// Rendered when agents exist but the filter has hidden every row.
/// Distinct from the empty-state hint so the user knows their filter is what's hiding the rows.
fn render_no_match(buf: &mut Buffer, area: Rect, theme: &Theme, filter: &Filter) {
    if area.area() == 0 {
        return;
    }
    let hint = match filter {
        Filter::None => "No matching rows.".to_string(),
        Filter::Agent(n) => format!("No agents match `a:{n}`. Press Esc to clear the filter."),
        Filter::State(s) => format!(
            "No agents in state `{}`: press Esc to clear the filter.",
            s.group_label()
        ),
        Filter::Substring(n) => format!("No rows match `{n}`: press Esc to clear the filter."),
    };
    let truncated = truncate_str(&hint, area.width.saturating_sub(2) as usize);

    let y_offset: u16 = if area.height >= 2 { 1 } else { 0 };
    buf.set_string(
        area.x + 1,
        area.y + y_offset,
        truncated,
        Style::default().fg(theme.gray),
    );
}

fn render_empty_state(buf: &mut Buffer, area: Rect, theme: &Theme, loading: bool) {
    if area.area() == 0 {
        return;
    }

    let line = if loading {
        "Loading sessions…"
    } else {
        "No agents yet, type a prompt to start one."
    };
    let truncated = truncate_str(line, area.width.saturating_sub(2) as usize);
    // See `render_no_match` for the precedence rationale.
    let y_offset: u16 = if area.height >= 2 { 1 } else { 0 };
    buf.set_string(
        area.x + 1,
        area.y + y_offset,
        truncated,
        Style::default().fg(theme.gray),
    );
}

/// Paint a rounded-box chrome around the dispatch input so it reads as a real input field.
/// On a 3-row rect the layout is:
///
/// ```text
/// ╭──────────────────────────────────────────────────────────╮
/// │ ❯ Dispatch a new agent                                   │
/// ╰──────────────────────────────────────────────────────────╯
/// ```
///
/// On a 1-row rect (very short terminals) we fall back to the bare `❯ {text}` line so the input stays usable.
///
/// `reply_label` flips the placeholder between `Dispatch a new agent` (`None`) and `Reply to {label}` (`Some`).
/// The chrome then reflects what Enter will do: dispatch a new session, or enqueue / send a prompt to the currently-selected agent.
/// Paint a short right-aligned feedback badge onto the dispatch box's **top border**, in a neutral accent colour.
/// Examples: `✗ Session no longer exists`, `✓ Theme: Grok Day`.
/// The message is painted VERBATIM: it already carries its own status glyph.
/// Errors are built via [`DashboardState::set_error_toast`] (`✗`), while successes and info arrive from the `show_toast` builders (`✓` / `⚠`).
/// The badge therefore neither prepends a glyph nor forces a colour.
/// Prepending doubled the glyph (`✗ ✓ …`), and a forced colour painted non-errors (like "Session closed") red.
/// The glyph, not the colour, conveys severity, mirroring the per-agent toast in [`crate::app::agent_view`].
/// The badge ends one column before the right corner (`╮`) and is truncated to fit.
/// No-op when there's no toast or the box is too narrow.
fn paint_dispatch_feedback_badge(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    error_toast: Option<&str>,
) {
    let Some(err) = error_toast else {
        return;
    };

    let max_w = area.width.saturating_sub(4);
    if max_w < 6 {
        return;
    }

    let label = format!(" {err} ");
    let trunc = truncate_str(&label, max_w as usize);
    let label_w = UnicodeWidthStr::width(trunc.as_str()) as u16;
    // Right-align so the badge ends one column before the `╮` corner.
    let x = area.x + area.width.saturating_sub(1 + label_w);
    buf.set_string(
        x,
        area.y,
        &trunc,
        Style::default()
            .fg(theme.accent_user)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD),
    );
}

/// Paint the model and mode indicator onto the dispatch box's **bottom border**.
/// Reuses the shared prompt info-line renderer so its style, spacing, and position match the chat prompt exactly (`╰──model · flag──╯`).
/// The model name renders in dim secondary text; the mode shows as a `plan` (plan accent) or `always-approve` (default) flag.
/// No-op when the box is too short or there's nothing to show.
fn paint_dispatch_config_badge(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    state: &DashboardState,
    input_focused: bool,
) {
    use crate::views::dashboard::DashboardDispatchMode;
    use crate::views::prompt_widget::{PromptFlag, PromptInfo};

    if area.height < 3 || area.width < 6 {
        return;
    }
    let model_label = state
        .pending_model
        .as_ref()
        .map(|m| match m.effort {
            Some(effort) => format!("{} ({effort})", m.display),
            None => m.display.clone(),
        })
        .or_else(|| state.models.current_model_name())
        .unwrap_or_default();

    // Mode flag, styled exactly like the chat prompt's mode flags.
    let mut flags: Vec<PromptFlag> = Vec::new();
    match state.pending_mode {
        DashboardDispatchMode::Plan => flags.push(PromptFlag {
            text: "plan",
            color: Some(theme.accent_plan),
            bold: false,
        }),
        DashboardDispatchMode::Auto => flags.push(PromptFlag {
            text: "auto",
            color: Some(theme.accent_system),
            bold: false,
        }),
        DashboardDispatchMode::AlwaysApprove => flags.push(PromptFlag {
            text: "always-approve",
            color: None,
            bold: false,
        }),
        DashboardDispatchMode::Normal => {}
    }

    if model_label.is_empty() && flags.is_empty() && !state.multiline_mode {
        return;
    }

    let info = PromptInfo {
        model_name: &model_label,
        flags: &flags,
        multiline: state.multiline_mode,
        usage_warning: None,
        usage_warning_critical: false,
    };

    let info_rect = Rect {
        x: area.x + 1,
        y: area.y + area.height - 1,
        width: area.width.saturating_sub(2),
        height: 1,
    };
    state
        .dispatch
        .render_info_line(buf, info_rect, &info, theme.bg_base, theme, input_focused);
}

/// Paint the left-aligned `● rec` badge on a box's top border while the mic is hot.
/// Shared by the dispatch box and the peek panel that replaces it, so a capture started in either box shows the same indicator.
pub(super) fn paint_record_badge(buf: &mut Buffer, area: Rect, theme: &Theme, listening: bool) {
    if listening && area.width >= 12 {
        buf.set_string(
            area.x + 2,
            area.y,
            " \u{25CF} rec ",
            Style::default()
                .fg(theme.accent_error)
                .bg(theme.bg_base)
                .add_modifier(Modifier::BOLD),
        );
    }
}

fn render_dispatch(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    state: &mut DashboardState,
    overlay_area: Option<Rect>,
) -> Option<(u16, u16)> {
    use ratatui::widgets::{Block, BorderType, Borders, Widget};

    use crate::views::prompt_widget::{PromptBg, PromptStyle};

    if area.area() == 0 {
        return None;
    }
    let bg = Style::default().bg(theme.bg_base);
    let fill = " ".repeat(area.width as usize);
    for dy in 0..area.height {
        buf.set_string(area.x, area.y + dy, &fill, bg);
    }

    let input_focused = !state.list_focused;
    let border_fg = if input_focused {
        theme.selection_border
    } else {
        theme.prompt_border
    };

    // Draw the rounded box and carve out the content rows

    let content = if area.height >= 3 {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_fg).bg(theme.bg_base));
        let inner = block.inner(area);
        block.render(area, buf);

        paint_dispatch_feedback_badge(buf, area, theme, state.error_toast.as_deref());

        paint_dispatch_config_badge(buf, area, theme, state, input_focused);
        paint_record_badge(buf, area, theme, state.voice_listening);
        Rect {
            x: inner.x + 1,
            y: inner.y,
            width: inner.width.saturating_sub(2),
            height: inner.height.max(1),
        }
    } else {
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        }
    };
    if content.width == 0 {
        return None;
    }

    if state.search_mode {
        let prefix = "Search: ";
        let prefix_w = UnicodeWidthStr::width(prefix) as u16;
        let painted_prefix_w = prefix_w.min(content.width);
        buf.set_span(
            content.x,
            content.y,
            &Span::styled(
                prefix,
                Style::default()
                    .fg(theme.warning)
                    .bg(theme.bg_base)
                    .add_modifier(Modifier::BOLD),
            ),
            painted_prefix_w,
        );
        let editor_x = content.x + painted_prefix_w;
        let avail = content.width - painted_prefix_w;
        let cursor_column = if state.dispatch.text().is_empty() {
            if avail > 0 {
                let placeholder = truncate_str("Type to filter sessions\u{2026}", avail as usize);
                buf.set_string(
                    editor_x,
                    content.y,
                    placeholder,
                    Style::default().fg(theme.gray_dim).bg(theme.bg_base),
                );
            }
            0
        } else {
            let viewport = xai_ratatui_textarea::EditBuffer::from_parts(
                state.dispatch.text(),
                state.dispatch.cursor(),
            )
            .single_line_viewport(avail as usize);
            let visible = &state.dispatch.text()[viewport.visible_byte_range];
            if avail > 0 {
                buf.set_span(
                    editor_x,
                    content.y,
                    &Span::styled(
                        visible,
                        Style::default().fg(theme.text_primary).bg(theme.bg_base),
                    ),
                    (UnicodeWidthStr::width(visible) as u16).min(avail),
                );
            }
            viewport.cursor_display_column as u16
        };
        let cursor_offset = painted_prefix_w
            .saturating_add(cursor_column)
            .min(content.width - 1);
        let cx = content.x + cursor_offset;
        return input_focused.then_some((cx, content.y));
    }

    if content.width < 4 {
        return None;
    }

    let prefix = "\u{276F} ";
    let prefix_w = UnicodeWidthStr::width(prefix) as u16;

    let voice_overlay = (state.voice_listening || state.voice_interim.is_some()).then_some(
        crate::views::prompt_widget::VoicePromptOverlay {
            interim: state.voice_interim.as_deref(),
            color: theme.accent_running,
        },
    );

    if state.dispatch.text().is_empty() && voice_overlay.is_none() {
        buf.set_string(
            content.x,
            content.y,
            prefix,
            Style::default().fg(theme.accent_user).bg(theme.bg_base),
        );

        if !input_focused {
            let msg = "Dispatch a new agent";
            let style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
            let trunc = truncate_str(msg, content.width.saturating_sub(prefix_w) as usize);
            buf.set_string(content.x + prefix_w, content.y, trunc, style);
        }
        return input_focused.then_some((content.x + prefix_w, content.y));
    }

    let style = PromptStyle {
        focused: input_focused,
        show_prefix: true,
        vpad_top: 0,
        chrome: false,
        bg: PromptBg::Canvas(theme.bg_base),
        image_preview: false,
        ..PromptStyle::default()
    };
    state
        .dispatch
        .draw(buf, content, overlay_area, &style, None, voice_overlay)
        .cursor_pos
}

/// Desired number of *text* rows for the dispatch box given its content and the box's outer width.
/// Grows the box for multiline input (Alt+Enter) while capping growth so the row list keeps usable space.
/// Returns at least 1.
fn dispatch_text_rows(state: &DashboardState, dispatch_width: u16, area_height: u16) -> u16 {
    use crate::views::prompt_widget::PromptStyle;

    let content_w = dispatch_width.saturating_sub(4);
    if content_w < 4 {
        return 1;
    }

    let max_text_rows = (area_height / 3).clamp(1, 8);
    let style = PromptStyle {
        focused: true,
        show_prefix: true,
        vpad_top: 0,
        chrome: false,
        ..PromptStyle::default()
    };
    state
        .dispatch
        .desired_height(content_w, &style, false, max_text_rows)
}

/// Top and bottom border rows of a dispatch dropdown panel.
const DROPDOWN_CHROME_ROWS: u16 = 2;

/// Smallest panel that still carries both borders and one item row.
const MIN_DROPDOWN_PANEL_ROWS: u16 = DROPDOWN_CHROME_ROWS + 1;

/// Render the `/command` completion dropdown above the dispatch box.
/// Mirrors `agent_view`'s slash dropdown chrome.
/// No-op (and clears the stored hit rect) when the dropdown is closed.
fn render_slash_dropdown(
    buf: &mut Buffer,
    area: Rect,
    dispatch_rect: Rect,
    theme: &Theme,
    state: &mut DashboardState,
) {
    use ratatui::widgets::{Clear, Widget};

    use crate::views::slash_dropdown::{desired_item_rows, render_dropdown as render_slash};

    let snap = state.dispatch.slash_snapshot();
    if !snap.open || snap.matches.is_empty() {
        state.slash_dropdown_items_area = None;
        state.slash_dropdown_hit = Default::default();
        return;
    }

    let item_count = snap.matches.len();

    let item_rows = desired_item_rows(&snap.matches, dispatch_rect.width.saturating_sub(2));

    let panel_h = item_rows
        .saturating_add(DROPDOWN_CHROME_ROWS)
        .min(dispatch_rect.y.saturating_sub(area.y));
    if panel_h < MIN_DROPDOWN_PANEL_ROWS {
        state.slash_dropdown_items_area = None;
        state.slash_dropdown_hit = Default::default();
        return;
    }
    let top_y = dispatch_rect.y - panel_h;
    let panel_x = dispatch_rect.x;
    let panel_width = dispatch_rect.width;
    if panel_width < 4 {
        state.slash_dropdown_items_area = None;
        state.slash_dropdown_hit = Default::default();
        return;
    }
    let panel_area = Rect {
        x: panel_x,
        y: top_y,
        width: panel_width,
        height: panel_h,
    };

    Clear.render(panel_area, buf);
    buf.set_style(
        panel_area,
        Style::default().fg(theme.text_primary).bg(theme.bg_light),
    );

    let border_style = Style::default().fg(theme.bg_highlight).bg(theme.bg_base);
    let bar: String = "\u{2500}".repeat(panel_width as usize);
    buf.set_string(panel_x, top_y, &bar, border_style);
    buf.set_string(panel_x, top_y + panel_h - 1, &bar, border_style);

    let hint = format!("{item_count}");
    let hint_w = hint.len() as u16;
    if hint_w + 2 <= panel_width {
        let hint_x = panel_x + panel_width - hint_w - 1;
        buf.set_string(
            hint_x,
            top_y,
            &hint,
            Style::default().fg(theme.gray).bg(theme.bg_base),
        );
    }

    let items_x = panel_x + 1;
    let items_width = panel_width.saturating_sub(2);
    let items_area = Rect {
        x: items_x,
        y: top_y + 1,
        width: items_width,
        height: panel_h - DROPDOWN_CHROME_ROWS,
    };
    state.slash_dropdown_hit = render_slash(
        buf,
        items_area,
        &snap,
        state.dispatch.slash_hovered(),
        theme,
    );
    state.slash_dropdown_items_area = Some(items_area);
}

/// Working directory of the agent owning the peeked `row`, used to root the reply's `@` file picker.
/// Top-level rows use their own cwd; subagent rows reply to (and resolve `@paths` against) their parent; roster rows have no local agent (`None`).
/// `None` too when the agent has since vanished.
fn peeked_agent_cwd(
    row: &super::DashboardRowId,
    agents: &IndexMap<AgentId, AgentView>,
) -> Option<std::path::PathBuf> {
    let id = match row {
        super::DashboardRowId::TopLevel(id) => *id,
        super::DashboardRowId::Subagent { parent, .. } => *parent,
        super::DashboardRowId::Roster { .. } | super::DashboardRowId::Workspace { .. } => {
            return None;
        }
    };
    agents.get(&id).map(|a| a.session.cwd.clone())
}

/// Render the session-less `@` file-context picker dropdown above the dispatch box.
/// Twin of [`render_slash_dropdown`] with a `k/n` count hint.
/// No-op (and clears the hit rect) when the picker is hidden.
fn render_file_search_dropdown(
    buf: &mut Buffer,
    area: Rect,
    dispatch_rect: Rect,
    theme: &Theme,
    state: &mut DashboardState,
) {
    state.file_search_dropdown_items_area = render_file_search_dropdown_for(
        buf,
        area,
        dispatch_rect,
        theme,
        &mut state.dispatch.file_search,
    );
}

/// Paint a session-less `@` file-context dropdown ABOVE `anchor_rect` for the given [`FileSearchState`].
/// `anchor_rect` is the dispatch box, or the peek box when the reply is the active input.
/// Returns the items-area rect for mouse routing, or `None` when nothing was drawn (hidden, empty, or no room above the anchor).
fn render_file_search_dropdown_for(
    buf: &mut Buffer,
    area: Rect,
    anchor_rect: Rect,
    theme: &Theme,
    file_search: &mut crate::views::file_search::FileSearchState,
) -> Option<Rect> {
    use ratatui::widgets::{Clear, Widget};

    use crate::views::file_search::dropdown::{MAX_DROPDOWN_ROWS, render_dropdown as render_files};

    if !file_search.is_visible() {
        return None;
    }
    let item_count = file_search.result_count();
    let item_rows = (item_count as u16).min(MAX_DROPDOWN_ROWS);
    if item_rows == 0 {
        return None;
    }
    let panel_h = item_rows.saturating_add(2);
    let max_top = anchor_rect.y.saturating_sub(1);
    if max_top <= area.y {
        return None;
    }
    let top_y = max_top.saturating_sub(panel_h - 1);
    if top_y < area.y {
        return None;
    }
    let panel_x = anchor_rect.x;
    let panel_width = anchor_rect.width;
    if panel_width < 4 {
        return None;
    }

    // Keep the selected row inside the visible window before painting.
    file_search.ensure_visible(item_rows as usize);

    let panel_area = Rect {
        x: panel_x,
        y: top_y,
        width: panel_width,
        height: panel_h,
    };
    Clear.render(panel_area, buf);
    buf.set_style(
        panel_area,
        Style::default().fg(theme.text_primary).bg(theme.bg_light),
    );

    let border_style = Style::default().fg(theme.bg_highlight).bg(theme.bg_base);
    let bar: String = "\u{2500}".repeat(panel_width as usize);
    buf.set_string(panel_x, top_y, &bar, border_style);
    buf.set_string(panel_x, top_y + panel_h - 1, &bar, border_style);

    let (k, n) = (file_search.result_count(), file_search.total_items());
    let hint = if k >= 1000 {
        format!("1k+/{n}")
    } else {
        format!("{k}/{n}")
    };
    let hint_w = hint.len() as u16;
    if hint_w + 2 <= panel_width {
        let hint_x = panel_x + panel_width - hint_w - 1;
        buf.set_string(
            hint_x,
            top_y,
            &hint,
            Style::default().fg(theme.gray).bg(theme.bg_base),
        );
    }

    let items_x = panel_x + 1;
    let items_width = panel_width.saturating_sub(2);
    let items_area = Rect {
        x: items_x,
        y: top_y + 1,
        width: items_width,
        height: item_rows,
    };
    render_files(buf, items_area, file_search, theme);
    Some(items_area)
}

/// Render the dashboard's footer / shortcuts hint row.
///
/// Uses the shared `ShortcutsBar` widget so the dashboard's shortcut bar matches the agent view's bottom bar.
/// The form is `Key:label` with bold keys and dim ` │ ` separators on `bg_base`.
/// When a stop-confirm is armed, `ShortcutsBar::with_pending` paints the `press again to {label}` hint in place of the regular list.
/// The agent view uses the same mechanism.
///
/// Keys are resolved from the action registry every frame so user rebindings show up immediately.
/// Footer hints flip based on the selected row's state:
///
/// - A `NeedsInput` selection shows `Enter:see details · Ctrl+x:stop · ?:shortcuts`.
///   There is no inline approve/reject yet; the dashboard is intentionally a navigator, not a permission UI.
/// - Anything else shows `Enter:open · Ctrl+x:stop|delete · ?:shortcuts`.
///
/// The Ctrl+x chip label follows the selected agent's state: `stop` for an agent with a live turn, `delete` otherwise.
/// NeedsInput counts as a live turn: it is paused but still running, so the first Ctrl+x cancels.
#[allow(clippy::too_many_arguments)]
fn render_footer(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    state: &DashboardState,
    registry: &crate::actions::ActionRegistry,
    selected_state: Option<RowState>,
    peek_active: bool,
    pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
) {
    use ratatui::widgets::Widget;

    use crate::input::key::{KeyShortcut, key};
    use crate::views::shortcuts_bar::{HintItem, PendingHint, ShortcutsBar};

    if area.area() == 0 {
        return;
    }

    const FOOTER_PAD_LEFT: u16 = 2;
    let _ = theme;
    let inner = Rect {
        x: area.x.saturating_add(FOOTER_PAD_LEFT),
        y: area.y,
        width: area.width.saturating_sub(FOOTER_PAD_LEFT),
        height: area.height,
    };

    if let Some(pending) = pending_hint {
        ShortcutsBar::new(&[])
            .with_pending(Some(pending))
            .render(inner, buf);
        return;
    }

    if state.armed_delete_row_ref().is_some() {
        if state.list_focused {
            let hints = vec![
                HintItem::new(key!('y'), "confirm delete"),
                HintItem::new(key!('n'), "cancel"),
            ];
            ShortcutsBar::new(&hints)
                .compact(4, None)
                .render(inner, buf);
        } else {
            let stop_key = registry
                .find(crate::actions::ActionId::DashboardStop)
                .map(|d| d.default_key)
                .unwrap_or_else(|| key!('x', CONTROL));
            let pending = PendingHint {
                shortcut: stop_key,
                label: "delete this session",
            };
            ShortcutsBar::new(&[])
                .with_pending(Some(pending))
                .render(inner, buf);
        }
        return;
    }

    if state.rename.is_some() {
        let hints = vec![
            HintItem::new(key!(Enter), "save"),
            HintItem::new(key!(Esc), "cancel"),
        ];
        ShortcutsBar::new(&hints)
            .compact(4, None)
            .render(inner, buf);
        return;
    }

    if state.search_mode {
        let hints = vec![
            HintItem::paired(key!(Up), key!(Down), "nav"),
            HintItem::new(key!(Enter), "apply"),
            HintItem::new(key!(Esc), "cancel"),
        ];
        ShortcutsBar::new(&hints)
            .compact(4, None)
            .render(inner, buf);
        return;
    }

    let show_ctrl_x = !state
        .selected
        .as_ref()
        .is_some_and(DashboardRowId::is_workspace)
        && selected_state.is_some_and(|s| {
            matches!(s, RowState::Working | RowState::NeedsInput) || s.allows_delete()
        });
    let stop_label = if matches!(
        selected_state,
        Some(RowState::Working | RowState::NeedsInput)
    ) {
        "stop"
    } else {
        "delete"
    };

    //

    if state.list_focused && !peek_active {
        let key_for = |id: crate::actions::ActionId, fallback: KeyShortcut| -> KeyShortcut {
            registry.find(id).map(|d| d.default_key).unwrap_or(fallback)
        };
        let stop = key_for(crate::actions::ActionId::DashboardStop, key!('x', CONTROL));
        let help = key_for(
            crate::actions::ActionId::DashboardShortcutsHelp,
            key!('.', CONTROL),
        );
        // The ↑/↓ (and vim j/k) nav chip is intentionally omitted

        if state.selected_idle_overflow {
            let toggle = if state.idle_show_all {
                "show fewer"
            } else {
                "show all"
            };
            let hints = vec![
                HintItem::new(key!(Enter), toggle),
                HintItem::new(key!(Tab), "input"),
            ];
            ShortcutsBar::new(&hints)
                .compact(4, Some(HintItem::new(help, "shortcuts")))
                .render(inner, buf);
            return;
        }

        if let Some(section) = state.selected_section {
            let toggle = if state.is_section_collapsed(section) {
                "expand"
            } else {
                "collapse"
            };
            let hints = vec![
                HintItem::new(key!(Enter), toggle),
                HintItem::new(key!(Tab), "input"),
            ];
            ShortcutsBar::new(&hints)
                .compact(4, Some(HintItem::new(help, "shortcuts")))
                .render(inner, buf);
            return;
        }
        let mut hints = vec![
            HintItem::new(key!(Enter), "open"),
            HintItem::new(key!(Tab), "input"),
        ];
        if show_ctrl_x {
            hints.push(HintItem::new(stop, stop_label).pinned());
        }

        ShortcutsBar::new(&hints)
            .compact(4, Some(HintItem::new(help, "shortcuts")))
            .render(inner, buf);
        return;
    }

    let resolve = |id: crate::actions::ActionId, fallback: KeyShortcut| -> KeyShortcut {
        registry.find(id).map(|d| d.default_key).unwrap_or(fallback)
    };
    let enter = key!(Enter);

    let send_open = key!('s', CONTROL);

    let send_key = if state.multiline_mode {
        if crate::terminal::terminal_context().shift_enter_unavailable() {
            key!(Enter, ALT)
        } else {
            key!(Enter, SHIFT)
        }
    } else {
        enter
    };
    let stop = resolve(crate::actions::ActionId::DashboardStop, key!('x', CONTROL));
    let help = resolve(
        crate::actions::ActionId::DashboardShortcutsHelp,
        key!('.', CONTROL),
    );

    let help_hint = HintItem::new(help, "shortcuts");

    let button_focused = state.new_agent_button_focused;
    let row_selected = state.selected.is_some();
    let prompt_empty = state.dispatch.text().trim().is_empty();

    let hints: Vec<HintItem> = if peek_active {
        //   vim + unfocused reply → Enter focuses reply ("input")
        //   focused + non-empty   → Enter sends
        //   otherwise             → Enter opens / attaches
        // Ctrl+S remains send+open whenever there is a non-empty draft.

        let esc = key!(Esc);

        // Mirrors `DashboardState::handle_peek_key`.
        let has_pending_question = state
            .peek
            .as_ref()
            .is_some_and(|p| p.question.is_some() && !p.options.is_empty());
        let option_selected = state
            .peek
            .as_ref()
            .is_some_and(|p| p.selected_option.is_some());
        let reply_empty = state.peek_reply.text().trim().is_empty();
        let esc_label = if reply_empty { "New Agent" } else { "back" };

        let esc_hint = {
            let h = HintItem::new(esc, esc_label);
            if reply_empty { h } else { h.pinned() }
        };
        let vim_mode = crate::appearance::cache::load_vim_mode();

        let peek_focused = state.peek.as_ref().map(|p| p.focused).unwrap_or(true);
        let question_focused = peek_focused && has_pending_question;
        let tab_hint = HintItem::new(key!(Tab), if peek_focused { "list" } else { "input" });
        // `1-9 select` hint for the question picker (no single bound key).
        let select_hint = HintItem {
            keys: vec![],
            label: "select".into(),
            custom_display: Some("1-9"),
            description: None,
            pinned: false,
        };
        if question_focused && option_selected {
            vec![HintItem::new(enter, "answer"), tab_hint, esc_hint]
        } else if has_pending_question && peek_focused {
            let mut h = vec![
                HintItem::new(enter, "open"),
                select_hint,
                tab_hint,
                esc_hint,
            ];
            if show_ctrl_x {
                h.push(HintItem::new(stop, stop_label).pinned());
            }
            h
        } else if vim_mode && !peek_focused {
            // Vim unfocused: Enter focuses the reply (not open/send).

            // Pending question: keep 1-9 select (digits still work unfocused).
            let mut h = vec![
                HintItem::new(enter, "input"),
                // Pin open: attach is the replacement for Enter in this mode.
                HintItem::new(key!(Right), "open").pinned(),
                tab_hint,
                esc_hint,
            ];
            if has_pending_question {
                h.insert(2, select_hint);
            }
            if !reply_empty {
                h.insert(1, HintItem::new(send_open, "send+open"));
            }
            if show_ctrl_x {
                h.push(HintItem::new(stop, stop_label).pinned());
            }
            h
        } else if has_pending_question {
            let mut h = vec![
                HintItem::new(enter, "open"),
                select_hint,
                tab_hint,
                esc_hint,
            ];
            if show_ctrl_x {
                h.push(HintItem::new(stop, stop_label).pinned());
            }
            h
        } else if peek_focused && !reply_empty {
            vec![
                HintItem::new(send_key, "send"),
                HintItem::new(send_open, "send+open"),
                tab_hint,
                HintItem::new(esc, "back").pinned(),
            ]
        } else {
            // Focused empty: open is on the submit chord (send_key)

            let open_key = if peek_focused { send_key } else { enter };
            let mut h = vec![HintItem::new(open_key, "open"), tab_hint, esc_hint];
            if show_ctrl_x {
                h.push(HintItem::new(stop, stop_label).pinned());
            }
            h
        }
    } else if let Some(section) = state.selected_section {
        if prompt_empty {
            let toggle = if state.is_section_collapsed(section) {
                "expand"
            } else {
                "collapse"
            };
            vec![
                HintItem::new(enter, toggle),
                HintItem::new(key!(Esc), "New Agent"),
            ]
        } else {
            vec![
                HintItem::new(send_key, "send"),
                HintItem::new(send_open, "send+open"),
                HintItem::new(key!(BackTab), "mode"),
            ]
        }
    } else if state.selected_idle_overflow {
        if prompt_empty {
            let toggle = if state.idle_show_all {
                "show fewer"
            } else {
                "show all"
            };
            vec![
                HintItem::new(enter, toggle),
                HintItem::new(key!(Esc), "New Agent"),
            ]
        } else {
            vec![
                HintItem::new(send_key, "send"),
                HintItem::new(send_open, "send+open"),
                HintItem::new(key!(BackTab), "mode"),
            ]
        }
    } else if button_focused {
        let mut h: Vec<HintItem> = vec![];
        if prompt_empty {
            h.push(HintItem::new(send_key, "create"));
            h.push(HintItem::new(key!(Tab), "list"));
        } else {
            h.push(HintItem::new(send_key, "send"));
            h.push(HintItem::new(send_open, "send+open"));
        }
        h.push(HintItem::new(key!(BackTab), "mode"));
        h
    } else if row_selected {
        let mut h: Vec<HintItem> = vec![];
        if prompt_empty {
            h.push(HintItem::new(send_key, "open"));
            h.push(HintItem::new(key!(Tab), "list"));
        } else {
            h.push(HintItem::new(send_key, "send"));
            h.push(HintItem::new(send_open, "send+open"));
        }
        if show_ctrl_x {
            h.push(HintItem::new(stop, stop_label).pinned());
        }
        h
    } else {
        vec![HintItem::new(send_key, "create")]
    };

    ShortcutsBar::new(&hints)
        .compact(4, Some(help_hint))
        .render(inner, buf);
}

fn state_icon(state: RowState, tick: u64) -> &'static str {
    match state {
        RowState::Working => {
            let frames = crate::glyphs::dot_spinner_frames();
            let i = (tick / SPINNER_DIVISOR) as usize % frames.len();
            frames[i]
        }

        RowState::Idle | RowState::Inactive => crate::glyphs::diamond_hollow(),
        RowState::NeedsInput | RowState::Completed | RowState::Failed => {
            crate::glyphs::diamond_filled()
        }
    }
}

fn state_color(state: RowState, theme: &Theme) -> Color {
    match state {
        RowState::Working => theme.accent_running,
        RowState::NeedsInput => theme.warning,
        RowState::Idle | RowState::Inactive => theme.gray_dim,
        RowState::Completed => theme.accent_success,
        RowState::Failed => theme.accent_error,
    }
}

/// Bullet colour for a `NeedsInput` row: blinks between full yellow (`warning`) and a dimmed yellow so an agent awaiting input draws the eye.
/// On non-truecolor terminals the dim blend falls back to the full colour (the blink is invisible but the bullet stays yellow).
fn needs_input_bullet_color(tick: u64, theme: &Theme) -> Color {
    let bright = (tick / NEEDS_INPUT_BLINK_DIVISOR).is_multiple_of(2);
    if bright {
        theme.warning
    } else {
        crate::render::color::blend_color(theme.bg_base, theme.warning, 0.5)
            .unwrap_or(theme.warning)
    }
}

fn badge_color(badge: RowBadge, theme: &Theme) -> Color {
    match badge {
        RowBadge::Worktree => theme.accent_user,
        RowBadge::NeedsInput => theme.warning,
        RowBadge::BgTask => theme.command,
        RowBadge::Pinned => theme.accent_running,
        RowBadge::Failed => theme.accent_error,
    }
}

/// Process-wide cached home directory via [`xai_dirs::home_dir`].
/// Shared by render and dispatch (`dispatch_dashboard_select`) so we don't re-resolve on every keystroke.
pub(crate) fn cached_home() -> Option<&'static str> {
    HOME.get_or_init(|| {
        xai_dirs::home_dir()
            .map(|home| home.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
    })
    .as_deref()
}

static HOME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

// ---------------------------------------------------------------------------
// Popup overlay (banner-style)
// ---------------------------------------------------------------------------

/// Compute the rect for the attached-agent popup overlay.
///
/// The popup takes the FULL bottom of the screen (no horizontal inset, no bottom inset).
/// Only a dynamic top inset is reserved for the dashboard banner.
///
/// The previous ~1/6-inset-on-all-sides design left the dashboard's dispatch input and footer visible BELOW the popup.
/// That produced two stacked input bars.
/// The banner above the popup carries the live row list in a bordered panel.
///
/// (Historical legacy comment kept for context: the old layout description is no longer accurate; see the function body for the current layout.)
/// terminal yields a ~164×48 popup with a ~36×12 dashboard frame visible around it.
///
/// On terminals too small to honour the minimum inset, falls through to a 0-inset takeover.
/// There is no escape hatch: any non-zero inset would clip the agent's prompt below readability.
pub fn popup_rect(view: Rect) -> Rect {
    //

    const BANNER_MIN_HEIGHT: u16 = 6;
    const BANNER_MAX_HEIGHT: u16 = 14;

    let banner_h: u16 = if view.height >= BANNER_MIN_HEIGHT + 10 {
        (view.height / 3).clamp(BANNER_MIN_HEIGHT, BANNER_MAX_HEIGHT)
    } else {
        0
    };
    Rect {
        x: view.x,
        y: view.y + banner_h,
        width: view.width,
        height: view.height.saturating_sub(banner_h),
    }
}

/// Render the popup overlay frame and the attached agent's view via the canonical [`picker::render_bordered_frame`].
/// That is the same chrome primitive `agent_view::draw_subagent_fullscreen` uses.
/// Returns `(cursor_pos, post_flush)` from the agent's draw.
/// The caller places the cursor on the popup's prompt and merges kitty/sixel image escape sequences into the frame's outgoing notification stream.
///
/// `title_label` is passed by the caller rather than computed here from a borrowed `AgentView`.
/// The closure can then take a mutable borrow of the agents map without conflicting with the title lookup.
///
/// Side effects on `state`:
/// - `popup_close_rect` is registered so a click on the `[✗]` button can dispatch a popup close in `handle_mouse`.
/// - `popup_outer_rect` is registered so clicks outside the popup can be routed correctly.
///
/// Too-small popup areas (`area.height < 5` or `area.width < 10`) get a fallback ("terminal too small: Esc to close") and `(None, None, false)`.
pub fn render_popup_overlay(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    title_label: &str,
    state: &mut DashboardState,
    draw_agent: impl FnOnce(
        Rect,
        &mut Buffer,
    ) -> (
        Option<(u16, u16)>,
        Option<crate::terminal::overlay::PostFlush>,
    ),
) -> (
    Option<(u16, u16)>,
    Option<crate::terminal::overlay::PostFlush>,
    bool,
) {
    use ratatui::widgets::{Block, Borders, Clear, Widget};

    if area.area() == 0 {
        state.popup_close_rect = None;
        state.popup_outer_rect = None;
        return (None, None, false);
    }
    state.popup_outer_rect = Some(area);

    let border_color = theme.selection_border;

    let Some(frame) =
        crate::views::picker::render_bordered_frame(buf, area, border_color, theme.bg_base)
    else {
        //

        Clear.render(area, buf);
        buf.set_style(area, Style::default().bg(theme.bg_base));
        let outline = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color).bg(theme.bg_base));
        outline.render(area, buf);
        if area.height >= 3 && area.width >= 6 {
            let hint = truncate_str(
                "(terminal too small: Esc to close)",
                area.width.saturating_sub(2) as usize,
            );
            buf.set_string(
                area.x + 1,
                area.y + area.height / 2,
                hint,
                Style::default().fg(theme.gray_dim).bg(theme.bg_base),
            );
        }
        state.popup_close_rect = None;
        return (None, None, false);
    };

    let title_row = frame.title_row;
    let inner = frame.content;

    let title_text = format!(" \u{2771} {title_label} ");

    let close_label = crate::glyphs::ballot_x_button();
    let close_w = UnicodeWidthStr::width(close_label) as u16;

    let title_max = title_row.width.saturating_sub(close_w + 2).max(1) as usize;
    let truncated = truncate_str(&title_text, title_max);
    buf.set_string(
        title_row.x,
        title_row.y,
        truncated,
        Style::default()
            .fg(theme.text_primary)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD),
    );

    if close_w + 1 < title_row.width {
        let close_x = title_row.x + title_row.width - close_w;
        buf.set_string(
            close_x,
            title_row.y,
            close_label,
            Style::default().fg(theme.gray).bg(theme.bg_base),
        );
        state.popup_close_rect = Some(Rect {
            x: close_x,
            y: title_row.y,
            width: close_w,
            height: 1,
        });
    } else {
        state.popup_close_rect = None;
    }

    let (cursor, post_flush) = draw_agent(inner, buf);
    (cursor, post_flush, true)
}

/// Paint a bordered frame around an attached agent view, matching the subagent-fullscreen pattern.
/// The frame reserves a title row at the top with `{title}` on the left and `{i}/{n} [‹][›] [Dashboard]` on the right.
/// Below that come a horizontal divider, then the agent's content area.
///
/// Layout:
///
/// ```text
/// ╭─────────────────────────────────────────────────────────╮
/// │ Add responsiveness to /context    1/3 [‹][›] [Dashboard] │
/// ├─────────────────────────────────────────────────────────┤
/// │                                                          │
/// │   <agent.draw paints into here>                          │
/// │                                                          │
/// ╰──────────────────────────────────────────────────────────╯
/// ```
///
/// The agent renders inside the frame's `content` rect.
/// The agent's own shortcuts bar sits at the bottom of the content, inside the frame.
/// That bar carries the `Ctrl+\\:dashboard` hint, and `Ctrl+[/]:prev/next agent` when the cycle order has more than one agent.
///
/// Returns `None` when the area is too small for the bordered frame; the caller falls back to a chromeless render so the user can use the agent.
///
/// `position` carries `(current_1_based, total)` for the visible row list.
/// When `Some` and `total > 1`, the renderer paints the position indicator and cycle chips.
/// When `None` (or only one row), the cycle chips are omitted.
/// `hover` flags drive per-button highlight so the mouse user gets visual feedback.
#[allow(clippy::too_many_arguments)]
pub fn render_dashboard_session_overlay(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    title_label: &str,
    position: Option<(usize, usize)>,
    hover_prev: bool,
    hover_next: bool,
    hover_close: bool,
) -> Option<DashboardOverlayChrome> {
    if area.area() == 0 || area.height < 5 || area.width < 20 {
        return None;
    }
    let border_color = theme.selection_border;
    let frame =
        crate::views::picker::render_bordered_frame(buf, area, border_color, theme.bg_base)?;

    let (close_rect, prev_rect, next_rect) = paint_session_title_bar(
        buf,
        frame.title_row,
        theme,
        title_label,
        position,
        hover_prev,
        hover_next,
        hover_close,
        1,
        1,
    );
    Some(DashboardOverlayChrome {
        content: frame.content,
        close_rect,
        prev_rect,
        next_rect,
    })
}

/// Like [`render_dashboard_session_overlay`] but paints ONLY a top header bar, with **no surrounding border**.
/// The bar is the agent's title on the left and the `{i}/{n} [‹][›] [Dashboard]` buttons on the right.
/// The returned `content` is `area` minus the header band, so the agent body below renders full-width.
/// Its prompt position and overall padding then match the dashboard list view (instead of being inset by a modal frame).
///
/// `pad_left` / `pad_right` / `pad_top` mirror the agent body's outer padding (`eff_hpad_left` / `eff_hpad_right` / `eff_outer_vpad`).
/// The title text lines up with the body's left content edge, and the chips line up with its right edge.
/// The bar gets the same top breathing room as everything below it.
/// The header band is `pad_top` blank rows plus one title row.
/// The agent body that fills `content` then applies its OWN top padding, separating it from the header.
#[allow(clippy::too_many_arguments)]
pub fn render_dashboard_session_header(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    title_label: &str,
    position: Option<(usize, usize)>,
    hover_prev: bool,
    hover_next: bool,
    hover_close: bool,
    pad_left: u16,
    pad_right: u16,
    pad_top: u16,
) -> Option<DashboardOverlayChrome> {
    let band_height = pad_top.saturating_add(1);
    if area.area() == 0
        || area.height <= band_height
        || area.width <= pad_left.saturating_add(pad_right).saturating_add(12)
    {
        return None;
    }

    let fill = " ".repeat(area.width as usize);
    for dy in 0..band_height {
        buf.set_string(
            area.x,
            area.y + dy,
            &fill,
            Style::default().bg(theme.bg_base),
        );
    }

    let title_row = Rect {
        x: area.x + pad_left,
        y: area.y + pad_top,
        width: area.width.saturating_sub(pad_left + pad_right),
        height: 1,
    };
    let (close_rect, prev_rect, next_rect) = paint_session_title_bar(
        buf,
        title_row,
        theme,
        title_label,
        position,
        hover_prev,
        hover_next,
        hover_close,
        0,
        0,
    );
    let content = Rect {
        x: area.x,
        y: area.y + band_height,
        width: area.width,
        height: area.height.saturating_sub(band_height),
    };
    Some(DashboardOverlayChrome {
        content,
        close_rect,
        prev_rect,
        next_rect,
    })
}

/// Paint the session title bar onto a single-row `title_row`, on `bg_base` with no button fills.
/// The bar is the title on the left and the `{i}/{n} [‹][›] [Dashboard]` buttons on the right.
/// Returns the three button hit rects.
/// Shared by the bordered overlay and the chromeless header bar.
///
/// `left_inset` / `right_inset` reserve that many columns inside `title_row` before the title text (left) and after the chips (right).
/// The bordered overlay passes `1` on each side for a small gap from its border.
/// The chromeless header passes `0` because its `title_row` is already positioned at the desired padding boundaries.
#[allow(clippy::too_many_arguments)]
fn paint_session_title_bar(
    buf: &mut Buffer,
    title_row: Rect,
    theme: &Theme,
    title_label: &str,
    position: Option<(usize, usize)>,
    hover_prev: bool,
    hover_next: bool,
    hover_close: bool,
    left_inset: u16,
    right_inset: u16,
) -> (Option<Rect>, Option<Rect>, Option<Rect>) {
    //

    let close_label = "[Dashboard]";
    let prev_label = format!("[{}]", crate::glyphs::chevron_left());
    let next_label = format!("[{}]", crate::glyphs::chevron());
    let close_w = UnicodeWidthStr::width(close_label) as u16;
    let prev_w = prev_label.width() as u16;
    let next_w = next_label.width() as u16;

    let left_bound = title_row.x.saturating_add(left_inset);

    let mut rx = title_row
        .x
        .saturating_add(title_row.width)
        .saturating_sub(right_inset);

    let close_rect = if rx >= left_bound + close_w {
        rx = rx.saturating_sub(close_w);
        let fg = if hover_close {
            theme.text_primary
        } else {
            theme.gray
        };
        let style = Style::default()
            .fg(fg)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD);
        buf.set_string(rx, title_row.y, close_label, style);
        Some(Rect {
            x: rx,
            y: title_row.y,
            width: close_w,
            height: 1,
        })
    } else {
        None
    };

    let cycle_enabled = position.is_some_and(|(_, n)| n > 1);
    let (prev_rect, next_rect) = if cycle_enabled {
        let mut prev_r = None;
        let mut next_r = None;

        // Paint `[›]` as plain text (no bg), like [Dashboard].
        if rx >= left_bound + 1 + next_w {
            rx = rx.saturating_sub(1 + next_w);
            let fg = if hover_next {
                theme.text_primary
            } else {
                theme.gray
            };
            let style = Style::default()
                .fg(fg)
                .bg(theme.bg_base)
                .add_modifier(Modifier::BOLD);
            buf.set_string(rx, title_row.y, &next_label, style);
            next_r = Some(Rect {
                x: rx,
                y: title_row.y,
                width: next_w,
                height: 1,
            });
        }

        if rx >= left_bound + prev_w {
            rx = rx.saturating_sub(prev_w);
            let fg = if hover_prev {
                theme.text_primary
            } else {
                theme.gray
            };
            let style = Style::default()
                .fg(fg)
                .bg(theme.bg_base)
                .add_modifier(Modifier::BOLD);
            buf.set_string(rx, title_row.y, &prev_label, style);
            prev_r = Some(Rect {
                x: rx,
                y: title_row.y,
                width: prev_w,
                height: 1,
            });
        }

        if let Some((cur, total)) = position {
            let pos_text = format!("{cur}/{total}");
            let pos_w = UnicodeWidthStr::width(pos_text.as_str()) as u16;
            if rx >= left_bound + 1 + pos_w {
                rx = rx.saturating_sub(1 + pos_w);
                let style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
                buf.set_string(rx, title_row.y, &pos_text, style);
            }
        }
        (prev_r, next_r)
    } else {
        (None, None)
    };

    if rx > left_bound {
        let title_avail = rx.saturating_sub(left_bound).saturating_sub(1) as usize;
        if title_avail > 0 {
            let trunc = truncate_str(title_label, title_avail);
            buf.set_string(
                left_bound,
                title_row.y,
                trunc,
                Style::default()
                    .fg(theme.text_primary)
                    .bg(theme.bg_base)
                    .add_modifier(Modifier::BOLD),
            );
        }
    }

    (close_rect, prev_rect, next_rect)
}

/// Output of [`render_dashboard_session_overlay`]: the agent's drawing rect (the bordered frame's inner content area).
/// Also carries the three hit rects for the title-bar buttons.
#[derive(Debug, Clone, Copy)]
pub struct DashboardOverlayChrome {
    pub content: Rect,
    pub close_rect: Option<Rect>,
    pub prev_rect: Option<Rect>,
    pub next_rect: Option<Rect>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::dashboard::DashboardRowId;
    use crate::views::dashboard::state::DashboardState;

    /// Spinner glyph stays stable for `SPINNER_DIVISOR` successive ticks before advancing.
    #[test]
    fn state_icon_spinner_advances_every_n_ticks() {
        let g0 = state_icon(RowState::Working, 0);
        // Same glyph for divisor-1 more ticks.
        for t in 1..SPINNER_DIVISOR {
            assert_eq!(state_icon(RowState::Working, t), g0);
        }
        let g1 = state_icon(RowState::Working, SPINNER_DIVISOR);
        assert_ne!(g1, g0, "spinner must advance after SPINNER_DIVISOR ticks");
    }

    /// Every `RowState` variant resolves to a glyph.
    #[test]
    fn state_icon_one_per_variant() {
        assert!(!state_icon(RowState::Working, 0).is_empty());
        assert!(!state_icon(RowState::NeedsInput, 0).is_empty());
        assert!(!state_icon(RowState::Idle, 0).is_empty());
        assert!(!state_icon(RowState::Completed, 0).is_empty());
        assert!(!state_icon(RowState::Failed, 0).is_empty());
    }

    /// The dispatch dropdown paints upward from the input.
    /// A panel taller than the space above it used to saturate to row 0 and run off the bottom of the buffer.
    #[test]
    fn slash_dropdown_never_paints_outside_a_short_dashboard() {
        for (top, height) in [(0u16, 24u16)]
            .into_iter()
            .chain((4..=24u16).map(|h| (0, h)))
            .chain((6..=12u16).map(|h| (3, h)))
        {
            let area = Rect::new(0, top, 80, height.saturating_sub(top));
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, height));
            let mut state = DashboardState::new();
            state.dispatch.set_text("/");
            state
                .dispatch
                .refresh_slash(&crate::acp::model_state::ModelState::default());
            assert!(
                !state.dispatch.slash_snapshot().matches.is_empty(),
                "`/` must offer commands, or the geometry below is never exercised"
            );
            let dispatch_rect = Rect::new(0, area.bottom().saturating_sub(2), 80, 1);

            render_slash_dropdown(&mut buf, area, dispatch_rect, &Theme::default(), &mut state);

            if area.height >= 8 {
                assert!(
                    state.slash_dropdown_items_area.is_some(),
                    "area={area:?}: dropdown must paint when the input has room above it"
                );
            }
            if let Some(items) = state.slash_dropdown_items_area {
                assert!(
                    items.height >= 1,
                    "area={area:?}: a clamped panel must still hold an item row"
                );
                assert!(
                    items.bottom() <= dispatch_rect.y,
                    "area={area:?}: items ran into the dispatch input"
                );
                assert!(
                    items.y >= area.y && items.bottom() <= area.bottom(),
                    "area={area:?}: items ran off the buffer"
                );
            }
        }
    }

    /// Helper: read buffer row-by-row so multi-cell substring checks see the visible text in left-to-right order.
    fn buf_to_text(buf: &Buffer) -> String {
        let mut content = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                content.push_str(buf[(x, y)].symbol());
            }
            content.push('\n');
        }
        content
    }

    #[test]
    fn workspace_dashboard_renders_snapshot_member_without_delete_control() {
        let area = Rect::new(0, 0, 100, 28);
        let mut buf = Buffer::empty(area);
        let mut state = DashboardState::new();
        state.hovered_row = Some(DashboardRowId::Workspace {
            session_id: "saved-session".to_owned(),
        });
        let mut agents = IndexMap::new();
        let registry = crate::actions::ActionRegistry::defaults();
        let snapshot = xai_grok_dashboard_store::WorkspaceSnapshot {
            grouping: xai_grok_dashboard_store::Grouping::State,
            members: vec![xai_grok_dashboard_store::Member {
                session_id: xai_grok_dashboard_store::SessionId::new("saved-session").unwrap(),
                kind: xai_grok_dashboard_store::MemberKind::Build,
                origin: xai_grok_dashboard_store::MemberOrigin::Local,
                cwd: Some("/tmp/saved".to_owned()),
                title: Some("Saved workspace session".to_owned()),
                model: Some("grok-test".to_owned()),
                last_turn_summary: Some("Stored summary".to_owned()),
                is_worktree: false,
                last_change_unix_ms: 1_725_000_000_000,
                pin_rank: None,
                order_rank: None,
            }],
            data_version: 1,
        };

        let _ = render_dashboard(
            &mut buf,
            area,
            &mut state,
            &mut agents,
            &registry,
            None,
            &[],
            true,
            Some(&snapshot),
            false,
            None,
        );

        let content = buf_to_text(&buf);
        assert!(content.contains("Saved workspace session"));
        assert!(content.contains("Stored summary"));
        assert!(
            state.row_delete_rects.is_empty(),
            "workspace-only rows must not expose v1's permanent delete action"
        );
    }

    /// The empty state with no agents renders the single hint line (never a fully blank screen).
    #[test]
    fn render_empty_state_paints_hint_line() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 10));
        let theme = Theme::current();
        render_empty_state(&mut buf, Rect::new(0, 0, 80, 10), &theme, false);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("No agents yet, type a prompt to start one."),
            "expected empty-state hint, got: {content:?}"
        );
    }

    #[test]
    fn render_dashboard_shows_roster_when_local_agents_empty() {
        use crate::app::roster::{RosterActivity, RosterEntry, RosterOrigin};

        let area = Rect::new(0, 0, 100, 24);
        let mut buf = Buffer::empty(area);
        let mut agents: IndexMap<AgentId, AgentView> = IndexMap::new();
        let mut state = DashboardState::new();
        let registry = crate::actions::ActionRegistry::defaults();
        let roster = [RosterEntry {
            session_id: "sess-fleet-1".into(),
            title: Some("Fix fleet dashboard".into()),
            cwd: "/repo/work".into(),
            is_worktree: false,
            model_id: None,
            yolo: false,
            activity: RosterActivity::Working,
            last_turn_summary: None,
            resident: true,
            last_change_unix_ms: 1_725_000_000_000,
            origin: RosterOrigin::default(),
        }];

        let _ = render_dashboard(
            &mut buf,
            area,
            &mut state,
            &mut agents,
            &registry,
            None,
            &roster,
            false,
            None,
            false,
            None,
        );

        let content = buf_to_text(&buf);
        assert!(
            content.contains("Fix fleet dashboard"),
            "roster-only working session must paint when local agents are empty, got: {content:?}"
        );
        assert!(
            !content.contains("No agents yet"),
            "must not show empty-state while roster rows exist, got: {content:?}"
        );
    }

    /// Hover `[✗]` paints only on settled rows, never on a busy one.
    #[test]
    fn render_dashboard_hover_shows_delete_x_only_for_settled_rows() {
        use crate::app::roster::{RosterActivity, RosterEntry, RosterOrigin};

        let ballot = crate::glyphs::ballot_x_button();
        let render_with = |activity: RosterActivity| -> String {
            let area = Rect::new(0, 0, 100, 24);
            let mut buf = Buffer::empty(area);
            let mut agents: IndexMap<AgentId, AgentView> = IndexMap::new();
            let mut state = DashboardState::new();
            let registry = crate::actions::ActionRegistry::defaults();
            let roster = [RosterEntry {
                session_id: "sess-hover".into(),
                title: Some("Hover me".into()),
                cwd: "/repo/work".into(),
                is_worktree: false,
                model_id: None,
                yolo: false,
                activity,
                last_turn_summary: None,
                resident: true,
                last_change_unix_ms: 1_725_000_000_000,
                origin: RosterOrigin::default(),
            }];
            state.hovered_row = Some(DashboardRowId::Roster {
                session_id: "sess-hover".into(),
            });
            let _ = render_dashboard(
                &mut buf,
                area,
                &mut state,
                &mut agents,
                &registry,
                None,
                &roster,
                false,
                None,
                false,
                None,
            );
            buf_to_text(&buf)
        };

        assert!(
            render_with(RosterActivity::Completed).contains(ballot),
            "hovering a settled (completed) row must show the [✗] delete affordance",
        );
        assert!(
            !render_with(RosterActivity::Working).contains(ballot),
            "hovering a busy (working) row must NOT show the [✗] delete affordance",
        );
    }

    /// While the local session roster is still loading the empty body shows a loading hint instead of the "no agents" copy.
    #[test]
    fn render_empty_state_paints_loading_hint() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 10));
        let theme = Theme::current();
        render_empty_state(&mut buf, Rect::new(0, 0, 80, 10), &theme, true);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("Loading sessions"),
            "expected loading hint while sessions load, got: {content:?}"
        );
        assert!(
            !content.contains("No agents yet"),
            "loading state must not show the empty-state copy, got: {content:?}"
        );
    }

    /// The hint still paints on a 1-row area (the `y_offset` collapses to 0 instead of overflowing the rect).
    #[test]
    fn render_empty_state_paints_on_single_row_area() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 1));
        let theme = Theme::current();
        render_empty_state(&mut buf, Rect::new(0, 0, 80, 1), &theme, false);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("No agents yet"),
            "expected empty-state hint on 1-row area, got: {content:?}"
        );
    }

    /// Session overlay paints a full bordered frame with
    /// `{title}` on the left of the title row and four
    /// affordances on the right: position indicator `{i}/{n}`,
    /// previous-row chip `[‹]`, next-row chip `[›]`, and close
    /// chip `[Dashboard]`. All four are plain bracketed text on
    /// `bg_base` (no button-fill background); hover only changes
    /// the foreground color.
    #[test]
    fn render_dashboard_session_overlay_paints_bordered_frame_chrome() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 10));
        let theme = Theme::current();
        let chrome = render_dashboard_session_overlay(
            &mut buf,
            Rect::new(0, 0, 80, 10),
            &theme,
            "Add responsiveness to /context",
            Some((1, 2)),
            false,
            false,
            false,
        )
        .expect("overlay must paint on a reasonably sized area");
        let content = buf_to_text(&buf);
        assert!(
            content.contains("Add responsiveness to /context"),
            "title must render, got: {content:?}",
        );
        for chip in ["[‹]", "[›]", "[Dashboard]"] {
            assert!(
                content.contains(chip),
                "overlay must paint `{chip}`, got: {content:?}",
            );
        }
        assert!(
            content.contains("1/2"),
            "overlay must paint the `1/2` position indicator, got: {content:?}",
        );
        // Frame chrome.
        for corner in ['\u{250c}', '\u{2510}', '\u{2514}', '\u{2518}'] {
            assert!(
                content.contains(corner),
                "overlay must paint frame corner `{corner}`, got: {content:?}",
            );
        }
        for tee in ['\u{251c}', '\u{2524}'] {
            assert!(
                content.contains(tee),
                "overlay must paint title-divider T-junction `{tee}`, got: {content:?}",
            );
        }
        assert!(chrome.prev_rect.is_some(), "prev_rect must be populated");
        assert!(chrome.next_rect.is_some(), "next_rect must be populated");
        assert!(chrome.close_rect.is_some(), "close_rect must be populated");

        let prev = chrome.prev_rect.unwrap();
        let prev_cell = &buf[(prev.x, prev.y)];
        assert_eq!(
            prev_cell.bg, theme.bg_base,
            "`[‹]` must paint on `bg_base` (plain text like [Dashboard], no button bg), got bg={:?}",
            prev_cell.bg,
        );

        let next = chrome.next_rect.unwrap();
        assert_eq!(
            prev.x + prev.width,
            next.x,
            "`[‹]` and `[›]` must be adjacent (no space between), got prev_end={}, next_start={}",
            prev.x + prev.width,
            next.x,
        );
        assert!(
            content.contains("[‹][›]"),
            "row must contain `[‹][›]` as a single adjacent group, got: {content:?}",
        );
        let close = chrome.close_rect.unwrap();
        let close_cell = &buf[(close.x, close.y)];
        assert_eq!(
            close_cell.bg, theme.bg_base,
            "close `[Dashboard]` must paint on `bg_base` (NOT a button), got bg={:?}",
            close_cell.bg,
        );
    }

    /// With `position = None` (overlay not active or single-agent dashboard), the overlay still paints the close button.
    /// It omits the position indicator and both cycle chips.
    #[test]
    fn render_dashboard_session_overlay_omits_cycle_chips_when_position_is_none() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 10));
        let theme = Theme::current();
        let chrome = render_dashboard_session_overlay(
            &mut buf,
            Rect::new(0, 0, 80, 10),
            &theme,
            "Investigate bug",
            None,
            false,
            false,
            false,
        )
        .expect("overlay must paint");
        let content = buf_to_text(&buf);
        assert!(content.contains("[Dashboard]"));
        assert!(!content.contains("[‹]"));
        assert!(!content.contains("[›]"));
        assert!(chrome.prev_rect.is_none());
        assert!(chrome.next_rect.is_none());
        assert!(chrome.close_rect.is_some());
    }

    /// `position = Some((1, 1))` (the user is the only attachable row) also omits the cycle chips.
    /// There is nowhere to walk to, so the chips would be dead clicks.
    #[test]
    fn render_dashboard_session_overlay_omits_cycle_chips_when_total_is_one() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 10));
        let theme = Theme::current();
        let chrome = render_dashboard_session_overlay(
            &mut buf,
            Rect::new(0, 0, 80, 10),
            &theme,
            "Solo agent",
            Some((1, 1)),
            false,
            false,
            false,
        )
        .expect("overlay must paint");
        let content = buf_to_text(&buf);
        assert!(!content.contains("[‹]"));
        assert!(!content.contains("[›]"));

        assert!(
            !content.contains("1/1"),
            "single-row overlays must omit the position indicator, got: {content:?}",
        );
        assert!(chrome.prev_rect.is_none());
        assert!(chrome.next_rect.is_none());
    }

    /// Hover feedback on the plain-text affordances (`[‹]`, `[›]`) only changes the foreground color (to `text_primary` on hover, `gray` otherwise).
    /// Background is always `bg_base` (no button fill).
    #[test]
    fn render_dashboard_session_overlay_highlights_hovered_affordance() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 10));
        let theme = Theme::current();
        let chrome = render_dashboard_session_overlay(
            &mut buf,
            Rect::new(0, 0, 80, 10),
            &theme,
            "x",
            Some((1, 2)),
            false,
            true, // hover_next
            false,
        )
        .expect("overlay must paint");
        let next = chrome.next_rect.unwrap();
        let next_cell = &buf[(next.x, next.y)];
        assert_eq!(
            next_cell.bg, theme.bg_base,
            "hovered `[›]` must paint on `bg_base` (plain text like [Dashboard]), got bg={:?}",
            next_cell.bg,
        );
        assert_eq!(
            next_cell.fg, theme.text_primary,
            "hovered `[›]` must use text_primary fg, got: {:?}",
            next_cell.fg,
        );

        let prev = chrome.prev_rect.unwrap();
        let prev_cell = &buf[(prev.x, prev.y)];
        assert_eq!(
            prev_cell.bg, theme.bg_base,
            "non-hovered `[‹]` must paint on `bg_base`, got bg={:?}",
            prev_cell.bg,
        );
        assert_eq!(
            prev_cell.fg, theme.gray,
            "non-hovered `[‹]` must use gray fg, got: {:?}",
            prev_cell.fg,
        );
    }

    /// The `[+ New Agent]` header button paints green (`accent_success`) when focused so the cursor is obvious, and dim gray otherwise.
    #[test]
    fn header_new_agent_button_focused_is_green() {
        let theme = Theme::current();
        let rows: Vec<DashboardRow> = Vec::new();
        let area = Rect::new(0, 0, 120, 1);

        // Focused (default for a fresh dashboard with no row selected).
        let mut focused = DashboardState::new();
        focused.focus_new_agent_button();
        let mut buf = Buffer::empty(area);
        render_header(&mut buf, area, &theme, &rows, &mut focused, None);
        let rect = focused
            .new_agent_button_hit
            .rect
            .expect("button must render");
        assert_eq!(
            buf[(rect.x, rect.y)].fg,
            theme.accent_success,
            "focused [+ New Agent] must paint green (accent_success), got {:?}",
            buf[(rect.x, rect.y)].fg,
        );

        // Unfocused (a row holds the cursor instead).
        let mut unfocused = DashboardState::new();
        unfocused.focus_row(super::super::state::DashboardRowId::TopLevel(
            crate::app::agent::AgentId(0),
        ));
        let mut buf2 = Buffer::empty(area);
        render_header(&mut buf2, area, &theme, &rows, &mut unfocused, None);
        let rect2 = unfocused
            .new_agent_button_hit
            .rect
            .expect("button must render");
        assert_eq!(
            buf2[(rect2.x, rect2.y)].fg,
            theme.gray,
            "unfocused [+ New Agent] must paint dim gray, got {:?}",
            buf2[(rect2.x, rect2.y)].fg,
        );
    }

    /// On hover the unfocused `[+ New Agent]` header button brightens its text from `gray` to `text_primary` so the mouse user sees it is clickable.
    /// Only the foreground changes; the background stays `bg_base` (no fill).
    /// The `hovered` flag, which the mouse-move handler flips via `HitArea::update_hover`, drives the styling.
    #[test]
    fn header_new_agent_button_hover_brightens_text() {
        let theme = Theme::current();
        let rows: Vec<DashboardRow> = Vec::new();
        let area = Rect::new(0, 0, 120, 1);

        let mut state = DashboardState::new();
        state.focus_row(super::super::state::DashboardRowId::TopLevel(
            crate::app::agent::AgentId(0),
        ));

        // First render populates the button's hit rect.
        let mut buf = Buffer::empty(area);
        render_header(&mut buf, area, &theme, &rows, &mut state, None);
        let rect = state.new_agent_button_hit.rect.expect("button must render");

        // Moving the mouse over the button flips hover on.
        assert!(
            state.new_agent_button_hit.update_hover(rect.x, rect.y),
            "moving the mouse over the button must flip hover on",
        );

        let mut buf2 = Buffer::empty(area);
        render_header(&mut buf2, area, &theme, &rows, &mut state, None);
        let cell = &buf2[(rect.x, rect.y)];
        assert_eq!(
            cell.fg, theme.text_primary,
            "hovered [+ New Agent] must use text_primary fg, got {:?}",
            cell.fg,
        );
        assert_eq!(
            cell.bg, theme.bg_base,
            "hovered [+ New Agent] must keep bg_base (no hover fill), got {:?}",
            cell.bg,
        );

        assert!(
            state.new_agent_button_hit.update_hover(0, 0),
            "moving the mouse off the button must flip hover off",
        );
        let mut buf3 = Buffer::empty(area);
        render_header(&mut buf3, area, &theme, &rows, &mut state, None);
        let cell3 = &buf3[(rect.x, rect.y)];
        assert_eq!(
            cell3.bg, theme.bg_base,
            "non-hovered [+ New Agent] must paint on bg_base, got {:?}",
            cell3.bg,
        );
        assert_eq!(
            cell3.fg, theme.gray,
            "non-hovered [+ New Agent] must use gray fg, got {:?}",
            cell3.fg,
        );
    }

    /// Regression: the header renders from the dashboard's STAGED `cwd` (synced from `app.cwd` on a `/cd`), not the live process cwd.
    /// A location change updates `state.cwd` immediately; the process cwd only moves later via `Effect::SetWorkingDir` (which can fail).
    /// The header must follow `state.cwd` to show where dispatches will run.
    #[test]
    fn header_location_renders_from_staged_cwd() {
        let theme = Theme::current();
        let rows: Vec<DashboardRow> = Vec::new();
        // Wide area so the path isn't width-truncated.
        let area = Rect::new(0, 0, 200, 1);

        let mut state = DashboardState::new();

        state.cwd = std::path::PathBuf::from("/grok-staged-cwd-marker");

        let mut buf = Buffer::empty(area);
        render_header(&mut buf, area, &theme, &rows, &mut state, None);

        let top_row: String = (0..area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            top_row.contains("/grok-staged-cwd-marker"),
            "header must render the staged cwd, not the process cwd; got: {top_row:?}",
        );
    }

    /// The header button reads `[+ New Worktree]` when worktree mode is armed in a git repo.
    /// It reads `[+ New Agent]` otherwise (off, or armed outside a git repo, where the mode can't take effect).
    #[test]
    fn header_button_label_reflects_worktree_mode() {
        let theme = Theme::current();
        let rows: Vec<DashboardRow> = Vec::new();
        let area = Rect::new(0, 0, 120, 1);

        let mut off = DashboardState::new();
        off.cwd_has_git_ancestor = true;
        let mut buf = Buffer::empty(area);
        render_header(&mut buf, area, &theme, &rows, &mut off, None);
        let text = buf_to_text(&buf);
        assert!(
            text.contains("[+ New Agent]") && !text.contains("Worktree"),
            "worktree mode off → [+ New Agent], got: {text:?}",
        );

        let mut armed = DashboardState::new();
        armed.cwd_has_git_ancestor = true;
        armed.dispatch_worktree = true;
        let mut buf2 = Buffer::empty(area);
        render_header(&mut buf2, area, &theme, &rows, &mut armed, None);
        let text2 = buf_to_text(&buf2);
        assert!(
            text2.contains("[+ New Worktree]"),
            "worktree mode armed in a repo → [+ New Worktree], got: {text2:?}",
        );

        let mut armed_no_git = DashboardState::new();
        armed_no_git.cwd_has_git_ancestor = false;
        armed_no_git.dispatch_worktree = true;
        let mut buf3 = Buffer::empty(area);
        render_header(&mut buf3, area, &theme, &rows, &mut armed_no_git, None);
        let text3 = buf_to_text(&buf3);
        assert!(
            text3.contains("[+ New Agent]") && !text3.contains("Worktree"),
            "armed outside a repo → [+ New Agent], got: {text3:?}",
        );
    }

    /// Tiny areas return `None` so the caller falls back to a chromeless render.
    #[test]
    fn render_dashboard_session_overlay_returns_none_on_tiny_area() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 3));
        let theme = Theme::current();
        let chrome = render_dashboard_session_overlay(
            &mut buf,
            Rect::new(0, 0, 12, 3),
            &theme,
            "x",
            Some((1, 2)),
            false,
            false,
            false,
        );
        assert!(chrome.is_none());
    }

    /// The chromeless header variant paints the title and chips on the title row, with the requested top/side padding aligning it with the body below.
    /// It populates the affordance hit rects, hands back a full-width `content` rect beneath the header band, and paints NO border frame.
    #[test]
    fn render_dashboard_session_header_paints_padded_top_bar_without_border() {
        const PAD_LEFT: u16 = 2;
        const PAD_RIGHT: u16 = 2;
        const PAD_TOP: u16 = 1;
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 10));
        let theme = Theme::current();
        let chrome = render_dashboard_session_header(
            &mut buf,
            Rect::new(0, 0, 80, 10),
            &theme,
            "Add responsiveness to /context",
            Some((1, 2)),
            false,
            false,
            false,
            PAD_LEFT,
            PAD_RIGHT,
            PAD_TOP,
        )
        .expect("header must paint on a reasonably sized area");
        let content = buf_to_text(&buf);
        assert!(
            content.contains("Add responsiveness to /context"),
            "title must render, got: {content:?}",
        );
        for chip in ["[‹]", "[›]", "[Dashboard]"] {
            assert!(
                content.contains(chip),
                "header must paint `{chip}`, got: {content:?}",
            );
        }
        assert!(
            content.contains("1/2"),
            "header must paint the `1/2` position indicator, got: {content:?}",
        );

        for glyph in [
            '\u{250c}', '\u{2510}', '\u{2514}', '\u{2518}', '\u{251c}', '\u{2524}', '\u{2502}',
        ] {
            assert!(
                !content.contains(glyph),
                "header must NOT paint frame glyph `{glyph}`, got: {content:?}",
            );
        }

        assert_eq!(
            chrome.content,
            Rect::new(0, PAD_TOP + 1, 80, 10 - (PAD_TOP + 1))
        );
        assert!(chrome.prev_rect.is_some(), "prev_rect must be populated");
        assert!(chrome.next_rect.is_some(), "next_rect must be populated");
        assert!(chrome.close_rect.is_some(), "close_rect must be populated");
        // All affordances live on the title row (below the top pad).
        assert_eq!(chrome.close_rect.unwrap().y, PAD_TOP);
        assert_eq!(chrome.prev_rect.unwrap().y, PAD_TOP);
        assert_eq!(chrome.next_rect.unwrap().y, PAD_TOP);

        assert_eq!(
            buf[(PAD_LEFT, PAD_TOP)].symbol(),
            "A",
            "title must start at column PAD_LEFT",
        );
        for x in 0..PAD_LEFT {
            assert_eq!(
                buf[(x, PAD_TOP)].symbol(),
                " ",
                "columns before the title must be blank left padding",
            );
        }

        let close = chrome.close_rect.unwrap();
        assert_eq!(
            close.x + close.width,
            80 - PAD_RIGHT,
            "close chip must end PAD_RIGHT columns from the right edge",
        );
    }

    /// Narrow-mode rendering truncates labels and still registers row_rects.
    #[test]
    fn render_narrow_mode_registers_row_rects() {
        use crate::app::agent::AgentId;
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 5));
        let mut state = DashboardState::new();
        let row = DashboardRow {
            id: DashboardRowId::TopLevel(AgentId(1)),
            label: "abcdefghij ".repeat(10),
            subtitle: None,
            state: RowState::Working,
            activity: None,
            secondary_line: None,
            cwd_display: String::new(),
            cwd: std::path::PathBuf::from("/tmp"),
            last_change_at: std::time::SystemTime::now(),
            pinned: false,
            badges: Vec::new(),
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        };
        let rows = vec![row];
        let theme = Theme::current();
        render_narrow_rows(&mut buf, Rect::new(0, 0, 30, 5), &theme, &rows, &mut state);
        assert!(!state.row_rects.is_empty());
    }

    /// Wide-mode hit rects include each item's trailing gap line and tile the list contiguously.
    /// Hover/click never falls into a dead zone between items.
    #[test]
    fn render_rows_hit_rects_leave_no_dead_zones() {
        let rows = vec![
            header_test_row(1, RowState::Working, "alpha"),
            header_test_row(2, RowState::Working, "beta"),
            header_test_row(3, RowState::Idle, "gamma"),
        ];
        let area = Rect::new(0, 0, 60, 30);
        let mut buf = Buffer::empty(area);
        let mut state = DashboardState::new();
        state.grouping = Grouping::State;
        let theme = Theme::current();
        render_rows(&mut buf, area, &theme, &rows, &mut state);

        assert_eq!(state.row_rects.len(), 3);
        for (id, rect) in &state.row_rects {
            assert_eq!(rect.height, ROW_HEIGHT, "row {id:?} must be full-height");
        }
        assert_eq!(state.section_rects.len(), 2);
        for (key, rect) in &state.section_rects {
            assert_eq!(
                rect.height, GROUP_HEADER_HEIGHT,
                "section {key:?} must be full-height",
            );
        }

        // Each hit rect starts exactly where the previous one ended.
        let mut rects: Vec<Rect> = state
            .row_rects
            .iter()
            .map(|(_, r)| *r)
            .chain(state.section_rects.iter().map(|(_, r)| *r))
            .collect();
        rects.sort_by_key(|r| r.y);
        for pair in rects.windows(2) {
            assert_eq!(
                pair[0].y + pair[0].height,
                pair[1].y,
                "hit rects must tile without gaps: {pair:?}",
            );
        }

        let theme = Theme::groknight();
        assert_ne!(theme.bg_hover, theme.bg_base);
        let (id, rect) = state.row_rects[0].clone();
        state.hovered_row = Some(id);
        render_rows(&mut buf, area, &theme, &rows, &mut state);
        let title_y = rect.y + 1;
        assert_eq!(
            buf[(rect.x, title_y)].style().bg,
            Some(theme.bg_hover),
            "hovered row must highlight its content line",
        );
        let above = &buf[(rect.x, title_y - 1)];
        assert_eq!(above.symbol(), "\u{2580}", "spacer above must be a halo");
        assert_eq!(
            above.style().bg,
            Some(theme.bg_hover),
            "halo above must show the hover colour in its bottom half",
        );
        let below = &buf[(rect.x, title_y + 1)];
        assert_eq!(below.symbol(), "\u{2580}", "spacer below must be a halo");
        assert_eq!(
            below.style().fg,
            Some(theme.bg_hover),
            "halo below must show the hover colour in its top half",
        );
    }

    /// A row's content is vertically centered within its 3-cell rect: a title-only row renders padding, title, padding.
    /// A row with a secondary line stays top-aligned (2 lines cannot center in 3 cells).
    #[test]
    fn render_row_centers_title_only_content() {
        let theme = Theme::current();
        let mut state = DashboardState::new();

        let row = header_test_row(1, RowState::Idle, "solo");
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 3));
        render_row(&mut buf, Rect::new(0, 0, 40, 3), &theme, &row, &mut state);
        assert_eq!(buf[(4, 1)].symbol(), "s", "title must sit on line 1");
        assert_eq!(buf[(4, 0)].symbol(), " ", "line 0 must be padding");
        assert_eq!(buf[(4, 2)].symbol(), " ", "line 2 must be padding");

        let mut row = header_test_row(2, RowState::Working, "pair");
        row.secondary_line = Some("Responding".to_string());
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 3));
        render_row(&mut buf, Rect::new(0, 0, 40, 3), &theme, &row, &mut state);
        assert_eq!(buf[(4, 0)].symbol(), "p", "title must sit on line 0");
        assert_eq!(buf[(4, 1)].symbol(), "R", "secondary must sit on line 1");
        assert_eq!(buf[(4, 2)].symbol(), " ", "line 2 must be padding");
    }

    /// Empty area is a quick exit.
    #[test]
    fn render_empty_state_zero_area_is_no_op() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 10));
        let theme = Theme::current();
        render_empty_state(&mut buf, Rect::new(0, 0, 0, 0), &theme, false);
        // No-op assertion: nothing crashes.
    }

    /// The no-match branch renders the filter feedback.
    #[test]
    fn render_no_match_paints_filter_hint() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 5));
        let theme = Theme::current();
        render_no_match(
            &mut buf,
            Rect::new(0, 0, 60, 5),
            &theme,
            &Filter::Agent("reviewer".into()),
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains("reviewer"),
            "no-match hint should embed the filter value, got: {content:?}"
        );
    }

    // ── snap_offset_to_line_boundary unit tests ──────────────────────

    /// An offset already on a boundary is returned unchanged.
    #[test]
    fn snap_offset_already_on_boundary_returns_input() {
        let heights = vec![3u16, 3, 3];
        assert_eq!(snap_offset_to_line_boundary(0, &heights), 0);
        assert_eq!(snap_offset_to_line_boundary(3, &heights), 3);
        assert_eq!(snap_offset_to_line_boundary(6, &heights), 6);
    }

    /// A sub-row offset (1 or 2 cells into a 3-cell row) snaps DOWN to the row's starting cell.
    /// The topmost visible row always paints from its first cell.
    #[test]
    fn snap_offset_subrow_clips_to_row_start() {
        let heights = vec![3u16, 3, 3];
        assert_eq!(snap_offset_to_line_boundary(1, &heights), 0);
        assert_eq!(snap_offset_to_line_boundary(2, &heights), 0);
        assert_eq!(snap_offset_to_line_boundary(4, &heights), 3);
        assert_eq!(snap_offset_to_line_boundary(5, &heights), 3);
    }

    /// Headers (2 cells) and rows (3 cells) mix; the helper snaps to whichever item boundary precedes the offset.
    #[test]
    fn snap_offset_mixed_heights() {
        // [header=2, row=3, row=3]
        let heights = vec![2u16, 3, 3];
        // Inside the header (0..2):
        assert_eq!(snap_offset_to_line_boundary(0, &heights), 0);
        assert_eq!(snap_offset_to_line_boundary(1, &heights), 0);
        // At the row 0 start:
        assert_eq!(snap_offset_to_line_boundary(2, &heights), 2);
        // Inside row 0 (2..5):
        assert_eq!(snap_offset_to_line_boundary(3, &heights), 2);
        assert_eq!(snap_offset_to_line_boundary(4, &heights), 2);
        // At the row 1 start:
        assert_eq!(snap_offset_to_line_boundary(5, &heights), 5);
    }

    /// Offsets past the last item just stay clamped at the last boundary.
    /// The bounds clamp in `clamp_viewport` prevents this in practice.
    /// `snap_offset_to_line_boundary` must still be safe on its own to keep the contract local.
    #[test]
    fn snap_offset_past_last_item_returns_last_boundary() {
        let heights = vec![3u16, 3, 3];
        // Last boundary is at cell 6 (start of row index 2).
        assert_eq!(snap_offset_to_line_boundary(7, &heights), 6);
        assert_eq!(snap_offset_to_line_boundary(99, &heights), 6);
    }

    /// With empty heights the snap returns 0 regardless of offset.
    #[test]
    fn snap_offset_empty_heights_returns_zero() {
        assert_eq!(snap_offset_to_line_boundary(0, &[]), 0);
        assert_eq!(snap_offset_to_line_boundary(5, &[]), 0);
    }

    /// `popup_rect` takes the FULL bottom area (no horizontal inset, no bottom inset) with only a top inset reserved for the dashboard banner.
    /// The previous centred-inset design left the dashboard's own dispatch input and footer visible below the popup, producing two stacked input bars.
    #[test]
    fn popup_rect_takes_full_bottom_area_with_top_banner() {
        let view = Rect::new(0, 0, 200, 80);
        let popup = popup_rect(view);

        assert_eq!(
            popup.x, view.x,
            "popup must start at view.x (no left inset)"
        );
        assert_eq!(
            popup.width, view.width,
            "popup must span the full view width",
        );

        assert!(
            popup.y > view.y,
            "popup must sit below the banner (y={} expected > view.y={})",
            popup.y,
            view.y,
        );

        assert_eq!(
            popup.y + popup.height,
            view.y + view.height,
            "popup must extend to the bottom of the view",
        );
    }

    /// Banner height is sized as ~1/3 of the screen, clamped into a sensible range (6-14 rows).
    /// The rows stay readable on tall terminals without crowding the popup.
    #[test]
    fn popup_rect_leaves_room_for_banner_on_large_terminal() {
        let view = Rect::new(0, 0, 200, 60);
        let popup = popup_rect(view);
        let banner_h = popup.y - view.y;
        assert!(
            (6..=14).contains(&banner_h),
            "banner height {banner_h} must be in [6, 14] on a 60-row terminal",
        );
        // Popup still gets the majority of the screen height.
        assert!(
            popup.height as u32 * 100 >= view.height as u32 * 70,
            "popup height {} <70% of {} — banner too tall",
            popup.height,
            view.height,
        );
    }

    /// Very short terminals (height < banner_min + 10) collapse the banner to 0 so the popup gets every available row.
    /// This mirrors the agent view's "drop bottom_vpad on short terminals" behaviour.
    #[test]
    fn popup_rect_collapses_banner_on_tiny_terminal() {
        let view = Rect::new(0, 0, 12, 6);
        let popup = popup_rect(view);
        // Tiny terminals: zero banner, popup IS the view.
        assert_eq!(popup.width, view.width);
        assert_eq!(popup.height, view.height);
        assert_eq!(popup.y, view.y);
    }

    /// Replacing the home-rolled chrome with `picker::render_bordered_frame` means the divider sits ABOVE the returned content rect.
    /// This test paints the chrome plus a "fake agent" pattern in the inner rect and verifies the divider's `─` glyph survives the inner paint.
    #[test]
    fn render_popup_overlay_divider_survives_inner_paint() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 20));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        let (_cursor, _post_flush, drawn) = render_popup_overlay(
            &mut buf,
            Rect::new(0, 0, 60, 20),
            &theme,
            "Test session",
            &mut state,
            |inner, buf| {
                for y in inner.y..inner.y + inner.height {
                    for x in inner.x..inner.x + inner.width {
                        buf.set_string(x, y, "x", Style::default());
                    }
                }
                (None, None)
            },
        );
        assert!(drawn);
        let content = buf_to_text(&buf);

        let divider_count = content.matches('\u{2500}').count();
        assert!(
            divider_count > 0,
            "divider U+2500 missing after inner paint; got: {content:?}",
        );
    }

    /// The popup paints a `[✗]` close affordance and registers its hit rect on `DashboardState` so `handle_mouse` can dispatch a popup close on click.
    #[test]
    fn render_popup_overlay_registers_close_hit_rect() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 10));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        let _ = render_popup_overlay(
            &mut buf,
            Rect::new(0, 0, 60, 10),
            &theme,
            "Sample",
            &mut state,
            |_inner, _buf| (None, None),
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains('\u{2717}'),
            "close affordance [✗] missing, got: {content:?}",
        );
        let close_rect = state
            .popup_close_rect
            .expect("popup_close_rect must be registered");

        assert_eq!(close_rect.y, 1);
        assert!(close_rect.x > 50);

        let outer = state
            .popup_outer_rect
            .expect("popup_outer_rect must be registered");
        assert_eq!(outer, Rect::new(0, 0, 60, 10));
    }

    /// When the popup area is too small for the canonical bordered frame, the overlay paints a fallback hint inside an outlined box.
    /// It never leaves the user staring at an empty popup.
    #[test]
    fn render_popup_overlay_small_area_paints_fallback_hint() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 4));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        let (cursor, post_flush, drawn) = render_popup_overlay(
            &mut buf,
            Rect::new(0, 0, 40, 4),
            &theme,
            "Tiny",
            &mut state,
            |_inner, _buf| {
                panic!("draw_agent must NOT be called on the fallback path");
            },
        );
        assert!(cursor.is_none());
        assert!(post_flush.is_none());
        assert!(!drawn);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("too small"),
            "fallback hint missing, got: {content:?}",
        );
    }

    /// The footer hint always includes the rename shortcut.
    /// This stops a regression where the footer dropped it behind a feature flag or omitted it during a conditional rebuild.
    #[test]
    fn render_footer_surfaces_shortcuts_link() {
        // Trailing shortcuts chip must match the registry primary key.
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let state = DashboardState::new();
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains("shortcuts"),
            "footer must mention `shortcuts` (the help chip), got: {content:?}",
        );
        let primary = registry
            .find(crate::actions::ActionId::DashboardShortcutsHelp)
            .map(|d| d.default_key.display())
            .unwrap_or_else(|| "Ctrl+.".into());
        let expected = format!("{primary}:shortcuts");
        assert!(
            content.contains(&expected),
            "footer must include `{expected}` chip (registry primary), got: {content:?}",
        );
    }

    /// The location picker opens with input focused, but under vim `Esc` drops it to NAV.
    /// Its footer must surface the `i search` hint there (and hide it in input mode / when vim is off).
    #[test]
    fn location_picker_footer_shows_i_hint_in_vim_nav() {
        use super::super::state::LocationPickerState;
        let make = || {
            LocationPickerState::new(
                vec![],
                std::path::PathBuf::from("/tmp"),
                std::collections::HashMap::new(),
            )
        };
        let area = Rect::new(0, 0, 160, 48);
        let theme = Theme::current();

        crate::appearance::cache::set_vim_mode(true);
        let mut nav = make();
        nav.picker.search_active = false;
        let mut buf = Buffer::empty(area);
        render_location_picker(&mut buf, area, &theme, &mut nav);
        assert!(
            buf_to_text(&buf).contains("i search"),
            "location picker footer must show `i search` in vim nav mode",
        );

        let mut input = make();
        input.picker.search_active = true;
        let mut buf_input = Buffer::empty(area);
        render_location_picker(&mut buf_input, area, &theme, &mut input);
        assert!(
            !buf_to_text(&buf_input).contains("i search"),
            "no `i search` hint while typing (input mode)",
        );

        crate::appearance::cache::set_vim_mode(false);
        let mut off = make();
        off.picker.search_active = false;
        let mut buf_off = Buffer::empty(area);
        render_location_picker(&mut buf_off, area, &theme, &mut off);
        assert!(
            !buf_to_text(&buf_off).contains("i search"),
            "no `i search` hint when vim-mode is off",
        );
    }

    /// Pressing the help key returns the `DashboardOpenShortcutsHelp` action so the dispatcher can build the modal state.
    /// No `error_toast` is set: an earlier iteration surfaced a hint via the dispatch input placeholder, which conflicted with the typing slot.
    #[test]
    fn dashboard_shortcuts_help_action_opens_modal() {
        use super::super::state::DashboardState;
        let mut state = DashboardState::new();
        let registry = crate::actions::ActionRegistry::defaults();
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let key = KeyEvent {
            code: KeyCode::Char('.'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        let outcome = state.handle_input(&Event::Key(key), &registry);
        assert!(
            matches!(
                outcome,
                crate::app::app_view::InputOutcome::Action(
                    crate::app::actions::Action::DashboardOpenShortcutsHelp,
                )
            ),
            "shortcuts key must emit DashboardOpenShortcutsHelp, got: {outcome:?}",
        );
        assert!(
            state.error_toast.is_none(),
            "no error_toast should be set — the modal carries the help, \
             not the dispatch input placeholder. Got: {:?}",
            state.error_toast,
        );
    }

    /// RenameDraft sanitation keeps control characters out of both render paths.
    #[test]
    fn sanitized_rename_draft_is_safe_in_both_render_paths() {
        use crate::app::agent::AgentId;
        let id = DashboardRowId::TopLevel(AgentId(7));
        let row = DashboardRow {
            id: id.clone(),
            label: "row label".to_string(),
            subtitle: None,
            state: RowState::Working,
            activity: None,
            secondary_line: None,
            cwd_display: String::new(),
            cwd: std::path::PathBuf::from("/tmp"),
            last_change_at: std::time::SystemTime::now(),
            pinned: false,
            badges: Vec::new(),
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        };
        let rows = vec![row];
        let theme = Theme::current();

        // Wide path.
        {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, 3));
            let mut state = DashboardState::new();
            state.selected = Some(id.clone());
            state.rename = Some(RenameDraft::new(id.clone(), "a\x1b[31m"));
            render_rows(&mut buf, Rect::new(0, 0, 80, 3), &theme, &rows, &mut state);
            let content = buf_to_text(&buf);
            assert!(
                !content.contains('\x1b'),
                "wide rename overlay must not retain ESC: {content:?}",
            );
            assert!(
                content.contains('a'),
                "wide rename overlay must keep the visible draft char: {content:?}",
            );
        }

        // Narrow path.
        {
            let mut buf = Buffer::empty(Rect::new(0, 0, 30, 3));
            let mut state = DashboardState::new();
            state.selected = Some(id.clone());
            state.rename = Some(RenameDraft::new(id.clone(), "a\x1b[31m"));
            render_narrow_rows(&mut buf, Rect::new(0, 0, 30, 3), &theme, &rows, &mut state);
            let content = buf_to_text(&buf);
            assert!(
                !content.contains('\x1b'),
                "narrow rename overlay must not retain ESC: {content:?}",
            );
            assert!(
                content.contains('a'),
                "narrow rename overlay must keep the visible draft char: {content:?}",
            );
        }
    }

    /// Rename rendering preserves row chrome and title alignment in both layouts.
    #[test]
    fn render_rename_overlay_aligns_with_title_and_keeps_icon() {
        use crate::app::agent::AgentId;
        let id = DashboardRowId::TopLevel(AgentId(7));
        let row = DashboardRow {
            id: id.clone(),
            label: "row label".to_string(),
            subtitle: None,
            state: RowState::Idle,
            activity: None,
            secondary_line: None,
            cwd_display: String::new(),
            cwd: std::path::PathBuf::from("/tmp"),
            last_change_at: std::time::SystemTime::now(),
            pinned: false,
            badges: Vec::new(),
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        };
        let rows = vec![row];
        let theme = Theme::current();
        let row_text = |buf: &Buffer, y: u16, w: u16| -> String {
            (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect()
        };

        // Wide path: this title-only row centers its title within its 3-cell rect
        // The title thus sits 3 below the group header (header + gap + row top padding)
        // `title_byte` is a byte offset (for `str::find` comparisons)
        // `title_col` is the display column for cursor math; the icon glyph is multi-byte UTF-8, so the two differ
        let (title_byte, title_col) = {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, 5));
            let mut state = DashboardState::new();
            render_rows(&mut buf, Rect::new(0, 0, 80, 5), &theme, &rows, &mut state);
            let line = row_text(&buf, 3, 80);
            let byte = line.find("row label").expect("title must render");
            (byte, line[..byte].chars().count() as u16)
        };
        {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, 5));
            let mut state = DashboardState::new();
            state.rename = Some(RenameDraft::new(id.clone(), "new name"));
            render_rows(&mut buf, Rect::new(0, 0, 80, 5), &theme, &rows, &mut state);
            let line = row_text(&buf, 3, 80);
            assert_eq!(
                line.find("rename: new name"),
                Some(title_byte),
                "wide: `rename:` must start at the title column, got: {line:?}",
            );
            assert_eq!(
                buf[(2, 3)].symbol(),
                crate::glyphs::diamond_hollow(),
                "wide: the state icon must stay in place while renaming",
            );
            // The cursor parks one cell past the typed draft: after the `rename: ` prefix, never overlapping it
            let prefix_w = "rename: ".len() as u16;
            let draft_w = "new name".len() as u16;
            assert_eq!(
                rename_cursor_pos(&state, &rows),
                Some((title_col + prefix_w + draft_w, 3)),
                "cursor must sit one cell past the draft text",
            );
            // With an empty draft the cursor sits immediately after `rename: ` (the position typing lands at)
            state.rename = Some(RenameDraft::new(id.clone(), ""));
            assert_eq!(
                rename_cursor_pos(&state, &rows),
                Some((title_col + prefix_w, 3)),
                "empty draft: cursor must sit right after `rename: `",
            );
        }

        // Narrow path: row sits 1 below the group header (no gap).
        let title_col = {
            let mut buf = Buffer::empty(Rect::new(0, 0, 30, 3));
            let mut state = DashboardState::new();
            render_narrow_rows(&mut buf, Rect::new(0, 0, 30, 3), &theme, &rows, &mut state);
            row_text(&buf, 1, 30)
                .find("row label")
                .expect("narrow title must render") as u16
        };
        {
            let mut buf = Buffer::empty(Rect::new(0, 0, 30, 3));
            let mut state = DashboardState::new();
            state.rename = Some(RenameDraft::new(id.clone(), "nn"));
            render_narrow_rows(&mut buf, Rect::new(0, 0, 30, 3), &theme, &rows, &mut state);
            let line = row_text(&buf, 1, 30);
            assert_eq!(
                line.find("rename: nn").map(|c| c as u16),
                Some(title_col),
                "narrow: `rename:` must start at the title column, got: {line:?}",
            );
            assert_eq!(
                buf[(2, 1)].symbol(),
                crate::glyphs::diamond_hollow(),
                "narrow: the state icon must stay in place while renaming",
            );
        }
    }

    #[test]
    fn rename_viewport_handles_long_unicode_in_wide_and_narrow_rows() {
        use crate::app::agent::AgentId;
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let id = DashboardRowId::TopLevel(AgentId(7));
        let row = DashboardRow {
            id: id.clone(),
            label: "row label".to_string(),
            subtitle: None,
            state: RowState::Idle,
            activity: None,
            secondary_line: None,
            cwd_display: String::new(),
            cwd: std::path::PathBuf::from("/tmp"),
            last_change_at: std::time::SystemTime::now(),
            pinned: false,
            badges: Vec::new(),
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        };
        let rows = vec![row];
        let text = format!("{}中e\u{301}👩🏽\u{200d}💻", "x".repeat(90));
        let theme = Theme::current();
        let registry = crate::actions::ActionRegistry::defaults();

        for (width, narrow, row_y) in [(80, false, 3), (30, true, 1)] {
            let area = Rect::new(0, 0, width, if narrow { 3 } else { 5 });
            let mut buffer = Buffer::empty(area);
            let mut state = DashboardState::new();
            state.rename = Some(RenameDraft::new(id.clone(), text.clone()));
            if narrow {
                render_narrow_rows(&mut buffer, area, &theme, &rows, &mut state);
            } else {
                render_rows(&mut buffer, area, &theme, &rows, &mut state);
            }
            let line = (0..width)
                .map(|x| buffer[(x, row_y)].symbol().to_string())
                .collect::<String>();
            assert!(line.contains('中'), "CJK tail missing: {line:?}");
            assert!(line.contains("e\u{301}"), "combining tail split: {line:?}");
            assert!(line.contains("👩🏽\u{200d}💻"), "ZWJ tail split: {line:?}",);
            let end_cursor = rename_cursor_pos(&state, &rows).expect("end cursor");

            let _ = state.handle_input(
                &Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
                &registry,
            );
            for _ in 0..20 {
                let _ = state.handle_input(
                    &Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
                    &registry,
                );
            }
            let mut middle_buffer = Buffer::empty(area);
            if narrow {
                render_narrow_rows(&mut middle_buffer, area, &theme, &rows, &mut state);
            } else {
                render_rows(&mut middle_buffer, area, &theme, &rows, &mut state);
            }
            let middle_cursor = rename_cursor_pos(&state, &rows).expect("middle cursor");
            assert_ne!(
                state.rename.as_ref().expect("rename draft").cursor_byte(),
                text.len()
            );
            if !narrow {
                assert_ne!(middle_cursor, end_cursor);
            }
            let prefix_x = (0..width)
                .find(|x| middle_buffer[(*x, row_y)].symbol() == "r")
                .expect("rename prefix");
            let row_rect = state
                .row_rects
                .iter()
                .find(|(row_id, _)| row_id == &id)
                .map(|(_, rect)| *rect)
                .expect("rename row rect");
            let editor_x = prefix_x + RENAME_PREFIX.len() as u16;
            let editor_width = row_rect
                .x
                .saturating_add(row_rect.width)
                .saturating_sub(editor_x);
            let expected_cursor = editor_x + 20u16.min(editor_width.saturating_sub(1));
            assert_eq!(middle_cursor, (expected_cursor, row_y));
        }
    }

    /// On a 3-row rect the dispatch input paints a rounded-box chrome so it reads as a real input field.
    /// The text row contains the `❯` prefix.
    #[test]
    fn render_dispatch_paints_rounded_box_chrome() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 3));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        let cursor = render_dispatch(&mut buf, Rect::new(0, 0, 60, 3), &theme, &mut state, None);
        assert!(cursor.is_some(), "dispatch must return a cursor position");
        let content = buf_to_text(&buf);
        for corner in ['\u{256d}', '\u{2570}', '\u{256e}', '\u{256f}'] {
            assert!(
                content.contains(corner),
                "dispatch chrome must paint corner `{corner}`, got: {content:?}",
            );
        }
        assert!(
            content.contains('\u{276F}'),
            "dispatch must paint ❯ prefix inside the box, got: {content:?}",
        );
    }

    #[test]
    fn render_search_mode_uses_textarea_cursor_not_text_end() {
        let area = Rect::new(0, 0, 40, 3);
        let mut buffer = Buffer::empty(area);
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.search_mode = true;
        state.dispatch.set_text("abcdef");
        state.dispatch.set_cursor(2);

        let cursor = render_dispatch(&mut buffer, area, &theme, &mut state, None)
            .expect("focused search cursor");
        let prefix_x = (0..area.width)
            .find(|x| buffer[(*x, cursor.1)].symbol() == "S")
            .expect("Search prefix");
        assert_eq!(cursor.0, prefix_x + "Search: ".len() as u16 + 2);
    }

    #[test]
    fn render_search_mode_clips_prefix_and_cursor_at_widths_one_through_nine() {
        let theme = Theme::current();
        for width in 1..=9 {
            let full = Rect::new(0, 0, 14, 1);
            let area = Rect::new(2, 0, width, 1);
            let mut buffer = Buffer::empty(full);
            buffer.set_string(0, 0, "#".repeat(full.width as usize), Style::default());
            let mut state = DashboardState::new();
            state.search_mode = true;
            state.dispatch.set_text("abcdef");
            state.dispatch.set_cursor(2);

            let cursor = render_dispatch(&mut buffer, area, &theme, &mut state, None)
                .expect("focused narrow search cursor");
            assert!(cursor.0 >= area.x && cursor.0 < area.x + area.width);
            for x in 0..full.width {
                if x < area.x || x >= area.x + area.width {
                    assert_eq!(
                        buffer[(x, 0)].symbol(),
                        "#",
                        "width {width} wrote outside at column {x}",
                    );
                }
            }
        }
    }

    #[test]
    fn render_dispatch_keeps_generic_paste_preview_but_suppresses_image_preview() {
        let area = Rect::new(0, 17, 80, 3);
        let overlay = Rect::new(0, 0, 80, 17);
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.dispatch.handle_paste("alpha\nbeta\ngamma\ndelta");
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 20));
        let _ = render_dispatch(&mut buf, area, &theme, &mut state, Some(overlay));
        let text = buf_to_text(&buf);
        assert!(text.contains("alpha"));
        assert!(text.contains("delta"));

        state.dispatch.set_text("");
        state
            .dispatch
            .insert_image(crate::prompt_images::from_clipboard_data(
                &crate::clipboard::ImageData {
                    data: vec![1, 2, 3],
                    mime_type: "image/png".into(),
                },
            ))
            .unwrap();
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 20));
        let _ = render_dispatch(&mut buf, area, &theme, &mut state, Some(overlay));
        let text = buf_to_text(&buf);
        assert!(text.contains("Image #1"));
        assert!(!text.contains("Format:"));
        assert!(!text.contains("Preview pending"));
    }

    /// On a 1-row rect the dispatch falls back to a bare `❯ {text}` line (no chrome) so the input stays usable on terminals too short for the box.
    #[test]
    fn render_dispatch_falls_back_to_single_line_on_short_area() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        let cursor = render_dispatch(&mut buf, Rect::new(0, 0, 60, 1), &theme, &mut state, None);
        assert!(
            cursor.is_some(),
            "single-line fallback must return a cursor"
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains('\u{276F}'),
            "fallback must paint ❯, got: {content:?}"
        );
        for corner in ['\u{256d}', '\u{2570}'] {
            assert!(
                !content.contains(corner),
                "single-line fallback must NOT paint `{corner}`, got: {content:?}",
            );
        }
    }

    /// Placeholder reads `Dispatch a new agent`, but only while the input is UNFOCUSED (overview list holds focus).
    /// The dispatch input always spawns a new session.
    #[test]
    fn render_dispatch_placeholder_paints_only_when_unfocused() {
        // Unfocused input (list focused): placeholder shows
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 3));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.list_focused = true;
        let _ = render_dispatch(&mut buf, Rect::new(0, 0, 80, 3), &theme, &mut state, None);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("Dispatch a new agent"),
            "unfocused placeholder missing, got: {content:?}",
        );
    }

    /// Focused input (the default) suppresses the placeholder: the visible caret is the affordance; the text area stays clear.
    #[test]
    fn render_dispatch_placeholder_hidden_when_focused() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 3));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        assert!(!state.list_focused, "fresh state must focus the input");
        let cursor = render_dispatch(&mut buf, Rect::new(0, 0, 80, 3), &theme, &mut state, None);
        let content = buf_to_text(&buf);
        assert!(
            !content.contains("Dispatch a new agent"),
            "focused input must not paint the placeholder, got: {content:?}",
        );
        // The `❯` prefix and the caret position survive.
        assert!(
            content.contains('\u{276F}'),
            "prefix must still paint, got: {content:?}",
        );
        assert!(cursor.is_some(), "focused input must report a caret");
    }

    /// The placeholder stays `Dispatch a new agent` even when a row is selected: the input never becomes a reply target.
    /// Selection is purely the overview navigation cursor; Enter on it OPENS the agent.
    /// This is the regression guard for the "stuck replying to the same agent" trap.
    #[test]
    fn render_dispatch_placeholder_stays_new_session_when_row_selected() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 3));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.focus_row(super::super::state::DashboardRowId::TopLevel(
            crate::app::agent::AgentId(0),
        ));
        // Unfocus the input (the placeholder only paints while unfocused)
        state.list_focused = true;
        let _ = render_dispatch(&mut buf, Rect::new(0, 0, 80, 3), &theme, &mut state, None);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("Dispatch a new agent"),
            "placeholder must stay new-session even with a row selected, got: {content:?}",
        );
        assert!(
            !content.contains("Reply to"),
            "the dispatch input must never show a reply placeholder, got: {content:?}",
        );
    }

    /// A pending dispatch-validation toast (e.g. Enter on a too-short prompt) paints as a badge on the box's TOP BORDER row.
    /// The badge is visible even while the rejected text is still in the input, unlike an empty-input placeholder.
    /// The placeholder itself stays the plain new-session text.
    #[test]
    fn render_dispatch_paints_feedback_badge_on_top_border() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 3));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        // Unfocus the input so the placeholder assertion below stays meaningful (the placeholder only paints while unfocused)
        state.list_focused = true;
        state.error_toast = Some("Too short — describe the task (4+ chars)".to_string());
        let _ = render_dispatch(&mut buf, Rect::new(0, 0, 80, 3), &theme, &mut state, None);

        // Toast text lands on the TOP border row (y == 0).
        let top_row: String = (0..80).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(
            top_row.contains("Too short"),
            "feedback badge must paint on the top border, got: {top_row:?}",
        );
        // The badge ends before the right `╮` corner (corner preserved).
        assert_eq!(
            buf[(79, 0)].symbol(),
            "\u{256e}",
            "right rounded corner must survive the badge",
        );
        // Placeholder remains the plain new-session text (the error is NOT shown inline anymore)
        let content = buf_to_text(&buf);
        assert!(
            content.contains("Dispatch a new agent"),
            "placeholder must remain the new-session text, got: {content:?}",
        );
    }

    /// The badge paints the toast VERBATIM in the neutral accent colour, never in the error red.
    /// A message that already carries its own glyph (as the `show_toast` builders produce, e.g. `✓ Theme: …`) keeps that single glyph.
    /// No `✗` is prepended (regression guard for the `✗ ✓ …` doubling).
    #[test]
    fn feedback_badge_renders_verbatim_in_neutral_color() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 3));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        let check = crate::glyphs::check_mark();
        state.error_toast = Some(format!("{check} Theme: Grok Day"));
        let _ = render_dispatch(&mut buf, Rect::new(0, 0, 80, 3), &theme, &mut state, None);

        let top_row: String = (0..80).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(
            top_row.contains(&format!("{check} Theme: Grok Day")),
            "badge must paint the message verbatim, got: {top_row:?}",
        );
        assert!(
            !top_row.contains(crate::glyphs::ballot_x()),
            "badge must not prepend a ✗ to a message that already has a glyph, got: {top_row:?}",
        );
        // Neutral colour: accent_user, never the error red
        let cx = (0..80)
            .find(|&x| buf[(x, 0)].symbol() == check)
            .expect("the ✓ glyph must be painted");
        assert_eq!(
            buf[(cx, 0)].fg,
            theme.accent_user,
            "badge must paint in the neutral accent_user colour (not the error red)",
        );
    }

    /// Helper for the group-header tests: build a top-level row with the given id and state, all other fields filled with sensible defaults.
    fn header_test_row(id: u32, state: RowState, label: &str) -> DashboardRow {
        use crate::app::agent::AgentId;
        DashboardRow {
            id: DashboardRowId::TopLevel(AgentId(id as usize)),
            label: label.to_string(),
            subtitle: None,
            state,
            activity: None,
            secondary_line: None,
            cwd_display: String::new(),
            cwd: std::path::PathBuf::from("/tmp"),
            last_change_at: std::time::SystemTime::now(),
            pinned: false,
            badges: Vec::new(),
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        }
    }

    /// A collapsed state section keeps its header (with the true count) but hides its rows; other sections are unaffected.
    #[test]
    fn build_dashboard_lines_hides_collapsed_state_section() {
        use std::collections::HashSet;
        let rows = vec![
            header_test_row(1, RowState::Working, "a"),
            header_test_row(2, RowState::Working, "b"),
            header_test_row(3, RowState::Idle, "c"),
        ];

        // Nothing collapsed: both Working rows present
        let none: HashSet<SectionKey> = HashSet::new();
        let lines =
            build_dashboard_lines(&rows, Grouping::State, &Filter::None, &none, false, false);
        let working_rows = lines
            .iter()
            .filter(|l| matches!(l, DashboardLine::Row(r) if r.state == RowState::Working))
            .count();
        assert_eq!(working_rows, 2, "expanded Working section shows both rows");

        // Collapse Working: header stays (count 2), rows hidden
        let mut collapsed = HashSet::new();
        collapsed.insert(SectionKey::State(RowState::Working));
        let lines = build_dashboard_lines(
            &rows,
            Grouping::State,
            &Filter::None,
            &collapsed,
            false,
            false,
        );
        assert!(
            lines.iter().any(|l| matches!(
                l,
                DashboardLine::Header { state, count } if *state == RowState::Working && *count == 2
            )),
            "collapsed Working header must still render with its true count",
        );
        let working_rows = lines
            .iter()
            .filter(|l| matches!(l, DashboardLine::Row(r) if r.state == RowState::Working))
            .count();
        assert_eq!(working_rows, 0, "collapsed Working section hides its rows");
        // The Idle section is unaffected.
        let idle_rows = lines
            .iter()
            .filter(|l| matches!(l, DashboardLine::Row(r) if r.state == RowState::Idle))
            .count();
        assert_eq!(idle_rows, 1, "other sections stay expanded");
    }

    /// A collapsed "Pinned" section keeps its header but hides the pinned rows (grouping ON).
    #[test]
    fn build_dashboard_lines_hides_collapsed_pinned_section() {
        use std::collections::HashSet;
        let mut pinned_row = header_test_row(1, RowState::Idle, "pinned");
        pinned_row.pinned = true;
        let rows = vec![pinned_row, header_test_row(2, RowState::Working, "other")];

        let mut collapsed = HashSet::new();
        collapsed.insert(SectionKey::Pinned);
        let lines = build_dashboard_lines(
            &rows,
            Grouping::State,
            &Filter::None,
            &collapsed,
            false,
            false,
        );
        assert!(
            lines
                .iter()
                .any(|l| matches!(l, DashboardLine::PinnedHeader { count } if *count == 1)),
            "collapsed Pinned header must still render",
        );
        // The pinned row is hidden; the (non-pinned) Working row remains.
        let visible_rows = lines
            .iter()
            .filter(|l| matches!(l, DashboardLine::Row(_)))
            .count();
        assert_eq!(visible_rows, 1, "only the non-pinned row stays visible");
    }

    /// An idle row last active `secs_ago` seconds in the past.
    fn aged_idle_row(id: u32, secs_ago: u64) -> DashboardRow {
        let mut r = header_test_row(id, RowState::Idle, "idle");
        r.last_change_at = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(secs_ago))
            .expect("test clock underflow");
        r
    }

    /// Seconds clearly outside the 1h Idle freshness window.
    const OLD_SECS: u64 = 2 * 60 * 60;

    fn idle_row_count(lines: &[DashboardLine]) -> usize {
        lines
            .iter()
            .filter(|l| matches!(l, DashboardLine::Row(r) if r.state == RowState::Idle))
            .count()
    }

    fn overflow_of(lines: &[DashboardLine]) -> Option<(usize, bool)> {
        lines.iter().find_map(|l| match l {
            DashboardLine::IdleOverflow { hidden, expanded } => Some((*hidden, *expanded)),
            _ => None,
        })
    }

    /// Old idle agents beyond `MAX_VISIBLE_IDLE` fold into the overflow row; the header still reports the true total.
    #[test]
    fn idle_cap_folds_old_agents() {
        use std::collections::HashSet;
        // (MAX_VISIBLE_IDLE + 3) OLD idle agents: cap shown, 3 folded
        let total = MAX_VISIBLE_IDLE as u32 + 3;
        let rows: Vec<DashboardRow> = (0..total).map(|i| aged_idle_row(i, OLD_SECS)).collect();
        let none: HashSet<SectionKey> = HashSet::new();
        let lines =
            build_dashboard_lines(&rows, Grouping::State, &Filter::None, &none, false, false);

        assert_eq!(
            idle_row_count(&lines),
            MAX_VISIBLE_IDLE,
            "caps to MAX_VISIBLE_IDLE"
        );
        assert_eq!(
            overflow_of(&lines),
            Some((total as usize - MAX_VISIBLE_IDLE, false)),
            "overflow row reports the folded count, not expanded",
        );
        // Header still shows the TRUE total, not the visible count.
        assert!(
            lines.iter().any(|l| matches!(
                l,
                DashboardLine::Header { state, count } if *state == RowState::Idle && *count == total as usize
            )),
            "Idle header keeps the true total count",
        );
    }

    /// Recent idle agents are never folded, even beyond the count cap: the freshness window keeps a burst of new sessions visible.
    #[test]
    fn idle_cap_keeps_recent_beyond_count() {
        use std::collections::HashSet;
        // 9 RECENT idle agents (just now): all shown, no overflow
        let rows: Vec<DashboardRow> = (0..9).map(|i| aged_idle_row(i, 0)).collect();
        let none: HashSet<SectionKey> = HashSet::new();
        let lines =
            build_dashboard_lines(&rows, Grouping::State, &Filter::None, &none, false, false);
        assert_eq!(
            idle_row_count(&lines),
            9,
            "all recent idle agents stay visible"
        );
        assert_eq!(overflow_of(&lines), None, "no overflow when nothing is old");
    }

    /// Mixed freshness: recent agents always show; the oldest beyond the cap fold.
    /// Rows arrive recent-first (matching the real sort).
    #[test]
    fn idle_cap_mixes_recent_and_old() {
        use std::collections::HashSet;
        // 4 recent + (cap - 1) old = cap + 3 total. base_limit = max(cap, 4) = cap, so 3 fold.
        let total = MAX_VISIBLE_IDLE as u32 + 3;
        let mut rows: Vec<DashboardRow> = (0..4).map(|i| aged_idle_row(i, 0)).collect();
        rows.extend((4..total).map(|i| aged_idle_row(i, OLD_SECS)));
        let none: HashSet<SectionKey> = HashSet::new();
        let lines =
            build_dashboard_lines(&rows, Grouping::State, &Filter::None, &none, false, false);
        assert_eq!(
            idle_row_count(&lines),
            MAX_VISIBLE_IDLE,
            "shows cap (incl. all 4 recent)"
        );
        assert_eq!(
            overflow_of(&lines),
            Some((total as usize - MAX_VISIBLE_IDLE, false))
        );
    }

    /// `idle_show_all` reveals every agent and flips the overflow row to the "show fewer" (expanded) state.
    #[test]
    fn idle_cap_show_all_reveals_all() {
        use std::collections::HashSet;
        let total = MAX_VISIBLE_IDLE as u32 + 3;
        let rows: Vec<DashboardRow> = (0..total).map(|i| aged_idle_row(i, OLD_SECS)).collect();
        let none: HashSet<SectionKey> = HashSet::new();
        let lines =
            build_dashboard_lines(&rows, Grouping::State, &Filter::None, &none, true, false);
        assert_eq!(
            idle_row_count(&lines),
            total as usize,
            "show-all reveals every idle agent"
        );
        assert_eq!(
            overflow_of(&lines),
            Some((total as usize - MAX_VISIBLE_IDLE, true)),
            "overflow stays as a 'show fewer' affordance when expanded",
        );
    }

    /// Folding only kicks in at MIN_IDLE_FOLD (2): a single over-cap row is shown rather than hidden behind a same-height overflow row.
    #[test]
    fn idle_cap_does_not_fold_a_single_row() {
        use std::collections::HashSet;
        let none: HashSet<SectionKey> = HashSet::new();
        // MAX_VISIBLE_IDLE + 1 old would hide only 1: no fold
        let rows: Vec<DashboardRow> = (0..MAX_VISIBLE_IDLE as u32 + 1)
            .map(|i| aged_idle_row(i, OLD_SECS))
            .collect();
        let lines =
            build_dashboard_lines(&rows, Grouping::State, &Filter::None, &none, false, false);
        assert_eq!(
            idle_row_count(&lines),
            MAX_VISIBLE_IDLE + 1,
            "1 over cap is not folded"
        );
        assert_eq!(overflow_of(&lines), None);
        // MAX_VISIBLE_IDLE + 2 old hides 2: folds
        let rows: Vec<DashboardRow> = (0..MAX_VISIBLE_IDLE as u32 + 2)
            .map(|i| aged_idle_row(i, OLD_SECS))
            .collect();
        let lines =
            build_dashboard_lines(&rows, Grouping::State, &Filter::None, &none, false, false);
        assert_eq!(overflow_of(&lines), Some((2, false)), "2 over cap fold");
    }

    /// The cap is suppressed under an active filter: when you search, every match shows (no folding).
    #[test]
    fn idle_cap_disabled_under_filter() {
        use std::collections::HashSet;
        let total = MAX_VISIBLE_IDLE as u32 + 3;
        let rows: Vec<DashboardRow> = (0..total).map(|i| aged_idle_row(i, OLD_SECS)).collect();
        let none: HashSet<SectionKey> = HashSet::new();
        let lines = build_dashboard_lines(
            &rows,
            Grouping::State,
            &Filter::Substring("idle".into()),
            &none,
            false,
            false,
        );
        assert_eq!(
            overflow_of(&lines),
            None,
            "no fold under a substring filter"
        );

        // Search mode with an EMPTY query keeps `Filter::None`, but folding must still be suspended (search is active)
        let lines = build_dashboard_lines(
            &rows,
            Grouping::State,
            &Filter::None,
            &none,
            false,
            /* search_active */ true,
        );
        assert_eq!(
            overflow_of(&lines),
            None,
            "no fold while search mode is active even with an empty query",
        );
        assert_eq!(
            idle_row_count(&lines),
            total as usize,
            "every idle agent shows while searching",
        );
    }

    /// A collapsed Idle section hides its rows (and the overflow) via the section-collapse path; the cap and collapse don't double up.
    #[test]
    fn idle_cap_yields_to_section_collapse() {
        use std::collections::HashSet;
        let rows: Vec<DashboardRow> = (0..9).map(|i| aged_idle_row(i, OLD_SECS)).collect();
        let mut collapsed = HashSet::new();
        collapsed.insert(SectionKey::State(RowState::Idle));
        let lines = build_dashboard_lines(
            &rows,
            Grouping::State,
            &Filter::None,
            &collapsed,
            false,
            false,
        );
        assert_eq!(idle_row_count(&lines), 0, "collapsed Idle hides all rows");
        assert_eq!(overflow_of(&lines), None, "no overflow row under collapse");
    }

    /// The overflow row is a keyboard cursor target.
    #[test]
    fn idle_overflow_is_focusable_when_capped() {
        let rows: Vec<DashboardRow> = (0..MAX_VISIBLE_IDLE as u32 + 3)
            .map(|i| aged_idle_row(i, OLD_SECS))
            .collect();
        let none = std::collections::HashSet::new();
        let f = focusables(&rows, Grouping::State, &Filter::None, &none, false, false);
        assert!(
            f.iter().any(|x| matches!(x, Focusable::IdleOverflow)),
            "focusables must include the overflow toggle when capped",
        );
        // And the hidden idle rows are NOT focusable (capped away).
        let row_targets = f.iter().filter(|x| matches!(x, Focusable::Row(_))).count();
        assert_eq!(
            row_targets, MAX_VISIBLE_IDLE,
            "only the visible idle rows are focusable"
        );
    }

    /// Pinned top-level agents are lifted into a dedicated "Pinned" section at the very top (above the state groups).
    /// A pinned (e.g. idle) agent thus reads as pinned rather than landing under its state header.
    /// The pinned rows are NOT counted in the state-group headers.
    #[test]
    fn render_rows_emits_pinned_section_at_top() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 30));
        let mut state = DashboardState::new();
        assert_eq!(state.grouping, Grouping::State);
        // Input is pre-sorted (pinned first), as `sort_rows` guarantees.
        let mut pinned = header_test_row(1, RowState::Idle, "pinned idle row");
        pinned.pinned = true;
        let rows = vec![
            pinned,
            header_test_row(2, RowState::Working, "working row"),
            header_test_row(3, RowState::Idle, "idle row"),
        ];
        let theme = Theme::current();
        render_rows(&mut buf, Rect::new(0, 0, 80, 30), &theme, &rows, &mut state);
        let content = buf_to_text(&buf);

        // A dedicated "Pinned" section header with a count of 1.
        assert!(
            content.contains("Pinned 1"),
            "missing `Pinned` section header, got: {content:?}",
        );
        // It sits ABOVE the state groups.
        let idx_pinned = content.find("Pinned").expect("Pinned header present");
        let idx_working = content.find("Working").expect("Working header present");
        assert!(
            idx_pinned < idx_working,
            "the Pinned section must be at the top, got: {content:?}",
        );
        // The remaining (unpinned) idle row still gets its own `Idle 1` header; the pinned idle row is NOT folded into it
        assert!(
            content.contains("Idle 1"),
            "unpinned idle row must keep its own `Idle 1` header, got: {content:?}",
        );
        assert!(
            content.contains("pinned idle row"),
            "pinned row label renders"
        );
    }

    /// With grouping OFF (Directory) the "Pinned" text header is suppressed.
    /// A textless divider (a horizontal rule, no label) separates the pinned block from the rest; no state headers are emitted either.
    #[test]
    fn render_rows_groups_off_uses_divider_not_pinned_header() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 30));
        let mut state = DashboardState::new();
        state.grouping = Grouping::Directory; // groups off (Ctrl+G)
        let mut pinned = header_test_row(1, RowState::Idle, "pinned row");
        pinned.pinned = true;
        let rows = vec![pinned, header_test_row(2, RowState::Working, "working row")];
        let theme = Theme::current();
        render_rows(&mut buf, Rect::new(0, 0, 80, 30), &theme, &rows, &mut state);
        let content = buf_to_text(&buf);

        // No labelled "Pinned"/state headers in groups-off mode.
        assert!(
            !content.contains("Pinned"),
            "the `Pinned` text header must be hidden when grouping is off, got: {content:?}",
        );
        assert!(
            !content.contains("Working "),
            "no state headers when grouping is off, got: {content:?}",
        );
        // A horizontal-rule divider separates the pinned block from the rest.
        assert!(
            content.contains('\u{2500}'),
            "a divider rule must separate pinned from non-pinned, got: {content:?}",
        );
        // The pinned row still renders above the divider, which is above the rest.
        let idx_pinned = content.find("pinned row").expect("pinned row present");
        let idx_rule = content.find('\u{2500}').expect("divider present");
        let idx_working = content.find("working row").expect("working row present");
        assert!(
            idx_pinned < idx_rule && idx_rule < idx_working,
            "order must be pinned → divider → rest, got: {content:?}",
        );
    }

    /// State group headers are emitted at every top-level state transition when grouping is `State`.
    /// The renderer must paint the headers in NeedsInput, Working, Idle, Completed, Failed order (matching `RowState::group_priority`).
    ///
    /// Header chrome is `  ● Label (N)`: a 2-col indent, a state-coloured dot, then the label and count in `gray_dim`.
    /// The previous full-row `── Label (N) ────────────────` chrome was dropped (the trailing dashes felt visually obnoxious).
    #[test]
    fn render_rows_emits_group_headers_in_state_order() {
        // Rows are 3 cells tall, headers 2 cells; 5 of each needs 25 cells of vertical room
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 30));
        let mut state = DashboardState::new();
        assert_eq!(state.grouping, Grouping::State);
        let rows = vec![
            header_test_row(1, RowState::NeedsInput, "needs-input row"),
            header_test_row(2, RowState::Working, "working row"),
            header_test_row(3, RowState::Idle, "idle row"),
            header_test_row(4, RowState::Completed, "completed row"),
            header_test_row(5, RowState::Failed, "failed row"),
        ];
        let theme = Theme::current();
        render_rows(&mut buf, Rect::new(0, 0, 80, 30), &theme, &rows, &mut state);
        let content = buf_to_text(&buf);
        // Group labels: Awaiting / Working / Idle / Done / Failed.
        for label in ["Awaiting", "Working", "Idle", "Done", "Failed"] {
            assert!(
                content.contains(label),
                "missing group header `{label}`, got: {content:?}",
            );
        }
        // Headers appear in the canonical priority order.
        let idx_aw = content.find("Awaiting").expect("Awaiting present");
        let idx_wk = content.find("Working").expect("Working present");
        let idx_id = content.find("Idle").expect("Idle present");
        let idx_dn = content.find("Done").expect("Done present");
        let idx_fl = content.find("Failed").expect("Failed present");
        assert!(idx_aw < idx_wk, "Awaiting must precede Working");
        assert!(idx_wk < idx_id, "Working must precede Idle");
        assert!(idx_id < idx_dn, "Idle must precede Done");
        assert!(idx_dn < idx_fl, "Done must precede Failed");
        // Group headers read `Label N ──────…`.
        assert!(
            content.contains("Awaiting 1"),
            "header label + count missing or malformed, got: {content:?}",
        );
        assert!(
            content.contains('\u{2500}'),
            "header must paint the trailing horizontal rule, got: {content:?}",
        );
        assert!(
            !content.contains("Awaiting ("),
            "old parenthesised `Awaiting (1)` form must be gone, got: {content:?}",
        );
        // Each row's label still renders.
        for label in [
            "needs-input row",
            "working row",
            "idle row",
            "completed row",
            "failed row",
        ] {
            assert!(
                content.contains(label),
                "missing row label `{label}`, got: {content:?}",
            );
        }
    }

    /// The list scrollbar is a thick `█` thumb overlaid on the right edge, and it does NOT reserve a column.
    /// The row content is byte-for-byte identical whether or not the scrollbar shows (no layout shift).
    #[test]
    fn render_rows_scrollbar_is_thick_overlay_without_layout_shift() {
        let theme = Theme::current();
        // 6 working rows: 1 header (2 cells) + 6 rows (3 cells) = 20 cells
        let rows: Vec<_> = (0..6)
            .map(|i| header_test_row(i, RowState::Working, "working task"))
            .collect();
        let w = 60u16;

        // Tall viewport: everything fits, no scrollbar
        let mut buf_fit = Buffer::empty(Rect::new(0, 0, w, 24));
        let mut state_fit = DashboardState::new();
        render_rows(
            &mut buf_fit,
            Rect::new(0, 0, w, 24),
            &theme,
            &rows,
            &mut state_fit,
        );

        // Short viewport: overflow, scrollbar overlays
        let h = 8u16;
        let mut buf_scroll = Buffer::empty(Rect::new(0, 0, w, h));
        let mut state_scroll = DashboardState::new();
        render_rows(
            &mut buf_scroll,
            Rect::new(0, 0, w, h),
            &theme,
            &rows,
            &mut state_scroll,
        );

        // The thick `█` thumb is painted on the rightmost column.
        let last_x = w - 1;
        let has_thumb = (0..h).any(|y| buf_scroll[(last_x, y)].symbol() == "\u{2588}");
        assert!(has_thumb, "scrollbar thumb (█) must overlay the right edge");
        // Old thin `│`-only thumb is gone.
        let thin_only = (0..h).all(|y| buf_scroll[(last_x, y)].symbol() != "\u{2588}");
        assert!(!thin_only, "thumb must be the thick block glyph");

        // No layout shift: every content column (all but the overlaid right edge) matches the no-scrollbar render across the visible top
        for y in 0..h {
            for x in 0..(w - 1) {
                assert_eq!(
                    buf_scroll[(x, y)].symbol(),
                    buf_fit[(x, y)].symbol(),
                    "content shifted at ({x},{y}) when the scrollbar appeared",
                );
            }
        }
    }

    /// Row layout is two visual lines:
    ///
    /// ```text
    ///   ◆ who are you?                                                     4s    <- title row
    ///     Responding                                                             <- secondary row
    /// ```
    ///
    /// - Col 0: selection marker (thin bar `▏` when selected, space otherwise).
    ///   The bar spans the full content height of a selected row (title and secondary lines). No hover glyph.
    /// - Col 1: 1-col gap.
    /// - Col 2: state icon.
    /// - Col 3: 1-col gap.
    /// - Col 4: label starts on row 0, and the secondary text starts at the same column on row 1.
    /// - Right edge: age column (`{n}s/m/h`).
    #[test]
    fn render_row_two_line_layout_paints_title_and_secondary() {
        use std::path::PathBuf;
        use std::time::SystemTime;
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 2));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.spinner_tick = 8; // Tick 8 selects dot_spinner_frames()[2], the `⸬` glyph.
        let row = DashboardRow {
            id: DashboardRowId::TopLevel(crate::app::agent::AgentId(1)),
            label: "who are you?".to_string(),
            subtitle: None,
            state: RowState::Working,
            activity: Some("Responding".to_string()),
            secondary_line: Some("Responding".to_string()),
            cwd_display: String::new(),
            cwd: PathBuf::from("/tmp"),
            last_change_at: SystemTime::now(),
            pinned: false,
            badges: Vec::new(),
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        };
        render_row(&mut buf, Rect::new(0, 0, 100, 2), &theme, &row, &mut state);

        // Title row.
        assert_eq!(
            buf[(0, 0)].symbol(),
            " ",
            "row 0 col 0 must be marker space"
        );
        assert_eq!(
            buf[(1, 0)].symbol(),
            " ",
            "row 0 col 1 must be the post-marker gap"
        );
        assert_eq!(
            buf[(2, 0)].symbol(),
            "\u{2e2c}",
            "row 0 col 2 must be the spinner glyph `⸬` at tick=8",
        );
        assert_eq!(
            buf[(3, 0)].symbol(),
            " ",
            "row 0 col 3 must be the post-icon gap"
        );
        assert_eq!(
            buf[(4, 0)].symbol(),
            "w",
            "row 0 col 4 must start the label"
        );

        // Secondary row: `Responding` starts at the same column as the title's label start (col 4)
        assert_eq!(
            buf[(4, 1)].symbol(),
            "R",
            "row 1 col 4 must start the secondary text",
        );

        // Age column right-aligns in the last few cells of row 0.
        let mut saw_s_in_age_zone = false;
        for x in (100 - 8)..100 {
            if buf[(x, 0)].symbol() == "s" {
                saw_s_in_age_zone = true;
                break;
            }
        }
        let content = buf_to_text(&buf);
        assert!(
            saw_s_in_age_zone,
            "age column must paint a duration ending in `s`, got: {content:?}",
        );
    }

    /// The SELECTED row brightens its secondary text from `gray_dim` to `text_secondary`.
    /// The user can then read what the agent is doing without leaving the dashboard.
    /// The unselected baseline stays dim: the row's metadata tail shouldn't compete with the title for attention.
    /// Both states are pinned in one test so a regression that flipped either direction would fail.
    #[test]
    fn render_row_selected_brightens_secondary_text() {
        use std::path::PathBuf;
        use std::time::SystemTime;
        let theme = Theme::current();
        let id = DashboardRowId::TopLevel(crate::app::agent::AgentId(7));
        let row = DashboardRow {
            id: id.clone(),
            label: "investigate caching".to_string(),
            subtitle: None,
            state: RowState::Working,
            activity: Some("Responding".to_string()),
            // The 'R' in "Responding" lives at column 4 (matches `render_row_two_line_layout_paints_title_and_secondary`), so we sample fg at (4, 1)
            secondary_line: Some("Responding".to_string()),
            cwd_display: String::new(),
            cwd: PathBuf::from("/tmp"),
            last_change_at: SystemTime::now(),
            pinned: false,
            badges: Vec::new(),
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        };

        // Unselected: dim secondary
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 2));
        let mut state_unselected = DashboardState::new();
        render_row(
            &mut buf,
            Rect::new(0, 0, 100, 2),
            &theme,
            &row,
            &mut state_unselected,
        );
        assert_eq!(
            buf[(4, 1)].fg,
            theme.gray_dim,
            "unselected row's secondary must paint in `gray_dim`",
        );

        // Selected: brighter secondary
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 2));
        let mut state_selected = DashboardState::new();
        state_selected.focus_row(id);
        render_row(
            &mut buf,
            Rect::new(0, 0, 100, 2),
            &theme,
            &row,
            &mut state_selected,
        );
        assert_eq!(
            buf[(4, 1)].fg,
            theme.text_secondary,
            "selected row's secondary must brighten to `text_secondary` \
             so the response line is readable",
        );
    }

    /// On a `NeedsInput` row the bullet is yellow and blinks to a dimmer yellow.
    /// The `[needs input]` badge is suppressed, and the `Pending:` subtitle prefix is painted yellow.
    #[test]
    fn render_row_needs_input_yellow_blink_no_badge_pending_prefix() {
        use std::path::PathBuf;
        use std::time::SystemTime;
        let theme = Theme::current();
        let make_row = || DashboardRow {
            id: DashboardRowId::TopLevel(crate::app::agent::AgentId(1)),
            label: "ask me".to_string(),
            subtitle: None,
            state: RowState::NeedsInput,
            activity: None,
            secondary_line: Some("Pending: plan approval".to_string()),
            cwd_display: String::new(),
            cwd: PathBuf::from("/tmp"),
            last_change_at: SystemTime::now(),
            pinned: false,
            badges: vec![RowBadge::NeedsInput],
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        };
        let render = |tick: u64| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 100, 2));
            let mut state = DashboardState::new();
            state.spinner_tick = tick;
            render_row(
                &mut buf,
                Rect::new(0, 0, 100, 2),
                &theme,
                &make_row(),
                &mut state,
            );
            buf
        };

        // Bright phase (tick 0): the bullet is full yellow.
        let bright = render(0);
        assert_eq!(
            bright[(2, 0)].symbol(),
            crate::glyphs::diamond_filled(),
            "bullet glyph"
        );
        assert_eq!(
            bright[(2, 0)].fg,
            theme.warning,
            "bright needs-input bullet must be yellow (warning)",
        );

        // No `[needs input]` badge on the title row.
        let mut title = String::new();
        for x in 0..bright.area.width {
            title.push_str(bright[(x, 0)].symbol());
        }
        assert!(
            !title.contains("needs input"),
            "needs-input badge must be hidden, got: {title:?}",
        );

        // `Pending:` subtitle prefix is painted yellow (the rest of the subtitle is painted separately in the dim secondary colour)
        assert_eq!(
            bright[(4, 1)].symbol(),
            "P",
            "secondary starts with `Pending:`"
        );
        assert_eq!(
            bright[(4, 1)].fg,
            theme.warning,
            "`Pending:` prefix must be yellow",
        );

        // The dim blink phase fades the bullet (only assertable when the theme supports blending; non-truecolor falls back to full yellow)
        if crate::render::color::blend_color(theme.bg_base, theme.warning, 0.5).is_some() {
            let dim = render(NEEDS_INPUT_BLINK_DIVISOR);
            assert_ne!(
                dim[(2, 0)].fg,
                theme.warning,
                "dim blink phase must fade the bullet away from full yellow",
            );
        }
    }

    /// The `New session #<id>` fallback title is painted two-tone: the `New session` head in the primary colour and the ` #id` suffix dim.
    #[test]
    fn render_row_new_session_fallback_label_is_two_tone() {
        use std::path::PathBuf;
        use std::time::SystemTime;
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 2));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        let row = DashboardRow {
            id: DashboardRowId::TopLevel(crate::app::agent::AgentId(1)),
            label: "New session #abc12345".to_string(),
            subtitle: None,
            state: RowState::Idle,
            activity: None,
            secondary_line: None,
            cwd_display: String::new(),
            cwd: PathBuf::from("/tmp"),
            last_change_at: SystemTime::now(),
            pinned: false,
            badges: Vec::new(),
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        };
        render_row(&mut buf, Rect::new(0, 0, 100, 2), &theme, &row, &mut state);

        // Title starts at col 4: "New session" (11 chars, cols 4..15) then " #abc12345" (suffix from col 15)
        assert_eq!(
            buf[(4, 0)].symbol(),
            "N",
            "title head starts with `New session`"
        );
        assert_eq!(
            buf[(4, 0)].fg,
            theme.text_primary,
            "`New session` head must use the primary colour",
        );
        // The `#` of the suffix sits at col 16 and must be dim.
        assert_eq!(buf[(16, 0)].symbol(), "#", "suffix must start with `#`");
        assert_eq!(
            buf[(16, 0)].fg,
            theme.gray_dim,
            "the `#id` suffix must be dim gray",
        );
    }

    /// Group header (section title) leads with a disclosure glyph at col 0, then the label at col 2, within the list area.
    /// Row content below is indented (marker col 0, gap col 1, icon col 2).
    /// The header is 2 visual cells tall (label + gap).
    /// The title-only row centers its title, so the title sits 3 rows below the header in this fixture.
    #[test]
    fn render_group_header_leads_with_disclosure_glyph() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 8));
        let mut state = DashboardState::new();
        let rows = vec![header_test_row(1, RowState::Idle, "session 019e5d9f")];
        let theme = Theme::current();
        render_rows(&mut buf, Rect::new(0, 0, 80, 8), &theme, &rows, &mut state);

        // Col 0 is the (expanded) disclosure glyph; the label starts at col 2 (glyph, then a space)
        // Rows below have their marker/icon in the left columns and text indented
        assert_eq!(
            buf[(0, 0)].symbol(),
            crate::glyphs::disclosure_open(),
            "section header must lead with the expanded disclosure glyph",
        );
        let header_label_x = buf[(2, 0)].symbol().to_string();
        assert_eq!(
            header_label_x, "I",
            "section title `Idle …` must start after the disclosure glyph, got: {header_label_x:?}",
        );

        // Header gap: row 1 is blank
        // The title-only row centers its title within its 3-cell rect (y=2..5), so the title sits at y=3
        // Rows still render their marker/icon in the left chrome columns
        let row_col0 = buf[(0, 3)].symbol().to_string();
        let row_col1 = buf[(1, 3)].symbol().to_string();
        let row_col2 = buf[(2, 3)].symbol().to_string();
        assert_eq!(
            row_col0, " ",
            "row's col 0 must be the marker space when nothing selected, got: {row_col0:?}",
        );
        assert_eq!(
            row_col1, " ",
            "row's col 1 must be the 1-col gap after the marker, got: {row_col1:?}",
        );
        assert_eq!(
            row_col2,
            crate::glyphs::diamond_hollow(),
            "row's col 2 must be the hollow diamond for Idle, got: {row_col2:?}",
        );
    }

    /// `Grouping::Directory` keeps cwd as the grouping primitive, so state headers are suppressed.
    ///
    /// The `(count)` parenthesis pattern is the specific fingerprint for a state header.
    #[test]
    fn render_rows_skips_headers_when_grouping_is_directory() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 10));
        let mut state = DashboardState::new();
        state.grouping = Grouping::Directory;
        let rows = vec![
            header_test_row(1, RowState::Working, "a"),
            header_test_row(2, RowState::Idle, "b"),
        ];
        let theme = Theme::current();
        render_rows(&mut buf, Rect::new(0, 0, 80, 10), &theme, &rows, &mut state);
        let content = buf_to_text(&buf);
        assert!(
            !content.contains("Working ("),
            "Directory grouping must suppress Working header, got: {content:?}",
        );
        assert!(
            !content.contains("Idle ("),
            "Directory grouping must suppress Idle header, got: {content:?}",
        );
        // Rows themselves still render.
        assert!(
            content.contains('a') && content.contains('b'),
            "rows must still render under Directory grouping, got: {content:?}",
        );
    }

    /// `Filter::State(_)` collapses the view to a single state, so the header would be redundant chrome.
    ///
    /// The test looks for `Working (` instead of `── Working` as the header fingerprint.
    #[test]
    fn render_rows_skips_headers_when_filter_is_state() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 10));
        let mut state = DashboardState::new();
        state.filter = Filter::State(RowState::Working);
        let rows = vec![
            header_test_row(1, RowState::Working, "first working"),
            header_test_row(2, RowState::Working, "second working"),
        ];
        let theme = Theme::current();
        render_rows(&mut buf, Rect::new(0, 0, 80, 10), &theme, &rows, &mut state);
        let content = buf_to_text(&buf);
        assert!(
            !content.contains("Working ("),
            "state-filtered view must suppress Working header, got: {content:?}",
        );
        // Rows themselves still render.
        assert!(
            content.contains("first working"),
            "first row must render, got: {content:?}",
        );
        assert!(
            content.contains("second working"),
            "second row must render, got: {content:?}",
        );
    }

    /// Subagent rows (indent > 0) must NOT trigger their own state header; they inherit their parent's group.
    /// A `Working` parent followed by `Completed` and `Failed` subagents must emit only the parent's `Working` header, not extra ones for the subagents.
    #[test]
    fn render_rows_subagents_do_not_trigger_their_own_headers() {
        use crate::app::agent::AgentId;
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 20));
        let mut state = DashboardState::new();
        let parent = DashboardRow {
            id: DashboardRowId::TopLevel(AgentId(1)),
            indent: 0,
            ..header_test_row(1, RowState::Working, "parent")
        };
        let sub_completed = DashboardRow {
            id: DashboardRowId::Subagent {
                parent: AgentId(1),
                child_session_id: "c1".to_string(),
            },
            label: "sub-completed".to_string(),
            indent: 1,
            ..header_test_row(11, RowState::Completed, "sub-completed")
        };
        let sub_failed = DashboardRow {
            id: DashboardRowId::Subagent {
                parent: AgentId(1),
                child_session_id: "c2".to_string(),
            },
            label: "sub-failed".to_string(),
            indent: 1,
            ..header_test_row(12, RowState::Failed, "sub-failed")
        };
        let rows = vec![parent, sub_completed, sub_failed];
        let theme = Theme::current();
        render_rows(&mut buf, Rect::new(0, 0, 80, 20), &theme, &rows, &mut state);
        let content = buf_to_text(&buf);
        // Group header reads `Working 1 ─────` (no parens; the trailing rule fills the rest of the row)
        assert!(
            content.contains("Working 1"),
            "parent's Working header must render, got: {content:?}",
        );
        // Subagents inherit their parent's group and must NOT emit their own headers
        // The trailing `\u{2500}` rule is the marker that distinguishes a header from a row
        assert!(
            !content.contains("Completed 1"),
            "subagent must NOT trigger a Completed header, got: {content:?}",
        );
        assert!(
            !content.contains("Failed 1"),
            "subagent must NOT trigger a Failed header, got: {content:?}",
        );
    }

    /// Narrow mode emits a compact `Done 12` header (no bullet, no parens, no trailing rule; narrow terminals don't have the width budget).
    #[test]
    fn render_narrow_rows_emits_compact_group_headers() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 10));
        let mut state = DashboardState::new();
        let rows = vec![
            header_test_row(1, RowState::Working, "wrk"),
            header_test_row(2, RowState::Idle, "idl"),
        ];
        let theme = Theme::current();
        render_narrow_rows(&mut buf, Rect::new(0, 0, 30, 10), &theme, &rows, &mut state);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("Working 1"),
            "narrow header `Working 1` missing, got: {content:?}",
        );
        assert!(
            content.contains("Idle 1"),
            "narrow header `Idle 1` missing, got: {content:?}",
        );
        // Narrow mode must NOT paint the wide trailing rule.
        assert!(
            !content.contains("\u{2500}\u{2500}\u{2500}"),
            "narrow header must not paint a trailing rule, got: {content:?}",
        );
    }

    /// Narrow-layout regression: the viewport clamp must follow a selected *section header*, not just a selected row.
    /// With the section cursor a header can become the keyboard target.
    /// A clamp that only tracks `state.selected` (a row) leaves that header off-screen even though the wide layout scrolls it in.
    /// Here the second group's "Idle" header lands below a 5-line viewport, so it is off-screen at offset 0 and must be scrolled in once selected.
    #[test]
    fn render_narrow_viewport_follows_selected_section_header() {
        let mut rows = Vec::new();
        for i in 0..6 {
            rows.push(header_test_row(i + 1, RowState::Working, "wrk"));
        }
        rows.push(header_test_row(99, RowState::Idle, "idl"));
        let theme = Theme::current();
        let area = Rect::new(0, 0, 30, 5);

        // Control (nothing selected): the Idle header starts off-screen
        {
            let mut buf = Buffer::empty(area);
            let mut state = DashboardState::new();
            render_narrow_rows(&mut buf, area, &theme, &rows, &mut state);
            let content = buf_to_text(&buf);
            assert!(
                !content.contains("Idle"),
                "fixture invalid — Idle header must start off-screen, got: {content:?}",
            );
        }

        // Selecting the Idle section must scroll its header into view.
        {
            let mut buf = Buffer::empty(area);
            let mut state = DashboardState::new();
            state.selected_section = Some(SectionKey::State(RowState::Idle));
            render_narrow_rows(&mut buf, area, &theme, &rows, &mut state);
            let content = buf_to_text(&buf);
            assert!(
                content.contains("Idle 1"),
                "selected Idle section header must be clamped into view, got: {content:?}",
            );
            assert!(
                state.viewport_offset > 0,
                "viewport must scroll to reveal the selected section header, got offset {}",
                state.viewport_offset,
            );
        }
    }

    /// `render_dashboard` must paint the theme's base background across the entire `area` before any sub-renderer runs.
    /// Without this, cells untouched by the header/list/dispatch/footer renderers keep stale paint from the previous frame.
    /// The dashboard then looks like it doesn't cover the full panel.
    ///
    /// The check pins a cell that no sub-renderer touches: the trailing whitespace one column past the right edge of the list.
    /// Its background must match `theme.bg_base`.
    /// Pre-seed the buffer with a contrasting bg so a regression that drops the fill is visible.
    /// The default-empty buffer would otherwise already show the right colour.
    #[test]
    fn render_dashboard_paints_full_area_background() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        // Seed every cell with a contrasting bg so a missing fill is detectable
        // Any cell still carrying this seed colour after `render_dashboard` runs means the fill didn't reach it
        let seed = ratatui::style::Color::Rgb(0xFF, 0x00, 0xFF);
        buf.set_style(area, Style::default().bg(seed));

        let mut agents: IndexMap<AgentId, AgentView> = IndexMap::new();
        let mut state = DashboardState::new();
        let registry = crate::actions::ActionRegistry::defaults();
        let _ = render_dashboard(
            &mut buf,
            area,
            &mut state,
            &mut agents,
            &registry,
            None,
            &[],
            false,
            None,
            false,
            None,
        );

        // Sample cells across the area; none may retain the seed bg colour
        // The dashboard fills with `theme.bg_base`; the exact colour need not match a constant, we only assert the seed is gone
        for y in 0..area.height {
            for x in 0..area.width {
                let cell_bg = buf[(x, y)].bg;
                assert_ne!(
                    cell_bg, seed,
                    "cell at ({x}, {y}) still carries the seed bg — render_dashboard must fill the entire area",
                );
            }
        }
        // And spot-check that at least one cell matches the theme bg, i.e., the fill actually used `theme.bg_base` (not just any non-seed colour)
        let mut saw_bg_base = false;
        for y in 0..area.height {
            for x in 0..area.width {
                if buf[(x, y)].bg == theme.bg_base {
                    saw_bg_base = true;
                    break;
                }
            }
            if saw_bg_base {
                break;
            }
        }
        assert!(
            saw_bg_base,
            "render_dashboard must paint at least one cell with theme.bg_base",
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Header redesign tests
    // ─────────────────────────────────────────────────────────────────

    /// Basename of the test process's cwd, the one deterministic fragment of the header's location label.
    /// The full label depends on global git caches (`git_info::*`) that parallel tests may touch.
    /// Every fallback path still renders a cwd display ending in the current directory's basename.
    fn cwd_basename() -> String {
        std::env::current_dir()
            .ok()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
            .expect("test process must have a cwd with a basename")
    }

    /// The header pairs the location label (cwd and git info) on the left with right-aligned state-count chips.
    #[test]
    fn render_header_paints_label_and_state_chips() {
        let theme = Theme::current();
        // Wide rect so the location label never truncates regardless of how deep the test machine's checkout path is
        let area = Rect::new(0, 0, 400, 1);
        let mut buf = Buffer::empty(area);
        let mut state = DashboardState::new();
        let rows = vec![
            header_test_row(1, RowState::NeedsInput, "a"),
            header_test_row(2, RowState::NeedsInput, "b"),
            header_test_row(3, RowState::Working, "c"),
            header_test_row(4, RowState::Idle, "d"),
        ];
        render_header(&mut buf, area, &theme, &rows, &mut state, None);
        let content = buf_to_text(&buf);
        let basename = cwd_basename();
        assert!(
            content.contains(&basename),
            "header must show the current location (`{basename}`), got: {content:?}",
        );
        for chip in ["2 awaiting", "1 working", "1 idle"] {
            assert!(
                content.contains(chip),
                "header missing chip `{chip}`, got: {content:?}",
            );
        }
        // The old static label and total count are gone.
        assert!(
            !content.contains("Agents"),
            "header must not paint the old `Agents` label, got: {content:?}",
        );
        assert!(
            !content.contains("4 agents"),
            "total must not appear as right-side chip, got: {content:?}",
        );
        // The button form is gone.
        assert!(
            !content.contains("[New agent +]"),
            "v2-round-2 header must not paint the `[New agent +]` button anymore, got: {content:?}",
        );
    }

    /// The header records a click target for the location label so the mouse handler can open the location picker.
    #[test]
    fn render_header_sets_location_click_target() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        let mut state = DashboardState::new();
        render_header(&mut buf, area, &theme, &[], &mut state, None);
        assert!(
            state.location_hit.rect.is_some(),
            "render_header must record a click target for the location label",
        );
    }

    /// On hover the location label underlines only its text; the leading inset space (and other whitespace padding) stays un-underlined.
    #[test]
    fn render_header_hover_underlines_only_text() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 400, 1);
        let mut buf = Buffer::empty(area);
        let mut state = DashboardState::new();
        state.location_hit.hovered = true;
        render_header(&mut buf, area, &theme, &[], &mut state, None);

        // Leading inset (x=0) is a space and must NOT be underlined
        let inset = buf.cell((0, 0)).expect("inset cell");
        assert_eq!(inset.symbol(), " ", "x=0 is the leading inset space");
        assert!(
            !inset.style().add_modifier.contains(Modifier::UNDERLINED),
            "the leading inset space must not be underlined",
        );

        // The recorded hit rect bounds the location label, so the right-side chips are excluded
        // The last visible glyph inside it is cwd path text, never the branch icon, so it must be underlined
        // Bounding to the label keeps this robust whether or not a branch is present for the test's cwd
        let label_end = state
            .location_hit
            .rect
            .expect("header records the location hit rect")
            .width;
        let last_text_x = (1..label_end)
            .rev()
            .find(|&x| {
                buf.cell((x, 0))
                    .is_some_and(|c| !c.symbol().trim().is_empty())
            })
            .expect("header must paint visible location text");
        assert!(
            buf.cell((last_text_x, 0))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::UNDERLINED),
            "the location path text must be underlined on hover",
        );
    }

    /// The git span (`{icon} {branch}`) underlines only the branch *name*: the branch icon and the space after it stay bare.
    /// Whitespace-only spans (inset, separator) stay bare; the path is underlined.
    #[test]
    fn underline_location_on_hover_excludes_branch_icon() {
        let icon = "\u{e0a0}";
        let plain = Style::default();
        let spans = vec![
            Span::styled(" ".to_string(), plain),        // leading inset
            Span::styled(format!("{icon} main"), plain), // git: icon + branch
            Span::styled(" ".to_string(), plain),        // git↔path separator
            Span::styled("/home/me/repo".to_string(), plain), // path
        ];
        let out = underline_location_on_hover(spans, icon);

        let underlined = |s: &Span<'static>| s.style.add_modifier.contains(Modifier::UNDERLINED);

        // The git span split into `{icon} ` (bare) and `main` (underlined)
        let icon_part = out
            .iter()
            .find(|s| s.content.starts_with(icon))
            .expect("icon span present");
        assert_eq!(icon_part.content.as_ref(), format!("{icon} "));
        assert!(
            !underlined(icon_part),
            "the branch icon and the space after it must not be underlined",
        );
        let branch = out
            .iter()
            .find(|s| &*s.content == "main")
            .expect("branch span present");
        assert!(underlined(branch), "the branch name must be underlined");

        // Whitespace-only spans stay bare.
        for s in &out {
            if s.content.chars().all(char::is_whitespace) {
                assert!(
                    !underlined(s),
                    "whitespace span must not be underlined: {:?}",
                    s.content,
                );
            }
        }
        // The path is underlined.
        let path = out
            .iter()
            .find(|s| &*s.content == "/home/me/repo")
            .expect("path span present");
        assert!(underlined(path), "the path must be underlined");
    }

    /// The location picker modal paints its title and candidate rows and records the content hit areas for mouse handling.
    #[test]
    fn render_location_picker_shows_candidates() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        let mut modal = LocationPickerState::new(
            vec![crate::views::dashboard::LocationCandidate {
                path: std::path::PathBuf::from("/home/me/frontend"),
                label: "frontend".to_string(),
                detail: "~/me/frontend".to_string(),
                worktree: None,
            }],
            std::path::PathBuf::from("/base"),
            std::collections::HashMap::new(),
        );
        render_location_picker(&mut buf, area, &theme, &mut modal);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("Change directory"),
            "modal title missing, got: {content:?}",
        );
        assert!(
            content.contains("path:"),
            "path input field missing, got: {content:?}",
        );
        assert!(
            content.contains("frontend"),
            "candidate label missing, got: {content:?}",
        );
        assert!(
            modal.content_hits.is_some(),
            "content hit areas must be recorded for the mouse handler",
        );
    }

    /// In a git repo the path row paints a worktree toggle reflecting the modal's `worktree_mode`, and records its hit rect for click handling.
    #[test]
    fn render_location_picker_shows_worktree_toggle_in_repo() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 24);
        // A temp dir with a `.git` child so the toggle is eligible (hermetic, unlike depending on the test's real cwd being a repo)
        let repo = std::env::temp_dir().join("grok-loc-wt-toggle-repo-test");
        std::fs::create_dir_all(repo.join(".git")).expect("mk .git");
        let mut modal =
            LocationPickerState::new(vec![], repo.clone(), std::collections::HashMap::new());

        // Off by default.
        let mut buf = Buffer::empty(area);
        render_location_picker(&mut buf, area, &theme, &mut modal);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("worktree:off"),
            "off-state button missing, got: {content:?}",
        );
        assert!(
            modal.worktree_hit.rect.is_some(),
            "the worktree button hit rect must be recorded",
        );

        // On after toggling.
        modal.worktree_mode = true;
        let mut buf = Buffer::empty(area);
        render_location_picker(&mut buf, area, &theme, &mut modal);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("worktree:on"),
            "on-state button missing, got: {content:?}",
        );
        // The "on" word is recolored green (accent_success).
        let hit = modal.worktree_hit.rect.expect("hit rect recorded");
        let on_x = hit.x + "[worktree:".len() as u16;
        let cell = buf.cell((on_x, hit.y)).expect("on cell");
        assert_eq!(cell.symbol(), "o", "expected the 'o' of \"on\"");
        assert_eq!(
            cell.style().fg,
            Some(theme.accent_success),
            "the \"on\" word must be green",
        );

        modal.worktree_hit.hovered = true;
        let mut buf = Buffer::empty(area);
        render_location_picker(&mut buf, area, &theme, &mut modal);
        let cell = buf.cell((hit.x, hit.y)).expect("button cell");
        assert_eq!(
            cell.style().fg,
            Some(theme.text_primary),
            "hovered button must brighten the label text",
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Outside a git repo the worktree toggle is hidden (and no hit rect is recorded) so dispatch proceeds as a normal session.
    #[test]
    fn render_location_picker_hides_worktree_toggle_outside_repo() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 24);
        let mut modal = LocationPickerState::new(
            vec![],
            std::path::PathBuf::from("/grok-not-a-repo-xyz-12345"),
            std::collections::HashMap::new(),
        );
        let mut buf = Buffer::empty(area);
        render_location_picker(&mut buf, area, &theme, &mut modal);
        let content = buf_to_text(&buf);
        assert!(
            !content.contains("worktree"),
            "the worktree toggle must be hidden outside a repo, got: {content:?}",
        );
        assert!(
            modal.worktree_hit.rect.is_none(),
            "no hit rect when the toggle is hidden",
        );
    }

    /// Worktree directories render a styled `worktree: <name>` badge.
    #[test]
    fn render_location_picker_tags_worktree() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        let mut modal = LocationPickerState::new(
            vec![crate::views::dashboard::LocationCandidate {
                path: std::path::PathBuf::from("/home/me/wt"),
                label: "wt".to_string(),
                detail: "~/me/wt".to_string(),
                worktree: Some("my-feature".to_string()),
            }],
            std::path::PathBuf::from("/base"),
            std::collections::HashMap::new(),
        );
        render_location_picker(&mut buf, area, &theme, &mut modal);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("worktree: my-feature"),
            "worktree badge missing, got: {content:?}",
        );
    }

    /// Truncation priority: the directory name (label) is shown in full and the path (right label) is truncated first.
    #[test]
    fn render_location_picker_truncates_path_not_label() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 54, 20);
        let mut buf = Buffer::empty(area);
        let mut modal = LocationPickerState::new(
            vec![crate::views::dashboard::LocationCandidate {
                path: std::path::PathBuf::from("/home/me/myproj"),
                label: "myproj".to_string(),
                detail: "~/very/long/path/that/keeps/going/UNIQUETAIL".to_string(),
                worktree: None,
            }],
            std::path::PathBuf::from("/base"),
            std::collections::HashMap::new(),
        );
        render_location_picker(&mut buf, area, &theme, &mut modal);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("myproj"),
            "full label must be shown, got: {content:?}",
        );
        assert!(
            !content.contains("UNIQUETAIL"),
            "long path must be truncated, got: {content:?}",
        );
    }

    /// The path input echoes what the user types, even when it matches no candidate (the list then shows "No matches").
    #[test]
    fn render_location_picker_echoes_typed_path() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        let mut modal = LocationPickerState::new(
            vec![crate::views::dashboard::LocationCandidate {
                path: std::path::PathBuf::from("/home/me/frontend"),
                label: "frontend".to_string(),
                detail: "~/me/frontend".to_string(),
                worktree: None,
            }],
            std::path::PathBuf::from("/base"),
            std::collections::HashMap::new(),
        );
        modal.picker.set_query("/tmp/zzz");
        render_location_picker(&mut buf, area, &theme, &mut modal);
        let content = buf_to_text(&buf);
        assert!(
            content.contains("/tmp/zzz"),
            "typed path must echo in the input field, got: {content:?}",
        );
    }

    /// Zero-count states are suppressed.
    #[test]
    fn render_header_suppresses_zero_count_chips() {
        let theme = Theme::current();
        let mut buf = Buffer::empty(Rect::new(0, 0, 120, 1));
        let mut state = DashboardState::new();

        let rows = vec![header_test_row(1, RowState::Idle, "x")];
        render_header(
            &mut buf,
            Rect::new(0, 0, 120, 1),
            &theme,
            &rows,
            &mut state,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains("1 idle"),
            "expected `1 idle`, got: {content:?}"
        );
        for absent in ["0 awaiting", "0 working", "0 done", "0 failed", "0 blocked"] {
            assert!(
                !content.contains(absent),
                "zero-count chip `{absent}` must be suppressed, got: {content:?}",
            );
        }
    }

    /// Inactive (roster-only) rows get no header chip; only the section header carries their count.
    #[test]
    fn render_header_has_no_inactive_chip() {
        let theme = Theme::current();
        let mut buf = Buffer::empty(Rect::new(0, 0, 120, 1));
        let mut state = DashboardState::new();
        let rows = vec![
            header_test_row(1, RowState::Inactive, "a"),
            header_test_row(2, RowState::Idle, "b"),
        ];
        render_header(
            &mut buf,
            Rect::new(0, 0, 120, 1),
            &theme,
            &rows,
            &mut state,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains("1 idle"),
            "idle chip must still render, got: {content:?}"
        );
        assert!(
            !content.contains("inactive"),
            "no chip for Inactive rows, got: {content:?}"
        );
    }

    /// The left title is the current location (cwd display), shown with and without agent rows, mirroring the session views' top-bar location line.
    #[test]
    fn render_header_shows_location_label() {
        let theme = Theme::current();

        let area = Rect::new(0, 0, 400, 1);
        let mut state = DashboardState::new();
        let basename = cwd_basename();

        let mut buf = Buffer::empty(area);
        render_header(&mut buf, area, &theme, &[], &mut state, None);
        let c = buf_to_text(&buf);
        assert!(
            c.contains(&basename),
            "0-agent header must show the location (`{basename}`), got: {c:?}"
        );

        // 1 agent.
        let mut buf = Buffer::empty(area);
        let rows = vec![header_test_row(1, RowState::Idle, "x")];
        render_header(&mut buf, area, &theme, &rows, &mut state, None);
        let c = buf_to_text(&buf);
        assert!(
            c.contains(&basename),
            "header must show the location (`{basename}`), got: {c:?}"
        );
    }

    /// On a narrow header the location label truncates against the leftmost chip's separator.
    /// It never paints over the chips or the `[+ New Agent]` button.
    #[test]
    fn render_header_location_label_never_overlaps_chips() {
        let theme = Theme::current();

        let area = Rect::new(0, 0, 70, 1);
        let mut buf = Buffer::empty(area);
        let mut state = DashboardState::new();
        let rows = vec![
            header_test_row(1, RowState::NeedsInput, "a"),
            header_test_row(2, RowState::Working, "b"),
            header_test_row(3, RowState::Idle, "c"),
        ];
        render_header(&mut buf, area, &theme, &rows, &mut state, None);
        let content = buf_to_text(&buf);
        // Chips and button must survive the (long) location label.
        for chunk in ["1 awaiting", "1 working", "1 idle", "[+ New Agent]"] {
            assert!(
                content.contains(chunk),
                "`{chunk}` must not be overpainted by the location label, got: {content:?}",
            );
        }
    }

    /// Footer chips use the shared `ShortcutsBar` styling (`Key:label` separated by ` │ `).
    #[test]
    fn render_footer_uses_shared_shortcuts_bar_styling() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let state = DashboardState::new();
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains(":create") || content.contains(":attach"),
            "footer must use `Key:label` chip format, got: {content:?}",
        );
        assert!(
            content.contains(" \u{2502} "),
            "footer must use ` │ ` separator, got: {content:?}",
        );
    }

    /// Fresh `DashboardState` defaults to button-focused with an empty prompt.
    /// The footer surfaces `Enter:create` (single primary action) and the trailing shortcuts chip.
    /// The ↑/↓ nav chip is no longer shown (dropped to save space), and no send / send+open chip is shown because there's nothing to send.
    #[test]
    fn render_footer_default_compact_hints() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let state = DashboardState::new();
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        for chip in [":create", ":shortcuts"] {
            assert!(
                content.contains(chip),
                "footer must contain `{chip}` chip, got: {content:?}",
            );
        }
        // The nav chip is dropped from the bottom bar to save space.
        assert!(
            !content.contains(":nav"),
            "footer must NOT include the ↑/↓ nav chip, got: {content:?}",
        );

        assert!(
            !content.contains(":send"),
            "empty-prompt footer must NOT include send / send+open chips, \
             got: {content:?}",
        );
    }

    /// The dispatch input grows for multi-line drafts: a single-line prompt wants 1 text row, a 3-line prompt (Shift/Alt+Enter newlines) wants 3.
    /// Growth saturates at the cap so the box never starves the row list.
    #[test]
    fn dispatch_text_rows_grows_with_newlines() {
        let mut state = DashboardState::new();
        let width = 80;
        let height = 30;
        state.dispatch.set_text("just one line");
        assert_eq!(
            dispatch_text_rows(&state, width, height),
            1,
            "single-line prompt wants 1 text row",
        );
        state.dispatch.set_text("line one\nline two\nline three");
        assert_eq!(
            dispatch_text_rows(&state, width, height),
            3,
            "3-line prompt wants 3 text rows",
        );
        // Past the cap ((height/3).clamp(1,8) = 8 here) growth saturates.
        state.dispatch.set_text(&"x\n".repeat(40));
        assert_eq!(
            dispatch_text_rows(&state, width, height),
            8,
            "dispatch text rows saturate at the cap",
        );
    }

    /// Overview list focused (via Tab) with vim on: the nav chip is dropped from the bottom bar to save space.
    /// Neither the vim `j/k` nor the arrow nav is advertised; the action chips (open) remain.
    #[test]
    fn render_footer_list_focused_vim_on_omits_nav() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(0)));
        state.list_focused = true;
        let registry = crate::actions::ActionRegistry::defaults();
        crate::appearance::cache::set_vim_mode(true);
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::Idle),
            false,
            None,
        );
        let content = buf_to_text(&buf);
        // Reset before asserting so a failure doesn't leak vim state into the next test sharing this thread's cache
        crate::appearance::cache::set_vim_mode(false);
        assert!(
            !content.contains(":nav") && !content.contains("j/k"),
            "list-focused footer must omit the nav chip, got: {content:?}",
        );
        assert!(
            content.contains(":open"),
            "list-focused footer keeps the open chip, got: {content:?}",
        );
    }

    /// Overview list focused with vim off: the nav chip is likewise dropped (no arrow nav advertised), saving bottom-bar space for the action chips.
    #[test]
    fn render_footer_list_focused_vim_off_omits_nav() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(0)));
        state.list_focused = true;
        let registry = crate::actions::ActionRegistry::defaults();
        crate::appearance::cache::set_vim_mode(false);
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::Idle),
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            !content.contains(":nav") && !content.contains('\u{2191}'),
            "list-focused footer must omit the nav chip, got: {content:?}",
        );
    }

    #[test]
    fn render_footer_peek_mode_shows_peek_hints() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let state = DashboardState::new();
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::Idle),
            true,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains(":open") && content.contains(":New Agent"),
            "peek-mode footer must include open + New Agent (unselect) hints, got: {content:?}",
        );
        assert!(
            content.contains(":delete"),
            "peek-mode footer must keep the Ctrl+x delete chip, got: {content:?}",
        );
        // The nav chip is dropped to save bottom-bar space.
        assert!(
            !content.contains(":nav"),
            "peek-mode footer must NOT show the nav chip, got: {content:?}",
        );
        assert!(
            !content.contains(":switch"),
            "peek-mode footer must NOT use the old `switch` label, got: {content:?}",
        );
    }

    /// Peek footer flips to send affordances once the reply has text and is focused: `enter:send · ctrl+s:send+open · esc:back`.
    #[test]
    fn render_footer_peek_with_reply_text_shows_send() {
        crate::appearance::cache::set_vim_mode(false);
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.peek = Some(crate::views::dashboard::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(crate::app::agent::AgentId(0)),
            crate::views::dashboard::peek::PeekFields {
                label: "label".into(),
                time_ago: String::new(),
                response_type: "Idle".into(),
                last_user_message: None,
                question: None,
                options: Vec::new(),
                request_id: None,
                reject_option: None,
            },
        ));
        state.peek.as_mut().unwrap().focused = true;
        state.peek_reply.set_text("hi there");
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            true, // peek_active
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains(":send"),
            "peek footer with a typed reply must show `send`, got: {content:?}",
        );
    }

    /// Vim with an unfocused peek: Enter focuses the reply (`input`), not open/send.
    #[test]
    fn render_footer_vim_unfocused_peek_enter_shows_input() {
        crate::appearance::cache::set_vim_mode(true);
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.list_focused = true; // List focus used to steal the footer from the peek
        state.peek = Some(crate::views::dashboard::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(crate::app::agent::AgentId(0)),
            crate::views::dashboard::peek::PeekFields {
                label: "label".into(),
                time_ago: String::new(),
                response_type: "Idle".into(),
                last_user_message: None,
                question: None,
                options: Vec::new(),
                request_id: None,
                reject_option: None,
            },
        ));
        assert!(!state.peek.as_ref().unwrap().focused);
        state.peek_reply.set_text("draft");
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::Idle),
            true,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains(":input"),
            "vim unfocused peek must label Enter as input, got: {content:?}",
        );
        assert!(
            content.contains(":open"),
            "vim unfocused peek must surface Right:open for attach, got: {content:?}",
        );
        assert!(
            content.contains(":back"),
            "non-empty draft must label Esc as back, got: {content:?}",
        );
        crate::appearance::cache::set_vim_mode(false);
    }

    /// Non-vim unfocused peek with a typed draft: Esc clears the draft first (`back`), not New Agent.
    #[test]
    fn render_footer_peek_unfocused_with_draft_esc_is_back() {
        crate::appearance::cache::set_vim_mode(false);
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        let mut peek = crate::views::dashboard::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(crate::app::agent::AgentId(0)),
            crate::views::dashboard::peek::PeekFields {
                label: "label".into(),
                time_ago: String::new(),
                response_type: "Idle".into(),
                last_user_message: None,
                question: None,
                options: Vec::new(),
                request_id: None,
                reject_option: None,
            },
        );
        peek.focused = false;
        state.peek = Some(peek);
        state.peek_reply.set_text("draft");
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::Idle),
            true,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains(":back"),
            "unfocused peek with draft must show Esc:back, got: {content:?}",
        );
        assert!(
            !content.contains("New Agent"),
            "must not label Esc as New Agent while a draft remains, got: {content:?}",
        );
    }

    /// A pending question is an ANSWER surface only when focused AND an option is selected.
    /// Focused and selected shows `enter:answer` (and Tab `list`).
    /// Focused with no selection shows `enter:open` and `1-9:select`; unfocused still keeps `1-9:select` (digits work).
    /// None of the non-answer states show `answer`.
    #[test]
    fn render_footer_peek_question_focus_flips_answer_vs_open() {
        crate::appearance::cache::set_vim_mode(false);
        let theme = Theme::current();
        let registry = crate::actions::ActionRegistry::defaults();
        let make_state = |focused: bool, selected: Option<usize>| {
            let mut state = DashboardState::new();
            let mut peek = crate::views::dashboard::peek::PeekPanelState::new(
                DashboardRowId::TopLevel(crate::app::agent::AgentId(0)),
                crate::views::dashboard::peek::PeekFields {
                    label: "label".into(),
                    time_ago: String::new(),
                    response_type: "NeedsInput".into(),
                    last_user_message: None,
                    question: Some("Allow?".into()),
                    options: vec![
                        ("allow".into(), "Allow".into()),
                        ("deny".into(), "Deny".into()),
                    ],
                    request_id: None,
                    reject_option: None,
                },
            );
            peek.focused = focused;
            peek.selected_option = selected;
            state.peek = Some(peek);
            state
        };
        let render = |state: &DashboardState| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
            render_footer(
                &mut buf,
                Rect::new(0, 0, 200, 1),
                &theme,
                state,
                &registry,
                None,
                true, // peek_active
                None,
            );
            buf_to_text(&buf)
        };

        // Focused with an option selected: the answer surface (and Tab `list`)
        let answering = render(&make_state(true, Some(0)));
        assert!(
            answering.contains(":answer"),
            "focused + selected footer must show `answer`, got: {answering:?}",
        );
        assert!(
            answering.contains(":list"),
            "answer footer must surface the Tab `list` hint, got: {answering:?}",
        );

        // Focused with nothing selected: the navigation surface, open and `1-9 select`
        let picking = render(&make_state(true, None));
        assert!(
            picking.contains(":open"),
            "focused + no selection footer must show `open`, got: {picking:?}",
        );
        assert!(
            picking.contains(":select"),
            "focused + no selection footer must surface the `1-9 select` hint, got: {picking:?}",
        );
        assert!(
            !picking.contains(":answer"),
            "no-selection footer must NOT show `answer`, got: {picking:?}",
        );

        // Unfocused: Enter opens; 1-9 select still shown (digits work)
        let unfocused = render(&make_state(false, None));
        assert!(
            unfocused.contains(":open"),
            "unfocused question footer must show `open`, got: {unfocused:?}",
        );
        assert!(
            unfocused.contains(":select"),
            "unfocused question footer must keep 1-9 select, got: {unfocused:?}",
        );
        assert!(
            !unfocused.contains(":answer"),
            "unfocused question footer must NOT show `answer`, got: {unfocused:?}",
        );

        // Vim unfocused with a question: Enter:input, Right:open, still 1-9 select
        crate::appearance::cache::set_vim_mode(true);
        let mut vim_q = make_state(false, None);
        // Rebuild under vim so focused defaults false.
        vim_q.peek = Some({
            let mut peek = crate::views::dashboard::peek::PeekPanelState::new(
                DashboardRowId::TopLevel(crate::app::agent::AgentId(0)),
                crate::views::dashboard::peek::PeekFields {
                    label: "label".into(),
                    time_ago: String::new(),
                    response_type: "NeedsInput".into(),
                    last_user_message: None,
                    question: Some("Allow?".into()),
                    options: vec![
                        ("allow".into(), "Allow".into()),
                        ("deny".into(), "Deny".into()),
                    ],
                    request_id: None,
                    reject_option: None,
                },
            );
            peek.focused = false;
            peek
        });
        let vim_unfocused = render(&vim_q);
        assert!(
            vim_unfocused.contains(":input"),
            "vim unfocused question must label Enter as input, got: {vim_unfocused:?}",
        );
        assert!(
            vim_unfocused.contains(":select"),
            "vim unfocused question must keep 1-9 select, got: {vim_unfocused:?}",
        );
        crate::appearance::cache::set_vim_mode(false);
    }

    /// When a row (NeedsInput or otherwise) is selected with an empty prompt, the footer shows `Enter:open`.
    /// The previous `see details` label is gone: a selected row always opens.
    /// Every row's detail view is the answer surface for any user-input state, including `NeedsInput`.
    #[test]
    fn render_footer_row_selected_empty_prompt_shows_enter_open() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(0)));
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::NeedsInput),
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains(":open"),
            "row-selected + empty prompt footer must hint `Enter:open`, got: {content:?}",
        );
        assert!(
            !content.contains(":send"),
            "empty-prompt footer must NOT include `:send` chip, got: {content:?}",
        );
        // This mirrors the `[+ New Agent]` button's empty-prompt chip: Tab hands focus to the list
        assert!(
            content.contains(":list"),
            "row-selected + empty prompt footer must hint `Tab:list`, got: {content:?}",
        );
    }

    #[test]
    fn render_footer_inactive_row_shows_delete() {
        let theme = Theme::current();
        let registry = crate::actions::ActionRegistry::defaults();

        // Input focused, row selected.
        let mut state = DashboardState::new();
        state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(0)));
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::Inactive),
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains(":delete"),
            "list-focused idle-row footer must show the delete chip, got: {content:?}",
        );
        assert!(
            content.contains(":open"),
            "list-focused idle-row footer must show the open chip, got: {content:?}",
        );

        state.list_focused = true;
        let mut buf2 = Buffer::empty(Rect::new(0, 0, 200, 1));
        render_footer(
            &mut buf2,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::Inactive),
            false,
            None,
        );
        let content2 = buf_to_text(&buf2);
        assert!(content2.contains(":delete"), "{content2:?}");

        state.list_focused = false;
        let mut buf3 = Buffer::empty(Rect::new(0, 0, 200, 1));
        render_footer(
            &mut buf3,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::Idle),
            false,
            None,
        );
        let content3 = buf_to_text(&buf3);
        assert!(content3.contains(":delete"), "{content3:?}");
    }

    #[test]
    fn render_footer_stop_label_follows_state() {
        let theme = Theme::current();
        let registry = crate::actions::ActionRegistry::defaults();
        let mut state = DashboardState::new();
        state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(0)));

        // Working: `stop`
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::Working),
            false,
            None,
        );
        let working = buf_to_text(&buf);
        assert!(
            working.contains(":stop") && !working.contains(":close"),
            "Working agent footer must label Ctrl+x as `stop`, got: {working:?}",
        );

        // NeedsInput: `stop` (a paused-but-running turn; first Ctrl+x cancels, mirroring `dispatch_dashboard_stop`)
        let mut buf_ni = Buffer::empty(Rect::new(0, 0, 200, 1));
        render_footer(
            &mut buf_ni,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::NeedsInput),
            false,
            None,
        );
        let needs_input = buf_to_text(&buf_ni);
        assert!(
            needs_input.contains(":stop") && !needs_input.contains(":close"),
            "NeedsInput agent footer must label Ctrl+x as `stop`, got: {needs_input:?}",
        );

        // Idle: `delete`
        let mut buf2 = Buffer::empty(Rect::new(0, 0, 200, 1));
        render_footer(
            &mut buf2,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::Idle),
            false,
            None,
        );
        let idle = buf_to_text(&buf2);
        assert!(
            idle.contains(":delete") && !idle.contains(":stop"),
            "Idle agent footer must label Ctrl+x as `delete`, got: {idle:?}",
        );
    }

    /// When a section header is selected the footer shows the toggle (Enter:collapse / :expand) and `Esc:New Agent`.
    /// It omits the stop chip (no session under a header).
    #[test]
    fn render_footer_section_selected_shows_toggle_no_stop() {
        let theme = Theme::current();
        let registry = crate::actions::ActionRegistry::defaults();

        // Expanded section: Enter collapses
        let mut state = DashboardState::new();
        state.focus_section(SectionKey::State(RowState::Working));
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains(":collapse"),
            "expanded section footer must hint Enter:collapse, got: {content:?}",
        );
        assert!(
            content.contains("New Agent"),
            "section footer must hint Esc:New Agent, got: {content:?}",
        );
        assert!(
            !content.contains(":stop") && !content.contains(":close"),
            "section footer must NOT show the stop chip, got: {content:?}",
        );

        // Collapsed section: the toggle label flips to expand
        state.set_section_collapsed(SectionKey::State(RowState::Working), true);
        let mut buf2 = Buffer::empty(Rect::new(0, 0, 200, 1));
        render_footer(
            &mut buf2,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content2 = buf_to_text(&buf2);
        assert!(
            content2.contains(":expand"),
            "collapsed section footer must hint Enter:expand, got: {content2:?}",
        );
    }

    /// With a section header selected and the LIST focused (Tab), the footer shows the section's own hints (collapse / Tab:input).
    /// The generic row chips (`open` / `stop`) would lie: Enter toggles the section, and there's no session to stop.
    #[test]
    fn render_footer_list_focused_section_shows_toggle() {
        let theme = Theme::current();
        let registry = crate::actions::ActionRegistry::defaults();
        let mut state = DashboardState::new();
        state.focus_section(SectionKey::State(RowState::Working));
        state.list_focused = true;
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains(":collapse"),
            "list-focused section footer must hint Enter:collapse, got: {content:?}",
        );
        assert!(
            content.contains(":input"),
            "list-focused section footer must hint Tab:input, got: {content:?}",
        );
        assert!(
            !content.contains(":open") && !content.contains(":stop") && !content.contains(":close"),
            "list-focused section footer must NOT show open/stop chips, got: {content:?}",
        );
        assert!(
            content.contains(":shortcuts"),
            "list-focused section footer keeps the shortcuts chip, got: {content:?}",
        );
    }

    /// Section header selected with typed text in the (focused) dispatch input: the draft dispatches a NEW agent.
    /// A section header is never a reply target.
    /// The footer flips to the dispatch chips: send / send+open / mode.
    /// The collapse toggle is gone (it only fires on an empty prompt).
    #[test]
    fn render_footer_section_selected_with_prompt_shows_dispatch_chips() {
        let theme = Theme::current();
        let registry = crate::actions::ActionRegistry::defaults();
        let mut state = DashboardState::new();
        state.focus_section(SectionKey::State(RowState::Working));
        state.dispatch.set_text("kick off a fresh session");
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains(":send") && content.contains(":send+open"),
            "section + typed prompt footer must hint send / send+open, got: {content:?}",
        );
        assert!(
            content.contains(":mode"),
            "section + typed prompt footer must hint Shift+Tab:mode, got: {content:?}",
        );
        assert!(
            !content.contains(":collapse") && !content.contains(":expand"),
            "section + typed prompt footer must NOT show the toggle chip, got: {content:?}",
        );
        assert!(
            !content.contains(":stop") && !content.contains(":close"),
            "section footer never shows the stop chip, got: {content:?}",
        );
    }

    /// Rename mode shows only save and cancel actions.
    #[test]
    fn render_footer_rename_shows_save_and_cancel() {
        use crate::app::agent::AgentId;
        let theme = Theme::current();
        let registry = crate::actions::ActionRegistry::defaults();
        let mut state = DashboardState::new();
        let id = DashboardRowId::TopLevel(AgentId(0));
        state.focus_row(id.clone());
        state.rename = Some(RenameDraft::new(id, ""));
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains(":save"),
            "rename footer must hint Enter:save, got: {content:?}",
        );
        assert!(
            content.contains(":cancel"),
            "rename footer must hint Esc:cancel, got: {content:?}",
        );
        assert!(
            !content.contains(":stop") && !content.contains(":close") && !content.contains(":open"),
            "rename footer must hide the normal nav/stop chips, got: {content:?}",
        );
    }

    /// When a row is selected AND the user has typed, Enter sends (reply, stays on dashboard) and Ctrl+S sends and opens detail.
    /// The footer surfaces both chips so the chord is discoverable.
    #[test]
    fn render_footer_row_selected_with_prompt_shows_send_and_send_open() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(0)));
        state.dispatch.set_text("reply text");
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            Some(RowState::Idle),
            false,
            None,
        );
        let content = buf_to_text(&buf);
        for chip in [":send", ":send+open"] {
            assert!(
                content.contains(chip),
                "row-selected + non-empty footer must contain `{chip}`, got: {content:?}",
            );
        }
        assert!(
            !content.contains(":open  "),
            "send/send+open footer must NOT include the empty-prompt `:open` chip, \
             got: {content:?}",
        );
    }

    /// When the `[+ New Agent]` button is focused AND the user has typed, Enter sends (stays on dashboard) and Ctrl+S sends and opens detail.
    /// The stop chip is suppressed because the button has no underlying session to close.
    #[test]
    fn render_footer_button_focused_with_prompt_shows_send_and_send_open_no_stop() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        // Default state: button focused. Plant typed text.
        assert!(state.new_agent_button_focused);
        state.dispatch.set_text("kick off a fresh session");
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        for chip in [":send", ":send+open"] {
            assert!(
                content.contains(chip),
                "button-focused + non-empty footer must contain `{chip}`, \
                 got: {content:?}",
            );
        }
        assert!(
            content.contains("Enter:send"),
            "default compose: bare Enter is send, got: {content:?}",
        );
        assert!(
            !content.contains(":stop") && !content.contains(":close"),
            "button-focused footer must NOT show the stop chip (no row to close), \
             got: {content:?}",
        );
    }

    /// Multiline compose swaps the submit chord in the footer, matching how Enter and Shift/Alt+Enter swap in the agent view's keybar.
    #[test]
    fn render_footer_multiline_mode_send_uses_shift_or_alt_enter() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.multiline_mode = true;
        state.dispatch.set_text("multi\nline draft");
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains("Shift+Enter:send")
                || content.contains("Alt+Enter:send")
                || content.contains("Opt+Enter:send"),
            "multiline footer must advertise Shift/Alt/Opt+Enter as send, got: {content:?}",
        );
        // Bare Enter:send would appear as "  Enter:send" (footer pad); the modified chords contain the substring "Enter:send" so avoid that
        assert!(
            !content.contains("  Enter:send"),
            "multiline footer must not claim bare Enter:send, got: {content:?}",
        );
        assert!(
            content.contains(":send+open"),
            "Ctrl+S send+open must remain, got: {content:?}",
        );
    }

    /// Empty draft under multiline: create is on the submit chord, not bare Enter.
    #[test]
    fn render_footer_multiline_empty_create_uses_shift_or_alt_enter() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.multiline_mode = true;
        assert!(state.new_agent_button_focused);
        assert!(state.dispatch.text().trim().is_empty());
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.contains("Shift+Enter:create")
                || content.contains("Alt+Enter:create")
                || content.contains("Opt+Enter:create"),
            "multiline empty footer must advertise Shift/Alt/Opt+Enter as create, got: {content:?}",
        );
        assert!(
            !content.contains("  Enter:create"),
            "multiline empty footer must not claim bare Enter:create, got: {content:?}",
        );
    }

    /// Delete-confirm armed while the input is focused routes through `ShortcutsBar::with_pending` ("press Ctrl+x again to delete").
    #[test]
    fn render_footer_delete_confirm_uses_pending_hint() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.arm_delete(DashboardRowId::TopLevel(crate::app::agent::AgentId(1)));
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            content.to_lowercase().contains("press again"),
            "stop-confirm footer must say `press again`, got: {content:?}",
        );
        assert!(
            content.to_lowercase().contains("delete this session"),
            "delete-confirm footer must name the action, got: {content:?}",
        );
    }

    /// An EXPIRED delete-confirm (older than `CONFIRM_WINDOW`) must not claim the footer.
    /// The dispatcher would re-arm rather than delete on the next press, so "press again" would lie.
    /// Regular hints render instead (e.g. after a mouse click moved the selection without a keypress to disarm the confirm).
    #[test]
    fn render_footer_expired_delete_confirm_shows_regular_hints() {
        use std::time::{Duration, Instant};
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        let theme = Theme::current();
        let mut state = DashboardState::new();
        state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(1)));
        state.delete_confirm = Some((
            DashboardRowId::TopLevel(crate::app::agent::AgentId(1)),
            Instant::now() - (super::super::state::CONFIRM_WINDOW + Duration::from_secs(1)),
        ));
        let registry = crate::actions::ActionRegistry::defaults();
        render_footer(
            &mut buf,
            Rect::new(0, 0, 200, 1),
            &theme,
            &state,
            &registry,
            None,
            false,
            None,
        );
        let content = buf_to_text(&buf);
        assert!(
            !content.to_lowercase().contains("press again"),
            "expired stop-confirm must not paint the pending hint, got: {content:?}",
        );
        assert!(
            content.contains(":open"),
            "expired stop-confirm must fall back to the regular hints, got: {content:?}",
        );
    }

    /// Subagents inherit their parent's state and must NOT inflate the header chip tallies.
    /// The header counts top-level rows only.
    #[test]
    fn render_header_counts_top_level_rows_only() {
        let theme = Theme::current();
        let mut buf = Buffer::empty(Rect::new(0, 0, 160, 1));
        let mut state = DashboardState::new();
        let parent = DashboardRow {
            indent: 0,
            ..header_test_row(1, RowState::Working, "parent")
        };
        let sub_completed = DashboardRow {
            id: DashboardRowId::Subagent {
                parent: crate::app::agent::AgentId(1),
                child_session_id: "c1".to_string(),
            },
            indent: 1,
            ..header_test_row(11, RowState::Completed, "child")
        };
        let rows = vec![parent, sub_completed];
        render_header(
            &mut buf,
            Rect::new(0, 0, 160, 1),
            &theme,
            &rows,
            &mut state,
            None,
        );
        let content = buf_to_text(&buf);
        // Only the top-level parent counts: its Working chip shows.
        assert!(
            content.contains("1 working"),
            "expected `1 working` chip for the top-level parent, got: {content:?}"
        );
        // Subagent's Completed must NOT show up as `1 done`.
        assert!(
            !content.contains("1 done"),
            "header must not count subagent state, got: {content:?}",
        );
    }
}
