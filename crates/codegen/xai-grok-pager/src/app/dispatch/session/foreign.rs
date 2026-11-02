use super::picker_routing::{
    PickerRequest, PickerSeqKind, PickerTarget, abandon_picker_continuation, accept_picker_result,
    picker_for_host,
};
use crate::app::actions::Effect;
use crate::app::app_view::{AppView, SessionPickerEntry};
use crate::app::dispatch::ctx::{get_active_agent, get_active_agent_mut};
use crate::app::effects::ConversationsPartial;
use crate::views::modal::ActiveModal;
use crate::views::session_picker::{SessionListPagination, SessionListQuery, repo_name_from_cwd};
use crate::views::session_picker_surface::SessionPickerHost;

use xai_grok_shell::session::unified_list::ListScope;

/// The history `kind` filter for the welcome screen's multi-source history under `--chat`.
///
/// Sandbox maps to `chat` (gateway); Local maps to `build` (local-disk).
/// Modal and non-welcome fetches get `None` so the shell keeps its default of forcing chat mode.
pub(in crate::app::dispatch) fn welcome_history_kind_filter(app: &AppView) -> Option<Vec<String>> {
    #[cfg(feature = "local-workspace")]
    {
        if app.chat_mode && matches!(app.active_view, crate::app::app_view::ActiveView::Welcome) {
            return Some(vec![
                app.welcome_workspace_mode.history_kind_filter().to_string(),
            ]);
        }
    }
    let _ = app;
    None
}

/// A new list or debounced server search invalidates the old stream immediately.
pub(in crate::app::dispatch) fn next_picker_list_generation(app: &mut AppView) -> u64 {
    app.session_picker_lanes.pagination = Default::default();
    if let Some(target) = picker_for_host(app, SessionPickerHost::AgentModal) {
        target.lanes.pagination = Default::default();
    }
    app.session_picker_list_seq = app.session_picker_list_seq.wrapping_add(1);
    app.session_picker_list_seq
}

pub(in crate::app::dispatch) fn first_session_list_page(
    app: &mut AppView,
    host: SessionPickerHost,
    query: Option<String>,
    cwd_override: Option<std::path::PathBuf>,
) -> Vec<Effect> {
    let kind_filter = match host {
        SessionPickerHost::Dashboard => Some(vec!["build".to_owned()]),
        _ => welcome_history_kind_filter(app),
    };
    let cwd = cwd_override.as_ref().unwrap_or(&app.cwd).clone();
    let Some(target) = picker_for_host(app, host) else {
        return vec![];
    };
    let headless_policy = target.source_filter.headless_policy();
    target.lanes.pagination = SessionListPagination::new(
        target.list_seq,
        SessionListQuery {
            cwd,
            query: query.clone(),
            kind_filter: kind_filter.clone(),
            headless_policy,
        },
    );
    vec![Effect::FetchSessionList {
        host,
        generation: target.generation,
        seq: target.list_seq,
        cursor: None,
        cwd_override,
        query,
        kind_filter,
        headless_policy,
    }]
}

pub(in crate::app::dispatch) fn dispatch_load_more_sessions(
    app: &mut AppView,
    host: SessionPickerHost,
) -> Vec<Effect> {
    let Some(target) = picker_for_host(app, host) else {
        return vec![];
    };
    if target.source_filter.is_content_search_disabled()
        || target.lanes.pending_resume_argument.is_some()
    {
        return vec![];
    }
    let Some((request, cursor)) = target.lanes.pagination.begin_next(target.list_seq) else {
        return vec![];
    };
    vec![Effect::FetchSessionList {
        host,
        generation: target.generation,
        seq: target.list_seq,
        cwd_override: Some(request.cwd.clone()),
        query: request.query.clone(),
        kind_filter: request.kind_filter.clone(),
        headless_policy: request.headless_policy,
        cursor: Some(cursor),
    }]
}

