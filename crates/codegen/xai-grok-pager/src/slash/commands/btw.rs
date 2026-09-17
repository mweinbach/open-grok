//! `/btw` -- ask a side question without interrupting the running agent.
//!
//! Returns `CommandResult::Action(Action::SendBtw(...))` so the dispatch layer
//! fires it as an ACP ext method (`x.ai/btw`) that bypasses the prompt queue.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct BtwCommand;

impl SlashCommand for BtwCommand {
    slash_meta! {
        name: "btw",
        description: "Ask a side question without interrupting",
        usage: "/btw <question>",
        takes_args: true,
        args_required: true,
        session_scoped: true,
        can_hoist_from_mid_text: true,
        arg_placeholder: "<question>",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        CommandResult::Action(Action::SendBtw(args.trim().to_string()))
    }
}
