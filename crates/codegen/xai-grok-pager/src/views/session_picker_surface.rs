use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app::app_view::SessionPickerEntry;
use crate::theme::Theme;
use crate::views::session_picker::{SessionPickerLanes, SourceFilter};

/// Which surface a picker fetch was issued for.
/// Results route back to the requesting host's storage only; a live picker on another host never absorbs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPickerHost {
    /// Welcome-screen picker (`session_picker_*` fields on `AppView`).
    Welcome,
    /// `/resume` modal on the active agent (`ActiveModal::SessionPicker`).
    AgentModal,
    /// Dashboard picker (`AppView::dashboard_session_picker`).
    Dashboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SessionPageControl {
    #[default]
    None,
    More,
    Loading,
    Retry,
}

impl SessionPageControl {
    pub(crate) fn for_picker(lanes: &SessionPickerLanes, source: SourceFilter) -> Self {
        if source == SourceFilter::External {
            Self::None
        } else if lanes.pagination.is_loading() {
            Self::Loading
        } else if lanes.pagination.failed() {
            Self::Retry
        } else if lanes.pagination.has_more() {
            Self::More
        } else {
            Self::None
        }
    }

    pub(crate) fn label(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::More => Some("F5 load more"),
            Self::Loading => Some("Loading more…"),
            Self::Retry => Some("Load failed · F5 retry"),
        }
    }

    /// Handle explicit page requests and forward navigation at the loaded edge.
    /// Other input, including the search editor's End key, stays with the picker.
    pub(crate) fn handle_input(
        self,
        ev: &crossterm::event::Event,
        state: &mut crate::views::picker::PickerState,
        non_selectable: &[bool],
        host: SessionPickerHost,
    ) -> Option<crate::app::app_view::InputOutcome> {
        use crate::app::{actions::Action, app_view::InputOutcome};
        use crossterm::event::{Event, KeyCode, KeyEventKind, MouseEventKind};

        let load_more = || InputOutcome::Action(Action::LoadMoreSessions { host });
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release && key.modifiers.is_empty() => {
                if key.code == KeyCode::F(5) {
                    return Some(if matches!(self, Self::More | Self::Retry) {
                        load_more()
                    } else {
                        InputOutcome::Unchanged
                    });
                }
                if state.search_active || state.tabs_focused {
                    return None;
                }
                let mut following = non_selectable
                    .iter()
                    .enumerate()
                    .skip(state.selected.saturating_add(1))
                    .filter_map(|(i, &skip)| (!skip).then_some(i));
                let target = match key.code {
                    KeyCode::Down | KeyCode::Char('j') if self != Self::None => following.next(),
                    KeyCode::PageDown => {
                        let page_size = state
                            .hit_areas
                            .as_ref()
                            .map_or(1, |hit| hit.entry_indices.len().max(1));
                        following.nth(page_size - 1)
                    }
                    KeyCode::End => None,
                    _ => return None,
                };
                let last = non_selectable.iter().rposition(|&skip| !skip)?;
                let target = target.unwrap_or(last);
                if target != last && matches!(key.code, KeyCode::Down | KeyCode::Char('j')) {
                    return None;
                }
                state.selected = target;
                state.selection_hidden = false;
                state.hovered = None;
                state.scroll_offset = None;
                Some(if target == last && self == Self::More {
                    load_more()
                } else {
                    InputOutcome::Changed
                })
            }
            Event::Mouse(mouse)
                if self == Self::More && mouse.kind == MouseEventKind::ScrollDown =>
            {
                let hit = state.hit_areas.as_ref()?;
                let last = non_selectable.iter().rposition(|&skip| !skip)?;
                let pos = ratatui::layout::Position::new(mouse.column, mouse.row);
                (hit.entry_indices.contains(&last)
                    && hit.item_rects.iter().any(|r| r.contains(pos)))
                .then(load_more)
            }
            _ => None,
        }
    }
}

/// Shared by dashboard picker paint and input so a copy change cannot update only one site.
pub(crate) const DASHBOARD_PICKER_TITLE: &str = "Open session";

