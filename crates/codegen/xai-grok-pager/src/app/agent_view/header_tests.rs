//! The session header row: `branch worktree path` on the left,
//! `‹ i/n › │ [Dashboard]` on the right. Inside the dashboard overlay the
//! agent's title leads the row (`title │ branch …`); there is no separate
//! title band.
use super::{AgentView, AppRenderParams, BannerSlotParams, OverlayHeader, test_fixtures};
use crate::actions::ActionRegistry;
use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crate::app::bundle::BundleState;
use crate::scrollback::render::ScratchBuffer;
use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

const PATH: &str = "/grok-header-marker";

fn agent_at(width: u16) -> AgentView {
    let mut agent = test_fixtures::make_agent();
    agent.last_terminal_size = (width, 30);
    agent.session.cwd = std::path::PathBuf::from(PATH);
    agent.set_dashboard_visible(true);
    agent
}

fn draw(
    agent: &mut AgentView,
    registry: &ActionRegistry,
    in_overlay: bool,
    header: OverlayHeader<'_>,
) -> Buffer {
    let (width, height) = agent.last_terminal_size;
    let area = Rect::new(0, 0, width, height);
    let bundle = BundleState::default();
    let mut buf = Buffer::empty(area);
    let mut scratch = ScratchBuffer::new();
    agent.draw(
        area,
        &mut buf,
        registry,
        &mut scratch,
        None,
        false,
        BannerSlotParams::none(),
        &bundle,
        in_overlay,
        header.can_cycle(),
        &mut Vec::new(),
        AppRenderParams {
            overlay_header: header,
            ..AppRenderParams::default()
        },
    );
    buf
}

fn fg(buf: &Buffer, x: u16, y: u16) -> ratatui::style::Color {
    buf.cell((x, y))
        .unwrap_or_else(|| panic!("missing cell ({x},{y})"))
        .fg
}

fn text_at(buf: &Buffer, rect: Rect) -> String {
    (rect.x..rect.x + rect.width)
        .filter_map(|x| buf.cell((x, rect.y)).map(|c| c.symbol().to_string()))
        .collect()
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
        .collect()
}

