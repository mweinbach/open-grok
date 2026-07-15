//! Main event loop.
//!
//! A thin `tokio::select!` loop. All input routing, rendering, and state
//! management is delegated to [`AppView`]. The event loop only handles
//! IO plumbing: terminal events, ACP channel, spawned task results,
//! animation ticks, and hot-reloadable config changes.

use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until};

use crate::appearance::ConfigWatcher;
use crate::client_identity::{PAGER_CLIENT_TYPE, PAGER_CLIENT_VERSION};
use crate::theme::system_appearance::{self, SystemAppearanceWatcher};
use crate::theme::{Theme, ThemeKind, cache as theme_cache};

use agent_client_protocol as acp;
use xai_acp_lib::acp_send;

use super::actions::{Action, Effect, TaskResult};
use super::app_view::{
    ActiveView, AppView, AuthState, InputOutcome, PasteProvenance, TrustState, VoiceState,
};
use super::{PagerArgs, PagerTerminal, acp_handler, dispatch, effects};

/// Values resolved before `init_terminal` and consumed by the event loop.
///
/// All fields must be computed while stdin is still in cooked mode and
/// crossterm has not yet taken it over.
pub(crate) struct TerminalState {
    pub is_control_mode: bool,
    pub screen_mode: super::ScreenMode,
    /// This process was re-exec'd by `/minimal` (one-shot env override, already
    /// consumed). Queues the minimal welcome card so the viewport re-anchors at
    /// the top of the freshly cleared main screen.
    pub relaunched_into_minimal: bool,
    /// Do NOT re-resolve via `theme::cache::resolve_initial_theme()` here:
    /// its OSC 11 fallback reads stdin and competes with the input reader.
    pub initial_theme: ThemeKind,
}

/// Result of the event loop run.
pub(crate) struct RunResult {
    pub exit_info: Option<super::ExitInfo>,
    pub quit_for_update: bool,
    /// When set, the process should re-exec into the other screen mode after
    /// terminal restore. See `/minimal` and `/fullscreen`.
    pub relaunch: Option<super::app_view::ScreenModeRelaunch>,
}

/// In-flight reconnect re-initialization, tied to the agents whose reload
/// windows it opened so completion lands on them even if the user switches
/// views (or closes one) while the re-init runs.
struct ReconnectReinit {
    rx: tokio::sync::oneshot::Receiver<ReinitOutcome>,
    /// Agents being reloaded, active tab first; empty when the reconnect
    /// happened with no open sessions (init/auth are still re-run).
    agent_ids: Vec<super::agent::AgentId>,
    /// Reconnect generation that opened the reload windows.
    generation: u64,
}

/// Result of a reconnect re-initialization task.
struct ReinitOutcome {
    /// Whether initialize/authenticate succeeded; when false no load was
    /// attempted and `loads` is empty (every window finalizes as failed).
    init_ok: bool,
    loads: Vec<AgentLoadOutcome>,
}

/// Per-agent `session/load` outcome from the re-init task.
struct AgentLoadOutcome {
    agent_id: super::agent::AgentId,
    success: bool,
    /// `x.ai/runningPromptId` from the reload response: the turn another
    /// client is driving mid-reconnect, adopted at finalize (mirrors the
    /// `SessionLoaded` adoption in `dispatch.rs`).
    running_prompt_id: Option<String>,
}

/// Fields of the reconnect `session/load`, derived from the agent being
/// reloaded. `None` when the agent has no session yet.
struct ReconnectLoadPlan {
    session_id: acp::SessionId,
    /// The session's own cwd — its on-disk storage key — falling back to the
    /// pager cwd only when unset. The pager cwd only matches sessions started
    /// in it; worktree/cross-cwd sessions would fail to reload.
    cwd: std::path::PathBuf,
    /// `yoloMode` plus the optional reconnect `cursor`: the agent replays
    /// only the post-cursor tail (as live updates) when it finds the eventId,
    /// and full-replays when it doesn't.
    meta: serde_json::Value,
}

fn plan_reconnect_load(
    agent: &super::agent_view::AgentView,
    fallback_cwd: &std::path::Path,
) -> Option<ReconnectLoadPlan> {
    let session_id = agent.session.session_id.clone()?;
    let cwd = if agent.session.cwd.as_os_str().is_empty() {
        fallback_cwd.to_path_buf()
    } else {
        agent.session.cwd.clone()
    };
    let yolo = agent.session.is_yolo();
    // Set BOTH yoloMode and autoMode explicitly. The leader's capability injection
    // only fills ABSENT keys, so omitting autoMode here lets a stale launch-time
    // `ClientCapabilities.auto_mode` re-enable Auto after the user left it (e.g.
    // Shift+Tab to Ask). Auto is per-agent (symmetric with yolo) — derive it from
    // this agent's own `auto_mode` so a background tab reconnects with ITS mode,
    // not the active tab's global `current_ui` mirror.
    let auto = super::dispatch::effective_auto(yolo, agent.session.is_auto());
    let mut meta = serde_json::json!({ "yoloMode": yolo, "autoMode": auto });
    if let Some(ref cursor) = agent.last_seen_event_id {
        meta["cursor"] = serde_json::Value::String(cursor.clone());
    }
    Some(ReconnectLoadPlan {
        session_id,
        cwd,
        meta,
    })
}

/// Resolve the two post-reconnect restore outcomes from the per-agent
/// `session/load` results.
///
/// - `all_restored` (AND across every reloaded tab, plus `init_ok`) drives the
///   user-facing toast: it reports whether the WHOLE reconnect came back.
/// - `active_restored` is per-agent: the ACTIVE tab's OWN reload succeeded. It
///   gates that tab's post-reconnect queue drain. Gating the drain on
///   `all_restored` would let one failed background tab strand prompts queued
///   on a healthy active tab — the drain (`dispatch_drain_queue`) only ever
///   touches the active agent, so a background failure has no bearing on it.
///
/// `loads` maps each reloaded agent to `(success, running_prompt_id)`; an agent
/// in `pending_agent_ids` but absent from `loads` is treated as failed
/// (mirrors the `unwrap_or((false, _))` at the finalize site).
fn reconnect_restore_outcome(
    init_ok: bool,
    pending_agent_ids: &[super::agent::AgentId],
    loads: &std::collections::HashMap<super::agent::AgentId, (bool, Option<String>)>,
    active_agent_id: Option<super::agent::AgentId>,
) -> (bool, bool) {
    let load_ok = |id: &super::agent::AgentId| -> bool { loads.get(id).is_some_and(|(ok, _)| *ok) };
    let all_restored = init_ok && pending_agent_ids.iter().all(load_ok);
    let active_restored = init_ok
        && active_agent_id.is_some_and(|aid| pending_agent_ids.contains(&aid) && load_ok(&aid));
    (all_restored, active_restored)
}

/// Compute the folder-trust verdict for the session cwd and seed
/// [`AppView::trust_state`]. Pager-side mirror of the agent's resolve: read the
/// local store, scan for repo-local code-exec config, and run the pure
/// [`decide`](xai_grok_workspace::folder_trust::decide) precedence.
///
/// `TrustOutcome::Prompt` (interactive + untrusted + repo configs present)
/// becomes `TrustState::Pending` (show the question); everything else becomes
/// `TrustState::Done`. The feature-off fast path (kill-switch / opt-out /
/// local build) short-circuits before any I/O.
fn seed_trust_state(
    app: &mut AppView,
    remote: Option<&xai_grok_shell::util::config::RemoteSettings>,
) {
    use std::io::IsTerminal;
    use xai_grok_workspace::folder_trust::{
        TrustOutcome, decide, decide_inputs_with_interactive, feature_enabled,
    };
    use xai_grok_workspace::trust::workspace_key;

    let feature = feature_enabled(remote);
    if !feature {
        app.trust_state = TrustState::Done;
        return;
    }

    // The cwd the user launched in == the process cwd == `app.cwd` (set at
    // construction), matching the `--trust` grant's `std::env::current_dir()`.
    let cwd = app.cwd.clone();
    let key = workspace_key(&cwd);
    // Reuse the canonical gather (store trust + repo-config scan) but pass the
    // pager's stdin-only interactivity: the TUI prompts via the rendered
    // question + crossterm keyboard, NOT stderr (the pager redirects native
    // stderr at startup, so the engine's `stdin && stderr` would be false here
    // and the question would never show). TTY stdin => user can answer;
    // otherwise fail closed (no prompt).
    let inputs = decide_inputs_with_interactive(&cwd, &key, std::io::stdin().is_terminal());
    app.trust_state = match decide(feature, &inputs) {
        TrustOutcome::Prompt => TrustState::Pending { workspace: key },
        TrustOutcome::Trusted | TrustOutcome::Untrusted => TrustState::Done,
    };
}

/// Suspend the inline TUI, run a blocking child that takes over the tty
/// (`$EDITOR`, `$PAGER`, …), then restore. Shared by the editor and transcript
/// suspend paths so the subtle reader-park + raw-mode + alt-screen handoff lives
/// in one place.
///
/// The reader thread is parked first so the child (which inherits this tty)
/// keeps every keystroke instead of racing the reader; on return, buffered
/// terminal query replies and any pre-park keystroke are drained before the
/// reader resumes.
///
/// The frame **writer** thread is then drained (bounded) before the child
/// starts: frames are written to the tty asynchronously, and the frame that
/// armed this suspend (e.g. minimal's final `/transcript` pump slice) is
/// typically queued microseconds before we get here. Un-drained, those bytes
/// race the child's own output — they can land on the child's alternate
/// screen (so the main screen never receives them; on an inline viewport a
/// commit's scroll then leaves every following row off by its height) or tear
/// around the alt-screen switch, printing escape fragments (`[`…) that the
/// renderer's diff can't see and thus never repairs.
///
/// In minimal mode the physical cursor is probed (`ESC[6n`) right before the
/// child runs and right after it exits — while the reader thread is still
/// parked, so the replies can't be stolen. The returned "cursor after the
/// child, iff it moved" tells the caller whether the child *printed to the
/// main screen* (cat-style pager: cursor left below its output) or *restored
/// it* (less-style alt-screen pager: `rmcup` puts the cursor back exactly
/// where it was): the caller re-anchors the live region below the new output
/// in the first case and repaints in place in the second.
fn suspend_for_child(
    screen_mode: crate::app::ScreenMode,
    writer_sync: &crate::render::draw::WriterSync,
    input_paused: &std::sync::atomic::AtomicBool,
    reader_parked: &std::sync::atomic::AtomicBool,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<crossterm::event::Event>,
    run_child: impl FnOnce(),
) -> Option<(u16, u16)> {
    use std::sync::atomic::Ordering;
    // Pause the reader thread, then wait for a FRESH park (reader provably out of
    // crossterm) so the main thread is the sole poll/read caller; bounded so a
    // dead reader can't hang us.
    input_paused.store(true, Ordering::Release);
    reader_parked.store(false, Ordering::Release);
    let park_deadline = std::time::Instant::now() + Duration::from_millis(500);
    while !reader_parked.load(Ordering::Acquire) && std::time::Instant::now() < park_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    // Every queued frame must be ON the tty before the child takes it (and, in
    // fullscreen, before LeaveAlternateScreen below — mirroring teardown's
    // "no late frame after LeaveAlternateScreen" drain). Bounded like the
    // reader park so a wedged pty can't hang the suspend.
    if !writer_sync.wait_drained(Duration::from_millis(750)) {
        tracing::warn!("suspend: frame writer not drained within 750ms; proceeding");
    }
    // Pre-child cursor probe (minimal only — minimal's startup already proved
    // this terminal answers CPR). Reader is parked, so the reply is ours.
    let pre_cursor = screen_mode
        .is_minimal()
        .then(|| crossterm::cursor::position().ok())
        .flatten();
    if screen_mode.is_fullscreen() {
        xai_grok_shell::util::with_locked_stderr(|stderr| {
            let _ = crossterm::execute!(stderr, crossterm::terminal::LeaveAlternateScreen);
        });
    }
    let _ = crossterm::terminal::disable_raw_mode();
    run_child();
    let _ = crossterm::terminal::enable_raw_mode();
    if screen_mode.is_fullscreen() {
        xai_grok_shell::util::with_locked_stderr(|stderr| {
            let _ = crossterm::execute!(stderr, crossterm::terminal::EnterAlternateScreen);
        });
    }
    // Discard child-exit ANSI query replies (DA/DSR/cursor reports) the terminal
    // buffered; reader is parked, so the main thread is the only crossterm caller.
    while crossterm::event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let _ = crossterm::event::read();
    }
    // Post-child cursor probe: `Some` iff the child left the cursor somewhere
    // other than where it found it (see the doc comment).
    let moved_cursor = pre_cursor.and_then(|pre| {
        let post = crossterm::cursor::position().ok()?;
        (post != pre).then_some(post)
    });
    // Discard any keystroke the reader read during the brief pre-park window: it
    // lands in the channel, not the tty.
    while input_rx.try_recv().is_ok() {}
    input_paused.store(false, Ordering::Release);
    moved_cursor
}

/// Restore the inline/minimal live region after a tty-taking child exited.
///
/// Two child behaviors, two restores (common post-suspend handling):
///
/// - **Screen-restoring child** (`less` & friends: alt screen + `rmcup`, or a
///   child that printed nothing): the cursor is back where it was, the main
///   screen still shows the pre-suspend frame. Repaint in place — `clear()`
///   resets the back buffer so the next draw rewrites every viewport cell
///   (the restored screen may still differ subtly, e.g. a lost cell attribute).
/// - **Inline-printing child** (`PAGER=cat`, an editor that dumps to the tty):
///   its output scrolled the main screen and the cursor sits below it. The old
///   viewport rows are gone (scrolled up or overwritten) — re-anchor the
///   viewport at the cursor row, scrolling the screen up first when there
///   isn't a full viewport of room left (the same make-room dance as the
///   startup inline anchor), so the live region redraws below the child's
///   output instead of overpainting it.
///
/// Fullscreen mode needs neither: it re-enters the alternate screen, and
/// `clear()` + redraw repaints the whole thing.
fn restore_after_child(
    terminal: &mut PagerTerminal,
    screen_mode: crate::app::ScreenMode,
    moved_cursor: Option<(u16, u16)>,
) {
    use ratatui::backend::Backend as _;
    if let Some((_x, y)) = moved_cursor
        && screen_mode.is_minimal()
    {
        let screen = terminal.last_known_area();
        let cur = terminal.viewport_area();
        let vh = cur.height.max(1).min(screen.height.max(1));
        // Newlines printed from the cursor row scroll the screen exactly when
        // fewer than `vh` rows remain below it (append_lines is buffered on
        // the frame writer, so these bytes stay ordered before the clear +
        // redraw below).
        let _ = terminal.backend_mut().append_lines(vh.saturating_sub(1));
        let available = screen.height.saturating_sub(y).saturating_sub(1);
        let missing = vh.saturating_sub(1).saturating_sub(available);
        let top = y.saturating_sub(missing);
        terminal.set_viewport_area(ratatui::layout::Rect {
            y: top,
            height: vh,
            ..cur
        });
    }
    let _ = terminal.clear();
}

/// Consume a pending `$EDITOR` / `$PAGER` suspend request, if any.
///
/// Called at the TOP of every event-loop iteration — not from one specific
/// select arm — because the requests can be armed from ANY arm: a keypress
/// (`$EDITOR` from the agents modal), but also an animation tick (minimal's
/// incremental `/transcript` build finishes inside a tick-arm draw and arms
/// `pending_pager_path`). When consumption lived only in the input arm, a
/// build finishing on a tick sat armed until the next unrelated event — the
/// "progress hits done, then multi-second wait before `less` opens" lag.
#[allow(clippy::too_many_arguments)]
fn run_pending_suspends(
    app: &mut AppView,
    terminal: &mut PagerTerminal,
    input_paused: &std::sync::atomic::AtomicBool,
    reader_parked: &std::sync::atomic::AtomicBool,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<crossterm::event::Event>,
    last_draw_at: &mut Instant,
    draw_scheduled_at: &mut Option<Instant>,
) {
    // $EDITOR suspend: leave alt screen, disable raw mode, spawn
    // editor, wait for exit, then restore.
    if let Some(path) = app.pending_editor_path.take() {
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());
        let writer_sync = terminal.backend_mut().writer_mut().writer_sync().clone();
        let moved_cursor = suspend_for_child(
            app.screen_mode,
            &writer_sync,
            input_paused,
            reader_parked,
            input_rx,
            || {
                let _ = std::process::Command::new(&editor).arg(&path).status();
            },
        );
        if let Some(tab) = app.pending_agents_modal_refresh.take()
            && let ActiveView::Agent(id) = app.active_view
            && let Some(agent) = app.agents.get_mut(&id)
            && let Some(ref mut modal) = agent.agents_modal
        {
            modal.refresh_after_editor(tab);
        }
        // The child owned the screen; re-anchor if it printed inline, and
        // repaint the full viewport rather than diffing against a screen
        // state we can no longer vouch for.
        restore_after_child(terminal, app.screen_mode, moved_cursor);
        app.draw(terminal);
        *last_draw_at = Instant::now();
        *draw_scheduled_at = None;
    }

    // /transcript suspend: open the rendered transcript in $PAGER,
    // then restore and delete the temp file. Shares the editor's
    // suspend/restore dance (reader park, raw mode, alt screen).
    if let Some(path) = app.pending_pager_path.take() {
        let ansi = std::mem::take(&mut app.pending_pager_ansi);
        let pager = std::env::var("PAGER")
            .ok()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "less".to_string());
        let writer_sync = terminal.backend_mut().writer_mut().writer_sync().clone();
        let moved_cursor = suspend_for_child(
            app.screen_mode,
            &writer_sync,
            input_paused,
            reader_parked,
            input_rx,
            || {
                // $PAGER may carry flags (e.g. "less -R"); split on
                // whitespace so program + args are both honored.
                let mut parts = pager.split_whitespace();
                if let Some(prog) = parts.next() {
                    let mut args: Vec<String> = parts.map(str::to_string).collect();
                    // An ANSI transcript (minimal full view) needs
                    // `less` to interpret raw control codes, else the
                    // colors show as literal escapes. Add `-R` when
                    // using less and it isn't already requested.
                    let is_less = std::path::Path::new(prog)
                        .file_name()
                        .and_then(|n| n.to_str())
                        == Some("less");
                    if ansi
                        && is_less
                        && !args.iter().any(|a| {
                            matches!(
                                a.as_str(),
                                "-R" | "-r" | "--RAW-CONTROL-CHARS" | "--raw-control-chars"
                            )
                        })
                    {
                        args.push("-R".to_string());
                    }
                    // Open the transcript at its END: minimal's prompt sits at
                    // the bottom of the conversation, so the pager starts where
                    // the user already is (`g` jumps back to the top). less-only
                    // like `-R` — other $PAGERs may not understand `+G`.
                    if ansi && is_less && !args.iter().any(|a| a == "+G") {
                        args.push("+G".to_string());
                    }
                    let _ = std::process::Command::new(prog)
                        .args(&args)
                        .arg(&path)
                        .status();
                }
            },
        );
        let _ = std::fs::remove_file(&path);
        // The pager owned the screen; re-anchor if it printed inline (cat) and
        // repaint the full viewport rather than diffing against a screen state
        // we can no longer vouch for.
        restore_after_child(terminal, app.screen_mode, moved_cursor);
        app.draw(terminal);
        *last_draw_at = Instant::now();
        *draw_scheduled_at = None;
    }
}