/// State for one session-picker incarnation.
/// Host-agnostic: everything a picker accumulates between open and dismiss, nothing about how a host renders it or maps its keys.
#[derive(Debug)]
pub struct SessionPickerSurface {
    /// Incarnation identity; results apply only when it matches.
    pub generation: u64,
    pub state: crate::views::picker::PickerState,
    pub window: crate::views::modal_window::ModalWindowState,
    pub entries: Option<Vec<crate::app::app_view::SessionPickerEntry>>,
    pub loading: bool,
    pub lanes: crate::views::session_picker::SessionPickerLanes,
    pub content_results: Option<Vec<xai_grok_shell::extensions::session_search::SearchSessionHit>>,
    pub content_loading: bool,
    /// Per-surface counters; the dashboard host does not share the welcome picker's `session_picker_list_seq` / `session_picker_deep_search_seq`.
    pub list_seq: u64,
    pub deep_search_seq: u64,
    /// Invalidates in-flight card-detail reads when this surface's rows or filters change.
    pub detail_seq: u64,
    pub entries_query: Option<String>,
    pub source_filter: crate::views::session_picker::SourceFilter,
    pub pending_delete: Option<crate::views::session_picker::PendingDelete>,
}

impl SessionPickerSurface {
    #[must_use]
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            state: crate::views::picker::PickerState::default(),
            window: crate::views::modal_window::ModalWindowState::new(),
            entries: None,
            loading: false,
            lanes: Default::default(),
            content_results: None,
            content_loading: false,
            list_seq: 0,
            deep_search_seq: 0,
            detail_seq: 0,
            entries_query: None,
            source_filter: Default::default(),
            pending_delete: None,
        }
    }
}

pub(crate) enum SessionPickerRenderMode<'a> {
    Fullscreen,
    Modal {
        window: &'a mut crate::views::modal_window::ModalWindowState,
        title: &'a str,
    },
}

pub(crate) struct SessionPickerRenderCtx<'a> {
    pub(crate) state: &'a mut crate::views::picker::PickerState,
    pub(crate) sessions: Option<&'a [SessionPickerEntry]>,
    pub(crate) cwd: &'a std::path::Path,
    pub(crate) loading: bool,
    pub(crate) page_control: SessionPageControl,
    pub(crate) pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
    pub(crate) shortcuts_area: Option<Rect>,
    pub(crate) content_results:
        Option<&'a [xai_grok_shell::extensions::session_search::SearchSessionHit]>,
    pub(crate) content_loading: bool,
    pub(crate) entries_query: Option<&'a str>,
    pub(crate) tick: u64,
    pub(crate) grouped: bool,
    pub(crate) source_filter: crate::views::session_picker::SourceFilter,
    #[allow(dead_code)]
    pub(crate) pending_delete: bool,
    pub(crate) chat_mode: bool,
}

pub(crate) fn render_session_picker(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    mode: SessionPickerRenderMode<'_>,
    ctx: &mut SessionPickerRenderCtx<'_>,
) -> crate::views::picker::PickerHitAreas {
    match mode {
        SessionPickerRenderMode::Fullscreen => crate::views::welcome::render_session_picker(
            area,
            buf,
            theme,
            &mut crate::views::welcome::SessionPickerRenderCtx {
                state: ctx.state,
                sessions: ctx.sessions,
                cwd: ctx.cwd,
                loading: ctx.loading,
                page_control: ctx.page_control,
                pending_hint: ctx.pending_hint,
                shortcuts_area: ctx.shortcuts_area,
                content_results: ctx.content_results,
                content_loading: ctx.content_loading,
                entries_query: ctx.entries_query,
                tick: ctx.tick,
                grouped: ctx.grouped,
                source_filter: ctx.source_filter,
                chat_mode: ctx.chat_mode,
            },
        ),
        SessionPickerRenderMode::Modal { window, title } => {
            render_simple_session_picker_modal(area, buf, theme, window, title, ctx)
        }
    }
}

const SESSION_SEARCH_LABEL: &str = " search: ";

pub(crate) fn render_session_picker_search_bar(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    state: &crate::views::picker::PickerState,
) {
    crate::views::picker::render_picker_search_bar_with_label(
        buf,
        area.x,
        area.y,
        area.width,
        theme,
        SESSION_SEARCH_LABEL,
        state,
        state.search_active,
        true,
        Some(theme.bg_base),
    );
    let label_w = u16::try_from(SESSION_SEARCH_LABEL.len()).unwrap_or(0);
    if state.search_active {
        let width = label_w.min(area.width);
        if width > 0 {
            buf.set_style(
                Rect::new(area.x, area.y, width, 1),
                ratatui::style::Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            );
        }
    } else if !state.query().is_empty() {
        let start = area.x.saturating_add(label_w);
        let width = area.x.saturating_add(area.width).saturating_sub(start);
        if width > 0 {
            buf.set_style(Rect::new(start, area.y, width, 1), theme.dim());
        }
    }
}