fn header_row(agent: &AgentView, buf: &Buffer) -> String {
    let y = agent
        .hit_dashboard
        .rect
        .or(agent.hit_cwd.rect)
        .map(|r| r.y)
        .expect("status bar paints a cwd or [Dashboard] hit");
    row_text(buf, y)
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

fn click(column: u16, row: u16) -> Event {
    mouse(MouseEventKind::Down(MouseButton::Left), column, row)
}

fn switcher_text(cur: usize, total: usize) -> String {
    format!(
        "{} {cur}/{total} {}",
        crate::glyphs::chevron_left(),
        crate::glyphs::chevron()
    )
}

#[test]
fn worktree_session_header_keeps_badge_and_omits_main_repo_suffix() {
    let _theme = crate::theme::cache::pin_theme();
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    agent.is_worktree = true;
    agent.main_repo = Some("~/xai".into());
    let buf = draw(&mut agent, &registry, false, OverlayHeader::default());
    let row = header_row(&agent, &buf);
    assert!(row.contains("worktree"), "row = {row:?}");
    assert!(row.contains(PATH), "row = {row:?}");
    assert!(
        !row.contains("(worktree of"),
        "no leftover main-repo suffix, row = {row:?}"
    );
}

#[test]
fn session_header_always_shortens_deep_cwd() {
    let _theme = crate::theme::cache::pin_theme();
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    agent.session.cwd = std::path::PathBuf::from("/deep/alpha/bravo/charlie/delta");
    let buf = draw(&mut agent, &registry, false, OverlayHeader::default());
    let row = header_row(&agent, &buf);
    assert!(
        row.contains("/d/a/b/charlie/delta"),
        "always last-two shortening, row = {row:?}"
    );
    assert!(
        !row.contains("/deep/alpha"),
        "middle components must not stay full, row = {row:?}"
    );
    assert!(
        !row.contains("(worktree of"),
        "no leftover main-repo suffix, row = {row:?}"
    );
}

#[test]
fn plain_session_header_has_path_and_dashboard_button_only() {
    let _theme = crate::theme::cache::pin_theme();
    let theme = crate::theme::Theme::current();
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    let buf = draw(&mut agent, &registry, false, OverlayHeader::default());
    let row = header_row(&agent, &buf);
    let y = agent.hit_dashboard.rect.unwrap().y;
    assert!(row.contains(PATH), "row = {row:?}");
    assert!(row.contains("[Dashboard]"), "row = {row:?}");
    assert!(
        !row.contains(crate::glyphs::chevron_left()) && !row.contains(crate::glyphs::chevron()),
        "no switcher in a plain session, row = {row:?}"
    );
    assert!(agent.hit_overlay_prev.rect.is_none() && agent.hit_overlay_next.rect.is_none());
    let path_x = row.find(PATH).unwrap() as u16;
    assert_eq!(fg(&buf, path_x, y), theme.text_secondary);
    let dash = agent.hit_dashboard.rect.unwrap();
    assert!(matches!(
        agent.handle_input(&click(dash.x, dash.y), &registry),
        InputOutcome::Action(Action::OpenDashboard)
    ));
}

#[test]
fn dashboard_button_follows_the_feature_gate_outside_the_overlay() {
    let _theme = crate::theme::cache::pin_theme();
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    agent.set_dashboard_visible(false);
    let buf = draw(&mut agent, &registry, false, OverlayHeader::default());
    assert!(agent.hit_dashboard.rect.is_none());
    let screen: String = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
    assert!(
        screen.contains(PATH) && !screen.contains("[Dashboard]"),
        "the header row is painted, without the button"
    );
    draw(&mut agent, &registry, true, OverlayHeader::default());
    assert!(
        agent.hit_dashboard.rect.is_some(),
        "the overlay always offers the way back"
    );
}

#[test]
fn overlay_header_leads_with_title_and_paints_switcher() {
    let _theme = crate::theme::cache::pin_theme();
    let theme = crate::theme::Theme::current();
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    let header = OverlayHeader {
        title: Some("Refactor the theme loader"),
        position: Some((2, 5)),
    };
    let buf = draw(&mut agent, &registry, true, header);
    let row = header_row(&agent, &buf);
    let y = agent.hit_dashboard.rect.unwrap().y;
    let sep = crate::views::agent_status::separator(&theme);
    assert!(
        row.contains(&format!("Refactor the theme loader{}{PATH}", sep.content)),
        "title, separator, then the location, row = {row:?}"
    );
    let title_x = row.find("Refactor").unwrap() as u16;
    assert_eq!(fg(&buf, title_x, y), theme.text_secondary, "title tier");
    let path_x = row.find(PATH).unwrap() as u16;
    assert_eq!(
        fg(&buf, path_x, y),
        theme.gray_dim,
        "path steps back behind a title"
    );
    let switcher = switcher_text(2, 5);
    assert!(row.contains(&switcher), "row = {row:?}");
    let prev = agent.hit_overlay_prev.rect.expect("‹ rect");
    let next = agent.hit_overlay_next.rect.expect("› rect");
    assert_eq!(prev.width, 1);
    assert_eq!(next.width, 1);
    assert_eq!(
        next.x - prev.x,
        switcher.chars().count() as u16 - 1,
        "chevrons bracket the position"
    );
    assert!(matches!(
        agent.handle_input(&click(prev.x, prev.y), &registry),
        InputOutcome::Action(Action::DashboardOverlayPrev)
    ));
    assert!(matches!(
        agent.handle_input(&click(next.x, next.y), &registry),
        InputOutcome::Action(Action::DashboardOverlayNext)
    ));
    let dash = agent.hit_dashboard.rect.unwrap();
    assert!(matches!(
        agent.handle_input(&click(dash.x, dash.y), &registry),
        InputOutcome::Action(Action::DashboardOverlayExit)
    ));
}

#[test]
fn overlay_header_omits_title_when_unnamed_and_switcher_when_alone() {
    let _theme = crate::theme::cache::pin_theme();
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    let header = OverlayHeader {
        title: None,
        position: Some((1, 1)),
    };
    let buf = draw(&mut agent, &registry, true, header);
    let row = header_row(&agent, &buf);
    assert!(
        row.trim_start().starts_with(PATH),
        "location leads when there is no title, row = {row:?}"
    );
    assert!(
        !row.contains(crate::views::context_bar::SEPARATOR)
            || row.find(PATH) < row.find(crate::views::context_bar::SEPARATOR),
        "no title separator before the location, row = {row:?}"
    );
    assert!(
        !row.contains("1/1"),
        "no switcher for a lone agent, row = {row:?}"
    );
    assert!(agent.hit_overlay_prev.rect.is_none() && agent.hit_overlay_next.rect.is_none());
    assert!(agent.hit_dashboard.rect.is_some());
}

#[test]
fn narrow_overlay_header_caps_title_and_keeps_location_and_buttons() {
    let _theme = crate::theme::cache::pin_theme();
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(60);
    let header = OverlayHeader {
        title: Some("A generated title long enough to need trimming here"),
        position: Some((2, 5)),
    };
    let buf = draw(&mut agent, &registry, true, header);
    let row = header_row(&agent, &buf);
    assert!(
        row.contains(&switcher_text(2, 5)),
        "switcher survives, row = {row:?}"
    );
    assert!(agent.hit_dashboard.rect.is_some() && agent.hit_overlay_next.rect.is_some());
    assert!(
        row.contains("A generated") && !row.contains("trimming here"),
        "the title is cut, row = {row:?}"
    );
    assert!(
        row.contains("/grok-header") && !row.contains(PATH),
        "the location is cut from the right but outlives the title, row = {row:?}"
    );
    assert!(
        agent.hit_cwd.rect.is_some(),
        "the path keeps its click target while any of it is visible"
    );
}

#[test]
fn long_title_and_long_branch_leave_the_path_visible() {
    let _theme = crate::theme::cache::pin_theme();
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(100);
    agent.current_branch = Some("feature/very-long-branch-name".into());
    agent.is_worktree = true;
    let header = OverlayHeader {
        title: Some("A generated title long enough to need trimming here"),
        position: Some((2, 5)),
    };
    let buf = draw(&mut agent, &registry, true, header);
    let row = header_row(&agent, &buf);
    assert!(
        row.contains("A generated") && !row.contains("trimming here"),
        "the title is cut, row = {row:?}"
    );
    assert!(
        row.contains("feature/very-long-branch-name worktree /grok-h"),
        "branch, badge, and the start of the path survive the title, row = {row:?}"
    );
    let cwd = agent.hit_cwd.rect.expect("the path keeps its click target");
    let path_byte = row.find("/grok-h").expect("path on the row");
    let path_col = row.get(..path_byte).map_or(0, |s| s.chars().count()) as u16;
    assert_eq!(
        cwd.x, path_col,
        "the hit rect starts where the path is painted"
    );
}

#[test]
fn hover_brightens_only_the_pointed_affordance() {
    let _theme = crate::theme::cache::pin_theme();
    let theme = crate::theme::Theme::current();
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    let header = OverlayHeader {
        title: None,
        position: Some((2, 5)),
    };
    let buf = draw(&mut agent, &registry, true, header);
    let prev = agent.hit_overlay_prev.rect.unwrap();
    let next = agent.hit_overlay_next.rect.unwrap();
    let dash = agent.hit_dashboard.rect.unwrap();
    let faint_fg = theme.faint().fg.expect("pinned theme blends");
    assert_eq!(fg(&buf, next.x, next.y), faint_fg, "resting ›");
    assert_eq!(fg(&buf, dash.x, dash.y), theme.gray, "resting [Dashboard]");
    assert!(matches!(
        agent.handle_input(&mouse(MouseEventKind::Moved, next.x, next.y), &registry),
        InputOutcome::Changed
    ));
    let buf = draw(&mut agent, &registry, true, header);
    assert_eq!(fg(&buf, next.x, next.y), theme.text_primary, "hovered ›");
    assert_eq!(fg(&buf, prev.x, prev.y), faint_fg, "‹ untouched");
    agent.handle_input(&mouse(MouseEventKind::Moved, dash.x, dash.y), &registry);
    let buf = draw(&mut agent, &registry, true, header);
    assert_eq!(
        fg(&buf, dash.x, dash.y),
        theme.text_primary,
        "hovered [Dashboard]"
    );
    assert_eq!(fg(&buf, next.x, next.y), faint_fg, "› released");
}

#[test]
fn long_link_preview_yields_to_the_switcher_and_dashboard_button() {
    use crate::render::osc8::{LinkOverlay, LinkPresentation, LinkTarget, OverlayLink};
    let _theme = crate::theme::cache::pin_theme();
    let registry = ActionRegistry::defaults();
    let url = format!("https://example.com/{}", "segment/".repeat(30));
    let highlight = |agent: &mut AgentView| {
        let mut overlay = LinkOverlay::new();
        overlay.push(OverlayLink {
            screen_row: 0,
            col_start: 0,
            col_end: 10,
            target: LinkTarget::Url(std::sync::Arc::from(url.as_str())),
            presentation: LinkPresentation::Opaque,
            id: Some(0),
        });
        agent.visible_link_map.rebuild(1, &overlay, vec![]);
        agent.highlighted_link_idx = Some(0);
        assert!(agent.highlighted_link_url().is_some());
    };
    let header = OverlayHeader {
        title: None,
        position: Some((2, 5)),
    };
    let mut agent = agent_at(100);
    highlight(&mut agent);
    let buf = draw(&mut agent, &registry, true, header);
    let dash = agent
        .hit_dashboard
        .rect
        .expect("[Dashboard] stays on the row");
    let next = agent.hit_overlay_next.rect.expect("› stays on the row");
    assert!(dash.x + dash.width <= 100 && next.x < dash.x);
    let row = header_row(&agent, &buf);
    assert!(
        row.contains("https://example.com/segment/") && row.contains("…"),
        "the URL is what shortens, row = {row:?}"
    );
    assert!(
        row.contains(PATH),
        "the location keeps its floor next to the preview, row = {row:?}"
    );
    assert_eq!(text_at(&buf, dash), "[Dashboard]");
    let mut agent = agent_at(60);
    highlight(&mut agent);
    let buf = draw(&mut agent, &registry, true, header);
    let row = header_row(&agent, &buf);
    assert!(
        !row.contains("https://") && row.contains(PATH),
        "no preview, location intact, row = {row:?}"
    );
    assert!(agent.hit_overlay_next.rect.is_some() && agent.hit_dashboard.rect.is_some());
}

#[test]
fn open_dropdown_disarms_the_navigation_targets_until_it_closes() {
    let _theme = crate::theme::cache::pin_theme();
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    let header = OverlayHeader {
        title: None,
        position: Some((2, 5)),
    };
    draw(&mut agent, &registry, true, header);
    let dash = agent.hit_dashboard.rect.expect("armed with no dropdown");
    let next = agent.hit_overlay_next.rect.expect("armed with no dropdown");
    let _ = agent.prompt.handle_paste("/");
    agent.prompt.refresh_slash(&agent.session.models);
    assert!(
        agent.prompt.any_dropdown_open(),
        "setup: slash dropdown open"
    );
    draw(&mut agent, &registry, true, header);
    assert!(agent.hit_dashboard.rect.is_none() && agent.hit_overlay_prev.rect.is_none());
    assert!(agent.hit_overlay_next.rect.is_none());
    assert!(
        !matches!(
            agent.handle_input(&click(dash.x, dash.y), &registry),
            InputOutcome::Action(Action::DashboardOverlayExit)
        ),
        "a click where [Dashboard] was must not navigate"
    );
    assert!(!matches!(
        agent.handle_input(&click(next.x, next.y), &registry),
        InputOutcome::Action(Action::DashboardOverlayNext)
    ));
    agent.prompt.set_text("");
    agent.prompt.refresh_slash(&agent.session.models);
    assert!(!agent.prompt.any_dropdown_open(), "setup: dropdown closed");
    draw(&mut agent, &registry, true, header);
    assert_eq!(agent.hit_dashboard.rect, Some(dash));
    assert!(matches!(
        agent.handle_input(&click(next.x, next.y), &registry),
        InputOutcome::Action(Action::DashboardOverlayNext)
    ));
}

#[test]
fn subagent_takeover_drops_the_header_hits() {
    let _theme = crate::theme::cache::pin_theme();
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    let header = OverlayHeader {
        title: None,
        position: Some((2, 5)),
    };
    draw(&mut agent, &registry, true, header);
    assert!(agent.hit_dashboard.rect.is_some() && agent.hit_overlay_next.rect.is_some());
    agent.active_subagent = Some("child-sid".into());
    draw(&mut agent, &registry, true, header);
    assert!(agent.hit_dashboard.rect.is_none());
    assert!(agent.hit_overlay_prev.rect.is_none());
    assert!(agent.hit_overlay_next.rect.is_none());
}

#[test]
fn nested_subagent_keeps_the_parent_header_and_routes_like_it() {
    let _theme = crate::theme::cache::pin_theme();
    let registry = ActionRegistry::defaults();
    let mut parent = agent_at(120);
    let child_sid = "child-sid".to_string();
    parent.insert_subagent_view(child_sid.clone(), Box::new(agent_at(120)));
    parent.active_subagent = Some(child_sid.clone());
    let header = OverlayHeader {
        title: Some("Parent title"),
        position: Some((2, 5)),
    };
    let buf = draw(&mut parent, &registry, true, header);
    let child = parent
        .subagent_views
        .get(&child_sid)
        .expect("child view installed");
    let dash = child
        .hit_dashboard
        .rect
        .expect("the child paints [Dashboard]");
    let next = child
        .hit_overlay_next
        .rect
        .expect("the child keeps the parent's switcher");
    assert!(child.overlay_can_cycle, "and the footer's prev/next hint");
    let row = row_text(&buf, dash.y);
    assert!(
        row.contains("Parent title") && row.contains(&switcher_text(2, 5)),
        "the takeover row still describes the parent, row = {row:?}"
    );
    assert!(matches!(
        parent.handle_input(&click(next.x, next.y), &registry),
        InputOutcome::Action(Action::DashboardOverlayNext)
    ));
    assert!(matches!(
        parent.handle_input(&click(dash.x, dash.y), &registry),
        InputOutcome::Action(Action::DashboardOverlayExit)
    ));
}
