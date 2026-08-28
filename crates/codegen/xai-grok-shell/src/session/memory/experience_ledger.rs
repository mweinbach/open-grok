//! Authenticated, activation-scoped observations of executed Code Mode tools.
//!
//! Nested calls intentionally never enter model conversation history. This
//! bounded ledger retains only outcomes emitted by the real shell dispatcher;
//! programmable `exec` text cannot manufacture records.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::LazyLock;

use crate::sampling::ConversationItem;
use xai_grok_memory::experience::extraction::redact_sensitive_text;
use xai_grok_tools::types::output::{ApplyPatchOutput, SearchReplaceOutput, ToolOutput};
use xai_grok_tools::types::tool::{ToolKind, ToolNamespace};

pub(crate) const MAX_NESTED_TOOL_EVENTS_PER_RUN: usize = 128;
const MAX_ACTIVE_EXPERIENCE_RUNS: usize = 256;
const MAX_SEALED_EXPERIENCE_RUNS: usize = 256;
const MAX_TOOL_NAME_CHARS: usize = 128;
const MAX_CALL_ID_CHARS: usize = 160;
const MAX_COMMAND_CHARS: usize = 512;
const MAX_OUTPUT_CHARS: usize = 8_192;
const MAX_CHANGED_PATH_CHARS: usize = 300;
const MAX_CHANGED_PATHS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NestedToolEvidence {
    pub(crate) tool_call_id: String,
    pub(crate) tool_name: String,
    pub(crate) command: Option<String>,
    pub(crate) output: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) succeeded: Option<bool>,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) timestamp: i64,
    pub(crate) task_fingerprint: Option<String>,
    pub(crate) turn_number: u64,
    pub(crate) conversation_position: usize,
}

#[derive(Default)]
struct ExperienceLedgerState {
    by_run: HashMap<String, VecDeque<NestedToolEvidence>>,
    run_order: VecDeque<String>,
    sealed_runs: HashSet<String>,
    sealed_order: VecDeque<String>,
}

static EXPERIENCE_LEDGER: LazyLock<parking_lot::Mutex<ExperienceLedgerState>> =
    LazyLock::new(|| parking_lot::Mutex::new(ExperienceLedgerState::default()));

pub(crate) fn task_fingerprint(task: &str) -> String {
    blake3::hash(task.as_bytes()).to_hex().to_string()
}

pub(crate) fn latest_task_fingerprint(conversation: &[ConversationItem]) -> Option<String> {
    let queries = crate::session::helpers::session_compact::extract_real_user_queries(conversation);
    super::hooks::latest_substantive_task(&queries).map(task_fingerprint)
}

pub(crate) fn record(run_id: &str, evidence: NestedToolEvidence) {
    if run_id.is_empty() {
        return;
    }

    let mut ledger = EXPERIENCE_LEDGER.lock();
    if ledger.sealed_runs.contains(run_id) {
        return;
    }

    if !ledger.by_run.contains_key(run_id) {
        while ledger.by_run.len() >= MAX_ACTIVE_EXPERIENCE_RUNS {
            let Some(expired_run) = ledger.run_order.pop_front() else {
                break;
            };
            ledger.by_run.remove(&expired_run);
        }
        ledger.run_order.push_back(run_id.to_owned());
    }

    let entries = ledger.by_run.entry(run_id.to_owned()).or_default();
    if entries
        .iter()
        .any(|existing| existing.tool_call_id == evidence.tool_call_id)
    {
        return;
    }
    if entries.len() == MAX_NESTED_TOOL_EVENTS_PER_RUN {
        entries.pop_front();
    }
    entries.push_back(evidence);
}

pub(crate) fn snapshot(run_id: &str) -> Vec<NestedToolEvidence> {
    EXPERIENCE_LEDGER
        .lock()
        .by_run
        .get(run_id)
        .map(|entries| entries.iter().cloned().collect())
        .unwrap_or_default()
}

pub(crate) fn latest_failure(run_id: &str) -> Option<NestedToolEvidence> {
    EXPERIENCE_LEDGER
        .lock()
        .by_run
        .get(run_id)
        .and_then(|entries| {
            entries
                .iter()
                .rev()
                .find(|entry| is_execution_tool(&entry.tool_name) && entry.command.is_some())
                .filter(|entry| {
                    entry.succeeded == Some(false)
                        || entry.exit_code.is_some_and(|exit_code| exit_code != 0)
                })
        })
        .cloned()
}