/// Run the main event loop until quit.
///
/// Returns a [`RunResult`] with optional exit info (for the resume hint)
/// and a flag indicating whether the caller should restart the binary
/// to pick up a downloaded update.
///
/// The initial theme MUST come from `term_state.initial_theme`; see
/// [`TerminalState::initial_theme`] for why.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    terminal: &mut PagerTerminal,
    connection: crate::acp::AcpConnection,
    config_watcher: &mut ConfigWatcher,
    args: &PagerArgs,
    session_cwd: Option<std::path::PathBuf>,
    remote_settings: Option<xai_grok_shell::util::config::RemoteSettings>,
    term_state: TerminalState,
    materialized: crate::app::session_startup::MaterializedStartup,
    bg_update_rx: Option<
        tokio::sync::oneshot::Receiver<Option<xai_grok_update::auto_update::UpdateAvailable>>,
    >,
) -> anyhow::Result<RunResult> {
    // Initialize tracing capture. The channel `rx` will be wired to a
    // TracingModel (and ultimately a tracing pane) once integrated.
    // For now we drain-and-discard in `AppView::tick()` to avoid unbounded
    // memory growth.
    if args.log_sampling {
        // SAFETY: called before any threads are spawned by init_tracing.
        unsafe { std::env::set_var("GROK_LOG_SAMPLING", "1") };
    }
    let tracing_handle = crate::tracing::init_tracing();

    crate::unified_log::init(connection.tx.clone());
    crate::unified_log::info("pager started", None, None);
    let mut app = AppView::new(
        connection.tx,
        connection.models,
        connection.available_commands,
    );
    app.tracing_rx = Some(tracing_handle.rx);
    // Startup terminal height for the auto-compact derivation; kept fresh by
    // `Event::Resize` from here on. 0 (probe failure) never forces compact.
    app.last_known_terminal_rows = crossterm::terminal::size().map(|(_, r)| r).unwrap_or(0);
    // Leader mode: a live `leader_status_rx` means the pager is connected via a
    // leader. The dashboard itself is NOT gated on this flag (it renders local
    // sessions regardless); `leader_mode` only controls whether we additionally
    // poll the leader roster (see the roster-poll arm below).
    app.leader_mode = connection.leader_status_rx.is_some();
    app.screen_mode = term_state.screen_mode;
    // Agent/dashboard prompts pick the mode up at their creation sites
    // (`apply_app_scoped_gates` / `ensure_dashboard_state`); the welcome prompt
    // already exists, so inject here.
    app.welcome_prompt.set_screen_mode(term_state.screen_mode);
    // Screen-mode relaunch into minimal (`/minimal` from fullscreen): queue the
    // same top-of-screen welcome card `/new` uses so the first draw re-homes the
    // viewport to row 0 and the resumed session starts cleanly under it. The
    // `is_minimal` guard covers a Minimal→Inline probe downgrade in
    // `init_terminal`, where the card must not be queued.
    if app.screen_mode.is_minimal() && term_state.relaunched_into_minimal {
        app.minimal_state.welcome_pending = true;
    }
    let remote_permission_mode = remote_settings
        .as_ref()
        .and_then(|s| s.permission_mode.as_deref());
    let launch_yolo = xai_grok_shell::util::config::effective_yolo_for_launch(
        args.yolo,
        args.permission_mode_flag.as_deref(),
        remote_permission_mode,
    );
    app.default_yolo = launch_yolo.yolo;
    // Gated launch-auto (CLI `--permission-mode auto` or config). Hoisted so it can
    // be re-applied after `load_initial_ui_config()` replaces `current_ui` below.
    let launch_auto = xai_grok_shell::util::config::effective_auto_for_launch(
        args.yolo,
        args.permission_mode_flag.as_deref(),
        remote_permission_mode,
    );
    if launch_auto {
        app.current_ui.permission_mode = Some("auto".into());
    }
    // One effective-config read for launch-mode ownership + the display
    // resolve below (the launch resolvers above keep their own internal read).
    let launch_effective_ui = xai_grok_shell::config::load_effective_config()
        .ok()
        .and_then(|root| root.get("ui").cloned());
    // Soft-default owns the mode only when neither CLI nor effective TOML
    // claimed it; while owned, `settings/update` pushes may re-arm it.
    let cli_owns_mode = args.yolo || args.permission_mode_flag.is_some();
    let toml_owns_mode = launch_effective_ui
        .as_ref()
        .and_then(xai_grok_shell::util::config::permission_mode_from_ui_if_set)
        .is_some();
    app.permission_mode_from_soft_default = !cli_owns_mode && !toml_owns_mode;
    // Cached pin snapshot gating dispatch's runtime always-approve toggles. A
    // mid-session pin change is missed here, but only cosmetically: the agent's
    // permission manager re-clamps yolo authoritatively at decision time.
    app.yolo_policy_block = launch_yolo.policy_block;
    if let Some(warning) = launch_yolo.blocked_warning {
        tracing::warn!("{warning}");
        crate::unified_log::warn(warning, None, None);
        // Consumed by `switch_to_agent` once the first agent view opens.
        app.yolo_launch_block_notice = Some(warning);
    }
    app.require_plan_approval = xai_grok_shell::util::config::load_require_plan_approval();
    app.plan_mode = !args.no_plan;
    app.subagents = !args.no_subagents;
    app.ask_user = !args.no_ask_user;
    app.chat_mode = args.chat();
    app.restore_code = args.restore_code.then_some(true);
    if let Some(ref agent) = args.agent {
        match crate::headless::resolve_agent_arg(agent) {
            crate::headless::ResolvedAgent::FilePath(path) => {
                match xai_grok_shell::agent::config::AgentDefinition::from_file(&path) {
                    Ok(def) => app.agent_override = Some(def.to_json_value()),
                    Err(e) => {
                        tracing::warn!("--agent: failed to load agent file: {e}");
                    }
                }
            }
            crate::headless::ResolvedAgent::Name(name) => {
                app.agent_override = Some(serde_json::Value::String(name));
            }
        }
    }
    let headless_only: &[(&str, bool)] = &[
        ("--agents", args.agents_json.is_some()),
        ("--tools", args.cli_tools.is_some()),
        ("--disallowed-tools", args.cli_disallowed_tools.is_some()),
        ("--max-turns", args.max_turns.is_some()),
    ];
    for &(flag, set) in headless_only {
        if set {
            tracing::warn!("{flag} is only supported in headless mode (-p); ignored in TUI");
        }
    }
    tracing::info!(
        cli_restore_code = args.restore_code,
        mapped_restore_code = ?app.restore_code,
        worktree = ?args.worktree,
        resume = ?args.resume_session,
        "RESTORE_CODE_DEBUG: CLI args mapped"
    );
    app.cli_model_override = args
        .model
        .as_deref()
        .map(agent_client_protocol::ModelId::new);
    app.cli_effort_token = args.reasoning_effort.clone();
    app.auth_use_oauth = args.oauth;
    app.show_resolved_model = remote_settings
        .as_ref()
        .and_then(|s| s.show_resolved_model)
        .unwrap_or(true);
    app.sharing_enabled = remote_settings
        .as_ref()
        .and_then(|s| s.sharing_enabled)
        .unwrap_or(false);
    app.plugin_cta_enabled = xai_grok_config::env_bool("GROK_PLUGIN_CTA")
        .or_else(|| remote_settings.as_ref().and_then(|s| s.plugin_cta))
        .unwrap_or(false);
    // Voice is GA-on by default. Remote `voice_mode_enabled: false` is a kill
    // switch; `GROK_VOICE_MODE` overrides for local dev (env > remote > default on).
    // Free/X Basic still hit SuperGrok upsell via tier gates (separate).
    let voice_mode_enabled = crate::app::resolve_voice_mode_enabled(
        xai_grok_config::env_bool("GROK_VOICE_MODE"),
        remote_settings.as_ref().and_then(|s| s.voice_mode_enabled),
    );
    app.apply_voice_mode_enabled(voice_mode_enabled);
    app.session_picker_grouped = std::env::var("GROK_SESSION_PICKER_GROUPED")
        .ok()
        .and_then(|v| match v.as_str() {
            "1" | "true" => Some(true),
            "0" | "false" => Some(false),
            _ => None,
        })
        .or_else(|| {
            xai_grok_shell::config::load_effective_config()
                .ok()
                .and_then(|cfg| cfg.get("cli")?.get("session_picker_grouped")?.as_bool())
        })
        .or_else(|| {
            remote_settings
                .as_ref()
                .and_then(|s| s.session_picker_grouped)
        })
        .unwrap_or(true);
    app.cancel_rewind_enabled = connection.cancel_rewind_enabled;
    apply_session_recap_available(&mut app, connection.session_recap_available);

    // Preserve auth methods so logout→re-login works without restarting.
    app.auth_methods = connection.auth_methods.clone();

    // Seed auth state from ACP connection metadata.
    // --force-login overrides: show the login screen even when credentials exist.
    let force_login = args.force_login && !connection.auth_methods.is_empty();
    let needs_interactive_login = connection.needs_login || force_login;
    if needs_interactive_login {
        app.welcome_prompt_focused = false;

        if connection.needs_login {
            // Normal path: use the metadata from startup_auth_metadata()
            app.login_label = connection.login_label;
            app.login_method_id = connection.login_method_id;
            app.auth_start_mode = match connection.auth_start_mode {
                crate::acp::AuthStartMode::Pending => super::app_view::AuthMode::Pending,
                crate::acp::AuthStartMode::Command => super::app_view::AuthMode::Command,
            };
        } else {
            // --force-login: find the grok.com method from the advertised list
            let grok_com = connection
                .auth_methods
                .iter()
                .find(|m| m.id().0.as_ref() == "grok.com");
            if let Some(method) = grok_com {
                app.login_label = Some(method.name().to_string());
                app.login_method_id = Some(method.id().clone());
                let is_provider = method
                    .meta()
                    .as_ref()
                    .and_then(|v| v.get("external_provider"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                app.auth_start_mode = if is_provider {
                    super::app_view::AuthMode::Command
                } else {
                    super::app_view::AuthMode::Pending
                };
            } else {
                // No grok.com method available, use the first method as fallback
                let first = &connection.auth_methods[0];
                app.login_label = Some(first.name().to_string());
                app.login_method_id = Some(first.id().clone());
                app.auth_start_mode = super::app_view::AuthMode::Pending;
            }
        }

        // Skip the login splash screen — auto-trigger login immediately
        // by reusing dispatch_login. Effects are stashed and drained after
        // the initial render so the user sees the auth UI right away.
        // Empty auth_methods (preferred_method pin with no credentials) is
        // fail-closed: do not invent grok.com / auto-start OIDC.
        tracing::info!(
            method_id = ?app.login_method_id,
            methods_empty = connection.auth_methods.is_empty(),
            "auto-triggering login at startup"
        );
    }
    // else: auth_state defaults to Done (already authenticated eagerly)
    // Effects stashed until after the initial render, so the user sees the
    // welcome/auth UI right away.
    let mut post_render_effects = if needs_interactive_login {
        if connection.auth_methods.is_empty() {
            // preferred_method pin unavailable — no advertised method to start.
            app.auth_state = super::app_view::AuthState::Pending {
                error: Some(
                    xai_grok_shell::agent::auth_method::PREFERRED_API_KEY_UNAVAILABLE.to_string(),
                ),
            };
            vec![]
        } else {
            dispatch::dispatch(Action::Login, &mut app)
        }
    } else {
        vec![]
    };

    if let Some(meta) = connection.auth_meta.as_ref() {
        match serde_json::from_value::<xai_grok_shell::auth::AuthMeta>(meta.clone()) {
            Ok(auth_meta) => app.apply_auth_meta(&auth_meta),
            Err(e) => tracing::warn!("failed to deserialize auth_meta: {e}"),
        }
    } else {
        // No cached session — check if the API key is the active credential.
        app.is_api_key_auth = app.auth_methods.iter().any(|m| {
            m.id().0.as_ref() == xai_grok_shell::agent::auth_method::XAI_API_KEY_METHOD_ID
        });
        // No AuthMeta on this path — hide `/usage` and enable voice for API keys.
        if app.is_api_key_auth {
            app.usage_visible = false;
            app.ensure_voice_for_api_key();
        }
    }

    // Fallback: prefetch may have gate info the shell's AuthMeta missed.
    // Errs on the side of blocking if stale.
    if app.gate.is_none()
        && let Some(rs) = remote_settings.as_ref()
    {
        app.gate = AppView::gate_from_settings(rs);
    }

    // Re-impose the startup gate through the chokepoint: cached auth meta
    // and the settings prefetch are both possibly stale, so a consumer
    // session's gate is deferred for live verification before first paint.
    if let Some(gate) = app.gate.take() {
        post_render_effects.extend(app.impose_gate(gate));
    }

    // Load persisted per-ID hidden state
    app.hidden_announcement_ids = xai_grok_announcements::read_hidden_announcement_ids().await;

    // Load config layers once, resolve announcements, tips, and feature flags.
    let requirements = xai_grok_shell::config::load_merged_requirements();
    let user_config = xai_grok_shell::config::load_from_disk().ok();
    let managed_config = xai_grok_shell::config::load_managed_config().ok();

    // Full merge when every layer parses; partial merge below if any layer fails.
    let effective_config = match xai_grok_shell::config::load_effective_config() {
        Ok(raw) => Some(raw),
        Err(e) => {
            tracing::debug!(error = %e, "failed to load effective config, using partial layers");
            None
        }
    };
    let compat = xai_grok_shell::agent::config::resolve_compat_sessions_from_raw(
        effective_config.as_ref().ok_or(()),
        remote_settings.as_ref(),
    );
    app.foreign_session_compat =
        xai_grok_workspace::foreign_sessions::EnabledForeignSessionSources {
            claude: compat.claude.sessions,
            codex: compat.codex.sessions,
            cursor: compat.cursor.sessions,
        };

    // Load notification config from [ui.notifications] in config.toml.
    if let Some(ref raw) = effective_config {
        app.notification_service = crate::notifications::NotificationService::new(
            crate::notifications::load_notification_config(raw),
        );
        if let Some(table) = raw.as_table() {
            app.voice_config = xai_grok_voice::VoiceConfig::from_config_table(table);
        }
    }
    // Stamp request-identity headers so the STT handshake attributes voice usage
    // to grok-cli server-side (mirrors sampler / imagine). Done after
    // `from_config_table` — which yields a fresh config with these
    // `#[serde(skip)]` fields defaulted to empty — and unconditionally, so they
    // apply even when there is no `[voice]` table (or no config at all).
    app.voice_config.client_identifier = crate::client_identity::HEADLESS_CLIENT_TYPE.to_string();
    app.voice_config.user_agent = crate::client_identity::client_user_agent();

    app.zdr_access_enabled = xai_grok_shell::util::config::resolve_zdr_access_enabled(
        requirements.as_ref(),
        user_config.as_ref(),
        managed_config.as_ref(),
        remote_settings.as_ref(),
    );

    app.subscription_watch_interval_secs = remote_settings
        .as_ref()
        .and_then(|rs| rs.subscription_watch_interval_secs);

    // Full layered resolve (env/requirements/remote may beat plain `[ui]`).
    crate::appearance::cache::set_show_thinking_blocks(
        xai_grok_shell::util::config::resolve_show_thinking_blocks(
            requirements.as_ref(),
            user_config.as_ref(),
            managed_config.as_ref(),
            remote_settings.as_ref(),
        )
        .value,
    );
    crate::appearance::cache::set_group_tool_verbs(
        xai_grok_shell::util::config::resolve_group_tool_verbs(
            requirements.as_ref(),
            user_config.as_ref(),
            managed_config.as_ref(),
            remote_settings.as_ref(),
        )
        .value,
    );
    crate::appearance::cache::set_collapsed_edit_blocks(
        xai_grok_shell::util::config::resolve_collapsed_edit_blocks(
            requirements.as_ref(),
            user_config.as_ref(),
            managed_config.as_ref(),
            remote_settings.as_ref(),
        )
        .value,
    );

    app.usage_billing_redirect_url = remote_settings
        .as_ref()
        .and_then(|s| s.usage_billing_redirect_url.clone());

    if app.is_access_blocked() {
        app.welcome_prompt_focused = false;
    }

    {
        use xai_grok_shell::util::config::{resolve_announcements, resolve_tips};

        let remote_announcements = remote_settings
            .as_ref()
            .and_then(|s| s.announcements.as_deref());
        let announcements = resolve_announcements(
            requirements.as_ref(),
            user_config.as_ref(),
            managed_config.as_ref(),
            remote_announcements,
        );
        app.active_announcements = xai_grok_announcements::filter_expired(announcements);
        if !app.active_announcements.is_empty() {
            use rand::Rng;
            let idx = rand::rng().random_range(0..app.active_announcements.len());
            app.announcement = app.active_announcements.get(idx).cloned();
        }
        app.sync_session_announcement_slash_gate();

        let remote_tips = remote_settings.as_ref().and_then(|s| s.tips.as_deref());
        app.tips = resolve_tips(
            requirements.as_ref(),
            user_config.as_ref(),
            managed_config.as_ref(),
            remote_tips,
        );

        if !app.tips.is_empty() {
            let grok_home = xai_grok_tools::util::grok_home::grok_home();
            app.tip = xai_grok_shell::util::tips::pick_and_advance(&app.tips, &grok_home);
        }
    }

    let hints = xai_grok_shell::util::config::resolve_hints(
        effective_config.as_ref(),
        requirements.as_ref(),
        user_config.as_ref(),
        managed_config.as_ref(),
    );
    app.project_picker_disabled = hints.project_picker_disabled;
    // Per-tip contextual hints resolve from `[ui.contextual_hints]` (loaded into
    // `app.current_ui` further below) + the remote tier; the resolve + prompt
    // propagation happen after `current_ui` is hydrated.
    app.remote_contextual_hints = remote_settings
        .as_ref()
        .and_then(|s| s.contextual_hints.clone());
    app.new_session_worktree_mode = hints.new_session_worktree_mode.into();
    app.fork_worktree_mode = hints.fork_worktree_mode.into();
    // Ephemeral-tip seen counts are intentionally NOT hydrated: the cap is
    // per-session (in-memory `app.tip_seen_counts`), so each run starts fresh.

    // Cache whether cwd is inside a git repo (avoids repeated stat() in draw).
    app.cwd_has_git_ancestor = app.cwd.ancestors().any(|p| p.join(".git").exists());

    // Probe / auto-cadence / terminal telemetry — see `display_refresh_startup`.
    let motion = super::display_refresh_startup::start(
        requirements.as_ref(),
        user_config.as_ref(),
        managed_config.as_ref(),
        remote_settings.as_ref(),
    );
    let min_draw_interval = motion.min_draw_interval;
    let scroll_cadence = motion.scroll_cadence;

    // Collect structured startup warnings from the terminal diagnostics engine.
    // These are stored on AppView and rendered as a dismissible in-app banner
    // when the user enters an agent session.
    {
        let ctx = crate::terminal::terminal_context();
        let query = crate::diagnostics::LiveTmuxQuery;
        let mut warnings = crate::diagnostics::collect_startup_warnings(
            ctx,
            &query,
            term_state.is_control_mode,
            term_state.screen_mode.is_fullscreen(),
        );
        // Wayland no-data-control reads the live environment, so it rides its
        // own wrapper (keeps `collect_startup_warnings` hermetic for tests).
        warnings.extend(crate::diagnostics::diagnose_wayland_data_control_live());
        let notif_warnings = crate::diagnostics::collect_notification_warnings(
            ctx,
            app.notification_service.protocol(),
            app.notification_service.config().condition,
            &query,
        );
        // Deduplicate by category: general terminal warnings take priority
        // over notification-specific ones (e.g. DcsPassthrough can fire from
        // both sources when allow-passthrough is off).
        let mut seen = std::collections::HashSet::new();
        for w in &warnings {
            seen.insert(w.category);
        }
        let mut all_warnings = warnings;
        all_warnings.extend(
            notif_warnings
                .into_iter()
                .filter(|w| seen.insert(w.category)),
        );
        if !all_warnings.is_empty() {
            tracing::info!("Collected {} startup warnings", all_warnings.len());
        }
        // WezTerm without the Kitty keyboard protocol breaks local input
        // (Shift+Enter can't insert newlines), so its banner is surfaced
        // directly (no SSH gate) and first — see `assemble_startup_warnings`.
        // `xtversion::detected()` is structurally `None` here (the probe is
        // only sent further down, right before the input reader thread is
        // spawned), so this banner covers env-detected WezTerm; the SSH shape
        // surfaces in /terminal-setup once the async reply has landed.
        let wezterm_warning = crate::diagnostics::wezterm_kitty_keyboard_warning(
            ctx,
            crate::app::kitty_flags_pushed(),
            crate::terminal::xtversion::detected(),
        );
        // Wayland no-data-control is surfaced without the SSH gate of
        // `summarize_warnings` — the broken shape is local (see
        // `assemble_startup_warnings`).
        let wayland_clipboard_warning = all_warnings
            .iter()
            .find(|w| w.category == crate::diagnostics::WarningCategory::WaylandNoDataControl);
        app.startup_warnings = crate::diagnostics::assemble_startup_warnings(
            wezterm_warning.as_ref(),
            wayland_clipboard_warning,
            crate::diagnostics::summarize_warnings(&all_warnings)
                .into_iter()
                .collect(),
        );
    }

    // Apply initial config (may come from existing ~/.grok/pager.toml).
    let mut initial_config = config_watcher.current().clone();
    // The cache holds the USER compact value; the render value is derived
    // (auto-compact while the startup terminal is short).
    initial_config.prompt.compact = crate::views::agent::effective_compact(
        crate::appearance::cache::load(),
        app.last_known_terminal_rows,
    );
    initial_config.show_timestamps = crate::appearance::cache::load_timestamps();
    let tick_interval = initial_config.animation.tick_interval();
    crate::appearance::set_tab_width(initial_config.scrollback.display.tab_width);
    app.set_appearance(initial_config);

    // Seed app state from disk once at the I/O boundary so dispatch
    // stays sans-IO.
    app.current_ui = load_initial_ui_config();
    // Disk load replaces `current_ui`. Assign one policy-clamped resolved
    // launch mode unconditionally (CLI > TOML > remote > Ask) so disk Auto
    // cannot win over `--permission-mode ask`, and a policy-clamped remote
    // AlwaysApprove cannot leave the UI claiming AlwaysApprove while
    // enforcement is Ask.
    let display_mode: &'static str = if launch_auto {
        "auto"
    } else if launch_yolo.yolo {
        "always-approve"
    } else if let Some(cli) = args.permission_mode_flag.as_deref() {
        // CLI always-approve/auto that did not become launch_yolo/launch_auto
        // (policy pin / gate) display as Ask.
        xai_grok_shell::util::config::clamped_display_permission_mode(
            xai_grok_shell::util::config::parse_permission_mode_canonical(cli),
        )
    } else {
        xai_grok_shell::util::config::resolved_display_permission_mode(
            launch_effective_ui.as_ref(),
            remote_permission_mode,
        )
    };
    app.current_ui.permission_mode = Some(display_mode.to_string());
    super::dispatch::downgrade_displayed_auto_if_gated(&mut app);
    // Seed `/auto` feature-gate visibility from the resolved gate (so `/auto`
    // is offered on the welcome prompt when available).
    app.sync_permission_mode_slash_gate();
    // Settings UI language (`[ui].voice_stt_language`) overrides `[voice].language`
    // when set. Store the preference (including client-only `auto`); the voice
    // crate resolves the wire code at STT connect. When unset, keep whatever
    // `from_config_table` loaded (default `en`, or an explicit `[voice].language`).
    // Must run after `load_initial_ui_config()` hydrates `current_ui` from disk.
    if let Some(ref pref) = app.current_ui.voice_stt_language {
        app.voice_config.language =
            crate::settings::canonical_voice_stt_language(Some(pref)).to_string();
    }
    // Resolve the per-tip contextual hints now that `current_ui` is hydrated and
    // propagate the prompt-relevant tips to any agents built at startup. New
    // agents adopt the gates at creation; settings toggles re-apply at runtime.
    let resolved_hints = xai_grok_shell::util::config::resolve_contextual_hints(
        &app.current_ui.contextual_hints,
        app.remote_contextual_hints.as_ref(),
    );
    app.apply_contextual_hints(resolved_hints);

    // Opt-in mouse-reporting toggle shortcut (Ctrl+R on scrollback). Off unless
    // explicitly enabled. Resolved in shell config (env override > effective
    // config > the parsed `UiConfig` field) so a partial `UiConfig` deserialize
    // failure cannot silently drop it.
    let mouse_toggle = xai_grok_shell::util::config::resolve_mouse_reporting_toggle(
        effective_config.as_ref(),
        &app.current_ui,
    );
    app.registry = crate::actions::ActionRegistry::defaults_with_config(mouse_toggle.value);
    // Cache the resolved flag so the `/toggle-mouse-reporting` slash command can
    // gate its visibility/execution without re-reading config on every keystroke.
    crate::app::MOUSE_REPORTING_TOGGLE_ENABLED
        .store(mouse_toggle.value, std::sync::atomic::Ordering::Release);
    let action_registered = app
        .registry
        .find(crate::actions::ActionId::ToggleMouseCapture)
        .is_some();
    crate::unified_log::info(
        "mouse_reporting_toggle.startup",
        None,
        Some(serde_json::json!({
            "enabled": mouse_toggle.value,
            "source": mouse_toggle.source.to_string(),
            "ui_config_field": app.current_ui.mouse_reporting_toggle,
            "action_registered": action_registered,
            "shortcut": "Ctrl+R",
            "context": "scrollback_focused_only",
            "slash_command": "/toggle-mouse-reporting",
            "note": "the toggle chord is scrollback-only; press Tab to focus scrollback first, or use /toggle-mouse-reporting from anywhere",
        })),
    );
    let config_session_bools = load_initial_config_session_bools();
    app.show_tips = config_session_bools.show_tips;
    app.auto_update = config_session_bools.auto_update;
    app.ask_user_question_timeout_enabled = config_session_bools.ask_user_question_timeout_enabled;
    // Prime thread-local caches so first render doesn't hit disk.
    crate::appearance::cache::prime(&app.current_ui);
    // Re-derive the render-value compact flag from the hydrated `current_ui`:
    // the seed above used the pre-hydration disk read, which layered/remote
    // config can contradict — the canonical single-writer corrects it (and
    // fans out to any startup agents) before the first draw.
    app.apply_effective_compact();

    // Apply the scroll settings from the caches (seeded by `prime` above;
    // GROK_SCROLL_SPEED/_MODE/_LINES + GROK_INVERT_SCROLL env overrides
    // apply on first load).
    app.scroll_config = crate::input::mouse::ScrollConfig::from_settings();

    // Fire-and-forget XTVERSION query; must sit immediately before the input
    // reader thread is spawned so no earlier stdin consumer eats the reply.
    crate::terminal::xtversion::probe_at_startup();

    // Read terminal events on a dedicated thread and forward them over an mpsc
    // channel. The main `select!` consumes via `input_rx.recv()`, which is
    // cancellation-safe: when another arm wins, the recv future is dropped and
    // re-created without losing the wakeup. Polling crossterm's `EventStream`
    // directly in the select is NOT safe -- dropping its `next()` future
    // mid-poll (a losing arm) strands its background waker (crossterm #936), so
    // input on an idle screen was not serviced until an unrelated arm happened
    // to re-poll (every ~20s via recap_poll). The always-on tracing_rx tick
    // used to mask this by re-polling ~30Hz; this removes that dependency.
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    // Set true around tty handoffs (e.g. $EDITOR) so the reader stops touching
    // stdin and the inheriting child process keeps every keystroke.
    let input_paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_paused = input_paused.clone();
    // Set by the reader once it has parked (stopped calling crossterm) so the
    // $EDITOR handoff can wait for it: poll/read share one global lock, so the
    // main-thread drain must be the sole crossterm caller.
    let reader_parked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_parked_thread = reader_parked.clone();
    std::thread::spawn(move || {
        use std::sync::atomic::Ordering;
        // Short enough that a pause / receiver-drop is observed promptly, long
        // enough to keep the thread parked when idle. A `poll()` timeout here
        // does NOT wake the main loop -- only a successful `send` does -- so the
        // idle event loop still parks (no reintroduced metronome tick).
        const POLL_TIMEOUT: Duration = Duration::from_millis(100);
        let mut consecutive_event_errors: u32 = 0;
        loop {
            // Shutdown observed within one poll cycle in every state (idle or
            // paused); the send() break below covers close-while-sending.
            if input_tx.is_closed() {
                break;
            }
            // While a tty handoff owns stdin, do not read(): the child (e.g. the
            // editor) must keep its bytes. Re-check soon without touching stdin.
            if reader_paused.load(Ordering::Acquire) {
                // Signal the handoff that the reader is no longer in crossterm.
                reader_parked_thread.store(true, Ordering::Release);
                std::thread::sleep(POLL_TIMEOUT);
                continue;
            }
            // Active path: this thread owns crossterm again this iteration.
            reader_parked_thread.store(false, Ordering::Release);
            // poll()+read() (not a bare blocking read) so the pause flag and a
            // dropped receiver are observed within POLL_TIMEOUT.
            let event = match crossterm::event::poll(POLL_TIMEOUT) {
                Ok(true) => crossterm::event::read(),
                Ok(false) => continue,
                Err(e) => Err(e),
            };
            match event {
                Ok(ev) => {
                    consecutive_event_errors = 0;
                    if input_tx.send(ev).is_err() {
                        break; // event loop has shut down
                    }
                }
                Err(e) => {
                    // VTE terminals / SSH PTYs can emit garbage that crossterm's
                    // parser rejects; skip transient errors rather than kill the
                    // TUI (ratatui#1275), bailing only if they never stop.
                    consecutive_event_errors += 1;
                    if consecutive_event_errors >= 50 {
                        tracing::error!(
                            "crossterm read returned {consecutive_event_errors} \
                             consecutive errors, exiting reader: {e}"
                        );
                        break;
                    }
                    tracing::warn!("crossterm read error (skipping): {e}");
                }
            }
        }
    });
    let mut acp_rx = connection.rx;
    let connection_cancel = connection.cancel;
    let mut leader_status_rx = connection.leader_status_rx;
    let mut tasks: JoinSet<TaskResult> = JoinSet::new();
    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::unbounded_channel::<effects::RestoreProgressMsg>();

    // Voice STT pipeline is started lazily on first successful `/voice` (see
    // `VoiceState::ColdStart`), not at launch — avoids background work for users
    // who never enable voice mode. `AUDIO_SUPPORTED` reflects whether mic
    // capture is compiled in: true for production CLI builds on macOS/Windows
    // (cpal) and Linux (subprocess recorder), false for Bazel builds (no
    // capture in the test sandbox).
    let mut voice_rx = None::<tokio::sync::mpsc::Receiver<xai_grok_voice::VoiceEvent>>;
    let voice_auth_factory = connection.auth_manager.clone();

    // Animation tick: only scheduled when there are running entries.
    let mut tick_interval = tick_interval;
    let mut animation_tick_at: Option<Instant> = None;

    // Whether the extra Kitty keyboard layer (WASD release events) is
    // currently pushed for the /gboom game. Synced to `gboom_active` each
    // iteration so it is popped on every close path.
    let mut gboom_keyboard_pushed = false;

    const BILLING_POLL_INTERVAL: Duration = Duration::from_secs(30);
    let mut billing_poll_at: Option<Instant> = None;

    const GATE_POLL_INTERVAL: Duration = Duration::from_secs(30);
    let mut gate_poll_at: Option<Instant> = None;

    // Free→paid subscription watch (see `app::subscription`).
    let mut subscription_watch_at: Option<Instant> = if app.subscription_watch_wanted() {
        app.subscription_watch_interval()
            .map(|iv| Instant::now() + iv)
    } else {
        None
    };

    // Leader-mode roster poll (FleetView dashboard). Only fires while the
    // dashboard is open AND we're connected via a leader. Armed to fire
    // immediately at loop start so an already-open dashboard refreshes
    // without waiting a full interval.
    const ROSTER_POLL_INTERVAL: Duration = Duration::from_secs(1);
    let mut roster_poll_at: Option<Instant> = Some(Instant::now());

    // Pre-generate the automatic "return-from-away" recap while the terminal is
    // unfocused, so it's already in the scrollback (instant) when the user
    // returns. The arm is a cheap no-op while focused / not-yet-eligible; the
    // heavy lifting (the model call) only fires once per away period via
    // `should_pregenerate_away_recap`.
    const RECAP_POLL_INTERVAL: Duration = Duration::from_secs(20);
    let mut recap_poll_at: Option<Instant> = Some(Instant::now() + RECAP_POLL_INTERVAL);

    // Seed the folder-trust verdict BEFORE the first render and before any
    // session is created (no repo-local MCP/LSP/hooks/plugins have loaded yet).
    // Feature-off (kill-switch / opt-out / local build) resolves `Trusted`, so
    // this stays `TrustState::Done`.
    seed_trust_state(&mut app, remote_settings.as_ref());

    // Initial render
    app.draw(terminal);

    // status only; shell auto-syncs post-auth
    if matches!(app.auth_state, AuthState::Done) {
        let effs = dispatch::dispatch(Action::RequestBundleStatus, &mut app);
        if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
            return Ok(make_run_result(&app));
        }
        // Fetch billing early so the welcome screen can show a credit warning.
        if app.usage_visible {
            let effs = vec![super::actions::Effect::FetchAppBilling];
            if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                return Ok(make_run_result(&app));
            }
        }
        // Fetch changelog off the render path so the welcome screen
        // can display bullets and /release-notes uses the cached result.
        let effs = vec![super::actions::Effect::FetchChangelog];
        if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
            return Ok(make_run_result(&app));
        }
        if !app.has_access() {
            gate_poll_at = Some(Instant::now() + GATE_POLL_INTERVAL);
        }
    }

    if !post_render_effects.is_empty()
        && process_effects(post_render_effects, &mut tasks, &mut app, &progress_tx)
    {
        return Ok(make_run_result(&app));
    }

    // Session startup from pre-materialized CLI intent.
    // These actions are dispatched UNCONDITIONALLY: the session-creating
    // chokepoints self-gate when auth + folder trust is closed.
    use crate::app::session_startup::MaterializedStartup;
    let startup_action = match &materialized {
        MaterializedStartup::Resume { session_id, .. } if args.worktree.is_some() => {
            tracing::info!(
                session_id,
                restore_code = ?app.restore_code,
                "RESTORE_CODE_DEBUG: worktree+resume path taken"
            );
            Some(Action::NewWorktreeSession {
                load_session_id: Some(session_id.clone()),
                label: args.worktree.as_ref().filter(|s| !s.is_empty()).cloned(),
                git_ref: args.worktree_ref.clone(),
            })
        }
        MaterializedStartup::Resume { session_id, .. } => {
            // CLI resume has no roster entry: `chat_kind` on LoadSession is the
            // conversation-entry bit only (false here). Process-wide `--chat`
            // still stamps kind=chat via SessionFlags.chat_mode in the load
            // effect; local Build disk rows are refused in dispatch / startup.
            Some(Action::LoadSession(
                session_id.clone(),
                session_cwd.clone(),
                false,
            ))
        }
        MaterializedStartup::NewWithId { session_id } if args.worktree.is_some() => {
            // Stash preferred id; `dispatch_new_worktree_session` consumes it and
            // passes through `CreateWorktreeSession.preferred_session_id` so the
            // worktree + ACP session use the CLI-chosen id (not an auto `pager-*`).
            app.deferred_startup.preferred_session_id = Some(session_id.clone());
            Some(Action::NewWorktreeSession {
                load_session_id: None,
                label: args.worktree.as_ref().filter(|s| !s.is_empty()).cloned(),
                git_ref: args.worktree_ref.clone(),
            })
        }
        MaterializedStartup::NewWithId { session_id } => {
            Some(Action::NewSessionWithId(session_id.clone()))
        }
        MaterializedStartup::Fork {
            parent_session_id,
            parent_cwd,
            new_session_id,
            ..
        } => Some(Action::StartupForkSession {
            parent_session_id: parent_session_id.clone(),
            parent_cwd: parent_cwd.clone().or(session_cwd.clone()),
            new_session_id: new_session_id.clone(),
        }),
        MaterializedStartup::NewAuto if args.worktree.is_some() => {
            Some(Action::NewWorktreeSession {
                load_session_id: None,
                label: args.worktree.as_ref().filter(|s| !s.is_empty()).cloned(),
                git_ref: args.worktree_ref.clone(),
            })
        }
        MaterializedStartup::NewAuto => None,
    };

    if let Some(action) = startup_action {
        let effs = dispatch::dispatch(action, &mut app);
        if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
            return Ok(make_run_result(&app));
        }
        app.draw(terminal);
    } else if args.worktree.is_some() {
        // --worktree only: create worktree + new session.
        let effs = dispatch::dispatch(
            Action::NewWorktreeSession {
                load_session_id: None,
                label: args.worktree.as_ref().filter(|s| !s.is_empty()).cloned(),
                git_ref: args.worktree_ref.clone(),
            },
            &mut app,
        );
        if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
            return Ok(make_run_result(&app));
        }
        app.draw(terminal);
    }

    // Initial prompt from the CLI positional (`grok "fix the bug"`). When
    // already authenticated, hand it to the shared dispatcher helper (same
    // `NewSession`/`SendPrompt` path the welcome screen uses). ZDR-blocked
    // accounts cannot start a session, so drop the prompt — this mirrors the
    // deferred post-login path, which clears the startup prompt for ZDR-blocked
    // accounts. When not yet authenticated, stash it for `AuthComplete`.
    if let Some(initial_prompt) = args.initial_prompt() {
        if !app.session_startup_allowed() {
            app.deferred_startup.prompt = Some(initial_prompt.to_string());
        } else if !app.is_zdr_blocked() {
            let effs = dispatch::dispatch_initial_prompt(&mut app, initial_prompt.to_string());
            if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                return Ok(make_run_result(&app));
            }
            app.draw(terminal);
        }
    }

    // `grok dashboard` startup: open the dashboard view immediately. The
    // CLI subcommand wrote a `GROK_OPEN_DASHBOARD_AT_STARTUP=1` env var
    // so we don't have to thread a flag through every arg struct.
    if std::env::var("GROK_OPEN_DASHBOARD_AT_STARTUP").as_deref() == Ok("1") {
        // SAFETY: we are pre-multithreaded init for this app loop.
        unsafe { std::env::remove_var("GROK_OPEN_DASHBOARD_AT_STARTUP") };
        if app.session_startup_allowed() {
            let effs = dispatch::dispatch(Action::OpenDashboard, &mut app);
            if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                return Ok(make_run_result(&app));
            }
            app.draw(terminal);
        } else {
            // Not signed in yet — the env var is already consumed, so
            // without a stash the request would be silently dropped and
            // the post-login flow would land on the welcome screen.
            // Defer to the `AuthComplete` handler (mirrors
            // the deferred session/prompt owner).
            app.deferred_startup.open_dashboard = true;
        }
    }

    // Minimal (scrollback-native) mode has no welcome screen: the live region
    // only renders for an Agent view. If nothing above already started a
    // session (no resume / initial prompt / worktree / dashboard), open an
    // empty one so the user lands directly at the prompt. Unauthenticated /
    // ZDR-blocked startup stays on Welcome, where `crate::minimal::live` shows
    // a sign-in hint instead of a blank region.
    if term_state.screen_mode.is_minimal()
        && matches!(app.active_view, ActiveView::Welcome)
        && !app.is_zdr_blocked()
    {
        if app.session_startup_allowed() {
            // Already authenticated + trusted: open the empty session now so the
            // user lands directly at the prompt.
            let effs = dispatch::dispatch(Action::NewSession, &mut app);
            if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                return Ok(make_run_result(&app));
            }
            app.draw(terminal);
        } else {
            // Sign-in (or folder-trust) still pending: minimal renders the
            // device / external sign-in flow in its live region. Defer the
            // empty-session creation so the post-auth (or post-trust) drain
            // (`drain_startup_actions`) opens it — otherwise minimal would
            // authenticate but never create a session, stranding the user on the
            // sign-in screen.
            app.deferred_startup.new_session = true;
        }
    }

    // Startup intents are now fully classified; only an untouched welcome can nudge.
    if let Some(effect) = app.begin_foreign_resume_detection()
        && process_effects(vec![effect], &mut tasks, &mut app, &progress_tx)
    {
        return Ok(make_run_result(&app));
    }

    // Schedule the first animation tick so live updates start immediately
    // (without waiting for user input).
    schedule_tick(&mut animation_tick_at, &app, tick_interval);

    // Resize debounce: during continuous terminal drags, dozens of resize
    // events fire per second. Each would trigger a full layout rebuild of all
    // entries (the most expensive per-frame operation). Instead of drawing on
    // every resize, we schedule a single deferred draw after the size stabilizes.
    const RESIZE_DEBOUNCE: Duration = Duration::from_millis(16);
    let mut resize_debounce_at: Option<Instant> = None;

    // Cadences resolved once above (env > auto > 16ms). AppView/Default stays hermetic.
    app.scroll_state.set_redraw_cadence(scroll_cadence);
    // ACP batch bound: large enough to keep the hundreds-buffered streaming
    // case batched (draws stay cadence-throttled regardless), small enough that
    // loop-top work (suspends, deadline re-derivation) never waits on an
    // unbounded drain during a token firehose.
    const ACP_DRAIN_BATCH_MAX: usize = 32;
    let mut last_draw_at = Instant::now();
    let mut draw_scheduled_at: Option<Instant> = None;

    let mut reconnect_reinit: Option<ReconnectReinit> = None;
    let mut reconnect_abort_handle: Option<tokio::task::AbortHandle> = None;
    // Highest `Connected` generation already handled. Starts at 0 — the
    // initial pre-reconnect watch value — so startup never triggers a reload;
    // any greater generation is a reconnect, even when the intermediate
    // `Reconnecting` state was coalesced away by the watch channel.
    let mut last_leader_generation: u64 = 0;

    // Persistent CSI fragment filter — carries parsing state across
    // drain_and_process calls so a mouse report split across batches is still
    // caught; a focus report is only swallowed when its `\e` and `[I`/`[O`
    // land in the same batch.
    let mut csi_filter = super::csi_filter::CsiFragmentFilter::new();

    // Swallows the fire-and-forget XTVERSION reply whenever it arrives;
    // armed only when the startup query is still unanswered.
    let mut xt_filter = super::xt_filter::XtversionFilter::new();

    // Background update check: resolves when the spawned update task
    // determines whether a newer version is available.
    let mut bg_update_rx = bg_update_rx;

    // `app::run` publishes the resolved theme into `theme_cache::CURRENT`
    // before `init_terminal` so `apply_cursor_color()` sees it. Pin the
    // invariant so a future refactor that drops the `theme_cache::set` call
    // fails loudly in debug builds rather than silently regressing the
    // initial cursor color.
    debug_assert_eq!(term_state.initial_theme, theme_cache::current_kind());
    let mut appearance_watcher =
        SystemAppearanceWatcher::start_if_auto(theme_cache::is_auto_mode());

    // Registered so the signal handler can request a graceful quit; see signal_handler.
    let quit_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    crate::app::signal_handler::set_quit_notify(quit_notify.clone());

    loop {
        // Pending $EDITOR / $PAGER suspends first: they can be armed by ANY
        // arm of the select below (input, ticks — e.g. minimal's incremental
        // /transcript build finishing inside a tick draw — tasks, ACP), so
        // consuming them here keeps the handoff immediate instead of waiting
        // for the next unrelated event.
        run_pending_suspends(
            &mut app,
            terminal,
            &input_paused,
            &reader_parked,
            &mut input_rx,
            &mut last_draw_at,
            &mut draw_scheduled_at,
        );

        // Lazy voice pipeline: only after `/voice` or Ctrl+Space while gates
        // allow. Consume the queued cold-start, carrying its hold-ownership and
        // bound target forward into the live recording it spawns.
        if let VoiceState::ColdStart { hold, target } = app.voice_state {
            if app.voice_cmd_tx.is_none() && app.voice_can_start_pipeline() {
                let voice_auth = crate::voice::build_voice_auth(voice_auth_factory.clone());
                let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(32);
                let (event_tx, event_rx) = tokio::sync::mpsc::channel(128);
                let voice_config = app.voice_config.clone();
                tokio::spawn(xai_grok_voice::run_voice_pipeline(
                    voice_config,
                    voice_auth.clone(),
                    cmd_rx,
                    event_tx,
                ));
                app.voice_auth = Some(voice_auth);
                app.voice_cmd_tx = Some(cmd_tx);
                voice_rx = Some(event_rx);
                tracing::info!("voice pipeline started (/voice or Ctrl+Space)");
                // The spawn is async, so begin capture now the pipeline is live
                // — but only if the user is still on a surface that can receive
                // dictation (an agent prompt or the dashboard dispatch input).
                // This runs at loop-top before any new input, so the surface
                // normally can't have changed since the keypress; the else-arm
                // is defensive cleanup so voice mode can't stay armed without
                // capture ever starting.
                if matches!(
                    app.active_view,
                    ActiveView::Agent(_) | ActiveView::AgentDashboard
                ) {
                    app.voice_begin_recording(target, hold);
                } else {
                    app.voice_state = VoiceState::Idle;
                    app.voice_ui_active = false;
                }
            } else if app.voice_cmd_tx.is_none() {
                app.voice_state = VoiceState::Idle;
                app.voice_ui_active = false;
                app.show_toast("Voice pipeline could not start — restart grok");
            } else {
                // Defensive: a queued start with the pipeline already up (which
                // shouldn't occur) — drop it so we don't re-enter every tick.
                app.voice_state = VoiceState::Idle;
            }
            // The lazy spawn runs at loop-top, after the key/slash arm already
            // drew (with capture still off). Render now so the recording banner
            // appears immediately instead of waiting for the next input or
            // network event to wake the select! loop.
            app.draw(terminal);
        }

        // Stop voice if the user has left the recording session (see method).
        app.enforce_voice_session_bound();

        // Keep the /gboom keyboard layer in sync with whether the game is
        // open, so WASD emit releases while it runs and the layer is popped
        // on every close path (Esc, game-over dismiss, session switch).
        let want_gboom_keyboard = app.gboom_active();
        if want_gboom_keyboard {
            if !gboom_keyboard_pushed {
                super::push_gboom_keyboard_flags();
                gboom_keyboard_pushed = true;
            }
            // Only the active game receives release events; any other open
            // game must drop its latched holds, or it resumes walking with
            // no key down when reopened after a tab/view switch.
            app.gboom_release_backgrounded_games();
        } else if gboom_keyboard_pushed {
            super::pop_gboom_keyboard_flags();
            gboom_keyboard_pushed = false;
            // No game is the active input target now (switched to a non-game
            // view); clear every game's holds for the same reason.
            app.gboom_release_all_games();
        }

        // Re-arm the dashboard roster poll when the dashboard is open but the
        // poll has gone dormant — i.e. the dashboard was just opened. The poll
        // arm leaves `roster_poll_at = None` only when it fired with the
        // dashboard closed, so this fires an immediate refresh exactly on the
        // closed→open transition rather than every iteration. Applies in both
        // modes: leader mode polls the live roster, non-leader mode polls the
        // local on-disk idle-session list.
        if roster_poll_at.is_none() && matches!(app.active_view, ActiveView::AgentDashboard) {
            roster_poll_at = Some(Instant::now());
        }

        // (Re-)arm the subscription watch on the dormant→wanted transition
        // and after each fired tick.
        if subscription_watch_at.is_none()
            && app.subscription_watch_wanted()
            && let Some(iv) = app.subscription_watch_interval()
        {
            subscription_watch_at = Some(Instant::now() + iv);
        }

        // Future that sleeps until the next animation tick, or waits forever if none.
        let animation_tick = async {
            match animation_tick_at {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };

        // Dedicated scroll clock, derived fresh each iteration — a pure
        // function of scroll state, so no arm can forget to reschedule it.
        // Armed only while a wheel/trackpad stream is active, at the state
        // machine's own deadline (16ms cadence flushes while lines are
        // pending, the 80ms stream-gap finalize otherwise): scroll pacing
        // must never ride the slower animation fps, which turned residual
        // flushes into visible jumps.
        let scroll_tick_at = {
            let now = Instant::now();
            app.scroll_state
                .scroll_clock_deadline(now.into_std())
                .map(|delay| now + delay)
        };

        // Future that sleeps until the scroll deadline, or waits forever.
        let scroll_tick = async {
            match scroll_tick_at {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };

        // Future that sleeps until the resize debounce fires, or waits forever.
        let resize_debounce = async {
            match resize_debounce_at {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };

        // Future that sleeps until a throttled draw fires, or waits forever.
        let deferred_draw = async {
            match draw_scheduled_at {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };

        let billing_poll = async {
            match billing_poll_at {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };

        let gate_poll = async {
            match gate_poll_at {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };

        let subscription_watch = async {
            match subscription_watch_at {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };

        let roster_poll = async {
            match roster_poll_at {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };

        let recap_poll = async {
            match recap_poll_at {
                Some(at) => sleep_until(at).await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            biased;

            // Leader disconnect: the bridge fires cancel when the IPC
            // channel closes.  Without this arm the loop would hang
            // because AppView holds the client-side tx, keeping acp_rx open.
            _ = connection_cancel.cancelled() => {
                break;
            }

            // Graceful-quit request from the signal handler. Kept high in the
            // biased order so a SIGTERM quit isn't starved by an ACP firehose.
            _ = quit_notify.notified() => {
                let effs = dispatch::dispatch(Action::Quit, &mut app);
                let _ = process_effects(effs, &mut tasks, &mut app, &progress_tx);
                break;
            }

            // Biased order: the ACP stream, task/progress completions, background
            // update, terminal/keyboard input, and all render/poll timers are polled
            // before the voice STT arm, which is deliberately LAST (see its note at
            // the bottom of this select). A hot mic streams interim transcripts at
            // ~5–20 Hz and can keep `voice_rx` effectively always-ready, so voice
            // must sit below everything or it would starve keypresses, the agent
            // stream, and animation ticks.

            // Gated on empty terminal input: a token firehose keeps this arm
            // ready at every biased poll, so without the gate buffered
            // wheel/key events sat in input_rx until the stream went quiet.
            // Safe: whenever the gate disables this arm, the input arm below
            // is immediately ready, and it drains its whole backlog per
            // iteration, so ACP resumes on the next loop (no reverse starve).
            // Gating, not reordering: moving input above ACP would flip the
            // starvation direction (streaming redraws starving behind held
            // keys), and cancel/quit must stay above the firehose regardless.
            msg = acp_rx.recv(), if input_rx.is_empty() => {
                let Some(msg) = msg else { break };
                let mut state_changed = acp_handler::handle(msg, &mut app);
                if !app.pending_effects.is_empty() {
                    let effs = std::mem::take(&mut app.pending_effects);
                    if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                        break;
                    }
                }

                // Drain immediately-ready ACP messages before drawing.
                // During streaming, dozens of messages queue per frame;
                // batching avoids per-message draws that starve terminal input.
                // Bounded, and cut short the moment input arrives, so wheel/key
                // events wait at most one batch — never a whole token flood.
                // Starts at 1: the recv() above consumed this batch's first message.
                let mut drained = 1;
                while drained < ACP_DRAIN_BATCH_MAX && input_rx.is_empty() {
                    let Ok(msg) = acp_rx.try_recv() else { break };
                    drained += 1;
                    state_changed |= acp_handler::handle(msg, &mut app);
                    if !app.pending_effects.is_empty() {
                        let effs = std::mem::take(&mut app.pending_effects);
                        if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                            return Ok(make_run_result(&app));
                        }
                    }
                }

                if state_changed {
                    schedule_tick(&mut animation_tick_at, &app, tick_interval);
                    resize_debounce_at = None;
                    // Cap paint rate so terminal input isn't starved during
                    // heavy ACP streaming.
                    let now = Instant::now();
                    if now.duration_since(last_draw_at) >= min_draw_interval {
                        app.update_notifications();
                        app.draw(terminal);
                        last_draw_at = now;
                        draw_scheduled_at = None;
                    } else if draw_scheduled_at.is_none() {
                        draw_scheduled_at = Some(last_draw_at + min_draw_interval);
                    }
                }
            }

            Some(join_result) = tasks.join_next() => {
                match join_result {
                    Ok(result) => {
                        let effs = dispatch::dispatch(Action::TaskComplete(result), &mut app);
                        if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                            break;
                        }
                        schedule_tick(&mut animation_tick_at, &app, tick_interval);
                        resize_debounce_at = None;

                        // Schedule/clear poll timers.
                        if app.billing_poll_wanted && billing_poll_at.is_none() {
                            billing_poll_at = Some(Instant::now() + BILLING_POLL_INTERVAL);
                        } else if !app.billing_poll_wanted {
                            billing_poll_at = None;
                        }
                        if !app.has_access() && gate_poll_at.is_none() {
                            gate_poll_at = Some(Instant::now() + GATE_POLL_INTERVAL);
                        } else if app.has_access() {
                            gate_poll_at = None;
                        }

                        app.draw(terminal);
                        last_draw_at = Instant::now();
                        draw_scheduled_at = None;
                    }
                    Err(join_err) => {
                        // Task was aborted (e.g., auth cancel) or panicked.
                        if join_err.is_cancelled() {
                            tracing::debug!("Spawned task was cancelled (aborted)");
                        } else {
                            tracing::error!("Spawned task panicked: {join_err}");
                        }
                    }
                }
            }

            Some(msg) = progress_rx.recv() => {
                let result = TaskResult::SessionRestoreProgress {
                    agent_id: msg.agent_id,
                    message: msg.message,
                };
                let effs = dispatch::dispatch(Action::TaskComplete(result), &mut app);
                if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                    break;
                }
                app.draw(terminal);
                last_draw_at = Instant::now();
                draw_scheduled_at = None;
            }

            // Background update check completed.
            result = async {
                match bg_update_rx.as_mut() {
                    Some(rx) => rx.await.ok().flatten(),
                    None => std::future::pending().await,
                }
            } => {
                // Consume the receiver so this arm becomes inert.
                bg_update_rx = None;
                if let Some(update) = result {
                    tracing::info!(
                        latest_version = %update.latest_version,
                        "Background update check: newer version available"
                    );
                    let latest = update.latest_version;
                    app.pending_update_version = Some(latest.clone());
                    // The full TUI surfaces this on the welcome screen, which
                    // minimal has none of — commit a one-line notice into
                    // native scrollback instead (update notice).
                    if term_state.screen_mode.is_minimal() {
                        dispatch::commit_minimal_update_notice(&mut app, &latest);
                    }
                    app.draw(terminal);
                    last_draw_at = Instant::now();
                    draw_scheduled_at = None;
                }
            }

            maybe_ev = input_rx.recv() => {
                // Terminal events arrive via the dedicated reader thread set up
                // near the top of this function. `None` means that thread ended.
                let Some(ev) = maybe_ev else { break };
                let result = drain_and_process(
                    ev, &mut input_rx, &mut app, &mut tasks, &progress_tx,
                    &mut csi_filter, &mut xt_filter,
                ).await;
                if result.should_quit {
                    break;
                }
                if !app.pending_effects.is_empty() {
                    let effs = std::mem::take(&mut app.pending_effects);
                    if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                        break;
                    }
                }
                // Opportunistic clipboard-image poll: this iteration already ran
                // for input / FocusGained / resize, so ride it (throttled,
                // changeCount-first). Never scheduled by a timer — an idle app
                // polls zero times. Run before schedule_tick so a freshly shown
                // tip's TTL arms the animation ticks that later clear it.
                let tip_shown = app.poll_clipboard_focus_tip();
                schedule_tick(&mut animation_tick_at, &app, tick_interval);
                if result.needs_draw || tip_shown {
                    if result.force_repaint {
                        // Refocus heal wins over the resize debounce: a coalesced same-size
                        // resize wouldn't autoresize-clear, so clear + full repaint now.
                        resize_debounce_at = None;
                        let _ = terminal.clear();
                        app.draw(terminal);
                        last_draw_at = Instant::now();
                        draw_scheduled_at = None;
                    } else if result.resize_only && !tip_shown {
                        // Debounce: schedule a single draw after the size stabilizes.
                        // Each new resize resets the timer so we only rebuild layout once.
                        resize_debounce_at = Some(Instant::now() + RESIZE_DEBOUNCE);
                    } else {
                        // Non-resize change (or a shown tip): draw immediately
                        // (picks up any pending resize too).
                        resize_debounce_at = None;
                        app.draw(terminal);
                        last_draw_at = Instant::now();
                        draw_scheduled_at = None;
                    }
                }

                // Sync appearance watcher when auto-mode toggles.
                sync_appearance_watcher(&mut appearance_watcher);
            }

            // Debounced resize: draw once the terminal size has stabilized.
            _ = resize_debounce => {
                resize_debounce_at = None;
                app.draw(terminal);
                last_draw_at = Instant::now();
                draw_scheduled_at = None;
                schedule_tick(&mut animation_tick_at, &app, tick_interval);
            }

            // Deferred draw: fires when an ACP-triggered draw was throttled.
            _ = deferred_draw => {
                draw_scheduled_at = None;
                app.draw(terminal);
                last_draw_at = Instant::now();
            }

            // Scroll clock: flush residual wheel/trackpad lines and detect
            // the 80ms stream gap on the 16ms redraw cadence, not the slower
            // animation fps. The next deadline is re-derived at loop top
            // from the post-tick scroll state.
            _ = scroll_tick => {
                if app.tick_scroll() {
                    app.draw(terminal);
                    last_draw_at = Instant::now();
                    draw_scheduled_at = None;
                }
                // Scroll dispatch can start work that animates (e.g. viewport
                // state), so keep the animation arm in sync too.
                schedule_tick(&mut animation_tick_at, &app, tick_interval);
            }

            _ = animation_tick => {
                animation_tick_at = None;
                // Lost-response recovery: finish any turn whose
                // `prompt_complete` broadcast outlived the grace window
                // without its `session/prompt` RPC response arriving
                // (see `dispatch::reconcile_overdue_turn_ends`).
                // `needs_animation()` keeps ticks alive while a reconcile
                // is armed, so this check cannot be starved.
                // Lost-response recovery: finish turns whose terminal armed
                // reconcile and grace expired without PromptResponse
                // (`dispatch::reconcile_overdue_turn_ends`). `needs_animation()`
                // keeps ticks alive while reconcile is armed.
                let reconciled = dispatch::reconcile_overdue_turn_ends(&mut app);
                if let Some(effs) = reconciled {
                    if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                        break;
                    }
                    app.draw(terminal);
                    last_draw_at = Instant::now();
                    draw_scheduled_at = None;
                } else if app.tick() {
                    app.draw(terminal);
                    last_draw_at = Instant::now();
                    draw_scheduled_at = None;
                }
                // Keep ticking as long as there are running animations
                // or pending actions waiting to expire.
                schedule_tick(&mut animation_tick_at, &app, tick_interval);
            }

            _ = billing_poll => {
                billing_poll_at = None;
                if let ActiveView::Agent(id) = app.active_view {
                    let effs = vec![Effect::FetchBilling {
                        agent_id: id,
                        silent: true,
                    }];
                    if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                        break;
                    }
                }
                if app.billing_poll_wanted {
                    billing_poll_at = Some(Instant::now() + BILLING_POLL_INTERVAL);
                }
            }

            _ = gate_poll => {
                gate_poll_at = None;
                let effs = vec![Effect::RefreshGate];
                if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                    break;
                }
                if !app.has_access() {
                    gate_poll_at = Some(Instant::now() + GATE_POLL_INTERVAL);
                }
            }

            _ = subscription_watch => {
                subscription_watch_at = None;
                let effs = app.fire_subscription_check("watch");
                if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                    break;
                }
            }

            _ = roster_poll => {
                roster_poll_at = None;
                // Only poll while the dashboard is open. When it is not active
                // we deliberately do NOT re-arm, so the loop isn't woken once
                // per second forever. In leader mode we poll the live FleetView
                // roster; outside leader mode we poll the local on-disk
                // idle-session list so the dashboard still shows idle sessions.
                let dashboard_open = matches!(app.active_view, ActiveView::AgentDashboard);
                if dashboard_open {
                    let eff = if leader_status_rx.is_some() {
                        Effect::FetchRoster
                    } else {
                        Effect::FetchDashboardSessions
                    };
                    if process_effects(vec![eff], &mut tasks, &mut app, &progress_tx) {
                        break;
                    }
                    roster_poll_at = Some(Instant::now() + ROSTER_POLL_INTERVAL);
                }
            }

            // Pre-generate the away recap so it's already on screen when the
            // user returns. Cheap no-op while focused / not-yet-eligible.
            _ = recap_poll => {
                if should_pregenerate_away_recap(&app) {
                    let effs = dispatch::dispatch(Action::SendRecap { auto: true }, &mut app);
                    if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                        break;
                    }
                }
                // Always re-arm: a cheap no-op fire while focused / not-yet-eligible.
                recap_poll_at = Some(Instant::now() + RECAP_POLL_INTERVAL);
            }

            // Hot-reload: config file changed (dev mode) or initial load.
            Ok(()) = config_watcher.changed() => {
                let mut config = config_watcher.current().clone();
                // Preserve fields persisted via `~/.grok/config.toml [ui]`
                // rather than `~/.grok/pager.toml`. The watcher only knows
                // about pager.toml, so a hot-reload would otherwise revert
                // these to their hardcoded defaults. Compact carries the
                // PRE-reload render value; the canonical re-derive below owns
                // any correction, so its fast path cannot skip a needed
                // prompt-widget fan-out (`set_appearance` alone never syncs
                // `PromptWidget.compact`).
                config.prompt.compact = app.appearance.prompt.compact;
                config.show_timestamps = app.appearance.show_timestamps;
                tick_interval = config.animation.tick_interval();
                crate::appearance::set_tab_width(config.scrollback.display.tab_width);
                app.set_appearance(config);
                app.apply_effective_compact();

                // Reload the scroll settings from the pager caches (resynced
                // when a setting changes via the settings registry).
                app.scroll_config = crate::input::mouse::ScrollConfig::from_settings();
                app.draw(terminal);
                last_draw_at = Instant::now();
                draw_scheduled_at = None;
            }

            // System appearance changed (auto-theme mode).
            Ok(()) = async {
                if let Some(ref mut w) = appearance_watcher {
                    w.changed().await
                } else {
                    std::future::pending::<Result<(), _>>().await
                }
            } => {
                if let Some(ref w) = appearance_watcher
                    && let Some(appearance) = w.current()
                {
                    let config = theme_cache::auto_theme_config();
                    let new_kind = system_appearance::to_theme_kind(
                        appearance,
                        config.dark_theme,
                        config.light_theme,
                    );
                    let current = Theme::current_kind();
                    let effective = Theme::apply_kind(new_kind);
                    if effective != current {
                        tracing::info!(
                            ?appearance,
                            new_theme = %effective.display_name(),
                            previous_theme = %current.display_name(),
                            "system appearance changed, switching theme"
                        );
                        app.draw(terminal);
                        last_draw_at = Instant::now();
                        draw_scheduled_at = None;
                    }
                }
            }

            // Leader connection status changes (reconnect lifecycle).
            Ok(()) = async {
                match leader_status_rx.as_mut() {
                    Some(rx) => rx.changed().await.map_err(|_| ()),
                    None => std::future::pending::<Result<(), ()>>().await,
                }
            } => {
                use crate::acp::leader_bridge::ConnectionStatus;

                let Some(rx) = leader_status_rx.as_mut() else {
                    // Guard: the async block above pends when None, but
                    // defensive code should never .unwrap() in production.
                    continue;
                };
                let status = rx.borrow_and_update().clone();
                match status {
                    ConnectionStatus::Reconnecting { attempt } => {
                        // Unified-log marker: an IPC reconnect mints a new leader-side
                        // ClientId, which orphans responses to this client's in-flight
                        // RPCs and drops outbound lines held across the swap — the
                        // root trigger of the stuck-cancel bug.
                        // Without this marker the reconnect is invisible in the
                        // unified log (it only surfaced as ghost `session loaded`
                        // replays with no matching `session.load.start`).
                        crate::unified_log::warn(
                            "leader.ipc.reconnecting",
                            None,
                            Some(serde_json::json!({ "attempt": attempt })),
                        );
                        app.show_toast(&format!(
                            "Disconnected. Reconnecting... (attempt {attempt})"
                        ));
                        app.draw(terminal);
                        last_draw_at = Instant::now();
                        draw_scheduled_at = None;
                    }
                    ConnectionStatus::Connected { generation }
                        if generation > last_leader_generation =>
                    {
                        crate::unified_log::warn(
                            "leader.ipc.reconnected",
                            None,
                            Some(serde_json::json!({
                                "generation": generation,
                                "open_sessions": app
                                    .agents
                                    .values()
                                    .filter_map(|a| {
                                        a.session.session_id.as_ref().map(|s| s.0.to_string())
                                    })
                                    .collect::<Vec<_>>(),
                            })),
                        );
                        last_leader_generation = generation;
                        app.reconnect_pending = true;
                        // Connection-scoped: a re-elected shell reseeds its push gen from wall clock,
                        // so a surviving higher watermark would silently drop its fresh pushes.
                        app.announcements_last_gen = 0;

                        // Cancel any in-flight re-init from a previous reconnect
                        // cycle and restore those agents' stashed transcripts —
                        // their load requests rode the now-dead connection.
                        if let Some(handle) = reconnect_abort_handle.take() {
                            handle.abort();
                        }
                        if let Some(prev) = reconnect_reinit.take() {
                            for prev_id in prev.agent_ids {
                                if let Some(agent) = app.agents.get_mut(&prev_id) {
                                    agent.finish_session_reload(prev.generation, false);
                                }
                            }
                        }

                        // Open a reload window on EVERY agent with a session
                        // (active tab first so the visible one restores
                        // fastest): a freshly (re-)elected leader has no
                        // sessions in memory, so reloading only the active
                        // session would leave every other tab on a session id
                        // the new leader has never seen ("unknown session id"
                        // on its next prompt). Replay is staged into fresh
                        // state per agent and each existing transcript stays
                        // recoverable until its load outcome is known.
                        let fallback_cwd = app.cwd.clone();
                        let active_agent_id = match app.active_view {
                            ActiveView::Agent(id) => Some(id),
                            _ => None,
                        };
                        let mut agent_ids: Vec<super::agent::AgentId> =
                            app.agents.keys().copied().collect();
                        agent_ids.sort_by_key(|id| Some(*id) != active_agent_id);
                        let mut reload_agent_ids = Vec::new();
                        let mut load_plans = Vec::new();
                        for id in agent_ids {
                            let Some(agent) = app.agents.get_mut(&id) else {
                                continue;
                            };
                            let Some(plan) = plan_reconnect_load(agent, &fallback_cwd) else {
                                continue;
                            };
                            // Keep the per-session display flag in lockstep with the
                            // enforcement value (`autoMode`) we just re-seeded on this
                            // agent (yolo wins, computed inside `plan_reconnect_load`).
                            agent.session.auto_mode =
                                plan.meta["autoMode"].as_bool().unwrap_or(false);
                            agent.begin_session_reload(generation);
                            // The reload adoption supersedes a pre-disconnect stash.
                            app.pending_running_adoptions.remove(&id);
                            reload_agent_ids.push(id);
                            load_plans.push((id, plan));
                        }
                        let any_reload = !reload_agent_ids.is_empty();
                        // Per-agent `auto_mode` was just re-seeded from the reload
                        // meta; keep `/auto` feature-gate slash visibility in sync.
                        app.sync_permission_mode_slash_gate();

                        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                        reconnect_reinit = Some(ReconnectReinit {
                            rx: done_rx,
                            agent_ids: reload_agent_ids,
                            generation,
                        });

                        let acp_tx = app.acp_tx.clone();
                        let join_handle = tokio::spawn(async move {
                            // 30 s for initialize/authenticate plus a budget per
                            // session/load (each load replays history and may
                            // respawn MCP servers on the new leader).
                            let timeout = Duration::from_secs(
                                (30 + 30 * load_plans.len() as u64).min(300),
                            );

                            // Inner result: `None` = init/auth failure (no
                            // load was attempted); `Some(loads)` = per-agent
                            // load outcomes with the optional mid-turn running
                            // prompt id from each reload response.
                            let ok = tokio::time::timeout(timeout, async {
                                let init_req = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(acp::ClientCapabilities::new().fs(acp::FileSystemCapabilities::new()).terminal(false)).meta(serde_json::json!({
                                        "clientType": PAGER_CLIENT_TYPE,
                                        "clientVersion": PAGER_CLIENT_VERSION,
                                    }).as_object().cloned());
                                if let Err(e) = acp_send(init_req, &acp_tx).await {
                                    tracing::error!(error = %e, "reconnect: re-initialize failed");
                                    return None;
                                }

                                let auth_req = acp::AuthenticateRequest::new(acp::AuthMethodId::new(crate::obf::auth::CACHED_TOKEN!()));
                                if let Err(e) = acp_send(auth_req, &acp_tx).await {
                                    tracing::warn!(error = %e, "reconnect: re-authenticate failed");
                                }

                                let mut loads = Vec::with_capacity(load_plans.len());
                                for (agent_id, plan) in load_plans {
                                    // Reconnect path — no resolved compat in scope; default
                                    // (all-on) preserves existing behavior.
                                    let mcp_servers = xai_grok_shell::util::config::load_mcp_servers(
                                        &plan.cwd,
                                        &xai_grok_tools::types::compat::CompatConfig::default(),
                                    );
                                    let load_req = acp::LoadSessionRequest::new(plan.session_id, plan.cwd).mcp_servers(mcp_servers).meta(plan.meta.as_object().cloned());
                                    match acp_send(load_req, &acp_tx).await {
                                        Ok(resp) => {
                                            loads.push(AgentLoadOutcome {
                                                agent_id,
                                                success: true,
                                                running_prompt_id:
                                                    effects::parse_session_load_running_prompt_id(
                                                        resp.meta.as_ref(),
                                                    ),
                                            });
                                        }
                                        Err(e) => {
                                            tracing::error!(error = %e, "reconnect: reload session failed");
                                            // Keep restoring the remaining sessions —
                                            // one broken session must not doom the rest.
                                            loads.push(AgentLoadOutcome {
                                                agent_id,
                                                success: false,
                                                running_prompt_id: None,
                                            });
                                        }
                                    }
                                }
                                Some(loads)
                            })
                            .await;

                            let outcome = match ok {
                                Ok(Some(loads)) => ReinitOutcome {
                                    init_ok: true,
                                    loads,
                                },
                                Ok(None) => ReinitOutcome {
                                    init_ok: false,
                                    loads: Vec::new(),
                                },
                                Err(_) => {
                                    tracing::error!("reconnect re-initialization timed out");
                                    ReinitOutcome {
                                        init_ok: false,
                                        loads: Vec::new(),
                                    }
                                }
                            };
                            let _ = done_tx.send(outcome);
                        });
                        reconnect_abort_handle = Some(join_handle.abort_handle());

                        app.show_toast(if any_reload {
                            "Reconnected. Reloading session..."
                        } else {
                            "Reconnected. Re-initializing..."
                        });
                        app.draw(terminal);
                        last_draw_at = Instant::now();
                        draw_scheduled_at = None;
                    }
                    ConnectionStatus::Failed { ref error } => {
                        app.show_toast(&format!("Connection failed: {error}"));
                        app.draw(terminal);
                        last_draw_at = Instant::now();
                        draw_scheduled_at = None;
                    }
                    _ => {}
                }
            }

            // Reconnect re-initialization completed (or failed).
            result = async {
                match reconnect_reinit.as_mut() {
                    Some(pending) => (&mut pending.rx).await,
                    None => std::future::pending::<Result<ReinitOutcome, _>>().await,
                }
            } => {
                let Some(pending) = reconnect_reinit.take() else {
                    continue;
                };
                reconnect_abort_handle = None;
                app.reconnect_pending = false;

                let outcome = match result {
                    Ok(outcome) => outcome,
                    Err(_) => {
                        tracing::error!("reconnect re-init task failed (sender dropped)");
                        ReinitOutcome { init_ok: false, loads: Vec::new() }
                    }
                };

                // Finalize the reload windows on the agents the re-init was
                // started for — NOT whatever view is active now (see
                // `SessionReload` for the outcome handling). Each window
                // resolves on ITS load outcome (one broken session must not
                // discard the other tabs' replayed transcripts), then a
                // mid-reconnect running turn is adopted, mirroring the
                // `SessionLoaded` adoption in dispatch.rs.
                let mut loads: std::collections::HashMap<_, _> = outcome
                    .loads
                    .into_iter()
                    .map(|l| (l.agent_id, (l.success, l.running_prompt_id)))
                    .collect();
                // Resolved BEFORE the finalize loop drains `loads` via `remove`
                // (see `reconnect_restore_outcome`).
                let active_agent_id = match app.active_view {
                    ActiveView::Agent(id) => Some(id),
                    _ => None,
                };
                let (restored, active_restored) = reconnect_restore_outcome(
                    outcome.init_ok,
                    &pending.agent_ids,
                    &loads,
                    active_agent_id,
                );
                for id in &pending.agent_ids {
                    let (ok, running_prompt_id) = loads.remove(id).unwrap_or((false, None));
                    if let Some(agent) = app.agents.get_mut(id) {
                        agent.finalize_reload_and_maybe_adopt(
                            pending.generation,
                            ok,
                            running_prompt_id,
                        );
                    }
                }

                if pending.agent_ids.is_empty() {
                    // Nothing was reloaded (no open sessions at reconnect).
                    app.show_toast("Reconnected.");
                } else if restored {
                    app.show_toast("Session restored. In-progress tools and terminals were lost.");
                } else {
                    app.show_toast("Session restore failed. Kept the existing transcript.");
                }

                // Re-trigger the queue drain suppressed during the outage: every
                // normal trigger (PromptResponse, DrainQueue, send-prompt,
                // session-created) early-returns while `reconnect_pending` is set
                // and defers here, and the agent was just force-idled above. Gate
                // on the active tab's own restore (see `reconnect_restore_outcome`):
                // a failed active restore suppresses the drain, since sending into
                // an unrestored session would be wrong.
                if active_restored {
                    let drain_effects = dispatch::dispatch(Action::DrainQueue, &mut app);
                    if process_effects(drain_effects, &mut tasks, &mut app, &progress_tx) {
                        return Ok(make_run_result(&app));
                    }
                }

                app.draw(terminal);
                last_draw_at = Instant::now();
                draw_scheduled_at = None;
            }

            // Voice STT — DELIBERATELY THE LAST (lowest-priority) arm. In a
            // biased select, an arm that is ready on most iterations masks every
            // arm below it. A hot mic (toggle capture stays open across pauses)
            // streams interim transcripts at ~5–20 Hz and can backlog the 128-slot
            // channel during a burst, so `voice_rx` is effectively always-ready.
            // Keeping it last guarantees it can never starve cancellation, the ACP
            // stream, task/progress completions, keyboard input, or the render/
            // animation/poll timers — voice is only serviced when nothing else is
            // pending. Draw throttle uses min_draw_interval.
            ev = async {
                match voice_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match ev {
                    Some(ev) => {
                        let needs_draw = crate::voice::handle_voice_event(&mut app, ev);
                        if needs_draw {
                            schedule_tick(&mut animation_tick_at, &app, tick_interval);
                            let now = Instant::now();
                            if now.duration_since(last_draw_at) >= min_draw_interval {
                                app.update_notifications();
                                app.draw(terminal);
                                last_draw_at = now;
                                draw_scheduled_at = None;
                            } else if draw_scheduled_at.is_none() {
                                draw_scheduled_at = Some(last_draw_at + min_draw_interval);
                            }
                        }
                        if !app.pending_effects.is_empty() {
                            let effs = std::mem::take(&mut app.pending_effects);
                            if process_effects(effs, &mut tasks, &mut app, &progress_tx) {
                                break;
                            }
                        }
                    }
                    // Closed channel: revert to pending() (avoid hot-loop on None).
                    None => {
                        voice_rx = None;
                        let was_listening = app.voice_listening();
                        app.voice_cmd_tx = None;
                        // Pipeline is gone: drop any session/interim entirely.
                        app.voice_reset();
                        if was_listening {
                            app.show_toast("Voice stopped — pipeline ended");
                        }
                        app.draw(terminal);
                        last_draw_at = Instant::now();
                        draw_scheduled_at = None;
                    }
                }
            }
        }
    }

    app.notification_service.shutdown();

    Ok(make_run_result(&app))
}

