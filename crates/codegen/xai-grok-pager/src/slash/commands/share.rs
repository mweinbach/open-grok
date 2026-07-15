//! `/share` -- share current session via URL.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Share the current session via a public URL.
pub struct ShareCommand;

impl SlashCommand for ShareCommand {
    fn name(&self) -> &str {
        "share"
    }

    fn description(&self) -> &str {
        "Share this session via URL"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        "/share"
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        // Check if we have an active session
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session to share".to_string());
        }

        CommandResult::Action(Action::ShareSession)
    }
}
