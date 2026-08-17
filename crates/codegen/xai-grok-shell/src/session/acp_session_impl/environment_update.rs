//! Append-only environment updates for long-running sessions.
//!
//! The first-turn `<user_info>` prefix and the spawn-time AGENTS.md
//! `ProjectInstructions` item are frozen so the prompt-cache prefix stays
//! byte-stable. When the live environment later diverges (date, cwd/os/shell,
//! or project-rule files), inject a hidden `<system-reminder>` at the tail
//! instead of rewriting those prefix items.

use super::*;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use xai_grok_agent::prompt::agents_md::AgentConfigFile;
use xai_grok_sampling_types::conversation::{ContentPart, ConversationItem, SyntheticReason};

/// Opening marker for an appended environment-update reminder.
pub(crate) const ENVIRONMENT_UPDATE_OPEN: &str = "<environment-update";
const USER_INFO_SOURCE: &str = "user_info";
const PROJECT_RULES_SOURCE: &str = "project_rules";

pub(crate) fn fingerprint_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Hash of the stable `<user_info>` identity, excluding the date line.
///
/// Date rollover already has its own reminder. Including the date here would
/// double-announce every midnight.
pub(crate) fn user_info_identity_fingerprint(user_info_block: &str) -> u64 {
    fingerprint_text(&strip_user_info_date_line(user_info_block))
}

