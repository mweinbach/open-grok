//! `/login` -- log in or re-authenticate with your account.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

pub struct LoginCommand;

/// Provider choices shared by slash completion and the modal opened by a bare
/// `/login`. The modal can include live API-key credential
/// sources while the inline completion path uses the provider-neutral
/// description.
pub(crate) fn provider_items(
    kimi_status: Option<crate::settings::SecretStatus>,
    fireworks_status: Option<crate::settings::SecretStatus>,
    deepseek_status: Option<crate::settings::SecretStatus>,
    meta_status: Option<crate::settings::SecretStatus>,
    opencode_go_status: Option<crate::settings::SecretStatus>,
    wafer_status: Option<crate::settings::SecretStatus>,
    zai_status: Option<crate::settings::SecretStatus>,
) -> Vec<ArgItem> {
    let api_key_description = |status: Option<crate::settings::SecretStatus>| match status {
        Some(status) => format!("API key · {}", status.display()),
        None => "Configure an API key and query models".to_owned(),
    };
    let kimi_description = api_key_description(kimi_status);
    let fireworks_description = api_key_description(fireworks_status);
    let deepseek_description = api_key_description(deepseek_status);
    let meta_description = api_key_description(meta_status);
    let opencode_go_description = api_key_description(opencode_go_status);
    let wafer_description = api_key_description(wafer_status);
    let zai_description = api_key_description(zai_status);
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
        ArgItem {
            display: "Fireworks AI".to_owned(),
            match_text: "fireworks ai api key glm deepseek".to_owned(),
            insert_text: "fireworks".to_owned(),
            description: fireworks_description,
        },
        ArgItem {
            display: "DeepSeek".to_owned(),
            match_text: "deepseek api direct key".to_owned(),
            insert_text: "deepseek".to_owned(),
            description: deepseek_description,
        },
        ArgItem {
            display: "Meta API".to_owned(),
            match_text: "meta ai api key muse spark responses web search".to_owned(),
            insert_text: "meta".to_owned(),
            description: meta_description,
        },
        ArgItem {
            display: "OpenCode Go".to_owned(),
            match_text: "opencode go api key dynamic models".to_owned(),
            insert_text: "opencode-go".to_owned(),
            description: opencode_go_description,
        },
        ArgItem {
            display: "Wafer AI".to_owned(),
            match_text: "wafer wafer ai api key chat completions dynamic models".to_owned(),
            insert_text: "wafer".to_owned(),
            description: wafer_description,
        },
        ArgItem {
            display: "Z AI".to_owned(),
            match_text: "z ai zai api key glm coding plan chat completions dynamic models"
                .to_owned(),
            insert_text: "zai".to_owned(),
            description: zai_description,
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
        "fireworks" => Ok(Action::OpenFireworksApiKeyEditor),
        "deepseek" | "deep-seek" | "deepseek-api" => Ok(Action::OpenDeepSeekApiKeyEditor),
        "meta" | "meta-ai" | "meta_ai" | "meta-api" => Ok(Action::OpenMetaApiKeyEditor),
        "opencode" | "opencode-go" | "opencode_go" | "go" => Ok(Action::OpenOpenCodeGoApiKeyEditor),
        "wafer" | "wafer-ai" | "wafer_ai" => Ok(Action::OpenWaferApiKeyEditor),
        "zai" | "z-ai" | "z_ai" => Ok(Action::OpenZaiApiKeyEditor),
        _ => Err(format!(
            "Unknown provider: {}. Use /login xai, /login codex, /login kimi, /login fireworks, /login deepseek, /login meta, /login wafer, /login zai, or /login opencode-go",
            args.trim()
        )),
    }
}

impl SlashCommand for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }

    fn description(&self) -> &str {
        "Connect xAI, OpenAI Codex, Kimi, Fireworks AI, DeepSeek, Meta API, Wafer AI, Z AI, or OpenCode Go"
    }

    fn usage(&self) -> &str {
        "/login [xai|codex|kimi|fireworks|deepseek|meta|wafer|zai|opencode-go]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("provider")
    }

    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        Some(provider_items(None, None, None, None, None, None, None))
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