fn empty_hit_areas() -> crate::views::picker::PickerHitAreas {
    crate::views::picker::PickerHitAreas {
        close_button: Rect::default(),
        search_bar: Rect::default(),
        item_rects: vec![],
        entry_indices: vec![],
        tab_rects: vec![],
        filter_rect: None,
    }
}

fn render_simple_session_picker_modal(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    window: &mut crate::views::modal_window::ModalWindowState,
    title: &str,
    ctx: &mut SessionPickerRenderCtx<'_>,
) -> crate::views::picker::PickerHitAreas {
    use crate::views::modal_window::{ModalSizing, ModalWindowConfig, Shortcut};
    use crate::views::picker::{self, PickerField};

    let mut shortcuts = vec![
        Shortcut {
            label: "\u{2191}\u{2193} nav",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Enter select",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "/ search",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Esc close",
            clickable: false,
            id: 0,
        },
    ];
    if let Some(label) = ctx.page_control.label() {
        shortcuts.push(Shortcut {
            label,
            clickable: false,
            id: 0,
        });
    }
    let modal_config = ModalWindowConfig {
        title,
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing {
            width_pct: 0.65,
            max_width: 120,
            min_width: 48,
            v_margin: 4,
            h_pad: 2,
            v_pad: 1,
            footer_lines: 2,
        },
        fold_info: None,
    };
    let Some(modal) =
        crate::views::modal_window::render_modal_window(buf, area, window, &modal_config, theme)
    else {
        ctx.state.hit_areas = None;
        return empty_hit_areas();
    };

    let content = modal.content;
    render_session_picker_search_bar(
        buf,
        Rect::new(content.x, content.y, content.width, 1),
        theme,
        ctx.state,
    );
    ctx.state.filter_area = None;
    let separator_y = content.y + 1;
    if separator_y < content.y + content.height {
        picker::render_divider(
            buf,
            modal.inner_x,
            separator_y,
            modal.inner_width,
            theme,
            Some(theme.bg_base),
        );
    }

    let query =
        crate::views::session_picker::effective_filter_query(ctx.state.query(), ctx.entries_query);
    let sessions = ctx.sessions.unwrap_or(&[]);
    let filtered =
        crate::app::app_view::filter_session_entries(ctx.sessions, query, ctx.source_filter);
    let built = crate::views::session_picker::build_session_entry_data(
        sessions,
        &filtered,
        ctx.state,
        content.width,
    );
    let fields: Vec<Vec<PickerField<'_>>> = built
        .iter()
        .map(|entry| {
            entry
                .field_data
                .iter()
                .map(|(label, value)| PickerField { label, value })
                .collect()
        })
        .collect();
    let current_repo = crate::views::session_picker::repo_name_from_cwd(&ctx.cwd.to_string_lossy());
    let (entries, non_selectable) = crate::views::session_picker::build_grouped_picker_entries(
        sessions,
        &filtered,
        &built,
        &fields,
        ctx.state,
        Some(current_repo.as_str()),
    );
    let entries_area = Rect {
        x: content.x,
        y: separator_y + 1,
        width: content.width,
        height: content
            .height
            .saturating_sub(separator_y.saturating_add(1).saturating_sub(content.y)),
    };
    let content_hit = picker::render_picker_content_with_scrollbar_x(
        buf,
        entries_area,
        theme,
        ctx.state,
        &entries,
        &non_selectable,
        &[],
        Some(theme.bg_base),
        ctx.loading,
        ctx.tick,
        modal.inner_x + modal.inner_width - 1,
    );
    crate::views::picker::PickerHitAreas {
        close_button: Rect::default(),
        search_bar: Rect::new(content.x, content.y, content.width, 1),
        item_rects: content_hit.item_rects,
        entry_indices: content_hit.entry_indices,
        tab_rects: vec![],
        filter_rect: None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;

    use super::{
        SessionPageControl, SessionPickerHost, empty_hit_areas, render_session_picker_search_bar,
    };
    use crate::app::{actions::Action, app_view::InputOutcome};
    use crate::theme::Theme;
    use crate::views::picker::PickerState;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn loads_page(input: Option<InputOutcome>, expected: SessionPickerHost) -> bool {
        matches!(input, Some(InputOutcome::Action(Action::LoadMoreSessions { host })) if host == expected)
    }

    #[test]
    fn session_pagination_navigates_past_headers_and_keeps_the_last_row_focused() {
        let host = SessionPickerHost::AgentModal;
        let more = SessionPageControl::More;
        let rows = [true, false, true, false, false, true, false, true];
        let mut state = PickerState::default();
        state.selected = 1;
        let mut hit = empty_hit_areas();
        hit.entry_indices = vec![1, 3];
        state.hit_areas = Some(hit);
        assert!(matches!(
            more.handle_input(&key(KeyCode::PageDown), &mut state, &rows, host),
            Some(InputOutcome::Changed)
        ));
        assert_eq!(state.selected, 4);
        assert!(loads_page(
            more.handle_input(&key(KeyCode::Down), &mut state, &rows, host),
            host
        ));
        assert_eq!(state.selected, 6);
        assert!(matches!(
            SessionPageControl::Loading.handle_input(&key(KeyCode::Down), &mut state, &rows, host),
            Some(InputOutcome::Changed)
        ));
        assert_eq!(state.selected, 6);
        assert!(!state.search_active);
        state.selected = 1;
        assert!(loads_page(
            more.handle_input(&key(KeyCode::End), &mut state, &rows, host),
            host
        ));
        assert_eq!(state.selected, 6);
    }

    #[test]
    fn session_pagination_explicitly_retries_empty_results_during_search() {
        let host = SessionPickerHost::Welcome;
        let mut state = PickerState::input_active();
        state.set_query("older");
        for control in [SessionPageControl::More, SessionPageControl::Retry] {
            assert!(loads_page(
                control.handle_input(&key(KeyCode::F(5)), &mut state, &[], host),
                host
            ));
            assert!(
                control
                    .handle_input(&key(KeyCode::End), &mut state, &[], host)
                    .is_none()
            );
        }
        assert!(matches!(
            SessionPageControl::Loading.handle_input(&key(KeyCode::F(5)), &mut state, &[], host),
            Some(InputOutcome::Unchanged)
        ));
        assert_eq!(state.query(), "older");
        assert!(state.search_active);
    }

    #[test]
    fn session_pagination_scrolls_only_at_the_visible_bottom_of_the_list() {
        let host = SessionPickerHost::Dashboard;
        let rows = [true, false, false];
        let mut hit = empty_hit_areas();
        hit.entry_indices = vec![1];
        hit.item_rects = vec![Rect::new(5, 4, 24, 1)];
        let mut state = PickerState::default();
        state.hit_areas = Some(hit);
        let scroll = |row| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        let more = SessionPageControl::More;
        assert!(
            more.handle_input(&scroll(4), &mut state, &rows, host)
                .is_none()
        );
        state.hit_areas.as_mut().unwrap().entry_indices = vec![2];
        assert!(loads_page(
            more.handle_input(&scroll(4), &mut state, &rows, host),
            host
        ));
        assert!(
            more.handle_input(&scroll(1), &mut state, &rows, host)
                .is_none()
        );
    }

    fn row_text(buf: &Buffer) -> String {
        (0..buf.area.width).fold(String::new(), |mut text, x| {
            text.push_str(buf[(x, 0)].symbol());
            text
        })
    }

    #[test]
    fn inactive_nonempty_query_keeps_search_label_without_caret() {
        let theme = Theme::tokyonight();
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let mut state = crate::views::picker::PickerState::default();
        state.search_active = false;
        state.set_query("alpha");
        render_session_picker_search_bar(&mut buf, area, &theme, &state);
        let text = row_text(&buf);
        assert!(
            text.contains(" search:"),
            "idle query must keep the search label, got {text:?}"
        );
        assert!(
            !text.contains(">search:"),
            "idle query must not use the editing marker, got {text:?}"
        );
        assert!(
            text.contains("alpha"),
            "idle query must keep the typed text, got {text:?}"
        );
        let caret = (0..buf.area.width).any(|x| buf[(x, 0)].bg == theme.text_primary);
        assert!(!caret, "idle query must not paint a caret, got {text:?}");
    }

    #[test]
    fn focused_search_label_uses_the_title_color() {
        let theme = Theme::tokyonight();
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let mut state = crate::views::picker::PickerState::default();
        state.search_active = true;
        render_session_picker_search_bar(&mut buf, area, &theme, &state);
        let text = row_text(&buf);
        assert!(text.contains(" search:"), "{text:?}");
        assert!(!text.contains('>'), "{text:?}");
        let labeled = (0..buf.area.width).any(|x| {
            let cell = &buf[(x, 0)];
            cell.symbol() == "s"
                && cell.fg == theme.text_primary
                && cell.modifier.contains(Modifier::BOLD)
        });
        assert!(labeled, "focused label must match the modal title color");
    }
}