/// Schedule the next animation tick if there are running entries and none is pending.
///
/// Load `UiConfig` from the shell's layered config at startup.
/// Falls back to `UiConfig::default()` on any failure.
pub(crate) fn load_initial_ui_config() -> xai_grok_shell::agent::config::UiConfig {
    use xai_grok_shell::agent::config::UiConfig;
    let Ok(root) = xai_grok_shell::config::load_effective_config() else {
        return UiConfig::default();
    };
    let Some(ui_value) = root.get("ui").cloned() else {
        return UiConfig::default();
    };
    ui_value.try_into::<UiConfig>().unwrap_or_default()
}

/// Config `Option<bool>` mirrors seeded once at startup. `None` = no
/// TOML override; the modal falls back to the per-setting default.
#[derive(Default)]
struct InitialConfigSessionBools {
    show_tips: Option<bool>,
    auto_update: Option<bool>,
    ask_user_question_timeout_enabled: Option<bool>,
}

fn load_initial_config_session_bools() -> InitialConfigSessionBools {
    let Ok(root) = xai_grok_shell::config::load_effective_config() else {
        return InitialConfigSessionBools::default();
    };
    let cli_bool = |key: &str| -> Option<bool> { root.get("cli")?.get(key)?.as_bool() };
    InitialConfigSessionBools {
        show_tips: cli_bool("show_tips"),
        auto_update: cli_bool("auto_update"),
        ask_user_question_timeout_enabled: root
            .get("toolset")
            .and_then(|t| t.get("ask_user_question"))
            .and_then(|a| a.get("timeout_enabled"))
            .and_then(|v| v.as_bool()),
    }
}

