//! `/login` -- log in or re-authenticate with your account.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

pub struct LoginCommand;

/// Provider choices shared by slash completion and the modal opened by a bare
/// `/login`. The modal can include the live Kimi credential source while the
/// inline completion path uses the provider-neutral description.
pub(crate) fn provider_items(kimi_status: Option<crate::settings::SecretStatus>) -> Vec<ArgItem> {
    let kimi_description = match kimi_status {
        Some(status) => format!("API key · {}", status.display()),
        None => "Configure an API key and query models".to_owned(),
    };
    vec![
        ArgItem {
            display: "xAI Grok".to_owned(),
            match_text: "xai grok oauth".to_owned(),
            insert_text: "xai".to_owned(),
            description: "Sign in with xAI".to_owned(),
        },
        ArgItem {
            display: "ChatGPT Codex".to_owned(),
            match_text: "codex openai chatgpt oauth".to_owned(),
            insert_text: "codex".to_owned(),
            description: "Connect an OpenAI Codex account".to_owned(),
        },
        ArgItem {
            display: "Kimi".to_owned(),
            match_text: "kimi moonshot api key coding".to_owned(),
            insert_text: "kimi".to_owned(),
            description: kimi_description,
        },
    ]
}

/// Resolve a user-facing provider token to its concrete login action. Shared
/// by typed slash execution and the provider picker so modal selections do not
/// need to synthesize and re-submit a slash command.
pub(crate) fn provider_action(args: &str) -> Result<Action, String> {
    let provider = args.trim().to_ascii_lowercase();
    match provider.as_str() {
        "xai" | "grok" => Ok(Action::Login),
        "codex" | "openai" | "chatgpt" => Ok(Action::LoginCodex),
        "kimi" | "moonshot" => Ok(Action::OpenKimiApiKeyEditor),
        _ => Err(format!(
            "Unknown provider: {}. Use /login xai, /login codex, or /login kimi",
            args.trim()
        )),
    }
}

impl SlashCommand for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }

    fn description(&self) -> &str {
        "Connect xAI, OpenAI Codex, or Kimi"
    }

    fn usage(&self) -> &str {
        "/login [xai|codex|kimi]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("provider")
    }

    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        Some(provider_items(None))
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if args.trim().is_empty() {
            return CommandResult::Action(Action::OpenLoginProviderPicker);
        }
        match provider_action(args) {
            Ok(action) => CommandResult::Action(action),
            Err(message) => CommandResult::Error(message),
        }
    }
}
