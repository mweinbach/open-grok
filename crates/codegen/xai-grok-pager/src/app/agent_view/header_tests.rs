//! Session status-bar header: shortened cwd, no `(worktree of …)` suffix, `[Dashboard]`.
use super::test_fixtures::make_agent;
use super::{AgentView, AppRenderParams, BannerSlotParams};
use crate::actions::ActionRegistry;
use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crate::app::bundle::BundleState;
use crate::scrollback::render::ScratchBuffer;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

const PATH: &str = "/grok-header-marker";

fn agent_at(width: u16) -> AgentView {
    let mut agent = make_agent();
    agent.last_terminal_size = (width, 30);
    agent.session.cwd = std::path::PathBuf::from(PATH);
    agent.set_dashboard_visible(true);
    agent
}

fn draw(agent: &mut AgentView, registry: &ActionRegistry, in_overlay: bool) -> Buffer {
    let (width, height) = agent.last_terminal_size;
    let area = Rect::new(0, 0, width, height);
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
        &BundleState::default(),
        in_overlay,
        false,
        &mut Vec::new(),
        AppRenderParams::default(),
    );
    buf
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

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    }
}

#[test]
fn worktree_session_header_keeps_badge_and_omits_main_repo_suffix() {
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    agent.is_worktree = true;
    agent.main_repo = Some("~/xai".into());
    let buf = draw(&mut agent, &registry, false);
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
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    agent.session.cwd = std::path::PathBuf::from("/deep/alpha/bravo/charlie/delta");
    let buf = draw(&mut agent, &registry, false);
    let row = header_row(&agent, &buf);
    assert!(
        row.contains("/d/a/b/charlie/delta"),
        "always last-two shortening, row = {row:?}"
    );
}

#[serial_test::serial(GROK_AGENT_DASHBOARD)]
#[test]
fn plain_session_header_paints_dashboard_chip() {
    unsafe { std::env::remove_var("GROK_AGENT_DASHBOARD") };
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    let buf = draw(&mut agent, &registry, false);
    let row = header_row(&agent, &buf);
    assert!(row.contains("[Dashboard]"), "row = {row:?}");
    let dash = agent.hit_dashboard.rect.expect("[Dashboard] hit area");
    let outcome = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        dash.x,
        dash.y,
    ));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::OpenDashboard)
    ));
}

#[serial_test::serial(GROK_AGENT_DASHBOARD)]
#[test]
fn disabled_dashboard_hides_chip_outside_overlay() {
    unsafe { std::env::set_var("GROK_AGENT_DASHBOARD", "0") };
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    let buf = draw(&mut agent, &registry, false);
    assert!(agent.hit_dashboard.rect.is_none());
    assert!(
        !header_row(&agent, &buf).contains("[Dashboard]"),
        "disabled dashboard must not paint a dead button"
    );
    let overlay = draw(&mut agent, &registry, true);
    assert!(
        agent.hit_dashboard.rect.is_some(),
        "overlay still paints [Dashboard] as the back-out chip"
    );
    assert!(header_row(&agent, &overlay).contains("[Dashboard]"));
    unsafe { std::env::remove_var("GROK_AGENT_DASHBOARD") };
}

#[serial_test::serial(GROK_AGENT_DASHBOARD)]
#[test]
fn overlay_dashboard_chip_exits_overlay() {
    unsafe { std::env::remove_var("GROK_AGENT_DASHBOARD") };
    let registry = ActionRegistry::defaults();
    let mut agent = agent_at(120);
    let _ = draw(&mut agent, &registry, true);
    let dash = agent
        .hit_dashboard
        .rect
        .expect("overlay paints [Dashboard]");
    let outcome = agent.handle_mouse(&mouse(
        MouseEventKind::Down(MouseButton::Left),
        dash.x,
        dash.y,
    ));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::DashboardOverlayExit)
    ));
}