/// Whether to pre-generate the automatic "return-from-away" recap right now.
///
/// True only when the terminal has been unfocused past the recap threshold
/// (once per away period, gated by [`FocusTracker::recap_due`]), the shell has
/// rolled out session recap (`session_recap_available`), the user has not opted
/// out via `ui.notifications.session_recap`, and the active agent has *finished
/// its turn* with nothing pending that could wake it — i.e. idle, no modal, no
/// pending question, an established session, and no running background task (a
/// bg task completing can auto-wake the agent). Generating it now means the
/// recap is already in the scrollback when the user returns.
/// Sync shell `sessionRecap` into execution gate + every existing slash surface.
/// Dashboard created later is seeded in `dispatch_open_dashboard`.
fn apply_session_recap_available(app: &mut AppView, available: bool) {
    app.session_recap_available = available;
    for agent in app.agents.values_mut() {
        agent.set_session_recap_available(available);
    }
    app.welcome_prompt.set_recap_visible(available);
    if let Some(dashboard) = app.dashboard.as_mut() {
        dashboard.set_recap_visible(available);
    }
}

fn should_pregenerate_away_recap(app: &AppView) -> bool {
    if !(app.session_recap_available
        && app.notification_service.focus_tracker.recap_due()
        && app.notification_service.config().session_recap)
    {
        return false;
    }
    let ActiveView::Agent(id) = app.active_view else {
        return false;
    };
    app.agents.get(&id).is_some_and(|agent| {
        agent.session.state.is_idle()
            && agent.active_modal.is_none()
            && agent.question_view.is_none()
            && agent.session.session_id.is_some()
            && !agent.session.has_running_bg_tasks()
    })
}

