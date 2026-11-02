//! Regression tests for paging the native rows of an open /resume picker.

use super::*;
use crate::app::app_view::SessionPickerEntry;
use crate::views::modal::ActiveModal;
use crate::views::session_picker::SourceFilter;
use xai_grok_shell::session::unified_list::{HeadlessPolicy, ListScope};

/// Echo the request actually issued by the reducer, as the list effect does.
struct Request {
    host: SessionPickerHost,
    generation: u64,
    seq: u64,
    query: Option<String>,
    cursor: Option<String>,
    cwd_override: Option<PathBuf>,
    kind_filter: Option<Vec<String>>,
    headless_policy: HeadlessPolicy,
}

impl Request {
    fn from_effects(effects: Vec<Effect>) -> Self {
        let mut fetches = effects.into_iter().filter_map(|effect| match effect {
            Effect::FetchSessionList {
                host,
                generation,
                seq,
                query,
                cursor,
                cwd_override,
                kind_filter,
                headless_policy,
            } => Some(Self {
                host,
                generation,
                seq,
                query,
                cursor,
                cwd_override,
                kind_filter,
                headless_policy,
            }),
            _ => None,
        });
        let request = fetches.next().expect("picker must request a native page");
        assert!(fetches.next().is_none(), "picker issued duplicate requests");
        request
    }

    fn success(
        &self,
        sessions: Vec<SessionPickerEntry>,
        next: Option<&str>,
        scope: ListScope,
    ) -> Action {
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: self.host,
            generation: self.generation,
            seq: self.seq,
            query: self.query.clone(),
            cursor: self.cursor.clone(),
            next_cursor: next.map(str::to_owned),
            scope,
            partial: None,
            sessions,
        })
    }

    fn deliver(&self, app: &mut AppView, ids: &[&str], next: Option<&str>) {
        let rows = ids
            .iter()
            .map(|id| {
                let mut entry = make_picker_entry(id, "/repo");
                entry.updated_at = chrono::DateTime::UNIX_EPOCH;
                entry.created_at = entry.updated_at;
                if self.headless_policy == HeadlessPolicy::Only {
                    entry.session_kind = Some("headless".into());
                }
                entry
            })
            .collect();
        let effects = dispatch(self.success(rows, next, ListScope::Cwd), app);
        assert!(
            effects.is_empty(),
            "browsing must leave the picker open: {effects:?}"
        );
    }

    fn fail(&self, app: &mut AppView) {
        dispatch(
            Action::TaskComplete(TaskResult::SessionListFailed {
                host: self.host,
                generation: self.generation,
                seq: self.seq,
                query: self.query.clone(),
                cursor: self.cursor.clone(),
                error: "connection interrupted".into(),
            }),
            app,
        );
    }
}

fn more(app: &mut AppView, host: SessionPickerHost) -> Vec<Effect> {
    dispatch(Action::LoadMoreSessions { host }, app)
}

fn next_page(app: &mut AppView, host: SessionPickerHost, token: &str) -> Request {
    let request = Request::from_effects(more(app, host));
    assert_eq!(request.host, host);
    assert_eq!(request.cursor.as_deref(), Some(token));
    request
}

fn entries(app: &AppView, host: SessionPickerHost) -> &[SessionPickerEntry] {
    match host {
        SessionPickerHost::Welcome => app.session_picker_entries.as_deref(),
        SessionPickerHost::AgentModal => {
            match get_active_agent(app).and_then(|a| a.active_modal.as_ref()) {
                Some(ActiveModal::SessionPicker { entries, .. }) => entries.as_deref(),
                _ => panic!("active modal picker missing"),
            }
        }
        SessionPickerHost::Dashboard => app
            .dashboard_session_picker
            .as_ref()
            .expect("dashboard picker")
            .entries
            .as_deref(),
    }
    .unwrap_or_default()
}

fn ids(app: &AppView, host: SessionPickerHost) -> Vec<&str> {
    entries(app, host)
        .iter()
        .map(|entry| entry.id.as_str())
        .collect()
}