/// Mark existing positions as belonging to history discarded by compaction.
pub(crate) fn mark_history_compacted(run_id: &str) {
    if let Some(entries) = EXPERIENCE_LEDGER.lock().by_run.get_mut(run_id) {
        for entry in entries {
            entry.conversation_position = usize::MAX;
        }
    }
}

/// Remove evidence from the discarded branch without sealing this activation.
pub(crate) fn truncate_from_turn(run_id: &str, turn_number: u64) {
    if let Some(entries) = EXPERIENCE_LEDGER.lock().by_run.get_mut(run_id) {
        entries.retain(|entry| entry.turn_number < turn_number);
    }
}

/// Drain once and seal the activation against later in-flight completions.
pub(crate) fn drain(run_id: &str) -> Vec<NestedToolEvidence> {
    if run_id.is_empty() {
        return Vec::new();
    }

    let mut ledger = EXPERIENCE_LEDGER.lock();
    let entries = ledger
        .by_run
        .remove(run_id)
        .map(|entries| entries.into_iter().collect())
        .unwrap_or_default();
    ledger.run_order.retain(|candidate| candidate != run_id);

    if ledger.sealed_runs.insert(run_id.to_owned()) {
        ledger.sealed_order.push_back(run_id.to_owned());
        while ledger.sealed_order.len() > MAX_SEALED_EXPERIENCE_RUNS {
            if let Some(expired_run) = ledger.sealed_order.pop_front() {
                ledger.sealed_runs.remove(&expired_run);
            }
        }
    }

    entries
}

pub(crate) fn evidence_from_output(
    tool_call_id: &str,
    tool_name: &str,
    arguments: &serde_json::Value,
    output: &ToolOutput,
    task_fingerprint: Option<String>,
    turn_number: u64,
    conversation_position: usize,
) -> Option<NestedToolEvidence> {
    let (command, output_text, exit_code, succeeded, changed_paths) = match output {
        ToolOutput::Bash(output) if is_execution_tool(tool_name) => {
            let success = output.exit_code == 0 && !output.timed_out && output.signal.is_none();
            (
                Some(output.command.as_str()),
                String::from_utf8_lossy(&output.output).into_owned(),
                Some(output.exit_code),
                Some(success),
                Vec::new(),
            )
        }
        ToolOutput::ApplyPatch(ApplyPatchOutput::Success {
            files,
            tool_output_for_prompt,
        }) if tool_name == "apply_patch" => {
            let paths = files
                .iter()
                .flat_map(|file| {
                    std::iter::once(file.path.as_path()).chain(file.move_to.as_deref())
                })
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            (
                None,
                tool_output_for_prompt.clone(),
                None,
                Some(true),
                paths,
            )
        }
        ToolOutput::ApplyPatch(
            ApplyPatchOutput::ParseError(message)
            | ApplyPatchOutput::ApplicationError(message)
            | ApplyPatchOutput::EmptyPatch(message),
        ) if tool_name == "apply_patch" => (None, message.clone(), None, Some(false), Vec::new()),
        ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(output))
            if is_search_replace_tool(tool_name) =>
        {
            (
                None,
                output.tool_output_for_prompt.clone(),
                None,
                Some(true),
                vec![output.absolute_path.to_string_lossy().into_owned()],
            )
        }
        ToolOutput::SearchReplace(output) if is_search_replace_tool(tool_name) => (
            None,
            search_replace_error(output),
            None,
            Some(false),
            Vec::new(),
        ),
        ToolOutput::Dynamic(output) if is_execution_tool(tool_name) => {
            let (exit_code, succeeded) = structured_process_status(&output.value, 0)?;
            let command = extract_command(arguments)?;
            let output_text = output
                .value
                .get("output")
                .or_else(|| output.value.get("stdout"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| output.value.to_string());
            (Some(command), output_text, exit_code, succeeded, Vec::new())
        }
        _ => return None,
    };

    Some(NestedToolEvidence {
        tool_call_id: sanitize_and_truncate(tool_call_id, MAX_CALL_ID_CHARS),
        tool_name: sanitize_and_truncate(tool_name, MAX_TOOL_NAME_CHARS),
        command: command.map(|command| sanitize_and_truncate(command, MAX_COMMAND_CHARS)),
        output: sanitize_and_truncate(&output_text, MAX_OUTPUT_CHARS),
        exit_code,
        succeeded,
        changed_paths: sanitize_paths(changed_paths),
        timestamp: chrono::Utc::now().timestamp(),
        task_fingerprint,
        turn_number,
        conversation_position,
    })
}