fn schedule_tick(tick_at: &mut Option<Instant>, app: &AppView, interval: Duration) {
    if tick_at.is_none() {
        let interval = match app.tick_demand() {
            crate::app::app_view::TickDemand::None => return,
            // A view can request a faster cadence than the configured
            // animation fps (e.g. the /gboom easter egg targets ~30 fps).
            crate::app::app_view::TickDemand::Fast => match app.tick_interval_ceiling() {
                Some(ceiling) => interval.min(ceiling),
                None => interval,
            },
            // Only low-frequency work (welcome shimmer, Cmd link poll):
            // don't spin the full 30fps loop for it.
            crate::app::app_view::TickDemand::Slow => {
                interval.max(crate::app::app_view::SLOW_TICK_INTERVAL)
            }
        };
        *tick_at = Some(Instant::now() + interval);
    }
}

/// Sync `appearance_watcher` with the current `AUTO_MODE` flag.
/// Starts or stops the watcher as needed; no-op when consistent.
fn sync_appearance_watcher(watcher: &mut Option<SystemAppearanceWatcher>) {
    let should_auto = theme_cache::is_auto_mode();
    if should_auto != watcher.is_some() {
        *watcher = SystemAppearanceWatcher::start_if_auto(should_auto);
    }
}

/// Build [`ExitInfo`] from the active agent's session (if any).
///
/// `exit_info` is only consumed on the plain-quit path; a pending `relaunch`
/// short-circuits before it is read and carries its own session id.
fn make_run_result(app: &AppView) -> RunResult {
    RunResult {
        exit_info: app.active_session_id().map(|sid| super::ExitInfo {
            session_id: sid.to_string(),
            minimal: app.screen_mode.is_minimal(),
        }),
        quit_for_update: app.quit_for_update,
        relaunch: app.relaunch.clone(),
    }
}

