//! `/add-dir` / `/remove-dir` dispatch: resolve the active agent's session,
//! pre-validate the path client-side for instant feedback, and fire the
//! working-directory mutation effect. The shell re-validates
//! canonically (the client check is UX, not the trust boundary).

use super::ctx::NO_SESSION_NOTICE;
use crate::app::actions::Effect;
use crate::app::app_view::{ActiveView, AppView};

pub(super) fn dispatch_working_directory_mutation(
    app: &mut AppView,
    input: String,
    remove: bool,
) -> Vec<Effect> {
    let trimmed = input.trim().to_string();
    let ActiveView::Agent(id) = app.active_view else {
        app.show_toast("No active session");
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if trimmed.is_empty() {
        agent.show_toast(if remove {
            "Usage: /remove-dir <path>"
        } else {
            "Usage: /add-dir <path>"
        });
        return vec![];
    }
    let Some(session_id) = agent.session.session_id.clone() else {
        agent.show_toast(NO_SESSION_NOTICE);
        return vec![];
    };
    // Instant client-side rejection for paths that clearly do not resolve;
    // the shell remains authoritative (symlinks, canonicalization, races).
    // Removal must tolerate paths that vanished from disk — the shell
    // matches stored canonical spellings — so only add pre-validates.
    if !remove
        && !super::dashboard::resolve_location_input(&trimmed, app.cwd.as_path())
            .is_some_and(|p| p.is_dir())
    {
        agent.show_toast(&format!("Not a directory: {trimmed}"));
        return vec![];
    }
    agent.show_toast(if remove {
        "Removing working directory…"
    } else {
        "Adding working directory…"
    });
    vec![Effect::SendWorkingDirectoryMutation {
        agent_id: id,
        session_id,
        path: trimmed,
        remove,
    }]
}
