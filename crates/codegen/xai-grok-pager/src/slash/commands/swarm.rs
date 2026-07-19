//! `/swarm` — control coordinated subagent swarms.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

pub struct SwarmCommand;

impl SlashCommand for SwarmCommand {
    fn name(&self) -> &str {
        "swarm"
    }
    fn description(&self) -> &str {
        "Toggle swarm mode or run a one-shot swarm task"
    }
    fn usage(&self) -> &str {
        "/swarm [on|off|task]"
    }
    fn takes_args(&self) -> bool {
        true
    }
    fn arg_placeholder(&self) -> Option<&str> {
        Some("[on|off|task]")
    }

    fn suggest_args(&self, _ctx: &AppCtx, prefix: &str) -> Option<Vec<ArgItem>> {
        [
            ("on", "Enable swarm mode persistently"),
            ("off", "Disable swarm mode persistently"),
        ]
        .into_iter()
        .filter(|(value, _)| value.starts_with(prefix.trim()))
        .map(|(value, description)| ArgItem {
            display: value.into(),
            match_text: value.into(),
            insert_text: value.into(),
            description: description.into(),
        })
        .collect::<Vec<_>>()
        .into()
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        match trimmed {
            "" => CommandResult::Action(Action::SetSwarmMode {
                enabled: !ctx.pager_state.swarm_mode,
                trigger: "manual",
                persist: true,
            }),
            "on" => CommandResult::Action(Action::SetSwarmMode {
                enabled: true,
                trigger: "manual",
                persist: true,
            }),
            "off" => CommandResult::Action(Action::SetSwarmMode {
                enabled: false,
                trigger: "manual",
                persist: true,
            }),
            _ => CommandResult::Action(Action::StartSwarmTask(trimmed.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    fn ctx(swarm_mode: bool) -> CommandExecCtx<'static> {
        let models = Box::leak(Box::new(ModelState::default()));
        let bundle = Box::leak(Box::new(BundleState::default()));
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            pager_state: PagerLocalSnapshot {
                swarm_mode,
                ..PagerLocalSnapshot::default()
            },
        }
    }

    #[test]
    fn parses_toggle_and_explicit_controls() {
        assert!(matches!(
            SwarmCommand.run(&mut ctx(false), ""),
            CommandResult::Action(Action::SetSwarmMode {
                enabled: true,
                persist: true,
                ..
            })
        ));
        assert!(matches!(
            SwarmCommand.run(&mut ctx(true), ""),
            CommandResult::Action(Action::SetSwarmMode {
                enabled: false,
                persist: true,
                ..
            })
        ));
        assert!(matches!(
            SwarmCommand.run(&mut ctx(false), "on"),
            CommandResult::Action(Action::SetSwarmMode { enabled: true, .. })
        ));
        assert!(matches!(
            SwarmCommand.run(&mut ctx(true), "off"),
            CommandResult::Action(Action::SetSwarmMode { enabled: false, .. })
        ));
    }

    #[test]
    fn task_is_preserved_as_a_one_shot_prompt() {
        match SwarmCommand.run(&mut ctx(false), "  investigate auth races  ") {
            CommandResult::Action(Action::StartSwarmTask(task)) => {
                assert_eq!(task, "investigate auth races")
            }
            other => panic!("expected one-shot task, got {other:?}"),
        }
    }
}