/// Result of draining and processing terminal events.
struct DrainResult {
    /// Whether any event produced a visual change requiring a draw.
    needs_draw: bool,
    /// Whether the app should quit.
    should_quit: bool,
    /// Whether resize was the only source of change (no key/mouse/action changes).
    /// When true, the caller should debounce the draw to avoid redundant layout
    /// rebuilds during continuous terminal resize drags.
    resize_only: bool,
    /// Whether the next draw must be preceded by a full clear+repaint, set on
    /// refocus in editor/multiplexer contexts to heal out-of-band stranded rows.
    force_repaint: bool,
}

struct RoutedInputEvent {
    event: Event,
    paste_provenance: PasteProvenance,
}

fn normalize_input_event(event: Event) -> RoutedInputEvent {
    #[cfg(target_os = "linux")]
    {
        use crossterm::event::{MouseButton, MouseEventKind};
        let is_unmodified_middle_down = match &event {
            Event::Mouse(mouse) => {
                mouse.kind == MouseEventKind::Down(MouseButton::Middle)
                    && mouse.modifiers.is_empty()
            }
            _ => false,
        };
        if is_unmodified_middle_down
            && let Some(text) = crate::clipboard::system_primary_selection_get()
        {
            return RoutedInputEvent {
                event: Event::Paste(text),
                paste_provenance: PasteProvenance::X11Primary,
            };
        }
    }
    RoutedInputEvent {
        event,
        paste_provenance: PasteProvenance::Terminal,
    }
}

/// Process a terminal event, then drain any buffered events before returning.
///
/// Crossterm buffers input events while the app is drawing. Without draining,
/// each event triggers a separate `draw()` call. When draw takes longer than
/// the scroll cadence (16 ms), hundreds of buffered scroll events cause hundreds
/// of sequential draws, freezing the UI for seconds or minutes.
///
/// Runs [`coalesce_rapid_keys`] and the persistent
/// [`CsiFragmentFilter`](super::csi_filter::CsiFragmentFilter) before
/// processing to fix paste on terminals without bracketed paste (e.g.
/// Windows PowerShell) and filter leaked CSI fragments (SGR mouse and focus reports).
async fn drain_and_process(
    first: Event,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    app: &mut AppView,
    tasks: &mut JoinSet<TaskResult>,
    progress_tx: &tokio::sync::mpsc::UnboundedSender<effects::RestoreProgressMsg>,
    csi_filter: &mut super::csi_filter::CsiFragmentFilter,
    xt_filter: &mut super::xt_filter::XtversionFilter,
) -> DrainResult {
    let mut needs_draw = false;
    let mut had_resize = false;
    let mut had_non_resize_change = false;
    let mut force_repaint = false;

    // Collect all immediately-available events for paste coalescing.
    let mut raw_events = vec![first];
    drain_immediate(&mut raw_events, input_rx);

    // XTVERSION reply removal must precede paste coalescing so reply chars
    // are never folded into a synthetic Paste.
    if xt_filter.armed() {
        raw_events =
            super::xt_filter::filter_with_fragment_wait(xt_filter, raw_events, input_rx).await;
    }

    // On terminals without bracketed paste, try to capture more events
    // that may still be in transit from the input reader thread.
    if should_extend_for_paste(&raw_events) && detect_paste(&mut raw_events, input_rx).await {
        collect_remaining_paste(&mut raw_events, input_rx).await;
        // The paste extension pulled more events off the channel without
        // running them through the still-armed filter — a late or split
        // XTVERSION reply could otherwise be folded into the paste.
        if xt_filter.armed() {
            raw_events =
                super::xt_filter::filter_with_fragment_wait(xt_filter, raw_events, input_rx).await;
        }
    }

    // The /gboom game tracks keys by press → release, so it needs the
    // release events that `coalesce_rapid_keys` strips (and it never
    // pastes). Skip coalescing while it owns input.
    let coalesced = if app.gboom_active() {
        raw_events
    } else {
        coalesce_rapid_keys(raw_events)
    };
    let coalesced = csi_filter.filter(coalesced);
    let coalesced = coalesced
        .into_iter()
        .map(normalize_input_event)
        .collect::<Vec<_>>();

    let mut handle_one = |routed: &RoutedInputEvent| -> bool {
        let ev = &routed.event;
        match ev {
            Event::FocusGained => {
                // Force a full repaint on refocus to heal out-of-band stranded rows.
                // Sets needs_draw (not had_non_resize_change); the draw site honors force_repaint
                // ahead of the resize debounce, clearing even a coalesced same-size resize.
                if crate::terminal::terminal_context().repaints_pane_out_of_band() {
                    force_repaint = true;
                    needs_draw = true;
                }
                // Capture recap eligibility BEFORE on_focus_gained() clears the
                // away timer. Auto recap requires the shell rollout flag plus
                // the notifications opt-in; manual `/recap` only needs the flag.
                let recap_due = app.session_recap_available
                    && app.notification_service.focus_tracker.recap_due()
                    && app.notification_service.config().session_recap;
                app.notification_service.focus_tracker.on_focus_gained();
                // Pre-warm AppKit's lazy dlopen off the UI thread (once) so the
                // first changeCount poll after returning is just the cheap
                // metadata read and never stalls a frame on the framework load.
                // FocusGained is itself an active loop iteration, so the
                // opportunistic poll (driven after drain_and_process) does the
                // actual clipboard check — no debounce, no timer, and
                // `needs_animation` is never kept hot for it.
                if app.contextual_hints.image_input
                    && crate::clipboard::clipboard_image_probe_supported()
                {
                    crate::clipboard::prewarm_image_probe();
                }
                // The user may have just subscribed in the browser and
                // tabbed back.
                let effs = app.fire_subscription_check("focus");
                if process_effects(effs, tasks, app, progress_tx) {
                    return true;
                }
                // Restore Prompt on refocus: needs-input overlay always, else idle non-vim.
                match app.active_view {
                    ActiveView::Agent(id) => {
                        if let Some(agent) = app.agents.get_mut(&id)
                            && agent.should_restore_prompt_on_focus_gained()
                        {
                            agent.set_active_pane(crate::views::agent::ActivePane::Prompt, false);
                            needs_draw = true;
                            had_non_resize_change = true;
                        }

                        // Automatic "where was I" recap: the user just returned
                        // after being away long enough. Only when the session is
                        // idle and not blocked by a modal or pending question.
                        // Compute eligibility into a bool first so the immutable
                        // agent borrow is dropped before dispatch (&mut app).
                        let eligible = app.agents.get(&id).is_some_and(|agent| {
                            agent.session.state.is_idle()
                                && agent.active_modal.is_none()
                                && agent.question_view.is_none()
                                && agent.session.session_id.is_some()
                                && !agent.session.has_running_bg_tasks()
                        });
                        if recap_due && eligible {
                            let effs = dispatch::dispatch(
                                crate::app::actions::Action::SendRecap { auto: true },
                                app,
                            );
                            if process_effects(effs, tasks, app, progress_tx) {
                                return true;
                            }
                            needs_draw = true;
                            had_non_resize_change = true;
                        }
                    }
                    ActiveView::Welcome => {
                        if matches!(app.auth_state, AuthState::Done) && !app.welcome_prompt_focused
                        {
                            app.welcome_prompt_focused = true;
                            needs_draw = true;
                            had_non_resize_change = true;
                        }
                    }
                    // The dashboard manages its own input/overview focus
                    // (`list_focused`); refocusing the terminal must not
                    // override the user's choice (e.g. vim overview focus).
                    ActiveView::AgentDashboard => {}
                }
                return false;
            }
            Event::FocusLost => {
                app.notification_service.focus_tracker.on_focus_lost();
                // The /gboom game latches held keys until their release; a
                // release can be lost while unfocused, so stop all movement.
                if app.gboom_active() {
                    app.gboom_release_all_games();
                    needs_draw = true;
                }
                return false;
            }
            _ => {}
        }
        // Voice capture chord (Ctrl+Space or F8), handled here before normal
        // routing so the release reaches us and the key never lands as text.
        // Hold-to-talk under Kitty (press records, release stops), else tap
        // toggle. A release is only ours when a hold session owns it, so a bare
        // Space release (Ctrl lifted first) stops hold-to-talk without eating
        // every Space release during normal typing.
        if let Event::Key(ke) = ev
            && app.voice_mode_enabled
            && xai_grok_voice::AUDIO_SUPPORTED
            && is_voice_chord(ke)
            && (ke.kind != KeyEventKind::Release || app.voice_hold_owned())
        {
            // Hold-to-talk only when selected AND the terminal reports key
            // releases (Kitty protocol); otherwise fall back to a tap toggle.
            let hold_mode = crate::settings::canonical_voice_capture_mode(
                app.current_ui.voice_capture_mode.as_deref(),
            ) == "hold";
            let action = voice_chord_action(
                hold_mode,
                crate::app::kitty_flags_pushed(),
                ke.kind,
                app.voice_listening(),
                app.voice_hold_owned(),
            );
            if let Some(action) = action {
                let effs = dispatch::dispatch(action, app);
                if process_effects(effs, tasks, app, progress_tx) {
                    return true;
                }
                needs_draw = true;
                had_non_resize_change = true;
            }
            return false;
        }
        let is_resize = matches!(ev, Event::Resize(_, _));
        match app.handle_input_with_paste_provenance(ev, routed.paste_provenance) {
            InputOutcome::Action(action) => {
                let effs = dispatch::dispatch(action, app);
                if process_effects(effs, tasks, app, progress_tx) {
                    return true;
                }
                needs_draw = true;
                had_non_resize_change = true;
            }
            InputOutcome::ActionThenForward(action) => {
                // Dispatch the action (e.g. create session), then re-process
                // the same event through the now-active view so the input
                // (character, paste) lands in the session's prompt.
                let effs = dispatch::dispatch(action, app);
                if process_effects(effs, tasks, app, progress_tx) {
                    return true;
                }
                if let InputOutcome::Action(follow_up) =
                    app.handle_input_with_paste_provenance(ev, routed.paste_provenance)
                {
                    let effs = dispatch::dispatch(follow_up, app);
                    if process_effects(effs, tasks, app, progress_tx) {
                        return true;
                    }
                }
                needs_draw = true;
                had_non_resize_change = true;
            }
            InputOutcome::ActionPair(first, second) => {
                // Dispatch both in order; first must fully resolve
                // before second (e.g. revert preview then open reset).
                let effs = dispatch::dispatch(first, app);
                if process_effects(effs, tasks, app, progress_tx) {
                    return true;
                }
                let effs = dispatch::dispatch(second, app);
                if process_effects(effs, tasks, app, progress_tx) {
                    return true;
                }
                needs_draw = true;
                had_non_resize_change = true;
            }
            InputOutcome::Changed => {
                needs_draw = true;
                if is_resize {
                    had_resize = true;
                } else {
                    had_non_resize_change = true;
                }
            }
            // AppView converts ArmPending → Changed; defensive if one slips through.
            InputOutcome::ArmPending { .. } => {
                needs_draw = true;
                had_non_resize_change = true;
            }
            InputOutcome::Unchanged => {}
        }
        false
    };

    for routed in &coalesced {
        if handle_one(routed) {
            return DrainResult {
                needs_draw,
                should_quit: true,
                resize_only: false,
                force_repaint: false,
            };
        }
    }

    DrainResult {
        needs_draw,
        should_quit: false,
        resize_only: had_resize && !had_non_resize_change,
        force_repaint,
    }
}

// ── Paste coalescing for terminals without bracketed paste ───────────

/// Timeout for the first extension round (detection).  If no event
/// arrives within this window the batch was a normal keystroke.
const PASTE_DETECT_TIMEOUT: Duration = Duration::from_millis(2);

/// Timeout for subsequent rounds once paste has been detected.
const PASTE_CONTINUE_TIMEOUT: Duration = Duration::from_millis(10);

/// Safety cap on events accumulated in one extension pass.
const PASTE_EXTEND_MAX_EVENTS: usize = 5_000;

