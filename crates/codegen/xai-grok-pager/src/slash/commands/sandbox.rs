use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct SandboxCommand;

impl SlashCommand for SandboxCommand {
    fn name(&self) -> &str {
        "sandbox"
    }

    fn description(&self) -> &str {
        "Open OS sandbox settings (off by default; restart to apply)"
    }

    fn usage(&self) -> &str {
        "/sandbox"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenSettingsFocus {
            key: "sandbox.profile",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::ScreenMode;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    #[test]
    fn sandbox_opens_settings_without_enabling_in_every_screen_mode() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        for screen_mode in [
            ScreenMode::Fullscreen,
            ScreenMode::Inline,
            ScreenMode::Minimal,
        ] {
            for args in ["", "enable", "disable"] {
                let mut context = CommandExecCtx {
                    models: &models,
                    session_id: None,
                    bundle_state: &bundle,
                    screen_mode,
                    billing_surface_visible: true,
                    pager_state: PagerLocalSnapshot::default(),
                };
                assert!(matches!(
                    SandboxCommand.run(&mut context, args),
                    CommandResult::Action(Action::OpenSettingsFocus {
                        key: "sandbox.profile",
                    })
                ));
            }
        }
        assert!(!SandboxCommand.takes_args());
        assert!(
            super::super::builtin_commands()
                .iter()
                .any(|command| command.name() == "sandbox")
        );
    }
}