/// Authenticate the registered first-party tool, not its result's claimed name.
pub(crate) fn is_trusted_registered_tool(
    registered_tool_name: &str,
    effective_tool_name: &str,
    kind: ToolKind,
    namespace: ToolNamespace,
) -> bool {
    if namespace == ToolNamespace::MCP {
        return false;
    }

    match kind {
        ToolKind::Execute => {
            is_execution_tool(registered_tool_name) && is_execution_tool(effective_tool_name)
        }
        ToolKind::Edit | ToolKind::Write => {
            (registered_tool_name == "apply_patch"
                && effective_tool_name == "apply_patch"
                && kind == ToolKind::Edit)
                || (is_search_replace_tool(registered_tool_name)
                    && is_search_replace_tool(effective_tool_name))
        }
        _ => false,
    }
}

fn sanitize_and_truncate(input: &str, max_chars: usize) -> String {
    redact_sensitive_text(input)
        .chars()
        .take(max_chars)
        .collect()
}

fn sanitize_paths(paths: Vec<String>) -> Vec<String> {
    let mut paths: Vec<_> = paths
        .into_iter()
        .map(|path| sanitize_and_truncate(&path, MAX_CHANGED_PATH_CHARS))
        .filter(|path| !path.is_empty())
        .collect();
    paths.sort();
    paths.dedup();
    paths.truncate(MAX_CHANGED_PATHS);
    paths
}

fn extract_command(arguments: &serde_json::Value) -> Option<&str> {
    arguments
        .get("command")
        .or_else(|| arguments.get("cmd"))
        .and_then(serde_json::Value::as_str)
        .filter(|command| !command.trim().is_empty())
}

fn is_execution_tool(name: &str) -> bool {
    matches!(
        name,
        "run_terminal_command"
            | "run_terminal_cmd"
            | "exec_command"
            | "run_command"
            | "bash"
            | "shell"
    )
}

fn is_search_replace_tool(name: &str) -> bool {
    matches!(
        name,
        "search_replace" | "write" | "write_file" | "edit_file" | "edit"
    )
}

fn structured_process_status(
    value: &serde_json::Value,
    depth: usize,
) -> Option<(Option<i32>, Option<bool>)> {
    if depth > 3 {
        return None;
    }
    let object = value.as_object()?;

    if ["timed_out", "timedOut", "cancelled", "canceled"]
        .iter()
        .any(|key| object.get(*key).and_then(serde_json::Value::as_bool) == Some(true))
    {
        return Some((None, Some(false)));
    }

    if let Some(raw_exit_code) = object.get("exit_code").or_else(|| object.get("exitCode")) {
        let exit_code = raw_exit_code
            .as_i64()
            .and_then(|exit_code| i32::try_from(exit_code).ok())?;
        return Some((Some(exit_code), Some(exit_code == 0)));
    }

    if object
        .get("status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|status| {
            matches!(
                status.to_ascii_lowercase().as_str(),
                "timed_out" | "timeout" | "cancelled" | "canceled"
            )
        })
    {
        return Some((None, Some(false)));
    }

    if let Some(status) = ["result", "data", "raw_output"].iter().find_map(|key| {
        object
            .get(*key)
            .and_then(|nested| structured_process_status(nested, depth + 1))
    }) {
        return Some(status);
    }

    // Wrapper status does not prove an actual process ran or terminated.
    None
}