pub(in crate::app::dispatch) fn dispatch_fetch_session_list(app: &mut AppView) -> Vec<Effect> {
    app.session_picker_generation = app.alloc_picker_generation();
    let modal_generation = matches!(
        get_active_agent(app).and_then(|agent| agent.active_modal.as_ref()),
        Some(ActiveModal::SessionPicker { .. })
    )
    .then(|| app.alloc_picker_generation());
    app.session_picker_loading = true;
    app.session_picker_entries = None;
    app.session_picker_state.selected = 0;
    app.session_picker_state.set_query("");
    app.session_picker_state.search_active = false;
    app.session_picker_state.expanded.clear();
    app.session_picker_content_results = None;
    app.session_picker_content_loading = false;
    app.session_picker_entries_query = None;
    app.session_picker_pending_delete = None;
    next_picker_list_generation(app);
    app.foreign_session_scan_seq += 1;
    let foreign_seq = app.foreign_session_scan_seq;
    #[cfg(feature = "local-workspace")]
    crate::views::welcome::workspace_mode::log_history_source(
        "session_list_fetch_dispatch",
        Some(app.welcome_workspace_mode),
        welcome_history_kind_filter(app).as_deref(),
        None,
    );
    let foreign_effect = if app.chat_mode {
        app.foreign_scan_coordinator.begin_request(foreign_seq);
        None
    } else {
        let grok_home = xai_grok_tools::util::grok_home::grok_home();
        crate::app::foreign_sessions::scan_effect(
            &app.cwd,
            app.foreign_session_compat,
            &grok_home,
            app.foreign_scan_coordinator.clone(),
            foreign_seq,
        )
    };
    let foreign_loading = foreign_effect.is_some();
    let mut modal_lanes_set = false;
    if let Some(agent) = get_active_agent_mut(app)
        && let Some(ActiveModal::SessionPicker {
            lanes, generation, ..
        }) = agent.active_modal.as_mut()
    {
        lanes.foreign_loading = foreign_loading;
        lanes.pending_notice = None;
        lanes.pending_resume_argument = None;
        if let Some(fresh) = modal_generation {
            *generation = fresh;
        }
        modal_lanes_set = true;
    }
    app.session_picker_lanes.foreign_loading = foreign_loading && !modal_lanes_set;
    app.session_picker_lanes.pending_notice = None;
    app.session_picker_lanes.pending_resume_argument = None;
    let host = if modal_lanes_set {
        SessionPickerHost::AgentModal
    } else {
        SessionPickerHost::Welcome
    };
    let mut effects = first_session_list_page(app, host, None, None);
    effects.extend(foreign_effect);
    effects
}

/// Resolve an exact ID before an exact normalized title. Never guess between rows.
fn unique_resume_match(
    entries: &[SessionPickerEntry],
    filter: crate::views::session_picker::SourceFilter,
    argument: &str,
) -> Option<usize> {
    let visible = || {
        entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| filter.matches(&entry.source, entry.session_kind.as_deref()))
    };
    let parsed_id = uuid::Uuid::try_parse(argument).ok();
    let ids: Vec<_> = visible()
        .filter(|(_, entry)| {
            entry.id == argument
                || parsed_id.is_some_and(|id| uuid::Uuid::try_parse(&entry.id).ok() == Some(id))
        })
        .map(|(index, _)| index)
        .collect();
    if !ids.is_empty() {
        return (ids.len() == 1).then_some(ids[0]);
    }
    if parsed_id.is_some() {
        return None;
    }
    let key = crate::app::session_title_resolve::title_key(argument);
    if key.is_empty() {
        return None;
    }
    let titles: Vec<_> = visible()
        .filter(|(_, entry)| crate::app::session_title_resolve::title_key(&entry.summary) == key)
        .map(|(index, _)| index)
        .collect();
    (titles.len() == 1).then(|| titles[0])
}

pub(in crate::app::dispatch) fn handle_session_list_loaded(
    app: &mut AppView,
    request: PickerRequest,
    sessions: Vec<SessionPickerEntry>,
    partial: Option<ConversationsPartial>,
    scope: ListScope,
    query: Option<String>,
    cursor: Option<String>,
    next_cursor: Option<String>,
) -> Vec<Effect> {
    let chat_mode = app.chat_mode;
    let is_browse = query.is_none();
    let notice;
    let auto_pick;
    {
        let Some(mut target) =
            accept_picker_result(app, request, PickerSeqKind::List, "session list")
        else {
            abandon_picker_continuation(app, request, cursor.as_deref());
            return vec![];
        };
        if !target
            .lanes
            .pagination
            .accept(request.seq, cursor.as_deref())
        {
            return vec![];
        }
        if target.lanes.pagination.scope_changed(scope) {
            target.lanes.pagination.advance(scope, None);
            app.show_toast("The session list changed directories. Reopen /resume to refresh it.");
            return vec![];
        }
        let advanced = target.lanes.pagination.advance(scope, next_cursor);
        if let Some(partial) = partial {
            crate::unified_log::warn(
                "session.list.partial",
                None,
                Some(serde_json::json!({ "reason": format!("{partial:?}") })),
            );
        }
        let empty_notice = partial.map_or_else(
            || "No sessions found for this directory".to_owned(),
            |partial| partial.picker_notice().to_owned(),
        );
        let partial_notice = partial.map(ConversationsPartial::picker_notice);
        if cursor.is_some() {
            target.append_native_page(sessions);
            auto_pick = None;
            notice = if !advanced {
                Some(
                    "Session history returned a repeated cursor. Reopen /resume to refresh it."
                        .to_owned(),
                )
            } else {
                partial_notice.filter(|_| chat_mode).map(str::to_owned)
            };
        } else {
            let argument = target.lanes.pending_resume_argument.take();
            notice = target.native_loaded(sessions, query, chat_mode, empty_notice, partial_notice);
            auto_pick = argument
                .filter(|argument| {
                    is_browse
                        && partial.is_none()
                        && !target.lanes.pagination.has_more()
                        && target.state.query().trim() == argument
                })
                .and_then(|argument| {
                    target.entries.as_deref().and_then(|entries| {
                        unique_resume_match(entries, target.source_filter, &argument)
                    })
                });
            *target.detail_seq += 1;
        }
    }
    if let Some(notice) = notice {
        app.show_toast(&notice);
    } else if partial == Some(ConversationsPartial::BuildIncomplete) {
        app.show_toast(ConversationsPartial::BuildIncomplete.picker_notice());
    } else if scope.is_relaxed()
        && app.session_picker_relaxed_notified_for.as_deref() != Some(app.cwd.as_path())
        && !matches!(app.active_view, crate::app::app_view::ActiveView::Welcome)
    {
        // Notify once per directory; the browse is scoped to `app.cwd`.
        app.session_picker_relaxed_notified_for = Some(app.cwd.clone());
        let message = match scope {
            ListScope::Repo => {
                "No sessions in this directory. Showing other sessions from this repository."
            }
            _ => "No sessions in this directory. Showing sessions from other directories.",
        };
        app.show_toast(message);
    }
    if !scope.is_relaxed() && is_browse {
        app.session_picker_relaxed_notified_for = None;
    }
    if let Some(index) = auto_pick {
        return match request.host {
            SessionPickerHost::Dashboard => {
                super::super::dashboard::dispatch_dashboard_pick_session(app, index)
            }
            SessionPickerHost::AgentModal | SessionPickerHost::Welcome => {
                super::load::dispatch_pick_session(app, index)
            }
        };
    }
    vec![]
}

