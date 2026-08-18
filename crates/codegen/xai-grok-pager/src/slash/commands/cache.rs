//! `/cache` — compare all-provider and cache-reporting hit rates with diagnostics.

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
        "Compare all-provider and supported cache hit rates with reset diagnostics"
    }

    fn usage(&self) -> &str {
        "/cache"
    }

    fn takes_args(&self) -> bool {
        false
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::ShowCache)
    }
}