#[test]
fn all_hosts_append_to_the_requesting_picker_and_reuse_its_filters() {
    use SessionPickerHost::{AgentModal, Dashboard, Welcome};
    for host in [Welcome, AgentModal, Dashboard] {
        let mut app = if host == AgentModal {
            test_app_with_agent()
        } else {
            test_app()
        };
        let cwd = PathBuf::from("/dashboard-repo");
        if host == Dashboard {
            app.workspace_dashboard_enabled = true;
            app.active_view = ActiveView::AgentDashboard;
            ensure_dashboard_state(&mut app);
            app.dashboard.as_mut().unwrap().cwd = cwd.clone();
        }
        let first = Request::from_effects(dispatch(Action::ShowSessionPicker, &mut app));
        assert_eq!(first.host, host);
        assert!(first.cursor.is_none());
        first.deliver(&mut app, &["first", "second"], Some("opaque+/="));
        if host == Dashboard {
            app.dashboard.as_mut().unwrap().cwd = PathBuf::from("/another-repo");
        }
        for other in [Welcome, AgentModal, Dashboard]
            .into_iter()
            .filter(|other| *other != host)
        {
            assert!(
                more(&mut app, other).is_empty(),
                "wrong picker borrowed the cursor"
            );
        }
        let page = next_page(&mut app, host, "opaque+/=");
        assert_eq!(page.generation, first.generation);
        assert_eq!(page.query, first.query);
        assert_eq!(page.kind_filter, first.kind_filter);
        assert_eq!(page.headless_policy, HeadlessPolicy::Exclude);
        if host == Dashboard {
            assert_eq!(page.cwd_override, Some(cwd));
            assert_eq!(page.kind_filter, Some(vec!["build".to_owned()]));
        }
        assert!(
            more(&mut app, host).is_empty(),
            "same page already in flight"
        );
        page.deliver(&mut app, &["second", "third", "third"], None);
        assert_eq!(ids(&app, host), ["first", "second", "third"]);
        assert!(more(&mut app, host).is_empty());
        if host != Welcome {
            assert!(entries(&app, Welcome).is_empty(), "rows leaked to Welcome");
        }
    }
}

#[test]
fn dashboard_grouped_selection_uses_dashboard_cwd_when_a_page_extends_its_pinned_group() {
    use crate::app::app_view::InputOutcome;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    let host = SessionPickerHost::Dashboard;
    let mut app = test_app();
    app.workspace_dashboard_enabled = true;
    app.active_view = ActiveView::AgentDashboard;
    app.cwd = PathBuf::from("/alpha");
    ensure_dashboard_state(&mut app);
    app.dashboard.as_mut().unwrap().cwd = PathBuf::from("/zeta");

    let row = |id: &str, cwd: &str| {
        let mut entry = make_picker_entry(id, cwd);
        entry.repo_name = crate::views::session_picker::repo_name_from_cwd(cwd);
        entry
    };
    let first = Request::from_effects(dispatch(Action::ShowSessionPicker, &mut app));
    dispatch(
        first.success(
            vec![row("keep", "/alpha"), row("first", "/zeta")],
            Some("older"),
            ListScope::Cwd,
        ),
        &mut app,
    );

    // The dashboard pins zeta first, putting the only alpha row below both headers.
    app.dashboard_session_picker
        .as_mut()
        .unwrap()
        .state
        .selected = 3;
    let assert_picks_keep = |app: &mut AppView| {
        let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let InputOutcome::Action(Action::DashboardPickSession(index)) = app.handle_input(&enter)
        else {
            panic!("Enter should pick the selected session");
        };
        assert_eq!(entries(app, host)[index].id, "keep");
    };
    assert_picks_keep(&mut app);
    let next = next_page(&mut app, host, "older");
    dispatch(
        next.success(vec![row("older", "/zeta")], None, ListScope::Cwd),
        &mut app,
    );
    assert_picks_keep(&mut app);
}