pub(in crate::app::dispatch) fn handle_session_list_failed(
    app: &mut AppView,
    request: PickerRequest,
    error: String,
    query: Option<String>,
    cursor: Option<String>,
) -> Vec<Effect> {
    let chat_mode = app.chat_mode;
    let is_search = query.is_some();
    let notice;
    {
        let Some(mut target) =
            accept_picker_result(app, request, PickerSeqKind::List, "session list failure")
        else {
            abandon_picker_continuation(app, request, cursor.as_deref());
            return vec![];
        };
        if !target
            .lanes
            .pagination
            .accept(request.seq, cursor.as_deref())
        {
            return vec![];
        }
        if cursor.is_some() {
            target.lanes.pagination.page_failed();
            app.show_toast(&format!("Couldn't load more sessions: {error}"));
            return vec![];
        }
        target.lanes.pagination = Default::default();
        target.lanes.pending_resume_argument = None;
        tracing::warn!(error = %error, "session list fetch failed");
        let error_notice = format!("Couldn't load sessions: {error}");
        notice = target.native_failed(error_notice, is_search, chat_mode);
        *target.detail_seq += 1;
    }
    if let Some(notice) = notice {
        app.show_toast(&notice);
    }
    vec![]
}

pub(in crate::app::dispatch) fn handle_foreign_sessions_scanned(
    app: &mut AppView,
    scanned: Vec<SessionPickerEntry>,
    seq: u64,
) -> Vec<Effect> {
    if app.chat_mode || seq != app.foreign_session_scan_seq {
        return vec![];
    }
    let list_seq = app.session_picker_list_seq;
    let mut scanned = Some(scanned);
    let mut notice = None;
    let mut handled = false;
    if let Some(agent) = get_active_agent_mut(app) {
        let current_repo = repo_name_from_cwd(&agent.session.cwd.to_string_lossy());
        if let Some(ActiveModal::SessionPicker {
            entries,
            loading,
            lanes,
            state,
            content_results,
            content_loading,
            deep_search_seq,
            generation,
            detail_seq,
            entries_query,
            source_filter,
            ..
        }) = agent.active_modal.as_mut()
            && lanes.foreign_loading
        {
            handled = true;
            let mut target = PickerTarget {
                entries,
                loading,
                lanes,
                state,
                content_results,
                content_loading,
                entries_query,
                source_filter: *source_filter,
                grouped: true,
                current_repo,
                generation: *generation,
                list_seq,
                deep_search_seq: *deep_search_seq,
                detail_seq,
            };
            notice = target.foreign_loaded(scanned.take().unwrap_or_default());
            *target.detail_seq += 1;
        }
    }
    if !handled && app.session_picker_lanes.foreign_loading {
        let current_repo = repo_name_from_cwd(&app.cwd.to_string_lossy());
        let mut target = PickerTarget {
            entries: &mut app.session_picker_entries,
            loading: &mut app.session_picker_loading,
            lanes: &mut app.session_picker_lanes,
            state: &mut app.session_picker_state,
            content_results: &mut app.session_picker_content_results,
            content_loading: &mut app.session_picker_content_loading,
            entries_query: &mut app.session_picker_entries_query,
            source_filter: app.session_picker_source_filter,
            grouped: app.session_picker_grouped,
            current_repo,
            generation: app.session_picker_generation,
            list_seq,
            deep_search_seq: app.session_picker_deep_search_seq,
            detail_seq: &mut app.session_picker_detail_seq,
        };
        notice = target.foreign_loaded(scanned.unwrap_or_default());
        *target.detail_seq += 1;
    }
    if let Some(notice) = notice {
        app.show_toast(&notice);
    }
    vec![]
}

pub(in crate::app::dispatch) fn invalidate_foreign_picker(app: &mut AppView) {
    app.foreign_session_scan_seq += 1;
    app.foreign_scan_coordinator
        .begin_request(app.foreign_session_scan_seq);
    app.session_picker_lanes = Default::default();
}
