//! `/add-dir <path>` / `/remove-dir <path>` — manage a live session's
//! additional working directories.
//!
//! Unlike `/cd` (a dashboard affordance that only changes where NEW
//! sessions spawn), these mutate the ACTIVE session's scope at runtime:
//! the shell appends session-scoped Read/Edit allow rules for the added
//! root, persists the set to `working_dirs.json`, and discloses the new
//! working set to the model as an environment update. Subagents inherit
//! the widened scope through the shared permission handle.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Add a directory to the active session's working set.
pub struct AddDirCommand;

impl SlashCommand for AddDirCommand {
    fn name(&self) -> &str {
        "add-dir"
    }

    fn description(&self) -> &str {
        "Add a directory to this session's working set"
    }

    fn usage(&self) -> &str {
        "/add-dir <path>"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("path")
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Error("Usage: /add-dir <path>".to_string());
        }
        CommandResult::Action(Action::AddSessionWorkingDirectory {
            input: trimmed.to_string(),
        })
    }
}

/// Remove a directory from the active session's working set.
pub struct RemoveDirCommand;

impl SlashCommand for RemoveDirCommand {
    fn name(&self) -> &str {
        "remove-dir"
    }

    fn description(&self) -> &str {
        "Remove a directory from this session's working set"
    }

    fn usage(&self) -> &str {
        "/remove-dir <path>"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("path")
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Error("Usage: /remove-dir <path>".to_string());
        }
        CommandResult::Action(Action::RemoveSessionWorkingDirectory {
            input: trimmed.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;

    /// Build a throwaway exec ctx over the given borrows. Mirrors the
    /// inline ctx construction in `cd.rs`'s command tests.
    fn ctx<'a>(models: &'a ModelState, bundle: &'a BundleState) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: false,
                yolo_mode: false,
                ..crate::settings::PagerLocalSnapshot::default()
            },
        }
    }

    #[test]
    fn add_dir_requires_a_path() {
        let (models, bundle) = (ModelState::default(), BundleState::default());
        let mut c = ctx(&models, &bundle);
        for args in ["", "   "] {
            match AddDirCommand.run(&mut c, args) {
                CommandResult::Error(msg) => {
                    assert!(msg.contains("Usage"), "expected usage error, got: {msg}");
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }
    }

    #[test]
    fn add_dir_path_arg_dispatches_action() {
        let (models, bundle) = (ModelState::default(), BundleState::default());
        let mut c = ctx(&models, &bundle);
        match AddDirCommand.run(&mut c, "  ~/projects/foo  ") {
            CommandResult::Action(Action::AddSessionWorkingDirectory { input }) => {
                assert_eq!(input, "~/projects/foo");
            }
            other => panic!("expected AddSessionWorkingDirectory, got {other:?}"),
        }
    }

    #[test]
    fn remove_dir_requires_a_path() {
        let (models, bundle) = (ModelState::default(), BundleState::default());
        let mut c = ctx(&models, &bundle);
        match RemoveDirCommand.run(&mut c, "") {
            CommandResult::Error(msg) => {
                assert!(msg.contains("Usage"), "expected usage error, got: {msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn remove_dir_path_arg_dispatches_action() {
        let (models, bundle) = (ModelState::default(), BundleState::default());
        let mut c = ctx(&models, &bundle);
        match RemoveDirCommand.run(&mut c, "~/projects/foo") {
            CommandResult::Action(Action::RemoveSessionWorkingDirectory { input }) => {
                assert_eq!(input, "~/projects/foo");
            }
            other => panic!("expected RemoveSessionWorkingDirectory, got {other:?}"),
        }
    }

    #[test]
    fn metadata() {
        let add = AddDirCommand;
        assert_eq!(add.name(), "add-dir");
        assert!(add.takes_args());
        assert_eq!(add.arg_placeholder(), Some("path"));
        assert!(!add.description().is_empty());
        assert!(!add.usage().is_empty());
        // Session-scoped: available on every surface, unlike dashboard-only /cd.
        assert!(!add.dashboard_only(), "/add-dir must not be dashboard-only");

        let remove = RemoveDirCommand;
        assert_eq!(remove.name(), "remove-dir");
        assert!(remove.takes_args());
        assert_eq!(remove.arg_placeholder(), Some("path"));
        assert!(!remove.description().is_empty());
        assert!(!remove.usage().is_empty());
        assert!(
            !remove.dashboard_only(),
            "/remove-dir must not be dashboard-only"
        );
    }
}