/// Returns `true` when the batch contains pasteable key events but no
/// `Event::Paste` (i.e. bracketed paste is not handling it).
fn should_extend_for_paste(events: &[Event]) -> bool {
    !events.iter().any(|e| matches!(e, Event::Paste(_)))
        && events.iter().any(is_pasteable_key_event)
}

/// Wait [`PASTE_DETECT_TIMEOUT`] for a follow-up event.  Returns `true`
/// if a **pasteable key event** arrives within the window.  Non-key events
/// (mouse, focus, releases) are collected but do not count as paste evidence.
async fn detect_paste(
    batch: &mut Vec<Event>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
) -> bool {
    match tokio::time::timeout(PASTE_DETECT_TIMEOUT, input_rx.recv()).await {
        Ok(Some(ev)) => {
            let prev_len = batch.len();
            batch.push(ev);
            drain_immediate(batch, input_rx);
            batch[prev_len..].iter().any(is_pasteable_key_event)
        }
        _ => false,
    }
}

/// Collect remaining paste events using [`PASTE_CONTINUE_TIMEOUT`].
/// Only pasteable key events extend the timeout; non-key events are
/// collected but do not keep the loop alive.
async fn collect_remaining_paste(
    batch: &mut Vec<Event>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
) {
    let mut extended = 0usize;
    loop {
        if extended >= PASTE_EXTEND_MAX_EVENTS {
            break;
        }
        match tokio::time::timeout(PASTE_CONTINUE_TIMEOUT, input_rx.recv()).await {
            Ok(Some(ev)) => {
                let prev_len = batch.len();
                batch.push(ev);
                extended += 1;
                drain_immediate(batch, input_rx);
                if !batch[prev_len..].iter().any(is_pasteable_key_event) {
                    continue;
                }
            }
            _ => break,
        }
    }
}

/// Non-blocking drain of all immediately available events.
pub(super) fn drain_immediate(
    batch: &mut Vec<Event>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
) {
    while let Ok(ev) = input_rx.try_recv() {
        batch.push(ev);
    }
}

/// Minimum key events in a run to trigger paste coalescing.
const PASTE_COALESCE_THRESHOLD: usize = 3;

/// Minimum run length for the Windows path-shape coalesce branch.
/// Covers the shortest realistic dropped image path (`C:\x.png`,
/// `/a.png`) while leaving short typed prose alone.
#[cfg(target_os = "windows")]
const PATH_COALESCE_THRESHOLD: usize = 8;

/// Check if a terminal event is a pasteable key press — a character,
/// Enter, or Tab with no control modifiers (Ctrl/Alt/Super).
///
/// Only matches `Press` (not `Repeat` or `Release`). Repeat events come
/// from held keys, not paste; Release events carry no semantic content.
fn is_pasteable_key_event(ev: &Event) -> bool {
    match ev {
        Event::Key(ke) if ke.kind == KeyEventKind::Press => match ke.code {
            KeyCode::Char(_) => {
                ke.modifiers.is_empty()
                    || ke.modifiers == KeyModifiers::SHIFT
                    || crate::input::key::is_altgr(ke.modifiers)
            }
            KeyCode::Enter | KeyCode::Tab => ke.modifiers.is_empty(),
            _ => false,
        },
        _ => false,
    }
}

/// Map a voice-chord key event to its action (pure, so it's unit-testable).
///
/// Hold mode on Kitty is press-to-record / release-to-stop, but only a
/// hold-*owned* session stops on release; a `/voice`/toggle session (not
/// hold-owned) has no release of its own, so a press toggles it off. Elsewhere
/// it's a tap toggle.
fn voice_chord_action(
    hold_mode: bool,
    kitty: bool,
    kind: KeyEventKind,
    listening: bool,
    hold_owned: bool,
) -> Option<crate::app::actions::Action> {
    use crate::app::actions::Action;
    if hold_mode && kitty {
        match kind {
            KeyEventKind::Press if !listening => Some(Action::EnableVoiceMode),
            KeyEventKind::Press if !hold_owned => Some(Action::VoiceToggle),
            KeyEventKind::Release => Some(Action::VoiceStop),
            _ => None, // repeat while a hold is held, or press of a hold-owned session
        }
    } else if kind == KeyEventKind::Press {
        Some(Action::VoiceToggle)
    } else {
        None
    }
}

/// The voice-capture chord: **Ctrl+Space** or **F8**. A press needs the exact
/// chord (matching the registry, so Shift+F8 / Ctrl+Alt+Space don't fire); a
/// release matches the key alone (Space/F8), since on Kitty the Ctrl release can
/// precede Space and drop the CONTROL bit. Callers gate release handling on an
/// owning hold session, so a stray bare release is a no-op.
fn is_voice_chord(ke: &KeyEvent) -> bool {
    match ke.kind {
        KeyEventKind::Release => matches!(ke.code, KeyCode::Char(' ') | KeyCode::F(8)),
        _ => {
            (ke.code == KeyCode::Char(' ') && ke.modifiers == KeyModifiers::CONTROL)
                || (ke.code == KeyCode::F(8) && ke.modifiers.is_empty())
        }
    }
}

/// Coalesce runs of rapid key events into synthetic `Event::Paste`
/// events. On terminals without bracketed paste, pasted text arrives
/// as individual key events; Enter keys mid-run would otherwise
/// trigger "submit prompt" and split multi-line pastes.
///
/// A contiguous run of character/Enter/Tab events is replaced with a
/// single `Event::Paste` when EITHER:
///
/// 1. `>= PASTE_COALESCE_THRESHOLD` events AND at least one Enter is
///    followed by more characters (distinguishes `type + submit` from
///    `pasted multiline`).
/// 2. **Windows only:** `>= PATH_COALESCE_THRESHOLD` events AND the
///    assembled text starts with a drag-drop-style path anchor. Some
///    Windows Terminal versions deliver dropped paths as keystrokes
///    instead of a bracketed paste; this branch recovers them.
///
/// No-op when bracketed paste already arrives as `Event::Paste`.
fn coalesce_rapid_keys(events: Vec<Event>) -> Vec<Event> {
    // Fast path: not enough events for coalescing to trigger.
    if events.len() < PASTE_COALESCE_THRESHOLD {
        return events;
    }

    // If Event::Paste fragments are mixed with key events (Windows
    // Terminal can split a large bracketed paste across read boundaries),
    // merge everything into a single Event::Paste.
    let (mut has_paste, mut has_keys) = (false, false);
    for e in events.iter() {
        has_paste |= matches!(e, Event::Paste(_));
        has_keys |= is_pasteable_key_event(e);
    }
    if has_paste {
        return if has_keys {
            merge_paste_fragments(events)
        } else {
            events
        };
    }

    // Remove Release events — handlers ignore them and they'd break run
    // detection. Exception: voice-chord releases (needed for hold-to-talk).
    let events: Vec<Event> = events
        .into_iter()
        .filter(|ev| {
            !matches!(ev, Event::Key(ke)
                if ke.kind == KeyEventKind::Release && !is_voice_chord(ke))
        })
        .collect();

    let mut result = Vec::with_capacity(events.len());
    let mut i = 0;

    while i < events.len() {
        if is_pasteable_key_event(&events[i]) {
            let run_start = i;
            let mut text = String::new();
            let mut seen_enter = false;
            let mut has_char_after_enter = false;

            while i < events.len() && is_pasteable_key_event(&events[i]) {
                if let Event::Key(ke) = &events[i] {
                    match ke.code {
                        KeyCode::Char(c) => {
                            text.push(c);
                            if seen_enter {
                                has_char_after_enter = true;
                            }
                        }
                        KeyCode::Enter => {
                            text.push('\n');
                            seen_enter = true;
                        }
                        KeyCode::Tab => {
                            text.push('\t');
                            if seen_enter {
                                has_char_after_enter = true;
                            }
                        }
                        _ => unreachable!("is_pasteable_key_event guards this"),
                    }
                }
                i += 1;
            }

            let run_len = i - run_start;
            let multiline_paste = run_len >= PASTE_COALESCE_THRESHOLD && has_char_after_enter;
            // Windows fallback for drag-drops that arrive as a key
            // burst instead of a bracketed paste — reuse the drop
            // classifier's anchor detector so the two layers can't
            // drift on what counts as a path.
            #[cfg(target_os = "windows")]
            let path_shaped_drop = run_len >= PATH_COALESCE_THRESHOLD
                && crate::prompt_images::starts_with_drop_anchor(&text);
            #[cfg(not(target_os = "windows"))]
            let path_shaped_drop = false;
            if multiline_paste || path_shaped_drop {
                tracing::debug!(
                    run_len,
                    text_len = text.len(),
                    path_shape = path_shaped_drop,
                    "coalesced rapid key events into paste"
                );
                result.push(Event::Paste(text));
            } else {
                for ev in &events[run_start..i] {
                    result.push(ev.clone());
                }
            }
        } else {
            result.push(events[i].clone());
            i += 1;
        }
    }

    result
}

pub(super) fn is_bare_esc_press(ev: &Event) -> bool {
    matches!(
        ev,
        Event::Key(ke) if ke.code == KeyCode::Esc
            && ke.kind == KeyEventKind::Press
            && ke.modifiers == KeyModifiers::NONE
    )
}

/// Merge `Event::Paste` fragments and interleaved key events into a
/// single `Event::Paste`.  Non-paste, non-key events (Resize, Mouse,
/// Focus) are preserved in order around the merged paste.
fn merge_paste_fragments(events: Vec<Event>) -> Vec<Event> {
    let mut result = Vec::new();
    let mut merged_text = String::new();

    for ev in events {
        match &ev {
            Event::Paste(text) => merged_text.push_str(text),
            Event::Key(ke) if is_pasteable_key_event(&ev) => match ke.code {
                KeyCode::Char(c) => merged_text.push(c),
                KeyCode::Enter => merged_text.push('\n'),
                KeyCode::Tab => merged_text.push('\t'),
                _ => {}
            },
            // Non-pasteable keys (Ctrl+C, Backspace, arrows, Release
            // events, etc.) are artifacts of paste fragmentation — drop.
            Event::Key(_) => {}
            _ => {
                if !merged_text.is_empty() {
                    result.push(Event::Paste(std::mem::take(&mut merged_text)));
                }
                result.push(ev);
            }
        }
    }

    if !merged_text.is_empty() {
        result.push(Event::Paste(merged_text));
    }

    result
}

