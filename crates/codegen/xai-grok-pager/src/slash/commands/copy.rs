//! `/copy` -- copy the last (or Nth) assistant message to the clipboard.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Copy an assistant message to the clipboard.
pub struct CopyCommand;

impl SlashCommand for CopyCommand {
    fn name(&self) -> &str {
        "copy"
    }

    fn description(&self) -> &str {
        "Copy last response to clipboard (/copy N for Nth-latest)"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        "/copy [N]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[N]")
    }

    /// Minimal mode has no in-app copy path — native terminal selection
    /// replaces it (K7/§6.13). Gated off with a message.
    fn available_in_minimal(&self) -> bool {
        false
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        let n = if trimmed.is_empty() {
            1
        } else {
            match trimmed.parse::<usize>() {
                Ok(0) => {
                    return CommandResult::Error(
                        "Usage: /copy [N] where N is 1 (latest), 2, 3, ...".to_string(),
                    );
                }
                Ok(v) => v,
                Err(_) => {
                    return CommandResult::Error(format!(
                        "/copy {trimmed} (invalid number)\nUsage: /copy [N] where N is 1 (latest), 2, 3, ..."
                    ));
                }
            }
        };
        CommandResult::Action(Action::CopyAssistantMessage { n })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::actions::Action;

    static DEFAULT_BUNDLE_STATE: crate::app::bundle::BundleState =
        crate::app::bundle::BundleState {
            has_cache: false,
            version: String::new(),
            personas: Vec::new(),
            roles: Vec::new(),
            agents: Vec::new(),
            skills: Vec::new(),
            persona_details: Vec::new(),
            role_details: Vec::new(),
        };

    fn make_ctx(models: &ModelState) -> CommandExecCtx<'_> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: &DEFAULT_BUNDLE_STATE,
            screen_mode: crate::app::ScreenMode::Inline,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn no_args_copies_latest() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = CopyCommand;
        match cmd.run(&mut ctx, "") {
            CommandResult::Action(Action::CopyAssistantMessage { n }) => assert_eq!(n, 1),
            other => panic!("expected Action(CopyAssistantMessage), got {other:?}"),
        }
    }

    #[test]
    fn explicit_1_copies_latest() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = CopyCommand;
        match cmd.run(&mut ctx, "1") {
            CommandResult::Action(Action::CopyAssistantMessage { n }) => assert_eq!(n, 1),
            other => panic!("expected Action(CopyAssistantMessage), got {other:?}"),
        }
    }

    #[test]
    fn explicit_3_copies_third() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = CopyCommand;
        match cmd.run(&mut ctx, "3") {
            CommandResult::Action(Action::CopyAssistantMessage { n }) => assert_eq!(n, 3),
            other => panic!("expected Action(CopyAssistantMessage), got {other:?}"),
        }
    }

    #[test]
    fn zero_returns_error() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = CopyCommand;
        assert!(matches!(cmd.run(&mut ctx, "0"), CommandResult::Error(_)));
    }

    #[test]
    fn non_numeric_returns_error() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = CopyCommand;
        match cmd.run(&mut ctx, "abc") {
            CommandResult::Error(msg) => assert!(msg.contains("invalid number")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn whitespace_only_copies_latest() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = CopyCommand;
        match cmd.run(&mut ctx, "   ") {
            CommandResult::Action(Action::CopyAssistantMessage { n }) => assert_eq!(n, 1),
            other => panic!("expected Action(CopyAssistantMessage), got {other:?}"),
        }
    }

    #[test]
    fn not_available_in_minimal() {
        // Native terminal selection replaces the in-app copy path in minimal.
        assert!(!CopyCommand.available_in_minimal());
    }
}
