//! `/mission` -- manage autonomous missions across Open Grok and Factory Droid.
//!
//! Provides unified lifecycle control, cross-tool discovery, and continuity between
//! Factory Droid missions (`~/.factory/missions`) and Open Grok (`$OPENGROK_HOME/missions`).

use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};
use std::fs;
use std::path::Path;
use xai_grok_tools::mission::{
    MissionFileService, MissionSource, discover_all_missions,
    discover_missions_for_workspace, find_mission, opengrok_missions_dir,
};

/// Slash command `/mission`
pub struct MissionCommand;

impl SlashCommand for MissionCommand {
    fn name(&self) -> &str {
        "mission"
    }

    fn description(&self) -> &str {
        "Manage autonomous missions across Open Grok and Factory Droid"
    }

    fn usage(&self) -> &str {
        "/mission [list|status|continue|new|import|pause]"
    }

    fn suggest_args(&self, _ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let trimmed = args_query.trim_start();
        let parts: Vec<&str> = trimmed.split_whitespace().collect();

        if parts.is_empty() || (parts.len() == 1 && !trimmed.ends_with(' ')) {
            let ops = [
                ("list", "List all missions in Open Grok and Factory Droid"),
                ("status", "Show detailed status and progress of a mission"),
                ("continue", "Resume autonomous execution of a mission"),
                ("new", "Propose and plan a new mission for this workspace"),
                ("import", "Import an existing Factory Droid mission into Open Grok"),
                ("pause", "Pause the currently running autonomous mission"),
            ];
            let q = parts.first().copied().unwrap_or("");
            return Some(
                ops.iter()
                    .filter(|(op, _)| op.starts_with(q))
                    .map(|(op, desc)| ArgItem {
                        display: op.to_string(),
                        match_text: op.to_string(),
                        insert_text: format!("{op} "),
                        description: desc.to_string(),
                    })
                    .collect(),
            );
        }

        let op = parts[0].to_lowercase();
        if (op == "continue" || op == "status" || op == "import") && (parts.len() == 1 || (parts.len() == 2 && !trimmed.ends_with(' '))) {
            let id_query = parts.get(1).copied().unwrap_or("");
            let all = discover_all_missions();
            return Some(
                all.iter()
                    .filter(|m| {
                        if op == "import" {
                            m.source == MissionSource::FactoryDroid
                        } else {
                            true
                        }
                    })
                    .filter(|m| m.id.to_lowercase().starts_with(&id_query.to_lowercase()) || m.title.to_lowercase().contains(&id_query.to_lowercase()))
                    .take(20)
                    .map(|m| ArgItem {
                        display: format!("{} ({})", m.title, &m.id[..8.min(m.id.len())]),
                        match_text: m.id.clone(),
                        insert_text: format!("{} {} ", op, m.id),
                        description: format!("[{}] {}/{} done ({})", m.source, m.completed_features, m.total_features, m.state),
                    })
                    .collect(),
            );
        }

        None
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() || trimmed == "help" {
            return CommandResult::Message(format!(
                "**Open Grok Mission Engine**\n\n\
                 Available subcommands:\n\
                 - `/mission list`: Discover and view all missions across Open Grok and Factory Droid\n\
                 - `/mission status [id]`: Show detailed feature progress and handoffs\n\
                 - `/mission continue [id]`: Resume autonomous execution of a mission\n\
                 - `/mission new <title>`: Plan and propose a new mission\n\
                 - `/mission import <id>`: Import a Factory Droid mission into Open Grok\n\
                 - `/mission pause`: Pause the active mission\n"
            ));
        }

        let mut parts = trimmed.split_whitespace();
        let subcmd = parts.next().unwrap_or("").to_lowercase();
        let rest: Vec<&str> = parts.collect();
        let target_arg = rest.join(" ");

        match subcmd.as_str() {
            "list" => handle_list(),
            "status" => handle_status(&target_arg),
            "continue" | "run" => handle_continue(&target_arg),
            "new" => handle_new(&target_arg),
            "import" => handle_import(&target_arg),
            "pause" => handle_pause(),
            _ => CommandResult::Error(format!(
                "Unknown subcommand: '{}'. Run `/mission help` for usage.",
                subcmd
            )),
        }
    }
}

fn handle_list() -> CommandResult {
    let missions = discover_all_missions();
    if missions.is_empty() {
        return CommandResult::Message(
            "No missions found in Open Grok (`$OPENGROK_HOME/missions`) or Factory Droid (`~/.factory/missions`).\n\
             Use `/mission new <title>` to propose a new mission."
                .to_string(),
        );
    }

    let mut out = String::from("### Discovered Missions\n\n");
    out.push_str("| ID | Source | Title | State | Features | Working Directory |\n");
    out.push_str("| --- | --- | --- | --- | --- | --- |\n");

    for m in missions {
        let short_id = if m.id.len() > 8 {
            &m.id[..8]
        } else {
            &m.id
        };
        let src_label = match m.source {
            MissionSource::OpenGrok => "Open Grok",
            MissionSource::FactoryDroid => "Droid",
        };
        let wd = m.working_directory.as_deref().unwrap_or("-");
        let short_wd = if wd.len() > 25 {
            format!("...{}", &wd[wd.len() - 22..])
        } else {
            wd.to_string()
        };

        out.push_str(&format!(
            "| `{}` | {} | {} | `{}` | {}/{} done | `{}` |\n",
            short_id, src_label, m.title, m.state, m.completed_features, m.total_features, short_wd
        ));
    }

    out.push_str("\n*Tip: Run `/mission status <id>` to inspect or `/mission continue <id>` to resume an existing Droid or Open Grok mission.*");
    CommandResult::Message(out)
}

