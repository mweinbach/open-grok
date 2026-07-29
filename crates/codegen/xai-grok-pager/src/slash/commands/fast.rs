//! `/fast` — toggle Fast service tier (priority routing) when the active
//! model advertises it (Codex catalog `service_tiers` / legacy speed tiers).

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, CommandExecCtx, CommandResult, SlashCommand};

/// Toggle Fast mode for the current model.
pub struct FastCommand;

impl SlashCommand for FastCommand {
    fn name(&self) -> &str {
        "fast"
    }

    fn description(&self) -> &str {
        "Toggle Fast mode (priority routing) for the current model"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        "/fast"
    }

    fn visible(&self, ctx: &AppCtx) -> bool {
        ctx.models.current_supports_fast()
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        let Some(model_id) = ctx.models.current.clone() else {
            return CommandResult::Error("No active model".into());
        };
        let Some(fast_id) = ctx.models.current_fast_service_tier_id() else {
            return CommandResult::Error("current model does not support Fast mode".into());
        };

        let next_tier = if ctx.models.fast_mode_enabled() {
            // Explicit standard routing so the shell clears the prior selection.
            Some(None)
        } else {
            Some(Some(fast_id))
        };

        CommandResult::Action(Action::SwitchModel {
            model_id,
            // Preserve the session effort when only the service tier changes.
            effort: ctx.models.reasoning_effort,
            service_tier: next_tier,
        })
    }
}