pub(crate) fn strip_user_info_date_line(block: &str) -> String {
    block
        .lines()
        .filter(|line| {
            !line
                .trim_start()
                .starts_with(crate::session::user_message::USER_INFO_DATE_MARKER)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn rules_fingerprint(files: &[AgentConfigFile]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for file in files {
        file.file_path.hash(&mut hasher);
        file.content.hash(&mut hasher);
    }
    hasher.finish()
}

pub(crate) fn extract_user_info_block(text: &str) -> Option<&str> {
    let start = text.find("<user_info>")?;
    let end = text[start..].find("</user_info>")?;
    Some(&text[start..start + end + "</user_info>".len()])
}

pub(crate) fn format_user_info_update(user_info_block: &str) -> String {
    format!(
        "{ENVIRONMENT_UPDATE_OPEN} source=\"{USER_INFO_SOURCE}\">\n\
         The session environment has changed since this conversation started. \
         The earlier user_info snapshot from session start is now stale. \
         Use this updated environment instead:\n\n\
         {user_info_block}\n\
         </environment-update>"
    )
}

pub(crate) fn format_project_rules_update(formatted_rules: &str) -> String {
    format!(
        "{ENVIRONMENT_UPDATE_OPEN} source=\"{PROJECT_RULES_SOURCE}\">\n\
         Project instructions (AGENTS.md / rules) have changed since this session started. \
         The earlier project-instructions block is a snapshot and is now stale. \
         Use these updated instructions instead:\n\n\
         {formatted_rules}\n\
         </environment-update>"
    )
}

fn item_text(item: &ConversationItem) -> Option<&str> {
    let ConversationItem::User(user) = item else {
        return None;
    };
    user.content.iter().find_map(|part| match part {
        ContentPart::Text { text } => Some(text.as_ref()),
        _ => None,
    })
}

/// Recover the last announced user-info identity hash from a persisted
/// conversation (resume). Prefers the newest environment-update, then the
/// original prefix `<user_info>` block.
pub(crate) fn recover_user_info_fingerprint(conversation: &[ConversationItem]) -> Option<u64> {
    for item in conversation.iter().rev() {
        let Some(text) = item_text(item) else {
            continue;
        };
        if let Some(block) = extract_environment_update_inner(text, USER_INFO_SOURCE)
            && let Some(user_info) = extract_user_info_block(block)
        {
            return Some(user_info_identity_fingerprint(user_info));
        }
    }
    conversation.iter().find_map(|item| {
        extract_user_info_block(item_text(item)?).map(user_info_identity_fingerprint)
    })
}

/// Recover the last announced project-rules hash. Prefers the newest
/// environment-update, then the spawn-time ProjectInstructions item.
pub(crate) fn recover_rules_fingerprint(conversation: &[ConversationItem]) -> Option<u64> {
    for item in conversation.iter().rev() {
        let Some(text) = item_text(item) else {
            continue;
        };
        if let Some(inner) = extract_environment_update_inner(text, PROJECT_RULES_SOURCE) {
            return Some(fingerprint_text(inner.trim()));
        }
        if let ConversationItem::User(user) = item
            && user.synthetic_reason == Some(SyntheticReason::ProjectInstructions)
        {
            return Some(fingerprint_text(text.trim()));
        }
    }
    None
}

fn extract_environment_update_inner<'a>(text: &'a str, source: &str) -> Option<&'a str> {
    let open = format!("{ENVIRONMENT_UPDATE_OPEN} source=\"{source}\">");
    let start = text.find(&open)? + open.len();
    let rest = &text[start..];
    let end = rest.find("</environment-update>")?;
    Some(&rest[..end])
}

/// True when the conversation already has model progress, so rewriting the
/// cached user-info prefix would bust the prompt cache.
pub(crate) fn conversation_has_model_progress(conversation: &[ConversationItem]) -> bool {
    conversation
        .iter()
        .any(|item| matches!(item, ConversationItem::Assistant(_)))
}

impl SessionActor {
    /// Record fingerprints for whatever we just put in the prefix so later
    /// turns only announce genuine drift.
    pub(super) fn record_announced_environment(&self, prefix: &str, rules: &[AgentConfigFile]) {
        if let Some(block) = extract_user_info_block(prefix) {
            self.last_announced_user_info_hash
                .set(Some(user_info_identity_fingerprint(block)));
        }
        if !rules.is_empty() {
            self.last_announced_rules_hash
                .set(Some(rules_fingerprint(rules)));
        }
    }

    pub(super) async fn maybe_inject_user_info_update_reminder(&self) {
        let display_path = self
            .display_cwd
            .get()
            .map(|s| s.as_str())
            .unwrap_or(&self.session_info.cwd);
        let cwd = std::path::Path::new(display_path);
        let current = crate::session::user_message::construct_user_message_minimal(cwd, None);
        let Some(block) = extract_user_info_block(&current) else {
            return;
        };
        let hash = user_info_identity_fingerprint(block);
        match self.last_announced_user_info_hash.get() {
            Some(previous) if previous == hash => return,
            None => {
                self.last_announced_user_info_hash.set(Some(hash));
                return;
            }
            Some(_) => {}
        }
        self.last_announced_user_info_hash.set(Some(hash));
        self.push_system_reminder(&format_user_info_update(block));
        tracing::info!(
            session_id = %self.session_info.id.0,
            "Injected user_info environment-update reminder"
        );
    }

    pub(super) async fn maybe_inject_project_rules_update_reminder(&self) {
        let cwd = self
            .display_cwd
            .get()
            .map(|s| s.as_str())
            .unwrap_or(&self.session_info.cwd);
        let files = xai_grok_agent::prompt::agents_md::read_agents_config_with_paths(
            cwd,
            self.rebuild_spec.compat,
        )
        .await;
        let hash = rules_fingerprint(&files);
        match self.last_announced_rules_hash.get() {
            Some(previous) if previous == hash => return,
            None if files.is_empty() => {
                self.last_announced_rules_hash.set(Some(hash));
                return;
            }
            None => {
                // Resume with no recoverable snapshot: treat current disk
                // contents as already announced so we don't replay the
                // spawn-time AGENTS.md block on the first turn.
                self.last_announced_rules_hash.set(Some(hash));
                return;
            }
            Some(_) => {}
        }
        let Some(formatted) = xai_grok_agent::prompt::agents_md::format_agents_md_section(&files)
        else {
            self.last_announced_rules_hash.set(Some(hash));
            self.push_system_reminder(&format_project_rules_update(
                "All previously injected project instruction files have been removed.",
            ));
            return;
        };
        self.last_announced_rules_hash.set(Some(hash));
        self.push_system_reminder(&format_project_rules_update(&formatted));
        tracing::info!(
            session_id = %self.session_info.id.0,
            file_count = files.len(),
            "Injected project-rules environment-update reminder"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_info_fingerprint_ignores_date_line() {
        let monday = "<user_info>\nOS Version: macos\nShell: /bin/zsh\nWorkspace Path: /repo\nToday's date: 2026-08-16\n</user_info>";
        let tuesday = "<user_info>\nOS Version: macos\nShell: /bin/zsh\nWorkspace Path: /repo\nToday's date: 2026-08-17\n</user_info>";
        assert_eq!(
            user_info_identity_fingerprint(monday),
            user_info_identity_fingerprint(tuesday)
        );
    }

    #[test]
    fn user_info_fingerprint_changes_with_cwd() {
        let a = "<user_info>\nOS Version: macos\nShell: /bin/zsh\nWorkspace Path: /repo-a\nToday's date: 2026-08-17\n</user_info>";
        let b = "<user_info>\nOS Version: macos\nShell: /bin/zsh\nWorkspace Path: /repo-b\nToday's date: 2026-08-17\n</user_info>";
        assert_ne!(
            user_info_identity_fingerprint(a),
            user_info_identity_fingerprint(b)
        );
    }

    #[test]
    fn rules_fingerprint_changes_with_content() {
        let before = [AgentConfigFile {
            file_name: "AGENTS.md".into(),
            file_path: "/repo/AGENTS.md".into(),
            content: "use rust".into(),
        }];
        let after = [AgentConfigFile {
            file_name: "AGENTS.md".into(),
            file_path: "/repo/AGENTS.md".into(),
            content: "use rust\nand tests".into(),
        }];
        assert_ne!(rules_fingerprint(&before), rules_fingerprint(&after));
    }

    #[test]
    fn recover_prefers_latest_environment_update() {
        let conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::user(
                "<user_info>\nOS Version: macos\nShell: /bin/zsh\nWorkspace Path: /old\nToday's date: 2026-08-16\n</user_info>",
            ),
            ConversationItem::system_reminder(format_user_info_update(
                "<user_info>\nOS Version: macos\nShell: /bin/zsh\nWorkspace Path: /new\nToday's date: 2026-08-17\n</user_info>",
            )),
        ];
        let recovered = recover_user_info_fingerprint(&conv).unwrap();
        let expected = user_info_identity_fingerprint(
            "<user_info>\nOS Version: macos\nShell: /bin/zsh\nWorkspace Path: /new\nToday's date: 2026-08-17\n</user_info>",
        );
        assert_eq!(recovered, expected);
    }

    #[test]
    fn recover_rules_from_project_instructions() {
        let reminder = "\n\n<system-reminder>\nAs you answer the user's questions, you can use the following context (ordered from repo root to current directory - deeper files take precedence on conflicts):\n\n## From: /repo/AGENTS.md\nuse rust\n</system-reminder>";
        let conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::project_instructions(reminder),
        ];
        assert_eq!(
            recover_rules_fingerprint(&conv),
            Some(fingerprint_text(reminder.trim()))
        );
    }

    #[test]
    fn conversation_with_assistant_has_progress() {
        let conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hi"),
            ConversationItem::assistant("hello"),
        ];
        assert!(conversation_has_model_progress(&conv));
        assert!(!conversation_has_model_progress(&[
            ConversationItem::system("sys"),
            ConversationItem::user("hi"),
        ]));
    }
}