#[test]
fn selected_transcript_hit_stays_on_the_build_when_a_page_adds_its_native_row() {
    use crate::app::app_view::InputOutcome;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    let host = SessionPickerHost::Welcome;
    let id = "dbdec280-ba19-4579-aa12-5c13f632f287";
    let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    for (grouped, load_page) in [(false, false), (false, true), (true, true)] {
        let mut app = test_app();
        app.session_picker_grouped = grouped;
        let first = Request::from_effects(dispatch(Action::FetchSessionList, &mut app));
        let mut chat = make_conversation_entry(id);
        chat.summary = "needle in chat".into();
        dispatch(
            first.success(vec![chat], Some("older"), ListScope::Cwd),
            &mut app,
        );
        app.session_picker_state.set_query("needle");
        let search = dispatch(Action::ForceDeepSearch, &mut app);
        let [
            Effect::DeepSearchSessions {
                generation, seq, ..
            },
        ] = search.as_slice()
        else {
            panic!("expected a transcript search, got {search:?}");
        };
        dispatch(
            Action::TaskComplete(TaskResult::DeepSearchResults {
                host,
                generation: *generation,
                seq: *seq,
                results: vec![content_hit(id), content_hit("another-build")],
            }),
            &mut app,
        );
        // One fuzzy row, a content header, and (in grouped mode) a repo header.
        app.session_picker_state.selected = 2 + usize::from(grouped);
        let rendered_hits = crate::views::session_picker::build_content_entry_data(
            app.session_picker_content_results.as_deref().unwrap(),
            entries(&app, host),
            &[0],
            &app.session_picker_state,
            app.session_picker_state.selected,
        );
        assert_eq!(rendered_hits[0].summary, id);
        assert!(rendered_hits[0].is_selected);
        let InputOutcome::Action(pick @ Action::PickContentSession { .. }) =
            app.handle_input(&enter)
        else {
            panic!("the same-ID conversation must leave the build transcript selectable");
        };
        assert!(matches!(&pick, Action::PickContentSession { session_id, .. } if session_id == id));
        if !load_page {
            assert!(
                dispatch(pick, &mut app)
                    .iter()
                    .any(|effect| matches!(effect,
                        Effect::RestoreAndLoadSession { session_id, .. } if session_id == id
                    )),
                "a local transcript hit must take the build load path"
            );
            continue;
        }

        let older = next_page(&mut app, host, "older");
        let mut target = make_picker_entry(id, "/repo");
        target.summary = "needle in the build".into();
        let decoy = make_picker_entry("needle-next", "/repo");
        dispatch(
            older.success(vec![target, decoy], None, ListScope::Cwd),
            &mut app,
        );
        let InputOutcome::Action(Action::PickSession(index)) = app.handle_input(&enter) else {
            panic!("the selected hit should now be a native build row");
        };
        let picked = &entries(&app, host)[index];
        assert_eq!((picked.source.as_str(), picked.id.as_str()), ("local", id));
    }
}

#[test]
fn foreign_and_native_rows_with_the_same_id_keep_their_selection_when_a_page_arrives() {
    let host = SessionPickerHost::Welcome;
    let mut app = test_app();
    app.session_picker_grouped = false;
    app.session_picker_source_filter = SourceFilter::All;
    app.foreign_session_compat.codex = true;
    let effects = dispatch(Action::FetchSessionList, &mut app);
    let scan_seq = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ScanForeignSessions { seq, .. } => Some(*seq),
            _ => None,
        })
        .expect("foreign scan");
    let first = Request::from_effects(effects);
    let row = |id: &str, source: &str, seconds: i64| {
        let mut entry = make_picker_entry(id, "/repo");
        entry.source = source.to_owned();
        entry.updated_at = chrono::DateTime::from_timestamp(seconds, 0).unwrap();
        entry
    };
    dispatch(
        first.success(
            vec![row("same", "local", 50)],
            Some("older"),
            ListScope::Cwd,
        ),
        &mut app,
    );
    dispatch(
        Action::TaskComplete(TaskResult::ForeignSessionsScanned {
            entries: vec![row("same", "codex", 10)],
            seq: scan_seq,
        }),
        &mut app,
    );
    app.session_picker_state.selected = 1;
    app.session_picker_state.scroll_offset = Some(1);
    let page = next_page(&mut app, host, "older");
    dispatch(
        page.success(
            vec![
                row("same", "both", 50),
                row("middle", "local", 30),
                row("same", "conversation", 20),
            ],
            None,
            ListScope::Cwd,
        ),
        &mut app,
    );
    let sources_and_ids: Vec<_> = entries(&app, host)
        .iter()
        .map(|row| (row.source.as_str(), row.id.as_str()))
        .collect();
    assert_eq!(
        sources_and_ids,
        [
            ("local", "same"),
            ("local", "middle"),
            ("conversation", "same"),
            ("codex", "same"),
        ]
    );
    assert_eq!(
        sources_and_ids[app.session_picker_state.selected],
        ("codex", "same")
    );
    assert_eq!(app.session_picker_state.scroll_offset, Some(3));
}