/// Spawn effects into the task set. Returns `true` if the app should quit.
fn process_effects(
    effs: Vec<super::actions::Effect>,
    tasks: &mut JoinSet<TaskResult>,
    app: &mut AppView,
    progress_tx: &tokio::sync::mpsc::UnboundedSender<effects::RestoreProgressMsg>,
) -> bool {
    let flags = effects::SessionFlags {
        plan_mode: app.plan_mode,
        subagents: app.subagents,
        ask_user: app.ask_user,
        restore_code: app.restore_code,
        agent_override: app.agent_override.clone(),
        yolo_mode: app.default_yolo,
        auto_mode: super::dispatch::effective_auto(
            app.default_yolo,
            matches!(app.current_ui.permission_mode.as_deref(), Some("auto")),
        ),
        chat_mode: app.chat_mode,
        screen_mode_label: Some(app.screen_mode.meta_label()),
        is_api_key_auth: app.is_api_key_auth,
    };
    for eff in effs {
        let (quit, meta) = effects::execute(eff, tasks, &app.acp_tx, &app.cwd, &flags, progress_tx);
        // Install auth abort handle if the current auth state still matches.
        if let Some((seq, abort_handle)) = meta.auth_abort_handle
            && let super::app_view::AuthState::Authenticating {
                request_seq,
                handle,
                ..
            } = &mut app.auth_state
            && *request_seq == seq
        {
            *handle = Some(abort_handle);
        }
        if quit {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventState};

    // ── is_voice_chord ───────────────────────────────────────────────────

    #[test]
    fn is_voice_chord_press_exact_release_keycode() {
        use KeyEventKind::{Press, Release};
        let hit = |code, mods, kind| {
            is_voice_chord(&KeyEvent {
                code,
                modifiers: mods,
                kind,
                state: KeyEventState::NONE,
            })
        };
        let (sp, f8, ctrl, none) = (
            KeyCode::Char(' '),
            KeyCode::F(8),
            KeyModifiers::CONTROL,
            KeyModifiers::NONE,
        );
        // Press: exact chord only — stray mods / bare Space don't fire (Thread 4).
        assert!(hit(sp, ctrl, Press) && hit(f8, none, Press));
        assert!(!hit(sp, ctrl | KeyModifiers::ALT, Press));
        assert!(!hit(f8, KeyModifiers::SHIFT, Press) && !hit(sp, none, Press));
        // Release: key alone — a bare Space release (Ctrl lifted first) matches so
        // hold-to-talk can still stop (Thread 3); non-chord keys don't.
        assert!(hit(sp, none, Release) && hit(f8, none, Release));
        assert!(!hit(KeyCode::Char('a'), none, Release));
    }

    // ── voice_chord_action ───────────────────────────────────────────────

    #[test]
    fn voice_chord_action_cases() {
        use crate::app::actions::Action;
        // (hold_mode, kitty, kind, listening, hold_owned) -> action tag, with the
        // toggle-stop case being a past regression.
        let press = KeyEventKind::Press;
        let release = KeyEventKind::Release;
        let tag = |a: Option<Action>| match a {
            Some(Action::EnableVoiceMode) => "start",
            Some(Action::VoiceStop) => "stop",
            Some(Action::VoiceToggle) => "toggle",
            None => "none",
            _ => "other",
        };
        let cases = [
            // hold+Kitty: press idle starts; release stops; press on a hold-owned
            // session waits; press on a non-hold (/voice/toggle) session toggles off.
            ((true, true, press, false, false), "start"),
            ((true, true, release, true, true), "stop"),
            ((true, true, press, true, true), "none"),
            ((true, true, press, true, false), "toggle"),
            // Non-hold (toggle mode or no Kitty releases): press toggles, release noops.
            ((false, false, press, false, false), "toggle"),
            ((false, false, release, true, false), "none"),
            ((true, false, release, true, false), "none"),
        ];
        for ((hold, kitty, kind, listening, owned), want) in cases {
            assert_eq!(
                tag(voice_chord_action(hold, kitty, kind, listening, owned)),
                want,
                "voice_chord_action({hold},{kitty},{kind:?},{listening},{owned})"
            );
        }
    }

    // ── plan_reconnect_load ──────────────────────────────────────────────

    #[test]
    fn plan_reconnect_load_requires_session_id() {
        let agent = crate::test_util::make_agent_view(None, "/work/project");
        assert!(plan_reconnect_load(&agent, std::path::Path::new("/pager/cwd")).is_none());
    }

    /// The session's own cwd keys its on-disk storage — the pager cwd
    /// is only a fallback for agents without one.
    #[test]
    fn plan_reconnect_load_prefers_session_cwd_over_fallback() {
        let agent = crate::test_util::make_agent_view(Some("sess-1"), "/work/worktree-a");
        let plan = plan_reconnect_load(&agent, std::path::Path::new("/pager/cwd")).unwrap();
        assert_eq!(plan.session_id.0.as_ref(), "sess-1");
        assert_eq!(plan.cwd, std::path::PathBuf::from("/work/worktree-a"));

        let agent = crate::test_util::make_agent_view(Some("sess-1"), "");
        let plan = plan_reconnect_load(&agent, std::path::Path::new("/pager/cwd")).unwrap();
        assert_eq!(plan.cwd, std::path::PathBuf::from("/pager/cwd"));
    }

    /// The reconnect cursor rides `_meta.cursor` when known; yolo mode
    /// always rides `_meta.yoloMode`. Auto rides `_meta.autoMode` per-agent.
    #[test]
    fn plan_reconnect_load_meta_carries_cursor_and_yolo() {
        let mut agent = crate::test_util::make_agent_view(Some("sess-1"), "/work");
        let plan = plan_reconnect_load(&agent, std::path::Path::new("/pager/cwd")).unwrap();
        assert_eq!(plan.meta["yoloMode"], serde_json::json!(false));
        assert!(
            plan.meta.get("cursor").is_none(),
            "no cursor key before any event was applied"
        );
        // autoMode is always set explicitly (false when not in auto) so the leader's
        // capability injection can't re-enable Auto on reconnect.
        assert_eq!(plan.meta["autoMode"], serde_json::json!(false));

        agent.last_seen_event_id = Some("sess-1-42".into());
        agent.session.yolo_mode = true;
        let plan = plan_reconnect_load(&agent, std::path::Path::new("/pager/cwd")).unwrap();
        assert_eq!(plan.meta["yoloMode"], serde_json::json!(true));
        assert_eq!(plan.meta["cursor"], serde_json::json!("sess-1-42"));
    }

    #[test]
    fn plan_reconnect_load_meta_carries_auto_mode_from_session() {
        // Auto rides `_meta.autoMode`, derived from THIS agent's own
        // `auto_mode` (per-agent, symmetric with yolo) — not the global UI mirror.
        let mut agent = crate::test_util::make_agent_view(Some("sess-1"), "/work");
        agent.session.auto_mode = true;
        let plan = plan_reconnect_load(&agent, std::path::Path::new("/pager/cwd")).unwrap();
        assert_eq!(plan.meta["yoloMode"], serde_json::json!(false));
        assert_eq!(plan.meta["autoMode"], serde_json::json!(true));

        // Yolo wins: autoMode is explicitly false even if the session is in auto.
        let mut agent = crate::test_util::make_agent_view(Some("sess-1"), "/work");
        agent.session.auto_mode = true;
        agent.session.yolo_mode = true;
        let plan = plan_reconnect_load(&agent, std::path::Path::new("/pager/cwd")).unwrap();
        assert_eq!(plan.meta["yoloMode"], serde_json::json!(true));
        assert_eq!(plan.meta["autoMode"], serde_json::json!(false));
    }

    /// Multi-agent reconnect must seed each tab's `autoMode` from ITS OWN
    /// session, not a shared global mirror: an active Auto tab and a background
    /// Ask tab reconnect with `autoMode:true` and `autoMode:false` respectively.
    #[test]
    fn plan_reconnect_load_multi_agent_uses_per_agent_auto() {
        let mut active = crate::test_util::make_agent_view(Some("sess-active"), "/work");
        active.session.auto_mode = true;
        let background = crate::test_util::make_agent_view(Some("sess-bg"), "/work");
        // background.session.auto_mode stays false (Ask).

        let active_plan = plan_reconnect_load(&active, std::path::Path::new("/pager/cwd")).unwrap();
        let background_plan =
            plan_reconnect_load(&background, std::path::Path::new("/pager/cwd")).unwrap();

        assert_eq!(active_plan.meta["autoMode"], serde_json::json!(true));
        assert_eq!(
            background_plan.meta["autoMode"],
            serde_json::json!(false),
            "background Ask tab must reconnect with autoMode:false regardless of the active tab"
        );
    }

    // ── reconnect_restore_outcome ────────────────────────────────────────

    /// The regression guard: one background tab fails, the active tab
    /// succeeds. The whole-reconnect flag goes false (toast says "failed"),
    /// but the active tab's OWN drain must still fire — a failed background tab
    /// must not strand prompts queued on the healthy active tab.
    #[test]
    fn reconnect_drain_gates_on_active_agent_not_all_agents() {
        use super::super::agent::AgentId;
        let active = AgentId(0);
        let background = AgentId(1);
        let mut loads = std::collections::HashMap::new();
        loads.insert(active, (true, None));
        loads.insert(background, (false, None));
        let pending = vec![active, background];

        let (all_restored, active_restored) =
            reconnect_restore_outcome(true, &pending, &loads, Some(active));
        assert!(
            !all_restored,
            "a failed background tab keeps the whole-reconnect flag false (toast)"
        );
        assert!(
            active_restored,
            "the active tab's own success still drains its queue"
        );
    }

    /// The active tab's OWN reload failed: its drain stays suppressed even
    /// though a background tab succeeded.
    #[test]
    fn reconnect_drain_blocked_when_active_agent_failed() {
        use super::super::agent::AgentId;
        let active = AgentId(0);
        let background = AgentId(1);
        let mut loads = std::collections::HashMap::new();
        loads.insert(active, (false, None));
        loads.insert(background, (true, None));
        let pending = vec![active, background];

        let (all_restored, active_restored) =
            reconnect_restore_outcome(true, &pending, &loads, Some(active));
        assert!(!all_restored);
        assert!(
            !active_restored,
            "the active tab's own failure must block its drain"
        );
    }

    /// Single-agent behavior is preserved: the lone active tab succeeds → both
    /// flags true (toast "restored" + drain).
    #[test]
    fn reconnect_drain_single_agent_success_preserved() {
        use super::super::agent::AgentId;
        let active = AgentId(0);
        let mut loads = std::collections::HashMap::new();
        loads.insert(active, (true, None));
        let pending = vec![active];

        let (all_restored, active_restored) =
            reconnect_restore_outcome(true, &pending, &loads, Some(active));
        assert!(all_restored);
        assert!(active_restored);
    }

    /// A failed init (`init_ok == false`, empty `loads`) suppresses everything.
    #[test]
    fn reconnect_drain_blocked_when_init_failed() {
        use super::super::agent::AgentId;
        let active = AgentId(0);
        let loads = std::collections::HashMap::new();
        let pending = vec![active];

        let (all_restored, active_restored) =
            reconnect_restore_outcome(false, &pending, &loads, Some(active));
        assert!(!all_restored);
        assert!(!active_restored);
    }

    /// No active agent (dashboard/welcome view): nothing to drain, even when
    /// every reloaded tab restored.
    #[test]
    fn reconnect_drain_blocked_when_no_active_agent() {
        use super::super::agent::AgentId;
        let background = AgentId(1);
        let mut loads = std::collections::HashMap::new();
        loads.insert(background, (true, None));
        let pending = vec![background];

        let (all_restored, active_restored) =
            reconnect_restore_outcome(true, &pending, &loads, None);
        assert!(all_restored);
        assert!(
            !active_restored,
            "no active agent → no active-tab drain to fire"
        );
    }

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn release(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        })
    }

    fn press_shift(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn press_ctrl(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[cfg(target_os = "linux")]
    fn mouse_event(kind: crossterm::event::MouseEventKind, modifiers: KeyModifiers) -> Event {
        Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column: 7,
            row: 11,
            modifiers,
        })
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unmodified_middle_down_reads_primary_once() {
        use crossterm::event::{MouseButton, MouseEventKind};
        crate::clipboard::set_clipboard_probe_hook(crate::clipboard::ClipboardProbeHook {
            text: Some("CLIPBOARD".to_owned()),
            primary_text: Some("PRIMARY\nexact".to_owned()),
            x11_primary_available: true,
            ..Default::default()
        });

        let normalized = normalize_input_event(mouse_event(
            MouseEventKind::Down(MouseButton::Middle),
            KeyModifiers::NONE,
        ));

        assert_eq!(normalized.event, Event::Paste("PRIMARY\nexact".to_owned()));
        assert_eq!(normalized.paste_provenance, PasteProvenance::X11Primary);
        assert_eq!(crate::clipboard::primary_selection_read_call_count(), 1);
        crate::clipboard::clear_clipboard_probe_hook();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nonqualifying_mouse_events_do_not_read_primary() {
        use crossterm::event::{MouseButton, MouseEventKind};
        crate::clipboard::set_clipboard_probe_hook(crate::clipboard::ClipboardProbeHook {
            primary_text: Some("PRIMARY".to_owned()),
            x11_primary_available: true,
            ..Default::default()
        });

        let release = mouse_event(MouseEventKind::Up(MouseButton::Middle), KeyModifiers::NONE);
        let normalized = normalize_input_event(release.clone());
        assert_eq!(normalized.event, release);
        assert_eq!(normalized.paste_provenance, PasteProvenance::Terminal);
        let modified = mouse_event(
            MouseEventKind::Down(MouseButton::Middle),
            KeyModifiers::SHIFT,
        );
        let normalized = normalize_input_event(modified.clone());
        assert_eq!(normalized.event, modified);
        assert_eq!(normalized.paste_provenance, PasteProvenance::Terminal);
        let left = mouse_event(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE);
        let normalized = normalize_input_event(left.clone());
        assert_eq!(normalized.event, left);
        assert_eq!(normalized.paste_provenance, PasteProvenance::Terminal);
        assert_eq!(crate::clipboard::primary_selection_read_call_count(), 0);
        crate::clipboard::clear_clipboard_probe_hook();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn empty_primary_preserves_original_middle_event() {
        use crossterm::event::{MouseButton, MouseEventKind};
        crate::clipboard::set_clipboard_probe_hook(crate::clipboard::ClipboardProbeHook {
            primary_text: Some(String::new()),
            x11_primary_available: true,
            ..Default::default()
        });
        let middle = mouse_event(
            MouseEventKind::Down(MouseButton::Middle),
            KeyModifiers::NONE,
        );

        let normalized = normalize_input_event(middle.clone());
        assert_eq!(normalized.event, middle);
        assert_eq!(normalized.paste_provenance, PasteProvenance::Terminal);
        assert_eq!(crate::clipboard::primary_selection_read_call_count(), 1);
        crate::clipboard::clear_clipboard_probe_hook();
    }

    #[test]
    fn coalesce_multiline_paste_without_bracketed_paste() {
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            press(KeyCode::Enter),
            press(KeyCode::Char('c')),
            press(KeyCode::Char('d')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("ab\ncd".to_string()));
    }

    #[test]
    fn coalesce_filters_release_events() {
        // Press+Release pairs (Windows Terminal, Kitty) must not break runs.
        let events = vec![
            press(KeyCode::Char('a')),
            release(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            release(KeyCode::Char('b')),
            press(KeyCode::Enter),
            release(KeyCode::Enter),
            press(KeyCode::Char('c')),
            release(KeyCode::Char('c')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("ab\nc".to_string()));
    }

    #[test]
    fn coalesce_preserves_shifted_chars() {
        let events = vec![
            press_shift(KeyCode::Char('H')),
            press(KeyCode::Char('i')),
            press(KeyCode::Enter),
            press_shift(KeyCode::Char('B')),
            press(KeyCode::Char('y')),
            press(KeyCode::Char('e')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("Hi\nBye".to_string()));
    }

    #[test]
    fn coalesce_below_threshold_no_change() {
        let events = vec![press(KeyCode::Char('a')), press(KeyCode::Enter)];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], Event::Key(ke) if ke.code == KeyCode::Char('a')));
        assert!(matches!(&result[1], Event::Key(ke) if ke.code == KeyCode::Enter));
    }

    #[test]
    fn coalesce_no_enter_no_change() {
        // No Enter in the run — no premature-send risk.
        let events = vec![
            press(KeyCode::Char('h')),
            press(KeyCode::Char('e')),
            press(KeyCode::Char('l')),
            press(KeyCode::Char('l')),
            press(KeyCode::Char('o')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 5);
        for ev in &result {
            assert!(matches!(ev, Event::Key(_)));
        }
    }

    #[test]
    fn coalesce_only_enters_no_change() {
        // All-Enter runs must not coalesce (held Enter key repeat).
        let events = vec![
            press(KeyCode::Enter),
            press(KeyCode::Enter),
            press(KeyCode::Enter),
            press(KeyCode::Enter),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn coalesce_preserves_non_key_events() {
        let events = vec![
            Event::Resize(80, 24),
            press(KeyCode::Char('a')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
            Event::Resize(100, 30),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], Event::Resize(80, 24)));
        assert_eq!(result[1], Event::Paste("a\nb".to_string()));
        assert!(matches!(&result[2], Event::Resize(100, 30)));
    }

    #[test]
    fn coalesce_ctrl_key_breaks_run() {
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            press_ctrl(KeyCode::Char('c')),
            press(KeyCode::Enter),
            press(KeyCode::Char('d')),
        ];
        let result = coalesce_rapid_keys(events);
        // "ab" (2, no Enter) | Ctrl+C | "\nd" (2) — both runs below threshold.
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn coalesce_tabs_in_pasted_code() {
        let events = vec![
            press(KeyCode::Char('i')),
            press(KeyCode::Char('f')),
            press(KeyCode::Enter),
            press(KeyCode::Tab),
            press(KeyCode::Char('x')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("if\n\tx".to_string()));
    }

    #[test]
    fn coalesce_exactly_at_threshold() {
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("a\nb".to_string()));
    }

    #[test]
    fn coalesce_type_then_submit_not_coalesced() {
        // Enter is the LAST event — "type + submit", not paste.
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            press(KeyCode::Char('c')),
            press(KeyCode::Enter),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 4);
        assert!(matches!(&result[3], Event::Key(ke) if ke.code == KeyCode::Enter));
    }

    #[test]
    fn fragmented_paste_merged_with_keys() {
        // Event::Paste mixed with key events — merge into one paste.
        let events = vec![
            Event::Paste("real paste".into()),
            press(KeyCode::Char('a')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("real pastea\nb".to_string()));
    }

    #[test]
    fn coalesce_single_event_passthrough() {
        let events = vec![press(KeyCode::Enter)];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], Event::Key(_)));
    }

    #[test]
    fn coalesce_empty_input() {
        let result = coalesce_rapid_keys(vec![]);
        assert!(result.is_empty());
    }

    // ── Multi-newline coalescing tests ───────────────────────────────

    #[test]
    fn coalesce_three_lines() {
        // "foo\nbar\nbaz" — 3 lines, 2 newlines.
        let events = vec![
            press(KeyCode::Char('f')),
            press(KeyCode::Char('o')),
            press(KeyCode::Char('o')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
            press(KeyCode::Char('a')),
            press(KeyCode::Char('r')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
            press(KeyCode::Char('a')),
            press(KeyCode::Char('z')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("foo\nbar\nbaz".to_string()));
    }

    #[test]
    fn coalesce_four_lines_trailing_newline() {
        // "a\nb\nc\nd\n" — 4 lines + trailing newline.
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
            press(KeyCode::Enter),
            press(KeyCode::Char('c')),
            press(KeyCode::Enter),
            press(KeyCode::Char('d')),
            press(KeyCode::Enter),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("a\nb\nc\nd\n".to_string()));
    }

    // ── should_extend_for_paste tests ───────────────────────────────

    #[test]
    fn extend_triggered_with_single_pasteable_key() {
        let events = vec![press(KeyCode::Char('a'))];
        assert!(should_extend_for_paste(&events));
    }

    #[test]
    fn extend_triggered_with_enter_key() {
        let events = vec![press(KeyCode::Enter)];
        assert!(should_extend_for_paste(&events));
    }

    #[test]
    fn extend_not_triggered_with_bracketed_paste() {
        let events = vec![
            Event::Paste("hello".into()),
            press(KeyCode::Char('a')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
        ];
        assert!(!should_extend_for_paste(&events));
    }

    #[test]
    fn extend_not_triggered_with_only_non_pasteable() {
        let events = vec![Event::Resize(80, 24)];
        assert!(!should_extend_for_paste(&events));
    }

    // ── merge_paste_fragments tests ─────────────────────────────────

    #[test]
    fn merge_paste_and_key_fragments() {
        // Fragmented bracketed paste: Event::Paste + loose key events.
        let events = vec![
            Event::Paste("hello\nwor".into()),
            press(KeyCode::Char('l')),
            press(KeyCode::Char('d')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("hello\nworld".to_string()));
    }

    #[test]
    fn merge_multiple_paste_fragments() {
        let events = vec![
            Event::Paste("aa\n".into()),
            Event::Paste("bb\n".into()),
            press(KeyCode::Char('c')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("aa\nbb\nc".to_string()));
    }

    #[test]
    fn merge_preserves_non_key_events() {
        let events = vec![
            Event::Paste("hello".into()),
            Event::Resize(80, 24),
            press(KeyCode::Char('x')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], Event::Paste("hello".to_string()));
        assert!(matches!(result[1], Event::Resize(80, 24)));
        assert_eq!(result[2], Event::Paste("x".to_string()));
    }

    #[test]
    fn merge_skips_release_events() {
        let events = vec![
            Event::Paste("ab".into()),
            press(KeyCode::Char('c')),
            release(KeyCode::Char('c')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("abc".to_string()));
    }

    #[test]
    fn pure_paste_no_merge_needed() {
        let events = vec![Event::Paste("hello\nworld".into())];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste("hello\nworld".to_string()));
    }

    // ── is_pasteable_key_event filtering tests ─────────────────────────

    #[test]
    fn pasteable_rejects_mouse_events() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let ev = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!is_pasteable_key_event(&ev));
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!is_pasteable_key_event(&click));
    }

    #[test]
    fn pasteable_rejects_focus_events() {
        assert!(!is_pasteable_key_event(&Event::FocusGained));
        assert!(!is_pasteable_key_event(&Event::FocusLost));
    }

    #[test]
    fn pasteable_rejects_release_events() {
        assert!(!is_pasteable_key_event(&release(KeyCode::Char('a'))));
        assert!(!is_pasteable_key_event(&release(KeyCode::Enter)));
    }

    #[test]
    fn pasteable_rejects_resize() {
        assert!(!is_pasteable_key_event(&Event::Resize(80, 24)));
    }

    #[test]
    fn pasteable_rejects_repeat_events() {
        let ev = Event::Key(KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: KeyEventState::NONE,
        });
        assert!(!is_pasteable_key_event(&ev));
    }

    #[test]
    fn pasteable_accepts_valid_key_presses() {
        assert!(is_pasteable_key_event(&press(KeyCode::Char('a'))));
        assert!(is_pasteable_key_event(&press_shift(KeyCode::Char('A'))));
        assert!(is_pasteable_key_event(&press(KeyCode::Enter)));
        assert!(is_pasteable_key_event(&press(KeyCode::Tab)));
    }

    #[test]
    fn extend_not_triggered_with_only_mouse_and_focus() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let events = vec![
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
            Event::FocusGained,
        ];
        assert!(!should_extend_for_paste(&events));
    }

    #[test]
    fn extend_triggered_only_when_key_present_in_mixed_batch() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let events = vec![
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
            press(KeyCode::Char('a')),
            Event::FocusLost,
        ];
        assert!(should_extend_for_paste(&events));
    }

    #[test]
    fn coalesce_mouse_events_interleaved_with_paste_chars() {
        // Simulates the batch produced by the fixed detect_paste:
        // a key press followed by mouse events. The mouse events
        // should not prevent the key from being processed.
        use crossterm::event::{MouseEvent, MouseEventKind};
        let events = vec![
            press(KeyCode::Char('a')),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 11,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
        ];
        let result = coalesce_rapid_keys(events);
        // Below coalesce threshold, all events pass through unchanged.
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], Event::Key(ke) if ke.code == KeyCode::Char('a')));
        assert!(matches!(&result[1], Event::Mouse(_)));
        assert!(matches!(&result[2], Event::Mouse(_)));
    }

    #[test]
    fn coalesce_mouse_breaks_key_run_preserves_events() {
        // A genuine paste batch that also collected mouse events.
        // The paste chars should still coalesce; mouse events are preserved.
        use crossterm::event::{MouseEvent, MouseEventKind};
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            press(KeyCode::Enter),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 5,
                row: 3,
                modifiers: KeyModifiers::NONE,
            }),
            press(KeyCode::Char('c')),
        ];
        let result = coalesce_rapid_keys(events);
        // The mouse event breaks the key run: [a, b, Enter] (3 keys, but
        // Enter is last in that sub-run → no char after Enter → not coalesced),
        // then [mouse], then [c] (1 key).
        assert_eq!(result.len(), 5);
    }

    // ── Windows path-shape coalescing (drag-drop without bracketed paste) ─
    //
    // Windows-gated: the path-shape branch only exists on Windows
    // (other platforms reliably get bracketed paste for drag-drop).

    #[cfg(target_os = "windows")]
    fn press_run(text: &str) -> Vec<Event> {
        text.chars().map(|c| press(KeyCode::Char(c))).collect()
    }

    /// Smoke test across every anchor variant the branch should match:
    /// drive-letter (both separators), UNC, Unix absolute, `file://`,
    /// and the Windows-Terminal-quoted form for paths with spaces.
    #[cfg(target_os = "windows")]
    #[test]
    fn coalesce_path_shape_matches_each_anchor() {
        for input in [
            r"C:\foo.png",
            "C:/foo.png",
            r"\\srv\share\a.png",
            "/Users/a/b.png",
            "file:///tmp/x.png",
            "\"C:\\My Pics\\a.png\"",
        ] {
            let result = coalesce_rapid_keys(press_run(input));
            assert_eq!(result.len(), 1, "input {input:?} should coalesce");
            assert_eq!(result[0], Event::Paste(input.to_string()));
        }
    }

    /// Below-threshold path-shape (< 8 chars) and non-path prose of any
    /// length must NOT coalesce — keep typed editing intact.
    #[cfg(target_os = "windows")]
    #[test]
    fn coalesce_path_shape_rejects_short_or_non_path() {
        let short = "/foo.tx"; // 7 chars, below PATH_COALESCE_THRESHOLD
        assert!(
            coalesce_rapid_keys(press_run(short))
                .iter()
                .all(|e| matches!(e, Event::Key(_)))
        );
        let prose = "helloworld"; // 10 chars, no path anchor
        assert!(
            coalesce_rapid_keys(press_run(prose))
                .iter()
                .all(|e| matches!(e, Event::Key(_)))
        );
    }

    /// `:` in a US-layout drive-letter path arrives as Shift+`;`;
    /// `is_pasteable_key_event` accepts SHIFT so the run must assemble
    /// cleanly.
    #[cfg(target_os = "windows")]
    #[test]
    fn coalesce_path_shape_handles_shift_modifier() {
        let mut events = vec![press(KeyCode::Char('C'))];
        events.push(press_shift(KeyCode::Char(':')));
        events.extend(press_run(r"\foo.png"));
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], Event::Paste(r"C:\foo.png".to_string()));
    }
}