fn handle_status(target: &str) -> CommandResult {
    let mission = if !target.is_empty() {
        find_mission(target)
    } else {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| discover_missions_for_workspace(&cwd).into_iter().next())
            .or_else(|| discover_all_missions().into_iter().next())
    };

    let Some(m) = mission else {
        return CommandResult::Error(format!(
            "No mission found matching '{}'. Use `/mission list` to see available missions.",
            target
        ));
    };

    let svc = MissionFileService::new(&m.dir);
    let features_file = svc.read_features().ok();

    let mut out = format!("### Mission: {}\n", m.title);
    out.push_str(&format!("- **ID**: `{}`\n", m.id));
    if let Some(mid) = &m.mission_id {
        out.push_str(&format!("- **Internal ID**: `{}`\n", mid));
    }
    out.push_str(&format!("- **Source**: {}\n", m.source));
    out.push_str(&format!("- **State**: `{}`\n", m.state));
    out.push_str(&format!("- **Directory**: `{}`\n", m.dir.display()));
    if let Some(wd) = &m.working_directory {
        out.push_str(&format!("- **Working Directory**: `{}`\n", wd));
    }

    out.push_str(&format!(
        "- **Progress**: {} completed, {} in-progress, {} pending (Total: {})\n\n",
        m.completed_features, m.in_progress_features, m.pending_features, m.total_features
    ));

    if let Some(ff) = features_file {
        if !ff.features.is_empty() {
            out.push_str("#### Recent / Active Features\n");
            for f in ff.features.iter().take(10) {
                let status_icon = match f.status {
                    xai_grok_tools::mission::FeatureStatus::Completed => "✅",
                    xai_grok_tools::mission::FeatureStatus::InProgress => "🔄",
                    xai_grok_tools::mission::FeatureStatus::Pending => "⏳",
                };
                out.push_str(&format!(
                    "- {} **{}** `[{}]`: {}\n",
                    status_icon, f.id, f.skill_name, f.description
                ));
            }
            if ff.features.len() > 10 {
                out.push_str(&format!("\n*...and {} more features*\n", ff.features.len() - 10));
            }
        }
    }

    CommandResult::Message(out)
}

fn handle_continue(target: &str) -> CommandResult {
    let mission = if !target.is_empty() {
        find_mission(target)
    } else {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| discover_missions_for_workspace(&cwd).into_iter().next())
            .or_else(|| discover_all_missions().into_iter().next())
    };

    let Some(m) = mission else {
        return CommandResult::Error(format!(
            "No mission found matching '{}'. Use `/mission list` to see available missions.",
            target
        ));
    };

    let prompt = format!(
        "Continue execution of the mission \"{}\" (id: {}, directory: {}). Call the `start_mission_run` tool to advance the next pending feature.",
        m.title, m.id, m.dir.display()
    );

    CommandResult::QueueCommand(prompt)
}

fn handle_new(title: &str) -> CommandResult {
    let t = if title.is_empty() {
        "New Autonomous Mission"
    } else {
        title
    };

    let prompt = format!(
        "The user wants to propose and initialize a new autonomous mission titled \"{}\".\n\
         First inspect the project structure, plan the architecture, milestones, and initial features, then call the `propose_mission` tool.",
        t
    );

    CommandResult::QueueCommand(prompt)
}

fn handle_pause() -> CommandResult {
    CommandResult::QueueCommand(
        "Pause the currently active autonomous mission using the `start_mission_run` tool with pause disposition.".to_string(),
    )
}

fn handle_import(target: &str) -> CommandResult {
    if target.is_empty() {
        return CommandResult::Error(
            "Please specify the Factory Droid mission ID to import: `/mission import <id>`".to_string(),
        );
    }

    let Some(m) = find_mission(target) else {
        return CommandResult::Error(format!("No mission found matching '{}'", target));
    };

    if m.source == MissionSource::OpenGrok {
        return CommandResult::Message(format!(
            "Mission '{}' is already an Open Grok native mission at `{}`.",
            m.title,
            m.dir.display()
        ));
    }

    let dest_dir = opengrok_missions_dir().join(&m.id);
    if dest_dir.exists() {
        return CommandResult::Message(format!(
            "Mission '{}' already exists in Open Grok at `{}`.",
            m.title,
            dest_dir.display()
        ));
    }

    if let Err(e) = copy_dir_all(&m.dir, &dest_dir) {
        return CommandResult::Error(format!("Failed to copy mission files: {e}"));
    }

    CommandResult::Message(format!(
        "Successfully imported Factory Droid mission **{}** into Open Grok!\n\
         - **Source**: `{}`\n\
         - **Imported to**: `{}`\n\
         - **Features**: {} ({} completed)\n\n\
         You can now run `/mission continue {}` to execute or continue this mission in Open Grok.",
        m.title,
        m.dir.display(),
        dest_dir.display(),
        m.total_features,
        m.completed_features,
        m.id
    ))
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    static DEFAULT_BUNDLE_STATE: BundleState = BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn make_ctx<'a>(models: &'a ModelState, bundle: &'a BundleState) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            pager_state: PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn test_mission_help() {
        let cmd = MissionCommand;
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = make_ctx(&models, &bundle);
        let res = cmd.run(&mut ctx, "help");
        match res {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Open Grok Mission Engine"));
                assert!(msg.contains("/mission list"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn test_mission_list() {
        let cmd = MissionCommand;
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = make_ctx(&models, &bundle);
        let res = cmd.run(&mut ctx, "list");
        match res {
            CommandResult::Message(msg) => {
                // If droid missions exist on machine, lists them; otherwise shows helpful empty message
                assert!(msg.contains("Missions") || msg.contains("No missions found"));
            }
            _ => panic!("Expected Message result"),
        }
    }
}