#[test]
fn empty_pages_remain_reachable_and_only_the_failed_continuation_retries() {
    let host = SessionPickerHost::Welcome;
    let mut app = test_app();
    let first = Request::from_effects(dispatch(Action::FetchSessionList, &mut app));
    first.deliver(&mut app, &[], Some("one"));
    let one = next_page(&mut app, host, "one");
    one.deliver(&mut app, &["keep"], Some("two"));
    let two = next_page(&mut app, host, "two");
    one.fail(&mut app);
    one.deliver(&mut app, &["duplicate completion"], Some("wrong"));
    assert!(
        more(&mut app, host).is_empty(),
        "obsolete callbacks cleared the active request"
    );
    two.fail(&mut app);
    assert_eq!(ids(&app, host), ["keep"]);
    let retry = next_page(&mut app, host, "two");
    retry.deliver(&mut app, &[], Some("three"));
    let three = next_page(&mut app, host, "three");
    three.deliver(&mut app, &["keep", "last"], None);
    assert_eq!(ids(&app, host), ["keep", "last"]);
    assert!(more(&mut app, host).is_empty());
}

#[test]
fn repeated_cursors_and_scope_changes_terminate_the_stream() {
    let host = SessionPickerHost::Welcome;
    for (next, scope) in [
        ("two", ListScope::Cwd),
        ("one", ListScope::Cwd),
        ("three", ListScope::Repo),
    ] {
        let mut app = test_app();
        let first = Request::from_effects(dispatch(Action::FetchSessionList, &mut app));
        first.deliver(&mut app, &["first"], Some("one"));
        next_page(&mut app, host, "one").deliver(&mut app, &["second"], Some("two"));
        let two = next_page(&mut app, host, "two");
        dispatch(
            two.success(vec![make_picker_entry("third", "/repo")], Some(next), scope),
            &mut app,
        );
        assert!(
            more(&mut app, host).is_empty(),
            "resumed after cursor cycle or scope change"
        );
        if scope == ListScope::Repo {
            assert_eq!(
                ids(&app, host),
                ["first", "second"],
                "mixed directory scopes"
            );
        }
    }
}

#[test]
fn changing_chat_query_or_headless_filter_discards_inflight_pages() {
    for chat in [true, false] {
        let (mut app, host) = if chat {
            (test_app(), SessionPickerHost::Welcome)
        } else {
            (test_app_with_agent(), SessionPickerHost::AgentModal)
        };
        app.chat_mode = chat;
        let first = Request::from_effects(dispatch(Action::ShowSessionPicker, &mut app));
        first.deliver(&mut app, &["previous"], Some("old"));
        let old = next_page(&mut app, host, "old");
        let refresh = if chat {
            app.session_picker_state.set_query("changed");
            dispatch(Action::ForceDeepSearch, &mut app)
        } else {
            dispatch(Action::CycleSessionSourceFilter, &mut app)
        };
        let fresh = Request::from_effects(refresh);
        assert!(fresh.cursor.is_none());
        old.deliver(&mut app, &["stale"], Some("wrong"));
        assert!(more(&mut app, host).is_empty());
        fresh.deliver(&mut app, &["current"], Some("new"));
        old.fail(&mut app);
        let new = next_page(&mut app, host, "new");
        assert_eq!(new.query.as_deref(), chat.then_some("changed"));
        assert_eq!(
            new.headless_policy,
            if chat {
                HeadlessPolicy::Exclude
            } else {
                HeadlessPolicy::Only
            }
        );
        new.deliver(&mut app, &["last"], None);
        assert_eq!(ids(&app, host), ["current", "last"]);
    }
}

#[test]
fn pending_exact_resume_ignores_an_old_browse_page_containing_the_same_title() {
    let host = SessionPickerHost::AgentModal;
    let mut app = test_app_with_agent();
    let first = Request::from_effects(dispatch(Action::ShowSessionPicker, &mut app));
    first.deliver(&mut app, &["first"], Some("old-page"));
    let old = next_page(&mut app, host, "old-page");
    let effects = dispatch(Action::ShowSessionPickerFor("target".into()), &mut app);
    let Some(Effect::FetchSessionListForResume {
        host: exact_host,
        generation,
        seq,
        ..
    }) = effects.first()
    else {
        panic!("expected exhaustive exact lookup, got {effects:?}");
    };
    old.deliver(&mut app, &["target"], Some("wrong"));
    assert!(entries(&app, host).is_empty());
    assert!(more(&mut app, host).is_empty());
    let mut target = make_conversation_entry("unique-chat");
    target.summary = "target".into();
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host: *exact_host,
            generation: *generation,
            seq: *seq,
            query: None,
            cursor: None,
            next_cursor: None,
            partial: None,
            scope: ListScope::Cwd,
            sessions: vec![target],
        }),
        &mut app,
    );
    assert!(effects.iter().any(|e| matches!(e, Effect::LoadSession { session_id, chat_kind: true, .. } if session_id == "unique-chat")));
}
