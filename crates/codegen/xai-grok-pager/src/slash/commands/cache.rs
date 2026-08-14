//! `/cache` — view prompt cache hit rates, break diagnostics, and turn records.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct CacheCommand;

impl SlashCommand for CacheCommand {
    fn name(&self) -> &str {
        "cache"
    }

    fn aliases(&self) -> &[&str] {
        &["cache-status", "prompt-cache"]
    }

    fn description(&self) -> &str {
        "View prompt cache hit rates, prefix divergence, and break diagnostics"
    }

    fn usage(&self) -> &str {
        "/cache"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn takes_args(&self) -> bool {
        false
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session".to_string());
        }

        CommandResult::Action(Action::ShowCache)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    static DEFAULT_BUNDLE_STATE: BundleState = BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn ctx<'a>(
        models: &'a ModelState,
        session_id: Option<&'a agent_client_protocol::SessionId>,
    ) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id,
            bundle_state: &DEFAULT_BUNDLE_STATE,
            billing_surface_visible: true,
            screen_mode: crate::app::ScreenMode::Inline,
            pager_state: PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn cache_without_session_errors() {
        let models = ModelState::default();
        let mut exec_ctx = ctx(&models, None);
        match CacheCommand.run(&mut exec_ctx, "") {
            CommandResult::Error(msg) => assert!(msg.contains("No active session")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn cache_with_session_dispatches_show_cache() {
        let models = ModelState::default();
        let sid = agent_client_protocol::SessionId::from("s1".to_string());
        let mut exec_ctx = ctx(&models, Some(&sid));
        assert!(matches!(
            CacheCommand.run(&mut exec_ctx, ""),
            CommandResult::Action(Action::ShowCache)
        ));
    }
}
