//! Tests for `/add-dir` / `/remove-dir` dispatch: session gating, input
//! validation asymmetry (add pre-validates, remove tolerates vanished
//! paths), and the emitted mutation effect's shape.
use super::*;
use crate::app::actions::Effect;

fn mutate(app: &mut AppView, input: &str, remove: bool) -> Vec<Effect> {
    super::super::working_dirs::dispatch_working_directory_mutation(app, input.to_string(), remove)
}

#[test]
fn add_dir_outside_agent_view_is_a_noop() {
    // Welcome screen: no active agent to scope the mutation to.
    let mut app = test_app();
    assert!(mutate(&mut app, "~/proj", false).is_empty());
}

#[test]
fn add_dir_requires_a_path() {
    let mut app = test_app_with_agent();
    assert!(mutate(&mut app, "   ", false).is_empty());
    assert!(mutate(&mut app, "   ", true).is_empty());
}

#[test]
fn add_dir_without_session_emits_nothing() {
    let mut app = test_app_with_agent();
    let ActiveView::Agent(id) = app.active_view else {
        panic!("expected agent view");
    };
    app.agents.get_mut(&id).unwrap().session.session_id = None;
    assert!(mutate(&mut app, ".", false).is_empty());
}

#[test]
fn add_dir_unresolvable_path_is_rejected_client_side() {
    let mut app = test_app_with_agent();
    // Add pre-validates: a path that resolves to nothing never reaches the shell.
    assert!(mutate(&mut app, "/definitely/not/a/real/dir", false).is_empty());
}

#[test]
fn add_dir_emits_mutation_effect_for_resolvable_path() {
    let mut app = test_app_with_agent();
    let effects = mutate(&mut app, ".", false);
    assert_eq!(effects.len(), 1);
    match &effects[0] {
        Effect::SendWorkingDirectoryMutation {
            agent_id,
            session_id,
            path,
            remove,
        } => {
            let ActiveView::Agent(active) = app.active_view else {
                panic!("expected agent view");
            };
            assert_eq!(agent_id, &active);
            assert_eq!(session_id.0.to_string(), "test-session");
            // The typed spelling is forwarded; the shell canonicalizes.
            assert_eq!(path, ".");
            assert!(!remove);
        }
        other => panic!("expected SendWorkingDirectoryMutation, got {other:?}"),
    }
}

#[test]
fn remove_dir_skips_client_side_validation() {
    let mut app = test_app_with_agent();
    // Removal must tolerate vanished paths — the shell matches stored
    // spellings — so even an unresolvable input reaches the shell.
    let effects = mutate(&mut app, "/definitely/not/a/real/dir", true);
    assert_eq!(effects.len(), 1);
    match &effects[0] {
        Effect::SendWorkingDirectoryMutation { path, remove, .. } => {
            assert_eq!(path, "/definitely/not/a/real/dir");
            assert!(*remove);
        }
        other => panic!("expected SendWorkingDirectoryMutation, got {other:?}"),
    }
}
