//! `/cache` — session prompt-cache hit rate and prefix-break log.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Show prompt-cache hit rate and where the prefix last broke.
pub struct CacheCommand;

impl SlashCommand for CacheCommand {
    fn name(&self) -> &str {
        "cache"
    }

    fn description(&self) -> &str {
        "View prompt cache hit rate and prefix breaks"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        "/cache"
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
    use crate::slash::commands::tests::make_ctx;

    #[test]
    fn cache_requires_a_session() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        match CacheCommand.run(&mut ctx, "") {
            CommandResult::Error(msg) => assert!(msg.contains("No active session")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn cache_with_session_emits_show_cache() {
        let models = ModelState::default();
        let session_id = agent_client_protocol::SessionId::new("sess-1");
        let mut ctx = make_ctx(&models);
        ctx.session_id = Some(&session_id);
        assert!(matches!(
            CacheCommand.run(&mut ctx, ""),
            CommandResult::Action(Action::ShowCache)
        ));
    }
}