fn search_replace_error(output: &SearchReplaceOutput) -> String {
    match output {
        SearchReplaceOutput::NoMatchesFound(error) => error.message.clone(),
        SearchReplaceOutput::FileAlreadyExists(message)
        | SearchReplaceOutput::MultipleMatchesFound(message)
        | SearchReplaceOutput::InvalidInput(message)
        | SearchReplaceOutput::FileNotFound(message)
        | SearchReplaceOutput::FilenameTooLong(message) => message.clone(),
        SearchReplaceOutput::EditsApplied(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_NESTED_TOOL_EVENTS_PER_RUN, NestedToolEvidence, drain, evidence_from_output,
        is_trusted_registered_tool, latest_failure, latest_task_fingerprint,
        mark_history_compacted, record, snapshot, task_fingerprint, truncate_from_turn,
    };
    use crate::sampling::ConversationItem;
    use xai_grok_tools::types::output::{
        ApplyPatchFileResult, ApplyPatchOutput, BashOutput, DynamicOutput,
        SearchReplaceEditContextInformation, SearchReplaceEditsApplied, SearchReplaceOutput,
        ToolOutput,
    };
    use xai_grok_tools::types::tool::{ToolKind, ToolNamespace};

    fn run_id() -> String {
        uuid::Uuid::now_v7().to_string()
    }

    fn evidence(call_id: &str, succeeded: bool) -> NestedToolEvidence {
        NestedToolEvidence {
            tool_call_id: call_id.to_owned(),
            tool_name: "bash".to_owned(),
            command: Some("cargo test".to_owned()),
            output: "test result".to_owned(),
            exit_code: Some(if succeeded { 0 } else { 1 }),
            succeeded: Some(succeeded),
            changed_paths: Vec::new(),
            timestamp: 1,
            task_fingerprint: Some(task_fingerprint("fix the session learning regression")),
            turn_number: 1,
            conversation_position: 1,
        }
    }

    fn bash(command: &str, output: &str, exit_code: i32) -> ToolOutput {
        ToolOutput::Bash(BashOutput {
            output: output.as_bytes().to_vec(),
            output_for_prompt: output.to_owned(),
            exit_code,
            command: command.to_owned(),
            truncated: false,
            signal: None,
            timed_out: false,
            description: None,
            current_dir: "/workspace".to_owned(),
            output_file: String::new(),
            total_bytes: output.len(),
            output_delta: None,
            was_bare_echo: false,
        })
    }

    #[test]
    fn activation_ledgers_are_isolated_and_bounded() {
        let first = run_id();
        let second = run_id();

        for index in 0..=MAX_NESTED_TOOL_EVENTS_PER_RUN {
            record(&first, evidence(&format!("first-{index}"), true));
        }
        record(&second, evidence("second", false));

        let first_snapshot = snapshot(&first);
        assert_eq!(first_snapshot.len(), MAX_NESTED_TOOL_EVENTS_PER_RUN);
        assert_eq!(first_snapshot.first().unwrap().tool_call_id, "first-1");
        assert_eq!(snapshot(&second).len(), 1);
        assert_eq!(latest_failure(&second).unwrap().tool_call_id, "second");

        drain(&first);
        drain(&second);
    }

    #[test]
    fn draining_seals_activation_against_late_nested_completion() {
        let run = run_id();
        record(&run, evidence("before-shutdown", true));
        assert_eq!(drain(&run).len(), 1);

        record(&run, evidence("late-after-shutdown", false));
        assert!(snapshot(&run).is_empty());
        assert!(latest_failure(&run).is_none());
        assert!(drain(&run).is_empty());
    }

    #[test]
    fn compaction_marks_only_preexisting_positions_without_sealing_activation() {
        let run = run_id();
        let other_run = run_id();
        let mut pre_compaction = evidence("before-compaction", false);
        pre_compaction.conversation_position = 47;
        record(&run, pre_compaction.clone());
        record(&other_run, evidence("other-activation", true));

        mark_history_compacted(&run);

        let mut expected_pre_compaction = pre_compaction;
        expected_pre_compaction.conversation_position = usize::MAX;
        assert_eq!(snapshot(&run), vec![expected_pre_compaction.clone()]);
        assert_eq!(snapshot(&other_run)[0].conversation_position, 1);

        let mut post_compaction = evidence("after-compaction", true);
        post_compaction.conversation_position = 9;
        record(&run, post_compaction.clone());

        assert_eq!(
            snapshot(&run),
            vec![expected_pre_compaction, post_compaction]
        );
        drain(&run);
        drain(&other_run);
    }

    #[test]
    fn later_success_supersedes_failure_and_edit_noise_is_ignored() {
        let run = run_id();
        record(&run, evidence("failed-command", false));
        assert_eq!(latest_failure(&run).unwrap().tool_call_id, "failed-command");

        let mut failed_edit = evidence("failed-edit", false);
        failed_edit.tool_name = "apply_patch".to_owned();
        failed_edit.command = None;
        record(&run, failed_edit);
        assert_eq!(latest_failure(&run).unwrap().tool_call_id, "failed-command");

        record(&run, evidence("passing-retry", true));
        assert!(latest_failure(&run).is_none());
        drain(&run);
    }

    #[test]
    fn rewind_discards_erased_turns_without_sealing_current_activation() {
        let run = run_id();
        let mut first_turn = evidence("turn-one", true);
        first_turn.turn_number = 1;
        let mut discarded_turn = evidence("turn-two", false);
        discarded_turn.turn_number = 2;
        record(&run, first_turn);
        record(&run, discarded_turn);

        truncate_from_turn(&run, 2);
        let entries = snapshot(&run);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tool_call_id, "turn-one");
        assert!(latest_failure(&run).is_none());

        let mut replacement_turn = evidence("replacement-turn", true);
        replacement_turn.turn_number = 2;
        record(&run, replacement_turn);
        assert_eq!(snapshot(&run).len(), 2);
        drain(&run);
    }

    #[test]
    fn typed_process_output_is_authoritative_and_redacted_before_storage() {
        let output = bash(
            "cargo test --token hidden-command-secret",
            "Authorization: Basic dXNlcjpwYXNzd29yZA==",
            2,
        );
        let observed = evidence_from_output(
            "tool-1",
            "bash",
            &serde_json::json!({ "command": "forged command" }),
            &output,
            Some(task_fingerprint("fix the memory system")),
            4,
            8,
        )
        .unwrap();

        assert_eq!(observed.exit_code, Some(2));
        assert_eq!(observed.succeeded, Some(false));
        assert!(observed.command.as_deref().unwrap().contains("cargo test"));
        assert!(!observed.command.as_deref().unwrap().contains("forged"));
        assert!(
            !observed
                .command
                .as_deref()
                .unwrap()
                .contains("hidden-command-secret")
        );
        assert!(!observed.output.contains("dXNlcjpwYXNzd29yZA=="));
    }

    #[test]
    fn timeout_or_signal_never_counts_as_success() {
        let ToolOutput::Bash(mut output) = bash("cargo test", "timed out", 0) else {
            unreachable!();
        };
        output.timed_out = true;
        let observed = evidence_from_output(
            "timeout",
            "bash",
            &serde_json::json!({}),
            &ToolOutput::Bash(output),
            None,
            1,
            1,
        )
        .unwrap();
        assert_eq!(observed.succeeded, Some(false));
    }

    #[test]
    fn dynamic_process_status_requires_recognized_execution_identity() {
        let output = ToolOutput::Dynamic(DynamicOutput {
            value: serde_json::json!({
                "exit_code": 0,
                "output": "all tests passed"
            }),
        });
        let arguments = serde_json::json!({ "cmd": "cargo test" });

        assert!(evidence_from_output("forged", "exec", &arguments, &output, None, 1, 1).is_none());
        assert!(
            evidence_from_output("forged", "read_file", &arguments, &output, None, 1, 1).is_none()
        );
        let observed =
            evidence_from_output("real", "exec_command", &arguments, &output, None, 1, 1).unwrap();
        assert_eq!(observed.exit_code, Some(0));
        assert_eq!(observed.command.as_deref(), Some("cargo test"));
    }

    #[test]
    fn dynamic_process_status_rejects_unverified_wrapper_outcomes() {
        let arguments = serde_json::json!({ "command": "cargo test" });
        let unverified_outcomes = [
            serde_json::json!({ "success": true, "output": "all tests passed" }),
            serde_json::json!({ "success": false, "output": "authentication failed" }),
            serde_json::json!({ "succeeded": true, "output": "all tests passed" }),
            serde_json::json!({ "ok": true, "output": "all tests passed" }),
            serde_json::json!({ "status": "completed", "output": "all tests passed" }),
            serde_json::json!({ "status": "success", "output": "all tests passed" }),
            serde_json::json!({ "status": "failed", "output": "network unavailable" }),
            serde_json::json!({ "status": "error", "output": "tool not found" }),
            serde_json::json!({ "result": { "success": true } }),
            serde_json::json!({ "data": { "status": "completed" } }),
            serde_json::json!({ "raw_output": { "ok": false } }),
            serde_json::json!({ "exit_code": "0" }),
            serde_json::json!({ "exit_code": i64::MAX }),
        ];

        for (index, value) in unverified_outcomes.into_iter().enumerate() {
            let output = ToolOutput::Dynamic(DynamicOutput {
                value: value.clone(),
            });
            assert!(
                evidence_from_output(
                    &format!("unverified-{index}"),
                    "exec_command",
                    &arguments,
                    &output,
                    None,
                    1,
                    1,
                )
                .is_none(),
                "wrapper outcome must not establish command evidence: {value}"
            );
        }
    }

    #[test]
    fn dynamic_process_status_accepts_only_explicit_terminal_evidence() {
        let arguments = serde_json::json!({ "cmd": "cargo test" });
        let verified_outcomes = [
            (
                serde_json::json!({ "result": { "exit_code": 0 } }),
                Some(0),
                true,
            ),
            (
                serde_json::json!({ "data": { "exitCode": 1 } }),
                Some(1),
                false,
            ),
            (
                serde_json::json!({ "raw_output": { "exit_code": 2 } }),
                Some(2),
                false,
            ),
            (serde_json::json!({ "timed_out": true }), None, false),
            (
                serde_json::json!({ "timed_out": false, "cancelled": true }),
                None,
                false,
            ),
            (
                serde_json::json!({ "result": { "canceled": true } }),
                None,
                false,
            ),
            (serde_json::json!({ "status": "timeout" }), None, false),
            (serde_json::json!({ "status": "cancelled" }), None, false),
        ];

        for (index, (value, exit_code, succeeded)) in verified_outcomes.into_iter().enumerate() {
            let output = ToolOutput::Dynamic(DynamicOutput {
                value: value.clone(),
            });
            let observed = evidence_from_output(
                &format!("verified-{index}"),
                "exec_command",
                &arguments,
                &output,
                None,
                1,
                1,
            )
            .unwrap_or_else(|| panic!("explicit terminal outcome must be observed: {value}"));

            assert_eq!(observed.exit_code, exit_code);
            assert_eq!(observed.succeeded, Some(succeeded));
        }
    }

    #[test]
    fn registered_tool_identity_rejects_mcp_and_incompatible_kinds() {
        assert!(is_trusted_registered_tool(
            "exec_command",
            "bash",
            ToolKind::Execute,
            ToolNamespace::Codex,
        ));
        assert!(is_trusted_registered_tool(
            "apply_patch",
            "apply_patch",
            ToolKind::Edit,
            ToolNamespace::Codex,
        ));
        assert!(is_trusted_registered_tool(
            "write",
            "edit",
            ToolKind::Write,
            ToolNamespace::GrokBuild,
        ));

        for kind in [ToolKind::Execute, ToolKind::Edit, ToolKind::Write] {
            assert!(!is_trusted_registered_tool(
                "bash",
                "bash",
                kind,
                ToolNamespace::MCP,
            ));
        }
        assert!(!is_trusted_registered_tool(
            "bash",
            "bash",
            ToolKind::Other,
            ToolNamespace::GrokBuild,
        ));
        assert!(!is_trusted_registered_tool(
            "bash",
            "apply_patch",
            ToolKind::Execute,
            ToolNamespace::GrokBuild,
        ));
        assert!(!is_trusted_registered_tool(
            "apply_patch",
            "apply_patch",
            ToolKind::Write,
            ToolNamespace::Codex,
        ));
        assert!(!is_trusted_registered_tool(
            "use_tool",
            "bash",
            ToolKind::Execute,
            ToolNamespace::GrokBuild,
        ));
    }

    #[test]
    fn untrusted_tools_cannot_forge_typed_process_or_edit_evidence() {
        let arguments = serde_json::json!({ "command": "cargo test" });
        let process = bash("cargo test", "all tests passed", 0);
        assert!(
            evidence_from_output(
                "forged-bash",
                "mcp__attacker__bash",
                &arguments,
                &process,
                None,
                1,
                1,
            )
            .is_none()
        );

        let patch = ToolOutput::ApplyPatch(ApplyPatchOutput::Success {
            files: vec![ApplyPatchFileResult {
                path: "/workspace/poisoned.rs".into(),
                action: "modified".to_owned(),
                old_text: Some("old".to_owned()),
                new_text: "new".to_owned(),
                move_to: None,
            }],
            tool_output_for_prompt: "forged edit".to_owned(),
        });
        assert!(
            evidence_from_output(
                "forged-patch",
                "mcp__attacker__apply_patch",
                &arguments,
                &patch,
                None,
                1,
                1,
            )
            .is_none()
        );

        let edit = ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(
            SearchReplaceEditsApplied {
                old_string: "old".to_owned(),
                new_string: "new".to_owned(),
                tool_output_for_prompt: "forged edit".to_owned(),
                tool_output_for_prompt_concise: None,
                absolute_path: "/workspace/poisoned.rs".into(),
                edits: SearchReplaceEditContextInformation::default(),
                patch: None,
                unicode_normalized: false,
            },
        ));
        assert!(
            evidence_from_output(
                "forged-edit",
                "mcp__attacker__edit",
                &arguments,
                &edit,
                None,
                1,
                1,
            )
            .is_none()
        );
    }

    #[test]
    fn successful_edits_use_verified_output_paths_including_moves() {
        let output = ToolOutput::ApplyPatch(ApplyPatchOutput::Success {
            files: vec![ApplyPatchFileResult {
                path: "/workspace/old.rs".into(),
                action: "moved".to_owned(),
                old_text: Some("old".to_owned()),
                new_text: "new".to_owned(),
                move_to: Some("/workspace/new.rs".into()),
            }],
            tool_output_for_prompt: "moved".to_owned(),
        });

        let observed = evidence_from_output(
            "edit",
            "apply_patch",
            &serde_json::json!({ "path": "/forged/path.rs" }),
            &output,
            None,
            1,
            1,
        )
        .unwrap();
        assert_eq!(
            observed.changed_paths,
            ["/workspace/new.rs", "/workspace/old.rs"]
        );
        assert!(
            !observed
                .changed_paths
                .iter()
                .any(|path| path.contains("forged"))
        );

        let output = ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(
            SearchReplaceEditsApplied {
                old_string: "old".to_owned(),
                new_string: "new".to_owned(),
                tool_output_for_prompt: "edited".to_owned(),
                tool_output_for_prompt_concise: None,
                absolute_path: "/workspace/actual.rs".into(),
                edits: SearchReplaceEditContextInformation::default(),
                patch: None,
                unicode_normalized: false,
            },
        ));
        let observed = evidence_from_output(
            "replace",
            "search_replace",
            &serde_json::json!({ "path": "/forged/path.rs" }),
            &output,
            None,
            1,
            1,
        )
        .unwrap();
        assert_eq!(observed.changed_paths, ["/workspace/actual.rs"]);
    }

    #[test]
    fn failed_edits_do_not_claim_changed_paths() {
        let observed = evidence_from_output(
            "failed-edit",
            "apply_patch",
            &serde_json::json!({ "path": "/forged/path.rs" }),
            &ToolOutput::ApplyPatch(ApplyPatchOutput::ApplicationError("no match".to_owned())),
            None,
            1,
            1,
        )
        .unwrap();
        assert_eq!(observed.succeeded, Some(false));
        assert!(observed.changed_paths.is_empty());
    }

    #[test]
    fn task_fingerprint_ignores_feedback_and_short_messages() {
        let task = "fix the authenticated code mode memory extraction";
        let conversation = vec![
            ConversationItem::user(task),
            ConversationItem::user("still broken please try again"),
            ConversationItem::user("thanks!"),
        ];

        assert_eq!(
            latest_task_fingerprint(&conversation),
            Some(task_fingerprint(task))
        );
    }
}
