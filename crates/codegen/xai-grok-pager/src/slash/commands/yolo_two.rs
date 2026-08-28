//! `/yolo-2` -- toggle always-approve only inside an applied OS sandbox.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Toggle always-approve while requiring actual process sandbox enforcement.
pub struct YoloTwoCommand;

impl SlashCommand for YoloTwoCommand {
    fn name(&self) -> &str {
        "yolo-2"
    }

    fn description(&self) -> &str {
        "Toggle always-approve mode while an OS sandbox is active"
    }

    fn usage(&self) -> &str {
        "/yolo-2"
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        yolo_two_result(ctx.pager_state.yolo_mode, xai_grok_sandbox::is_active())
    }
}

fn yolo_two_result(yolo_mode: bool, sandbox_active: bool) -> CommandResult {
    if !sandbox_active {
        return CommandResult::Error(
            "YOLO-2 requires an active OS sandbox. Restart Open Grok with `--sandbox workspace` before enabling it."
                .to_owned(),
        );
    }

    CommandResult::Action(Action::SetYoloMode(!yolo_mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_two_rejects_unsandboxed_processes() {
        let CommandResult::Error(message) = yolo_two_result(false, false) else {
            panic!("an unsandboxed process must not enable YOLO-2");
        };
        assert!(message.contains("active OS sandbox"));
        assert!(message.contains("--sandbox workspace"));
    }

    #[test]
    fn yolo_two_enables_always_approve_when_sandboxed() {
        assert!(matches!(
            yolo_two_result(false, true),
            CommandResult::Action(Action::SetYoloMode(true))
        ));
    }

    #[test]
    fn yolo_two_disables_always_approve_when_sandboxed() {
        assert!(matches!(
            yolo_two_result(true, true),
            CommandResult::Action(Action::SetYoloMode(false))
        ));
    }
}
