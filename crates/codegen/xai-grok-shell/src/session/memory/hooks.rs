//! Session lifecycle hooks for the memory system.
//!
//! Provides `on_session_end()` which auto-saves a session summary to memory
//! when a session ends. This runs best-effort — failures are logged but
//! don't prevent shutdown.
//!
//! ## What is saved
//!
//! The current implementation writes a **structured metadata summary** with
//! zero latency and no LLM call:
//! - message counts (user / assistant / tool results)
//! - the first few real user topics from the session (never synthetic prefixes)
//! - session date
//!
//! For richer content capture (decisions, patterns, reasoning)
//! use `/flush`, which is user-initiated and produces an LLM-generated summary.
//!
//! ## Reliability
//!
//! - **Minimum conversation gate:** Skip sessions with < 3 *real* user prompts
//!   or < 50 total query bytes (synthetic metadata-only prefixes and
//!   auto-continue markers are excluded).
//! - **`save_on_end` config gate:** Skipped when `[memory.session].save_on_end = false`.
//! - **SIGTERM:** Triggered via `SessionCommand::Shutdown` handler

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use crate::sampling::ConversationItem;
use crate::session::memory::storage::{MemoryStorage, slugify};
use xai_grok_memory::experience::extraction::{ObservedEvent, RunObservation, extract_experiences};
use xai_grok_memory::experience::store::ExperienceStore;

/// Minimum number of *real* user prompts required to save a session summary.
///
/// "Real" excludes synthetic metadata prefixes and auto-continue sentinels —
/// see [`extract_real_user_queries`].
const MIN_USER_MESSAGES: usize = 3;

/// Minimum total byte length of all real user queries required to save.
///
/// Prevents trivial sessions (e.g. "hey" / "ok" / "thanks") from being indexed
/// even when they technically exceed [`MIN_USER_MESSAGES`].
///
/// Uses `str::len()` (byte length) rather than `chars().count()` — for the
/// mostly-ASCII inputs this gate targets, the distinction is immaterial.
const MIN_TOTAL_QUERY_BYTES: usize = 50;

/// Result of the session end hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEndResult {
    /// Session was too short (< [`MIN_USER_MESSAGES`] real user prompts or
    /// < [`MIN_TOTAL_QUERY_BYTES`] total bytes), or `save_on_end` was false.
    Skipped,
    /// Summary was written to the daily log.
    Written(String),
    /// Hook failed (logged, not fatal).
    Failed(String),
}

/// Real user queries iff the conversation meets the session-end size gate
/// (enough real prompts, enough total bytes). `None` for empty/brief sessions.
/// Independent of `save_on_end` so exit dream can still consolidate prior
/// logs when auto-save is off for a substantial session.
pub(crate) fn queries_meeting_session_end_threshold(
    conversation: &[ConversationItem],
) -> Option<Vec<String>> {
    // Real queries exclude synthetic metadata prefixes and `__auto_continue__`
    // sentinels (raw user-item counts inflate the gate).
    let real_queries =
        crate::session::helpers::session_compact::extract_real_user_queries(conversation);
    if real_queries.len() < MIN_USER_MESSAGES {
        tracing::debug!(
            real_count = real_queries.len(),
            min = MIN_USER_MESSAGES,
            "session too short for memory save/dream"
        );
        return None;
    }
    let total_bytes: usize = real_queries.iter().map(|q| q.len()).sum();
    if total_bytes < MIN_TOTAL_QUERY_BYTES {
        tracing::debug!(
            total_bytes,
            min = MIN_TOTAL_QUERY_BYTES,
            "session content too brief for memory save/dream"
        );
        return None;
    }
    Some(real_queries)
}

/// Run the session end hook — save a structured metadata summary to memory.
///
/// This is called from the `SessionCommand::Shutdown` handler and the
/// channel-closed path. It is best-effort: errors are logged but do not
/// prevent shutdown.
///
/// Generates a metadata summary with zero latency — **no LLM call is made**.
/// The summary includes message counts, real user topics, and session date.
///
/// For rich content capture (decisions, patterns, reasoning), use `/flush`.
///
/// Returns the path written (if any) for logging purposes.
pub fn on_session_end(
    storage: &MemoryStorage,
    conversation: &[ConversationItem],
    session_id: &str,
    save_on_end: bool,
) -> SessionEndResult {
    // Respect the user's config choice.  Callers that have `save_on_end = false`
    // should still call this function (to keep the call-site simple), trusting
    // that the gate is enforced here.
    if !save_on_end {
        tracing::debug!("session end: save_on_end=false, skipping memory summary");
        return SessionEndResult::Skipped;
    }

    let Some(real_queries) = queries_meeting_session_end_threshold(conversation) else {
        return SessionEndResult::Skipped;
    };

    // Slug from first *real* query (not the synthetic prefix User item).
    let first_real_query = real_queries.first().map(String::as_str).unwrap_or("");
    let slug = slugify(first_real_query, 30);
    let slug = if slug.is_empty() { "session" } else { &slug };

    // Generate a lightweight summary from conversation metadata (no LLM).
    let summary = generate_metadata_summary(conversation, &real_queries);

    // Write to daily session log.
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    match storage.write_daily_log(&date, slug, session_id, &summary, false) {
        Ok(path) => {
            tracing::info!(
                path = %path.display(),
                real_user_messages = real_queries.len(),
                "session end: wrote memory summary"
            );
            SessionEndResult::Written(path.display().to_string())
        }
        Err(e) => {
            tracing::warn!(error = %e, "session end: failed to write memory summary");
            SessionEndResult::Failed(e.to_string())
        }
    }
}

#[derive(Clone)]
struct ObservedToolCall {
    name: String,
    command: Option<String>,
    changed_paths: Vec<String>,
}

/// Persist concise lessons supported by objective, correlated tool results.
pub fn persist_session_experiences(
    storage: &MemoryStorage,
    conversation: &[ConversationItem],
    run_id: &str,
) -> anyhow::Result<usize> {
    persist_session_experiences_with_trusted_events(
        storage,
        conversation,
        run_id,
        &[],
        &HashSet::new(),
    )
}

/// Persist lessons from both model-visible direct calls and independently
/// authenticated nested Code Mode dispatches. Programmable `exec` output is
/// never used as verification evidence.
pub(crate) fn persist_session_experiences_with_trusted_events(
    storage: &MemoryStorage,
    conversation: &[ConversationItem],
    run_id: &str,
    trusted_events: &[super::experience_ledger::NestedToolEvidence],
    prior_tool_result_ids: &HashSet<String>,
) -> anyhow::Result<usize> {
    if storage.is_ephemeral() {
        return Ok(0);
    }

    let real_queries =
        crate::session::helpers::session_compact::extract_real_user_queries(conversation);
    let Some(task_summary) = latest_substantive_task(&real_queries) else {
        return Ok(0);
    };

    let task_conversation = latest_task_conversation(conversation, task_summary);
    let task_start = conversation.len().saturating_sub(task_conversation.len());
    let task_prompt_index = task_conversation.first().and_then(|item| match item {
        ConversationItem::User(user) => user.prompt_index.map(|index| index as u64),
        _ => None,
    });
    let (mut positioned_events, mut changed_paths) =
        observed_tool_events_with_positions(task_conversation, prior_tool_result_ids, task_start);
    append_trusted_nested_tool_events(
        trusted_events,
        task_summary,
        task_start,
        task_prompt_index,
        conversation.len(),
        &mut positioned_events,
        &mut changed_paths,
    );
    positioned_events.sort_by_key(|(position, _)| *position);
    let events: Vec<ObservedEvent> = positioned_events
        .into_iter()
        .map(|(_, event)| event)
        .collect();
    if events.is_empty() {
        return Ok(0);
    }

    let user_feedback = real_queries
        .last()
        .filter(|query| is_user_feedback(query))
        .map(|query| redact_and_truncate(query, 240));
    let objective_outcome = objective_run_outcome(&events);
    let completed = if objective_outcome == Some(true)
        && user_feedback
            .as_deref()
            .is_some_and(is_negative_user_feedback)
    {
        Some(false)
    } else {
        objective_outcome
    };
    let strategy = events
        .iter()
        .filter_map(|event| event.command.as_deref())
        .take(3)
        .map(redact_experience_text)
        .collect::<Vec<_>>()
        .join("; ");

    let observation = RunObservation {
        run_id: run_id.to_owned(),
        task_type: classify_task_type(task_summary),
        task_summary: redact_and_truncate(task_summary, 400),
        repository_id: storage.workspace_dir().to_string_lossy().into_owned(),
        repository_revision: xai_grok_memory::experience::current_repository_revision(
            storage.workspace_path(),
        ),
        environment: xai_grok_memory::experience::execution_environment(),
        strategy,
        strategy_rationale: "Observed executable actions and their objective outcomes".to_owned(),
        key_decisions: Vec::new(),
        changed_paths,
        events,
        judge_feedback: None,
        user_feedback,
        completed,
    };

    let mut lessons = extract_experiences(&observation);
    if let Some(feedback) = observation.user_feedback.as_deref() {
        let preference = if is_negative_user_feedback(feedback) {
            0.0
        } else {
            1.0
        };
        for lesson in &mut lessons {
            lesson.outcome.user_preference = Some(preference);
            lesson.refresh_confidence();
        }
    }
    if lessons.is_empty() && completed.is_none() {
        return Ok(0);
    }

    let store = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))?;
    let previously_retrieved = store.retrieved_for_run(run_id)?;

    for lesson in &lessons {
        store.upsert(lesson)?;
    }

    for memory in previously_retrieved {
        if recommendation_was_followed(&memory, &observation) {
            store.record_followed(run_id, &memory.id)?;
        }
    }

    if let Some(success) = completed {
        store.finalize_run(run_id, success)?;
    }

    Ok(lessons.len())
}

fn append_trusted_nested_tool_events(
    trusted_events: &[super::experience_ledger::NestedToolEvidence],
    task_summary: &str,
    task_start: usize,
    task_prompt_index: Option<u64>,
    conversation_len: usize,
    events: &mut Vec<(usize, ObservedEvent)>,
    changed_paths: &mut Vec<String>,
) {
    let task_fingerprint = super::experience_ledger::task_fingerprint(task_summary);

    for event in trusted_events.iter().filter(|event| {
        event.task_fingerprint.as_deref() == Some(task_fingerprint.as_str())
            && task_prompt_index
                .is_none_or(|task_prompt_index| event.turn_number >= task_prompt_index)
            && (event.conversation_position > conversation_len
                || event.conversation_position >= task_start)
    }) {
        if event.succeeded == Some(true) {
            changed_paths.extend(
                event
                    .changed_paths
                    .iter()
                    .map(|path| redact_and_truncate(path, 300)),
            );
        }

        if !is_execution_tool(&event.tool_name)
            || event.command.is_none()
            || (event.exit_code.is_none() && event.succeeded.is_none())
        {
            continue;
        }

        events.push((
            if event.conversation_position > conversation_len {
                0
            } else {
                event.conversation_position
            },
            ObservedEvent {
                tool_name: event.tool_name.clone(),
                command: event
                    .command
                    .as_deref()
                    .map(|command| redact_and_truncate(command, 512)),
                output: redact_and_truncate(&event.output, 1_200),
                exit_code: event.exit_code,
                succeeded: event.succeeded,
                timestamp: event.timestamp,
            },
        ));
    }

    changed_paths.sort();
    changed_paths.dedup();
}

pub(crate) fn latest_substantive_task(queries: &[String]) -> Option<&str> {
    queries
        .iter()
        .rev()
        .find(|query| query.trim().chars().count() >= 12 && !is_user_feedback(query))
        .map(String::as_str)
}

fn latest_task_conversation<'conversation>(
    conversation: &'conversation [ConversationItem],
    task_summary: &str,
) -> &'conversation [ConversationItem] {
    let task_start = conversation
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, item)| {
            matches!(item, ConversationItem::User(_))
                .then(|| {
                    crate::session::helpers::session_compact::extract_real_user_queries(
                        std::slice::from_ref(item),
                    )
                })
                .filter(|queries| queries.iter().any(|query| query == task_summary))
                .map(|_| index)
        })
        .unwrap_or(0);
    &conversation[task_start..]
}

fn is_user_feedback(query: &str) -> bool {
    user_feedback_polarity(query).is_some()
}

fn is_negative_user_feedback(feedback: &str) -> bool {
    user_feedback_polarity(feedback) == Some(false)
}

fn user_feedback_polarity(feedback: &str) -> Option<bool> {
    let normalized = feedback.trim().to_ascii_lowercase();
    if normalized.len() > 160 {
        return None;
    }

    for phrase in [
        "thanks",
        "thank you",
        "looks good",
        "great work",
        "that works",
    ] {
        if has_feedback_prefix(&normalized, phrase, true) {
            return Some(true);
        }
    }

    for phrase in [
        "still broken",
        "still failing",
        "doesn't work",
        "does not work",
        "not working",
        "not fixed",
        "this is still broken",
        "it is still broken",
        "it's still broken",
    ] {
        if has_feedback_prefix(&normalized, phrase, true) {
            return Some(false);
        }
    }

    for phrase in ["incorrect", "rejected"] {
        if has_feedback_prefix(&normalized, phrase, false) {
            return Some(false);
        }
    }

    None
}

fn has_feedback_prefix(feedback: &str, phrase: &str, allow_word_suffix: bool) -> bool {
    feedback.strip_prefix(phrase).is_some_and(|suffix| {
        suffix.is_empty()
            || suffix.chars().next().is_some_and(|character| {
                character.is_ascii_punctuation()
                    || character.is_whitespace()
                        && (allow_word_suffix
                            || suffix.trim_start().starts_with("please")
                            || suffix.trim_start().starts_with("try again"))
            })
    })
}

fn classify_task_type(task: &str) -> String {
    let task = task.to_ascii_lowercase();
    if task.contains("refactor") {
        "refactor"
    } else if task.contains("migrat") || task.contains("upgrad") {
        "migration"
    } else if task.contains("performan") || task.contains("optim") {
        "performance"
    } else if task.contains("bug") || task.contains("fix") || task.contains("regression") {
        "bug_fix"
    } else if task.contains("test") {
        "testing"
    } else if task.contains("document") {
        "documentation"
    } else {
        "implementation"
    }
    .to_owned()
}

#[cfg(test)]
fn observed_tool_events(conversation: &[ConversationItem]) -> (Vec<ObservedEvent>, Vec<String>) {
    let (events, changed_paths) =
        observed_tool_events_with_positions(conversation, &HashSet::new(), 0);
    (
        events.into_iter().map(|(_, event)| event).collect(),
        changed_paths,
    )
}

fn observed_tool_events_with_positions(
    conversation: &[ConversationItem],
    prior_tool_result_ids: &HashSet<String>,
    conversation_offset: usize,
) -> (Vec<(usize, ObservedEvent)>, Vec<String>) {
    let mut calls = HashMap::<String, ObservedToolCall>::new();
    let mut events = Vec::new();
    let mut changed_paths = Vec::new();
    let timestamp = chrono::Utc::now().timestamp();

    for (position, item) in conversation.iter().enumerate() {
        match item {
            ConversationItem::Assistant(assistant) => {
                for call in &assistant.tool_calls {
                    let observed = ObservedToolCall {
                        name: call.name.clone(),
                        command: extract_command(&call.name, call.arguments.as_ref()),
                        changed_paths: extract_changed_paths(&call.name, call.arguments.as_ref()),
                    };
                    calls.insert(call.id.as_ref().to_owned(), observed.clone());
                    calls.insert(call.call_id().to_owned(), observed);
                }
            }
            ConversationItem::ToolResult(result) => {
                if prior_tool_result_ids.contains(&result.tool_call_id) {
                    continue;
                }
                if let Some(call) = calls.get(&result.tool_call_id) {
                    observe_tool_result(
                        call,
                        result.content.as_ref(),
                        timestamp,
                        conversation_offset + position,
                        &mut events,
                        &mut changed_paths,
                    );
                }
            }
            ConversationItem::CustomToolOutput(output) => {
                if prior_tool_result_ids.contains(&output.call_id) {
                    continue;
                }
                if let Some(call) = calls.get(&output.call_id) {
                    let content = output.text_content();
                    if !is_execution_tool(&call.name)
                        || serde_json::from_str::<serde_json::Value>(&content)
                            .ok()
                            .as_ref()
                            .and_then(parse_structured_status)
                            .is_none()
                    {
                        continue;
                    }
                    observe_tool_result(
                        call,
                        &content,
                        timestamp,
                        conversation_offset + position,
                        &mut events,
                        &mut changed_paths,
                    );
                }
            }
            _ => {}
        }
    }

    changed_paths.sort();
    changed_paths.dedup();
    (events, changed_paths)
}

fn observe_tool_result(
    call: &ObservedToolCall,
    output: &str,
    timestamp: i64,
    conversation_position: usize,
    events: &mut Vec<(usize, ObservedEvent)>,
    changed_paths: &mut Vec<String>,
) {
    let (exit_code, succeeded) = parse_objective_status(output, call);
    if exit_code.is_none() && succeeded.is_none() {
        return;
    }

    if succeeded == Some(true) {
        changed_paths.extend(call.changed_paths.iter().cloned());
    }

    events.push((
        conversation_position,
        ObservedEvent {
            tool_name: call.name.clone(),
            command: call
                .command
                .as_deref()
                .map(|command| redact_and_truncate(command, 512)),
            output: redact_and_truncate(output, 1_200),
            exit_code,
            succeeded,
            timestamp,
        },
    ));
}

fn parse_objective_status(output: &str, call: &ObservedToolCall) -> (Option<i32>, Option<bool>) {
    if call.name == "exec" {
        return (None, None);
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(output.trim()) {
        if let Some(status) = parse_structured_status(&value) {
            return status;
        }
        return (None, None);
    }

    parse_associated_execution_status(output, call)
}

fn explicit_process_status(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Option<(Option<i32>, Option<bool>)> {
    if object.get("timed_out").and_then(serde_json::Value::as_bool) == Some(true)
        || object.get("cancelled").and_then(serde_json::Value::as_bool) == Some(true)
    {
        return Some((None, Some(false)));
    }

    object
        .get("exit_code")
        .or_else(|| object.get("exitCode"))
        .and_then(serde_json::Value::as_i64)
        .and_then(|code| i32::try_from(code).ok())
        .map(|code| (Some(code), Some(code == 0)))
}

fn parse_structured_status(value: &serde_json::Value) -> Option<(Option<i32>, Option<bool>)> {
    let object = value.as_object()?;

    if let Some(status) = explicit_process_status(object) {
        return Some(status);
    }

    if let Some(status) = ["result", "data", "raw_output"]
        .iter()
        .find_map(|key| object.get(*key).and_then(parse_structured_status))
    {
        return Some(status);
    }

    let succeeded = object
        .get("success")
        .or_else(|| object.get("succeeded"))
        .or_else(|| object.get("ok"))
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            object
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .map(|is_error| !is_error)
        })
        .or_else(|| {
            object
                .get("status")
                .and_then(serde_json::Value::as_str)
                .and_then(|status| match status.to_ascii_lowercase().as_str() {
                    "completed" | "success" | "succeeded" | "passed" => Some(true),
                    "failed" | "error" | "timed_out" | "timeout" | "cancelled" => Some(false),
                    _ => None,
                })
        });
    if let Some(succeeded) = succeeded {
        return Some((None, Some(succeeded)));
    }

    if object.get("error").is_some_and(|error| !error.is_null()) {
        return Some((None, Some(false)));
    }

    None
}

fn parse_associated_execution_status(
    output: &str,
    call: &ObservedToolCall,
) -> (Option<i32>, Option<bool>) {
    if !is_execution_tool(&call.name) || call.command.is_none() {
        return (None, None);
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(output.trim())
        && let Some(status) = parse_structured_status(&value)
    {
        return status;
    }

    let first_line = output.lines().next().unwrap_or_default().trim();
    if let Some(rest) = first_line
        .strip_prefix("exit:")
        .or_else(|| first_line.strip_prefix("Exit code:"))
    {
        let token = rest.split_whitespace().next().unwrap_or_default();
        if let Ok(exit_code) = token.parse::<i32>() {
            return (Some(exit_code), Some(exit_code == 0));
        }
        if token == "killed" {
            return (None, Some(false));
        }
    }

    if call.command.as_deref().and_then(validation_category) == Some("test")
        && let Some(succeeded) = parse_objective_test_summary(output)
    {
        return (None, Some(succeeded));
    }

    if call
        .command
        .as_deref()
        .and_then(validation_category)
        .is_some()
        && has_objective_failure_diagnostic(output)
    {
        return (None, Some(false));
    }

    (None, None)
}

fn parse_objective_test_summary(output: &str) -> Option<bool> {
    static FAILED_COUNT: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(\d+)\s+failed\b")
            .expect("test failure count expression must compile")
    });

    let mut last_failure_count = None;
    for captures in FAILED_COUNT.captures_iter(output) {
        if let Some(count) = captures
            .get(1)
            .and_then(|value| value.as_str().parse::<usize>().ok())
        {
            last_failure_count = Some(count);
        }
    }
    if last_failure_count.is_some_and(|count| count > 0) {
        return Some(false);
    }

    for line in output.lines().rev() {
        let normalized = line.trim().to_ascii_lowercase();
        if let Some(summary) = normalized
            .split_once("test result:")
            .map(|(_, summary)| summary)
        {
            let summary = summary.trim_start();
            if summary.starts_with("failed") {
                return Some(false);
            }
            if summary.starts_with("ok") {
                return Some(true);
            }
        }
        if normalized.starts_with("failed ")
            || normalized.starts_with("failures:")
            || normalized.contains("error: test failed")
        {
            return Some(false);
        }
    }

    (last_failure_count == Some(0)).then_some(true)
}

fn has_objective_failure_diagnostic(output: &str) -> bool {
    output.lines().any(|line| {
        let normalized = line.trim().to_ascii_lowercase();
        normalized.starts_with("error[e")
            || normalized.starts_with("error ts")
            || normalized.contains("error: could not compile")
            || normalized.contains("compilation failed")
            || normalized.contains("lint failed")
    })
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

fn extract_command(tool_name: &str, arguments: &str) -> Option<String> {
    if !is_execution_tool(tool_name) {
        return None;
    }

    let arguments = arguments.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) {
        if let Some(command) = value
            .get("command")
            .or_else(|| value.get("cmd"))
            .and_then(serde_json::Value::as_str)
        {
            return Some(command.to_owned());
        }
    }

    if !arguments.starts_with('{') && !arguments.is_empty() {
        return Some(arguments.to_owned());
    }

    None
}

fn extract_changed_paths(tool_name: &str, arguments: &str) -> Vec<String> {
    if !matches!(
        tool_name,
        "apply_patch" | "search_replace" | "write" | "write_file" | "edit_file" | "edit"
    ) {
        return Vec::new();
    }

    let mut paths = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) {
        for key in ["path", "file_path", "filePath"] {
            if let Some(path) = value.get(key).and_then(serde_json::Value::as_str) {
                paths.push(redact_and_truncate(path, 300));
            }
        }
        for key in ["patch", "input", "content"] {
            if let Some(patch) = value.get(key).and_then(serde_json::Value::as_str) {
                extract_patch_paths(patch, &mut paths);
            }
        }
    } else {
        extract_patch_paths(arguments, &mut paths);
    }

    paths
}

fn extract_patch_paths(patch: &str, paths: &mut Vec<String>) {
    for line in patch.lines() {
        for prefix in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
            if let Some(path) = line.strip_prefix(prefix) {
                paths.push(redact_and_truncate(path.trim(), 300));
            }
        }
    }
}

fn objective_run_outcome(events: &[ObservedEvent]) -> Option<bool> {
    let mut validation_outcomes = HashMap::<(&str, String), bool>::new();
    let mut last_execution_outcome = None;

    for event in events {
        let Some(succeeded) = event
            .succeeded
            .or_else(|| event.exit_code.map(|code| code == 0))
        else {
            continue;
        };
        let Some(command) = event.command.as_deref() else {
            continue;
        };
        last_execution_outcome = Some(succeeded);
        if let Some(category) = validation_category(command) {
            validation_outcomes.insert((category, normalize_for_matching(command)), succeeded);
        }
    }

    if validation_outcomes.values().any(|succeeded| !succeeded) {
        Some(false)
    } else if !validation_outcomes.is_empty() {
        Some(true)
    } else if last_execution_outcome == Some(false) {
        Some(false)
    } else {
        None
    }
}

fn validation_category(command: &str) -> Option<&'static str> {
    let command = command.to_ascii_lowercase();
    if command.contains("clippy")
        || command.contains("eslint")
        || command.contains("ruff")
        || command.contains(" lint")
    {
        Some("lint")
    } else if command.contains("cargo test")
        || command.contains("pytest")
        || command.contains("npm test")
        || command.contains("npm run test")
        || command.contains("pnpm test")
        || command.contains("yarn test")
        || command.contains("go test")
        || command.contains("swift test")
        || command.contains("gradle test")
        || command.contains(" test ")
        || command.ends_with(" test")
    {
        Some("test")
    } else if command.contains("cargo check")
        || command.contains("cargo build")
        || command.contains(" build")
        || command.contains("tsc")
        || command.contains("typecheck")
        || command.contains("type-check")
    {
        Some("compile")
    } else if command.contains("benchmark") || command.contains(" bench") {
        Some("benchmark")
    } else {
        None
    }
}

fn recommendation_was_followed(
    memory: &xai_grok_memory::experience::types::ExperienceMemory,
    observation: &RunObservation,
) -> bool {
    let Some(recommendation) = memory.recommendation.as_deref() else {
        return false;
    };
    let guidance_tokens = meaningful_tokens(recommendation);

    observation.events.iter().any(|event| {
        let Some(command) = event.command.as_deref() else {
            return false;
        };
        let normalized_command = normalize_for_matching(command);
        if memory.category
            == xai_grok_memory::experience::types::ExperienceCategory::ToolProcessLesson
            && normalize_for_matching(recommendation).contains(&normalized_command)
            && memory
                .tests_run
                .iter()
                .any(|previous| normalize_for_matching(previous) == normalized_command)
        {
            return true;
        }

        if recommendation
            .split('`')
            .enumerate()
            .any(|(index, fragment)| {
                index % 2 == 1
                    && fragment.trim().len() >= 8
                    && normalized_command.contains(&normalize_for_matching(fragment))
            })
        {
            return true;
        }

        if validation_category(command).is_some()
            && memory.category
                != xai_grok_memory::experience::types::ExperienceCategory::ToolProcessLesson
        {
            return false;
        }

        let command_tokens = meaningful_tokens(command);
        let overlap = guidance_tokens.intersection(&command_tokens).count();
        overlap >= 2 && overlap * 2 >= guidance_tokens.len().min(command_tokens.len())
    })
}

fn normalize_for_matching(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn meaningful_tokens(value: &str) -> HashSet<String> {
    const STOP_WORDS: &[&str] = &[
        "the", "and", "for", "with", "this", "that", "from", "then", "before", "after", "should",
        "using", "existing", "changes", "check", "test", "tests", "run", "cargo", "npm",
    ];

    value
        .split(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '-'
        })
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() >= 4 && !STOP_WORDS.contains(&token.as_str()))
        .collect()
}

fn redact_and_truncate(value: &str, max_chars: usize) -> String {
    redact_experience_text(value)
        .chars()
        .take(max_chars)
        .collect()
}

fn redact_experience_text(value: &str) -> String {
    if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(value) {
        redact_json_secrets(&mut json);
        return serde_json::to_string(&json).unwrap_or_else(|_| "[REDACTED]".to_owned());
    }

    static PRIVATE_KEY_BLOCK: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
        )
        .expect("private key redaction expression must compile")
    });
    static URL_CREDENTIALS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)([a-z][a-z0-9+.-]*://)([^/\s@]+)@")
            .expect("URL credential redaction expression must compile")
    });
    static COOKIE_HEADER: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i)(set-cookie|cookie)\s*:\s*[^'"\r\n]+"#)
            .expect("cookie header redaction expression must compile")
    });

    let without_private_keys = PRIVATE_KEY_BLOCK.replace_all(value, "[REDACTED PRIVATE KEY]");
    let without_url_credentials =
        URL_CREDENTIALS.replace_all(&without_private_keys, "${1}[REDACTED]@");
    let sanitized = COOKIE_HEADER.replace_all(&without_url_credentials, "${1}: [REDACTED]");
    let sanitized = sanitized
        .split_inclusive('\n')
        .map(|line| match line.strip_suffix('\n') {
            Some(content) => format!(
                "{}\n",
                xai_grok_memory::experience::extraction::redact_sensitive_text(content)
            ),
            None => xai_grok_memory::experience::extraction::redact_sensitive_text(line),
        })
        .collect::<String>();

    let mut redacted = String::with_capacity(sanitized.len().min(1_200));
    let mut redact_next = false;
    for segment in sanitized.split_inclusive(char::is_whitespace) {
        let token = segment.trim_end_matches(char::is_whitespace);
        let separator = &segment[token.len()..];
        let lower = token.to_ascii_lowercase();

        if redact_next {
            if matches!(
                lower.as_str(),
                "bearer"
                    | "basic"
                    | "digest"
                    | "negotiate"
                    | "token"
                    | "oauth"
                    | "apikey"
                    | "api-key"
                    | "dpop"
                    | "ntlm"
                    | "signature"
                    | "aws4-hmac-sha256"
            ) {
                redacted.push_str(token);
            } else {
                redacted.push_str("[REDACTED]");
                redact_next = false;
            }
        } else if let Some((key, _)) = token.split_once('=')
            && is_secret_key(key)
        {
            redacted.push_str(key);
            redacted.push_str("=[REDACTED]");
        } else {
            redacted.push_str(token);
            if lower == "bearer" || is_secret_key(&lower) {
                redact_next = true;
            }
        }
        redacted.push_str(separator);
    }

    redacted
}

fn redact_json_secrets(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, entry) in object {
                if is_secret_key(key) {
                    *entry = serde_json::Value::String("[REDACTED]".to_owned());
                } else {
                    redact_json_secrets(entry);
                }
            }
        }
        serde_json::Value::Array(entries) => {
            for entry in entries {
                redact_json_secrets(entry);
            }
        }
        serde_json::Value::String(text) => {
            *text = redact_experience_text(text);
        }
        _ => {}
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("api_key")
        || key.contains("api-key")
        || key.contains("apikey")
        || key.contains("access_token")
        || key.contains("access-token")
        || key.contains("refresh_token")
        || key.contains("refresh-token")
        || key.contains("private_key")
        || key.contains("private-key")
        || key.contains("cookie")
        || key.contains("credential")
        || key.contains("session_id")
        || key.contains("sessionid")
        || key.contains("password")
        || key.contains("passwd")
        || key.contains("secret")
        || key.contains("authorization")
        || key == "token"
        || key == "--token"
}

/// Generate a structured session summary from conversation metadata.
///
/// Uses `real_queries` (pre-computed via [`extract_real_user_queries`]) for
/// the user-message count and topics so that synthetic bootstrap messages and
/// auto-continue sentinels are never surfaced in the saved summary.
///
/// This does NOT call an LLM — it extracts structured information directly
/// from the conversation items: message counts, session date, and the first
/// few real user topics. For richer content capture use `/flush`.
pub(crate) fn generate_metadata_summary(
    conversation: &[ConversationItem],
    real_queries: &[String],
) -> String {
    let real_count = real_queries.len();

    let assistant_count = conversation
        .iter()
        .filter(|item| matches!(item, ConversationItem::Assistant(_)))
        .count();

    let tool_count = conversation
        .iter()
        .filter(|item| {
            matches!(
                item,
                ConversationItem::ToolResult(_) | ConversationItem::CustomToolOutput(_)
            )
        })
        .count();

    // ── Assemble summary ─────────────────────────────────────────────────────
    let mut summary = String::new();
    summary.push_str("## Session Summary\n\n");
    summary.push_str(&format!(
        "- **Messages:** {} user, {} assistant, {} tool results\n",
        real_count, assistant_count, tool_count
    ));
    summary.push_str(&format!(
        "- **Date:** {}\n\n",
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
    ));

    // Topics — first few real queries (never the synthetic prefix text).
    // chars().take(100) avoids byte-boundary panics on multi-byte Unicode.
    let topics: Vec<String> = real_queries
        .iter()
        .take(5)
        .map(|q| q.chars().take(100).collect::<String>())
        .collect();

    if !topics.is_empty() {
        summary.push_str("## Topics Discussed\n\n");
        for (i, topic) in topics.iter().enumerate() {
            summary.push_str(&format!("{}. {}\n", i + 1, topic));
        }
        summary.push('\n');
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampling::conversation::{
        AssistantItem, ContentPart, CustomToolOutputItem, ToolCall, ToolResultItem, UserItem,
    };
    use tempfile::TempDir;
    use xai_grok_memory::experience::types::{ExperienceCategory, ExperienceMemory};

    fn make_user(text: &str) -> ConversationItem {
        ConversationItem::User(UserItem {
            content: vec![ContentPart::Text { text: text.into() }],
            synthetic_reason: None,
            ..Default::default()
        })
    }

    /// Build a realistic first-turn user message: metadata prefix + user query in tags.
    ///
    /// This matches what `construct_user_message` + `user_query()` produce.
    fn make_synthetic_prefix_with_query(query: &str) -> ConversationItem {
        make_user(&format!(
            "<user_info>\nOS Version: macos\nShell: /bin/bash\n</user_info>\n\
             <git_status>\n(no changes)\n</git_status>\n\
             <user_query>\n{query}\n</user_query>"
        ))
    }

    /// Build a metadata-only prefix (no <user_query> tag) — represents the
    /// synthetic bootstrap message on sessions that never received a real prompt.
    fn make_metadata_only() -> ConversationItem {
        make_user("<user_info>\nOS Version: macos\n</user_info>")
    }

    fn make_assistant(text: &str) -> ConversationItem {
        ConversationItem::Assistant(AssistantItem {
            content: text.into(),
            tool_calls: vec![],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        })
    }

    fn test_storage(tmp: &TempDir) -> MemoryStorage {
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        MemoryStorage::with_paths(global, workspace)
    }

    fn execution_items(call_id: &str, command: &str, output: &str) -> [ConversationItem; 2] {
        [
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: call_id.into(),
                name: "run_terminal_command".to_owned(),
                arguments: serde_json::json!({ "command": command }).to_string().into(),
            }]),
            ConversationItem::tool_result(call_id, output),
        ]
    }

    fn nested_execution_evidence(
        call_id: &str,
        task: &str,
        command: &str,
        output: &str,
        exit_code: i32,
        conversation_position: usize,
    ) -> super::super::experience_ledger::NestedToolEvidence {
        super::super::experience_ledger::NestedToolEvidence {
            tool_call_id: call_id.to_owned(),
            tool_name: "run_terminal_command".to_owned(),
            command: Some(command.to_owned()),
            output: output.to_owned(),
            exit_code: Some(exit_code),
            succeeded: Some(exit_code == 0),
            changed_paths: Vec::new(),
            timestamp: 1,
            task_fingerprint: Some(super::super::experience_ledger::task_fingerprint(task)),
            turn_number: 1,
            conversation_position,
        }
    }

    // -----------------------------------------------------------------------
    // Existing behaviour tests (updated for new signatures / semantics)
    // -----------------------------------------------------------------------

    #[test]
    fn test_on_session_end_skips_short_sessions() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        // Only 1 real user message — should skip.
        let conv = vec![make_user("hello"), make_assistant("hi")];
        let result = on_session_end(&storage, &conv, "test-session-id", true);
        assert_eq!(result, SessionEndResult::Skipped);
    }

    #[test]
    fn test_on_session_end_skips_brief_sessions() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        // 3 real messages but very short — total bytes < MIN_TOTAL_QUERY_BYTES (50).
        // "hi" (2) + "ok" (2) + "bye" (3) = 7 bytes
        let conv = vec![
            make_user("hi"),
            make_assistant("hello"),
            make_user("ok"),
            make_assistant("sure"),
            make_user("bye"),
            make_assistant("goodbye"),
        ];

        let result = on_session_end(&storage, &conv, "sess-brief", true);
        assert_eq!(
            result,
            SessionEndResult::Skipped,
            "sessions with brief content should be skipped even with enough messages"
        );
    }

    #[test]
    fn test_on_session_end_writes_summary() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        let conv = vec![
            make_user("help me fix the auth bug"),
            make_assistant("sure, let me look at auth.rs"),
            make_user("also check the tests"),
            make_assistant("found the issue"),
            make_user("great, can you fix the login page too"),
            make_assistant("on it"),
        ];

        let result = on_session_end(&storage, &conv, "sess12345678", true);
        assert!(
            matches!(result, SessionEndResult::Written(_)),
            "should write summary, got {result:?}"
        );

        // Verify file was created.
        let files = storage.list_memory_files().unwrap();
        let session_files: Vec<_> = files
            .iter()
            .filter(|f| {
                f.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .contains("sess1234")
            })
            .collect();
        assert!(!session_files.is_empty(), "session log file should exist");
    }

    #[test]
    fn test_on_session_end_summary_has_structure() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        let conv = vec![
            make_user("implement feature X"),
            make_assistant("working on it"),
            make_user("also add tests for edge cases"),
            make_assistant("done"),
            make_user("make sure everything compiles cleanly"),
            make_assistant("verified"),
            ConversationItem::ToolResult(ToolResultItem {
                tool_call_id: "tc_1".to_string(),
                content: "file written".into(),
                images: Vec::new(),
                ordered_content: Vec::new(),
            }),
        ];

        on_session_end(&storage, &conv, "sess12345678", true);

        let files = storage.list_memory_files().unwrap();
        let session_file = files
            .iter()
            .find(|f| {
                f.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .contains("sess1234")
            })
            .unwrap();

        let content = std::fs::read_to_string(session_file).unwrap();
        assert!(content.contains("## Session Summary"));
        assert!(content.contains("3 user"));
        assert!(content.contains("## Topics Discussed"));
        assert!(content.contains("implement feature X"));
        assert!(content.contains("also add tests for edge cases"));
    }

    #[test]
    fn test_on_session_end_empty_conversation() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        let result = on_session_end(&storage, &[], "test-id", true);
        assert_eq!(result, SessionEndResult::Skipped);
    }

    #[test]
    fn test_generate_metadata_summary_format() {
        let conv = vec![
            make_user("first question"),
            make_assistant("answer"),
            ConversationItem::custom_tool_output(
                xai_grok_sampling_types::CustomToolOutputItem::text("exec-1", "done"),
            ),
            make_user("second question"),
        ];
        let real_queries = vec!["first question".to_string(), "second question".to_string()];
        let summary = generate_metadata_summary(&conv, &real_queries);
        assert!(summary.contains("## Session Summary"));
        assert!(summary.contains("2 user"));
        assert!(summary.contains("1 assistant"));
        assert!(summary.contains("1 tool results"));
        assert!(summary.contains("first question"));
    }

    // -----------------------------------------------------------------------
    // Real-user-query extraction tests
    // -----------------------------------------------------------------------

    /// `save_on_end = false` always skips — even for a long conversation.
    #[test]
    fn test_on_session_end_save_on_end_false_skips() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        let conv = vec![
            make_user("task one"),
            make_assistant("done"),
            make_user("task two"),
            make_assistant("done"),
        ];

        let result = on_session_end(&storage, &conv, "sess-disabled", false);
        assert_eq!(
            result,
            SessionEndResult::Skipped,
            "save_on_end=false must skip even with enough messages"
        );

        // No session log file should have been written (the MEMORY.md templates
        // created by ensure_initialized are expected and are not session logs).
        let files = storage.list_memory_files().unwrap();
        let session_logs: Vec<_> = files
            .iter()
            .filter(|f| f.components().any(|c| c.as_os_str() == "sessions"))
            .collect();
        assert!(
            session_logs.is_empty(),
            "no session log should be created when save_on_end=false"
        );
    }

    /// Threshold is independent of `save_on_end` so exit dream can still run.
    #[test]
    fn test_conversation_meets_session_end_threshold_ignores_save_config() {
        let short = vec![make_user("hi"), make_assistant("hey")];
        assert!(
            queries_meeting_session_end_threshold(&short).is_none(),
            "brief session must fail threshold"
        );

        let substantial = vec![
            make_user("help me fix the auth bug in login"),
            make_assistant("looking"),
            make_user("also check the integration tests please"),
            make_assistant("found it"),
            make_user("great, can you patch the login page too"),
            make_assistant("done"),
        ];
        assert!(
            queries_meeting_session_end_threshold(&substantial).is_some(),
            "substantial session must pass threshold even when save is off"
        );
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();
        assert_eq!(
            on_session_end(&storage, &substantial, "sess-no-save", false),
            SessionEndResult::Skipped,
            "save_on_end=false still skips write"
        );
    }

    /// Synthetic metadata-only prefix (no `<user_query>`) is excluded from the
    /// real-message count so it cannot push the session over the gate threshold.
    #[test]
    fn test_synthetic_prefix_alone_does_not_count_as_real_message() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        // The conversation has 2 User items, but the first is metadata-only
        // and the second is a real query.  Only 1 real prompt → still skipped.
        let conv = vec![
            make_metadata_only(), // synthetic, no <user_query>
            make_assistant("hi"),
            make_user("help me with something"), // 1 real prompt
            make_assistant("sure"),
        ];

        let result = on_session_end(&storage, &conv, "sess-synth", true);
        assert_eq!(
            result,
            SessionEndResult::Skipped,
            "metadata-only prefix must not count toward the real-message gate"
        );
    }

    /// With a real synthetic prefix + two real queries the session IS written,
    /// and the slug is derived from the first real query (not the prefix text).
    #[test]
    fn test_slug_derived_from_real_query_not_prefix() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        // First User item: metadata prefix wrapping the real first query.
        // Second User item: a follow-up plain-text query.
        let conv = vec![
            make_synthetic_prefix_with_query("fix the login bug"),
            make_assistant("on it"),
            make_user("also add a test for it"),
            make_assistant("done"),
            make_user("and update the error messages"),
            make_assistant("updated"),
        ];

        let result = on_session_end(&storage, &conv, "sess-slug-check", true);
        assert!(
            matches!(result, SessionEndResult::Written(_)),
            "should write, got {result:?}"
        );

        let files = storage.list_memory_files().unwrap();
        // The file name slug should come from "fix the login bug", not from the
        // raw metadata prefix text.
        assert!(
            files
                .iter()
                .any(|f| f.to_str().unwrap().contains("fix-the-login-bug")),
            "slug should be derived from the first real query"
        );
    }

    /// Topics in the summary contain real query text, not the metadata prefix.
    #[test]
    fn test_topics_contain_real_queries_not_metadata() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        let conv = vec![
            make_synthetic_prefix_with_query("implement the auth feature"),
            make_assistant("working on it"),
            make_user("add integration tests too"),
            make_assistant("done"),
            make_user("check the error handling as well"),
            make_assistant("verified"),
        ];

        on_session_end(&storage, &conv, "sess-topics", true);

        let files = storage.list_memory_files().unwrap();
        let session_file = files
            .iter()
            .find(|f| f.to_str().unwrap().contains("implement-the-auth"))
            .expect("session file should exist with correct slug");

        let content = std::fs::read_to_string(session_file).unwrap();
        assert!(
            content.contains("implement the auth feature"),
            "topics must include the real first query"
        );
        assert!(
            content.contains("add integration tests too"),
            "topics must include the second real query"
        );
        // The raw metadata prefix text must NOT appear as a topic.
        assert!(
            !content.contains("<user_info>"),
            "metadata tag text must not appear in topics"
        );
        assert!(
            !content.contains("OS Version"),
            "metadata content must not appear in topics"
        );
    }

    /// The *actual* AUTO_CONTINUE_PROMPT text pushed into the conversation after
    /// auto-compaction must not be counted as a real user message, and must not
    /// appear in session-end topics.
    ///
    /// This is the key regression test for the correctness fix: we use the
    /// real stored text, not just the `"__auto_continue__"` request-id sentinel.
    #[test]
    fn test_actual_auto_continue_prompt_excluded() {
        use crate::session::helpers::session_compact::AUTO_CONTINUE_PROMPT;

        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        // Old sessions may contain AUTO_CONTINUE_PROMPT as a User item after
        // auto-compaction. Verify it's excluded from real user query counts.
        let conv = vec![
            make_synthetic_prefix_with_query("implement feature Z"),
            make_assistant("done"),
            // Simulated auto-compaction: AUTO_CONTINUE_PROMPT is pushed as a User item.
            make_user(AUTO_CONTINUE_PROMPT),
            make_assistant("continuing..."),
        ];

        // Only 1 real query ("implement feature Z") — should skip (< MIN_USER_MESSAGES).
        let result = on_session_end(&storage, &conv, "sess-autocompact", true);
        assert_eq!(
            result,
            SessionEndResult::Skipped,
            "AUTO_CONTINUE_PROMPT must not count as a real user message"
        );
    }

    /// Long Unicode user queries are truncated at a character boundary, never a byte boundary.
    #[test]
    fn test_topics_unicode_truncation_no_panic() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        // Build a query that is > 100 chars when byte-counted but the 100th byte
        // falls inside a multi-byte sequence (emoji are 4 bytes each).
        let emoji_query = "🦀".repeat(30); // 30 × 4 = 120 bytes, but 30 chars
        assert!(emoji_query.len() > 100, "precondition: > 100 bytes");

        let conv = vec![
            make_user(&emoji_query),
            make_assistant("done"),
            make_user("second question about the codebase"),
            make_assistant("ok"),
            make_user("third question about testing"),
            make_assistant("yes"),
        ];

        // Must not panic; the summary should be produced successfully.
        let result = on_session_end(&storage, &conv, "sess-unicode", true);
        assert!(
            matches!(result, SessionEndResult::Written(_)),
            "should write summary without panic on Unicode query, got {result:?}"
        );
    }

    /// `__auto_continue__` sentinels do not count as real user messages.
    #[test]
    fn test_auto_continue_sentinel_excluded_from_count() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        // Session has only 1 real human query; the other two User items are
        // auto-continue sentinels.
        let conv = vec![
            make_user("<user_query>\n__auto_continue__\n</user_query>"),
            make_assistant("continuing"),
            make_user("real human question"),
            make_assistant("answer"),
            make_user("<user_query>\n__auto_continue__\n</user_query>"),
            make_assistant("done"),
        ];

        let result = on_session_end(&storage, &conv, "sess-autocont", true);
        assert_eq!(
            result,
            SessionEndResult::Skipped,
            "auto-continue sentinels must not count toward the real-message gate"
        );
    }

    // -----------------------------------------------------------------------
    // Summary format tests
    // -----------------------------------------------------------------------

    /// Summary only contains Session Summary and Topics Discussed — no
    /// Tools Used or Files Touched sections (those are low-value noise).
    #[test]
    fn test_generate_metadata_summary_only_session_and_topics() {
        let conv = vec![
            make_user("hello"),
            make_assistant("hi there"),
            make_user("how are you"),
            make_assistant("great"),
        ];
        let queries = vec!["hello".to_string(), "how are you".to_string()];
        let summary = generate_metadata_summary(&conv, &queries);

        assert!(summary.contains("## Session Summary"));
        assert!(summary.contains("## Topics Discussed"));
        assert!(
            !summary.contains("## Tools Used"),
            "tools section must not appear"
        );
        assert!(
            !summary.contains("## Files Touched"),
            "files section must not appear"
        );
        assert!(
            !summary.contains("## Shell Commands"),
            "commands section must not appear"
        );
    }

    #[test]
    fn test_experience_persists_single_task_with_compiler_and_test_evidence() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let mut conversation = vec![make_user("Fix the parser regression and verify the result")];
        conversation.extend(execution_items(
            "compile-call",
            "cargo check -p parser",
            r#"{"exit_code":0,"output":"Finished dev profile"}"#,
        ));
        conversation.extend(execution_items(
            "test-call",
            "cargo test -p parser regression",
            "exit: 0\ntest result: ok. 4 passed; 0 failed",
        ));

        let persisted =
            persist_session_experiences(&storage, &conversation, "single-task-run").unwrap();
        assert!(persisted > 0);

        let store = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite")).unwrap();
        let experiences = store.all().unwrap();
        assert!(experiences.iter().any(|experience| {
            experience.success == Some(true)
                && experience
                    .source_run_ids
                    .iter()
                    .any(|run| run == "single-task-run")
        }));
        assert!(experiences.iter().all(|experience| {
            experience.repository_id == storage.workspace_dir().to_string_lossy()
                && experience.environment == xai_grok_memory::experience::execution_environment()
        }));
        assert_eq!(
            on_session_end(&storage, &conversation, "single-task-run", true),
            SessionEndResult::Skipped,
            "experience extraction must not relax the legacy Markdown summary gate"
        );
    }

    #[test]
    fn test_experience_failed_test_overrides_successful_assistant_prose() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let mut conversation = vec![make_user("Fix authentication and run the regression tests")];
        conversation.extend(execution_items(
            "failed-test",
            "cargo test -p auth login_regression",
            r#"{"exit_code":101,"output":"test login_regression ... FAILED"}"#,
        ));
        conversation.push(make_assistant(
            "Everything is complete and all tests passed.",
        ));

        let persisted =
            persist_session_experiences(&storage, &conversation, "failed-test-run").unwrap();
        assert!(persisted > 0);

        let experiences = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))
            .unwrap()
            .all()
            .unwrap();
        assert!(experiences.iter().any(|experience| {
            experience.category == ExperienceCategory::FailureAntiPattern
                && experience.success == Some(false)
        }));
        assert!(!experiences.iter().any(|experience| {
            experience.category == ExperienceCategory::SuccessfulPattern
                && experience.success == Some(true)
        }));
    }

    #[test]
    fn test_experience_retains_partial_success_when_later_verification_fails() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let mut conversation = vec![make_user("Fix the parser implementation and its tests")];
        conversation.extend(execution_items(
            "partial-compile",
            "cargo check -p parser",
            "exit: 0\nFinished dev profile",
        ));
        conversation.extend(execution_items(
            "partial-failed-test",
            "cargo test -p parser generated_schema",
            "exit: 101\ntest generated_schema ... FAILED",
        ));

        assert!(persist_session_experiences(&storage, &conversation, "partial-run").unwrap() > 0);
        let experiences = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))
            .unwrap()
            .all()
            .unwrap();
        assert!(
            experiences.iter().any(|experience| {
                experience.category == ExperienceCategory::FailureAntiPattern
            })
        );
        assert!(experiences.iter().any(|experience| {
            experience
                .evidence
                .iter()
                .any(|signal| signal.command.as_deref() == Some("cargo check -p parser"))
        }));
    }

    #[test]
    fn test_experience_without_correlated_objective_evidence_is_not_persisted() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let conversation = vec![
            make_user("Implement the requested authentication change"),
            make_assistant("The change is finished and every test passed."),
            ConversationItem::tool_result("unmatched-call", r#"{"exit_code":0}"#),
        ];

        assert_eq!(
            persist_session_experiences(&storage, &conversation, "unverified-run").unwrap(),
            0
        );
        assert!(!storage.workspace_dir().join("index.sqlite").exists());
    }

    #[test]
    fn test_experience_extracts_direct_execution_custom_output() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let arguments = serde_json::json!({
            "command": "cargo test -p parser custom_case"
        })
        .to_string();
        let conversation = vec![
            make_user("Repair the parser direct execution regression"),
            ConversationItem::assistant_tool_calls(vec![ToolCall::custom(
                "provider-call",
                "provider-item",
                "run_terminal_command",
                arguments.as_str(),
            )]),
            ConversationItem::custom_tool_output(
                CustomToolOutputItem::text(
                    "provider-call",
                    r#"{"exit_code":0,"output":"test result: ok"}"#,
                )
                .with_name("run_terminal_command"),
            ),
        ];

        let persisted =
            persist_session_experiences(&storage, &conversation, "custom-tool-run").unwrap();
        assert!(persisted > 0);
        let experiences = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))
            .unwrap()
            .all()
            .unwrap();
        assert!(experiences.iter().any(|experience| {
            experience
                .tests_run
                .iter()
                .any(|command| command.contains("cargo test -p parser custom_case"))
        }));
    }

    #[test]
    fn test_experience_direct_custom_execution_requires_structured_status() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let arguments = serde_json::json!({
            "command": "cargo test -p parser custom_case"
        })
        .to_string();
        let conversation = vec![
            make_user("Repair the parser direct execution regression"),
            ConversationItem::assistant_tool_calls(vec![ToolCall::custom(
                "provider-call",
                "provider-item",
                "run_terminal_command",
                arguments.as_str(),
            )]),
            ConversationItem::custom_tool_output(
                CustomToolOutputItem::text("provider-call", "exit: 0\ntest result: ok")
                    .with_name("run_terminal_command"),
            ),
        ];

        assert_eq!(
            persist_session_experiences(&storage, &conversation, "unstructured-custom-run")
                .unwrap(),
            0
        );
        assert!(!storage.workspace_dir().join("index.sqlite").exists());
    }

    #[test]
    fn test_experience_structured_exit_status_overrides_untrusted_output_text() {
        let call = ObservedToolCall {
            name: "run_terminal_command".to_owned(),
            command: Some("cargo test -p parser".to_owned()),
            changed_paths: Vec::new(),
        };

        assert_eq!(
            parse_objective_status(
                r#"{"exit_code":101,"output":"exit: 0\\ntest result: ok"}"#,
                &call,
            ),
            (Some(101), Some(false))
        );
        assert_eq!(
            parse_objective_status(r#"{"output":"exit_code: 0"}"#, &call),
            (None, None)
        );
        assert_eq!(
            parse_objective_status(r#"{"status":"completed","result":{"exit_code":7}}"#, &call),
            (Some(7), Some(false))
        );
    }

    #[test]
    fn test_experience_code_mode_wrapper_status_is_never_objective_evidence() {
        let call = ObservedToolCall {
            name: "exec".to_owned(),
            command: Some("cargo test -p parser regression".to_owned()),
            changed_paths: Vec::new(),
        };

        assert_eq!(
            parse_objective_status(
                r#"{"success":true,"text":"test result: FAILED. 2 passed; 1 failed"}"#,
                &call,
            ),
            (None, None)
        );
        assert_eq!(
            parse_objective_status(
                r#"{"status":"completed","result":{"content":[{"type":"text","text":"test result: FAILED"}]}}"#,
                &call,
            ),
            (None, None)
        );
        assert_eq!(
            parse_objective_status(
                r#"{"success":true,"output":{"stdout":"4 passed; 0 failed"}}"#,
                &call,
            ),
            (None, None)
        );
        assert_eq!(
            parse_objective_status(
                r#"{"success":true,"output":"command submitted to the runner"}"#,
                &call,
            ),
            (None, None)
        );
    }

    #[test]
    fn test_experience_code_mode_source_never_supplies_an_execution_command() {
        let source = r#"const bait={command:"cargo test -p parser"}; text('{"exit_code":0}')"#;

        assert_eq!(extract_command("exec", source), None);
        assert_eq!(
            extract_command("exec", &serde_json::json!({ "source": source }).to_string()),
            None
        );
        assert_eq!(
            extract_command(
                "run_terminal_command",
                r#"{"command":"cargo test -p parser"}"#,
            ),
            Some("cargo test -p parser".to_owned())
        );
    }

    #[test]
    fn test_experience_code_mode_nested_failure_without_provenance_is_excluded() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let source =
            r#"await tools.run_terminal_command({"command":"cargo test -p parser regression"})"#;
        let conversation = vec![
            make_user("Repair the parser custom execution regression"),
            ConversationItem::assistant_tool_calls(vec![ToolCall::custom(
                "failed-provider-call",
                "provider-item",
                "exec",
                source,
            )]),
            ConversationItem::custom_tool_output(
                CustomToolOutputItem::text(
                    "failed-provider-call",
                    r#"{"success":true,"text":"test result: FAILED. 1 passed; 1 failed"}"#,
                )
                .with_name("exec"),
            ),
            make_assistant("The JavaScript wrapper completed successfully."),
        ];

        assert_eq!(
            persist_session_experiences(&storage, &conversation, "failed-wrapper-run").unwrap(),
            0
        );
        assert!(!storage.workspace_dir().join("index.sqlite").exists());
    }

    #[test]
    fn test_experience_authenticated_code_mode_execution_persists_and_reinforces() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();
        let store = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite")).unwrap();

        let task = "Repair the parser direct execution regression";
        let command = "cargo test -p parser authenticated_nested_case";
        let mut recommendation = ExperienceMemory::new(
            ExperienceCategory::ToolProcessLesson,
            "Run the focused parser regression",
            "prior-parser-run",
            1,
        );
        recommendation.repository_id = storage.workspace_dir().to_string_lossy().into_owned();
        recommendation.recommendation = Some(format!("Run `{command}`"));
        recommendation.tests_run = vec![command.to_owned()];
        let recommendation_id = store.upsert(&recommendation).unwrap();
        let run_id = "authenticated-code-mode-run";
        store
            .record_retrieval(run_id, std::slice::from_ref(&recommendation_id))
            .unwrap();
        drop(store);

        let conversation = vec![
            make_user(task),
            ConversationItem::assistant_tool_calls(vec![ToolCall::custom(
                "code-mode-wrapper",
                "provider-item",
                "exec",
                "await tools.run_terminal_command({command: 'cargo test'})",
            )]),
            ConversationItem::custom_tool_output(
                CustomToolOutputItem::text("code-mode-wrapper", "model-controlled cell output")
                    .with_name("exec"),
            ),
        ];
        let edit = super::super::experience_ledger::NestedToolEvidence {
            tool_call_id: "authenticated-edit".to_owned(),
            tool_name: "apply_patch".to_owned(),
            command: None,
            output: "updated src/parser.rs".to_owned(),
            exit_code: None,
            succeeded: Some(true),
            changed_paths: vec!["src/parser.rs".to_owned()],
            timestamp: 1,
            task_fingerprint: Some(super::super::experience_ledger::task_fingerprint(task)),
            turn_number: 1,
            conversation_position: 2,
        };
        let execution = nested_execution_evidence(
            "authenticated-test",
            task,
            command,
            "test result: ok. 4 passed; 0 failed",
            0,
            2,
        );

        assert!(
            persist_session_experiences_with_trusted_events(
                &storage,
                &conversation,
                run_id,
                &[edit, execution],
                &HashSet::new(),
            )
            .unwrap()
                > 0
        );

        let store = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite")).unwrap();
        let experiences = store.all().unwrap();
        assert!(experiences.iter().any(|experience| {
            experience
                .source_run_ids
                .iter()
                .any(|source| source == run_id)
                && experience.context.contains("src/parser.rs")
                && experience.tests_run.iter().any(|test| test == command)
        }));
        let reinforced = store.get(&recommendation_id).unwrap().unwrap();
        assert_eq!(reinforced.followed_count, 1);
        assert_eq!(reinforced.successful_reuse_count, 1);
    }

    #[test]
    fn test_experience_authenticated_code_mode_rejects_previous_task_evidence() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let conversation = vec![make_user("Implement the current parser escaping feature")];
        let previous_task_event = nested_execution_evidence(
            "previous-task-call",
            "Repair the old migration locking regression",
            "cargo test -p migrations lock_contention",
            "test result: FAILED. 1 failed",
            101,
            1,
        );

        assert_eq!(
            persist_session_experiences_with_trusted_events(
                &storage,
                &conversation,
                "current-task-run",
                &[previous_task_event],
                &HashSet::new(),
            )
            .unwrap(),
            0
        );
        assert!(!storage.workspace_dir().join("index.sqlite").exists());
    }

    #[test]
    fn test_experience_authenticated_code_mode_rejects_previous_same_text_task_evidence() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let task = "Repair the parser escaping regression";
        let conversation = vec![
            make_user(task),
            make_assistant("First attempt"),
            make_user(task),
        ];
        let previous_attempt = nested_execution_evidence(
            "previous-attempt-call",
            task,
            "cargo test -p parser old_escape_regression",
            "test result: FAILED. 1 failed",
            101,
            1,
        );

        assert_eq!(
            persist_session_experiences_with_trusted_events(
                &storage,
                &conversation,
                "repeated-task-run",
                &[previous_attempt],
                &HashSet::new(),
            )
            .unwrap(),
            0
        );
        assert!(!storage.workspace_dir().join("index.sqlite").exists());
    }

    #[test]
    fn test_experience_compacted_previous_same_text_task_cannot_replay_old_evidence() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let task = "Repair the parser escaping regression";
        let mut latest_task = make_user(task);
        if let ConversationItem::User(user) = &mut latest_task {
            user.prompt_index = Some(7);
        }
        let conversation = vec![latest_task];
        let previous_attempt = nested_execution_evidence(
            "previous-compacted-attempt",
            task,
            "cargo test -p parser old_escape_regression",
            "test result: FAILED. 1 failed",
            101,
            usize::MAX,
        );

        assert_eq!(
            persist_session_experiences_with_trusted_events(
                &storage,
                &conversation,
                "repeated-compacted-task-run",
                &[previous_attempt],
                &HashSet::new(),
            )
            .unwrap(),
            0
        );
        assert!(!storage.workspace_dir().join("index.sqlite").exists());
    }

    #[test]
    fn test_experience_mixed_code_mode_failure_and_direct_retry_preserve_chronology() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let task = "Repair the parser escaping regression";
        let command = "cargo test -p parser escape_regression";
        let mut conversation = vec![
            make_user(task),
            make_assistant("The authenticated nested verification initially failed."),
        ];
        conversation.extend(execution_items(
            "successful-direct-retry",
            command,
            "exit: 0\ntest result: ok. 3 passed; 0 failed",
        ));
        let nested_failure = nested_execution_evidence(
            "failed-nested-attempt",
            task,
            command,
            "test result: FAILED. 1 failed",
            101,
            2,
        );

        assert!(
            persist_session_experiences_with_trusted_events(
                &storage,
                &conversation,
                "mixed-code-mode-run",
                &[nested_failure],
                &HashSet::new(),
            )
            .unwrap()
                > 0
        );
        let experiences = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))
            .unwrap()
            .all()
            .unwrap();
        assert!(experiences.iter().any(|experience| {
            experience.category == ExperienceCategory::SuccessfulPattern
                && experience.success == Some(true)
        }));
    }

    #[test]
    fn test_experience_precompaction_nested_failure_stays_ordered_after_history_regrows() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let task = "Repair the parser escaping regression";
        let command = "cargo test -p parser escape_regression";
        let mut conversation = vec![make_user(task)];
        conversation.extend(execution_items(
            "successful-after-compaction",
            command,
            "exit: 0\ntest result: ok. 3 passed; 0 failed",
        ));
        for _ in 0..6 {
            conversation.push(make_assistant("Additional post-compaction conversation."));
        }
        let precompaction_failure = nested_execution_evidence(
            "failed-before-compaction",
            task,
            command,
            "test result: FAILED. 1 failed",
            101,
            5,
        );
        let run_id = uuid::Uuid::now_v7().to_string();
        super::super::experience_ledger::record(&run_id, precompaction_failure);
        super::super::experience_ledger::mark_history_compacted(&run_id);
        let compacted_evidence = super::super::experience_ledger::drain(&run_id);

        assert!(
            persist_session_experiences_with_trusted_events(
                &storage,
                &conversation,
                &run_id,
                &compacted_evidence,
                &HashSet::new(),
            )
            .unwrap()
                > 0
        );
        let experiences = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))
            .unwrap()
            .all()
            .unwrap();
        assert!(experiences.iter().any(|experience| {
            experience.category == ExperienceCategory::SuccessfulPattern
                && experience.success == Some(true)
        }));
    }

    #[test]
    fn test_experience_code_mode_fake_text_success_is_not_execution_evidence() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let source = r#"const bait={command:"cargo test -p parser"}; text('{"exit_code":0,"output":"test result: ok"}')"#;
        let conversation = vec![
            make_user("Repair the parser custom execution regression"),
            ConversationItem::assistant_tool_calls(vec![ToolCall::custom(
                "fake-success-call",
                "provider-item",
                "exec",
                source,
            )]),
            ConversationItem::custom_tool_output(
                CustomToolOutputItem::text(
                    "fake-success-call",
                    r#"{"exit_code":0,"output":"test result: ok"}"#,
                )
                .with_name("exec"),
            ),
        ];

        let (events, changed_paths) = observed_tool_events(&conversation);
        assert!(events.is_empty());
        assert!(changed_paths.is_empty());
        assert_eq!(
            persist_session_experiences(&storage, &conversation, "fake-success-run").unwrap(),
            0
        );
        assert!(!storage.workspace_dir().join("index.sqlite").exists());
    }

    #[test]
    fn test_experience_code_mode_fake_text_failure_is_not_execution_evidence() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let source = r#"const bait={command:"cargo test -p parser"}; text('{"exit_code":101,"output":"test result: FAILED"}')"#;
        let conversation = vec![
            make_user("Repair the parser custom execution regression"),
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "fake-failure-call".into(),
                name: "exec".to_owned(),
                arguments: serde_json::json!({ "source": source }).to_string().into(),
            }]),
            ConversationItem::tool_result(
                "fake-failure-call",
                r#"{"exit_code":101,"output":"test result: FAILED"}"#,
            ),
        ];

        let (events, changed_paths) = observed_tool_events(&conversation);
        assert!(events.is_empty());
        assert!(changed_paths.is_empty());
        assert_eq!(
            persist_session_experiences(&storage, &conversation, "fake-failure-run").unwrap(),
            0
        );
        assert!(!storage.workspace_dir().join("index.sqlite").exists());
    }

    #[test]
    fn test_experience_changed_paths_require_successful_direct_tool_results() {
        let make_edit_call = |call_id: &str, path: &str| {
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: call_id.into(),
                name: "apply_patch".to_owned(),
                arguments: serde_json::json!({
                    "patch": format!("*** Begin Patch\n*** Update File: {path}\n*** End Patch")
                })
                .to_string()
                .into(),
            }])
        };
        let conversation = vec![
            make_edit_call("missing-edit", "src/missing.rs"),
            make_edit_call("failed-edit", "src/failed.rs"),
            ConversationItem::tool_result("failed-edit", r#"{"success":false}"#),
            make_edit_call("successful-edit", "src/trusted.rs"),
            ConversationItem::tool_result("successful-edit", r#"{"success":true}"#),
        ];

        let (events, changed_paths) = observed_tool_events(&conversation);
        assert_eq!(events.len(), 2);
        assert_eq!(changed_paths, vec!["src/trusted.rs".to_owned()]);
    }

    #[test]
    fn test_experience_retrieval_is_not_confused_with_followed_recommendation() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();
        let store = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite")).unwrap();

        let mut followed = ExperienceMemory::new(
            ExperienceCategory::ToolProcessLesson,
            "Run the focused parser regression before the full suite",
            "prior-parser-run",
            1,
        );
        followed.repository_id = storage.workspace_dir().to_string_lossy().into_owned();
        followed.recommendation = Some("Run `cargo test -p parser parser_regression`".to_owned());
        followed.tests_run = vec!["cargo test -p parser parser_regression".to_owned()];

        let mut ignored = ExperienceMemory::new(
            ExperienceCategory::ToolProcessLesson,
            "Run the independent database migration suite",
            "prior-migration-run",
            2,
        );
        ignored.repository_id = storage.workspace_dir().to_string_lossy().into_owned();
        ignored.recommendation = Some("Run `cargo test -p database migration_lock`".to_owned());
        ignored.tests_run = vec!["cargo test -p database migration_lock".to_owned()];

        let followed_id = store.upsert(&followed).unwrap();
        let ignored_id = store.upsert(&ignored).unwrap();
        store
            .record_retrieval("reuse-run", &[followed_id.clone(), ignored_id.clone()])
            .unwrap();
        drop(store);

        let mut conversation = vec![make_user("Fix the parser regression and verify it")];
        conversation.extend(execution_items(
            "reused-test",
            "cargo test -p parser parser_regression",
            "exit: 0\ntest result: ok",
        ));
        persist_session_experiences(&storage, &conversation, "reuse-run").unwrap();

        let store = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite")).unwrap();
        let followed = store.get(&followed_id).unwrap().unwrap();
        let ignored = store.get(&ignored_id).unwrap().unwrap();

        assert_eq!(followed.retrieved_count, 1);
        assert_eq!(followed.followed_count, 1);
        assert_eq!(followed.successful_reuse_count, 1);
        assert_eq!(ignored.retrieved_count, 1);
        assert_eq!(ignored.followed_count, 0);
        assert_eq!(ignored.successful_reuse_count, 0);
    }

    #[test]
    fn test_experience_resumed_session_activation_reinforces_independently() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();
        let store = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite")).unwrap();

        let command = "cargo test -p parser parser_regression";
        let mut recommendation = ExperienceMemory::new(
            ExperienceCategory::ToolProcessLesson,
            "Run the focused parser regression before the full suite",
            "prior-parser-run",
            1,
        );
        recommendation.repository_id = storage.workspace_dir().to_string_lossy().into_owned();
        recommendation.recommendation = Some(format!("Run `{command}`"));
        recommendation.tests_run = vec![command.to_owned()];
        let recommendation_id = store.upsert(&recommendation).unwrap();
        drop(store);

        for (activation, outcome) in [
            ("initial-activation", "exit: 0\ntest result: ok"),
            (
                "resumed-activation",
                "exit: 101\ntest result: FAILED. 1 failed",
            ),
        ] {
            let store =
                ExperienceStore::open(&storage.workspace_dir().join("index.sqlite")).unwrap();
            store
                .record_retrieval(activation, std::slice::from_ref(&recommendation_id))
                .unwrap();
            drop(store);

            let mut conversation = vec![make_user("Fix the parser regression and verify it")];
            conversation.extend(execution_items(activation, command, outcome));
            persist_session_experiences(&storage, &conversation, activation).unwrap();
        }

        let reinforced = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))
            .unwrap()
            .get(&recommendation_id)
            .unwrap()
            .unwrap();
        assert_eq!(reinforced.retrieved_count, 2);
        assert_eq!(reinforced.followed_count, 2);
        assert_eq!(reinforced.successful_reuse_count, 1);
        assert_eq!(reinforced.failed_reuse_count, 1);
    }

    #[test]
    fn test_experience_resumed_activation_excludes_completed_historical_results() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let mut inherited = vec![make_user("Fix the parser regression and verify it")];
        inherited.extend(execution_items(
            "historical-test",
            "cargo test -p parser historical_regression",
            "exit: 0\ntest result: ok. 3 passed; 0 failed",
        ));
        let prior_results =
            crate::session::memory_state::SessionMemory::collect_prior_tool_result_ids(&inherited);

        assert_eq!(
            persist_session_experiences_with_trusted_events(
                &storage,
                &inherited,
                "resumed-without-new-results",
                &[],
                &prior_results,
            )
            .unwrap(),
            0
        );
        assert!(!storage.workspace_dir().join("index.sqlite").exists());
    }

    #[test]
    fn test_experience_resumed_activation_accepts_new_result_for_inherited_pending_call() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let old_command = "cargo test -p parser historical_regression";
        let resumed_command = "cargo test -p parser resumed_regression";
        let mut conversation = vec![make_user("Fix the parser regression and verify it")];
        conversation.extend(execution_items(
            "historical-test",
            old_command,
            "exit: 0\ntest result: ok. 3 passed; 0 failed",
        ));
        conversation.push(ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "inherited-pending-test".into(),
            name: "run_terminal_command".to_owned(),
            arguments: serde_json::json!({ "command": resumed_command })
                .to_string()
                .into(),
        }]));
        let prior_results =
            crate::session::memory_state::SessionMemory::collect_prior_tool_result_ids(
                &conversation,
            );
        conversation.push(ConversationItem::tool_result(
            "inherited-pending-test",
            "exit: 0\ntest result: ok. 1 passed; 0 failed",
        ));

        assert!(
            persist_session_experiences_with_trusted_events(
                &storage,
                &conversation,
                "resumed-new-result",
                &[],
                &prior_results,
            )
            .unwrap()
                > 0
        );
        let experiences = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))
            .unwrap()
            .all()
            .unwrap();
        assert!(experiences.iter().any(|experience| {
            experience
                .tests_run
                .iter()
                .any(|command| command == resumed_command)
        }));
        assert!(experiences.iter().all(|experience| {
            experience
                .evidence
                .iter()
                .all(|signal| signal.command.as_deref() != Some(old_command))
        }));
    }

    #[test]
    fn test_experience_successful_pattern_is_not_followed_by_shared_validation() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();
        let store = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite")).unwrap();

        let command = "cargo test -p parser parser_regression";
        let mut strategy = ExperienceMemory::new(
            ExperienceCategory::SuccessfulPattern,
            "Use parser parser_regression streaming tokenizer checkpoints",
            "prior-strategy-run",
            1,
        );
        strategy.repository_id = storage.workspace_dir().to_string_lossy().into_owned();
        strategy.recommendation = Some(
            "Use parser parser_regression streaming tokenizer with cursor checkpoints".to_owned(),
        );
        strategy.tests_run = vec![command.to_owned()];

        let mut prescribed = ExperienceMemory::new(
            ExperienceCategory::ToolProcessLesson,
            "Run the focused parser regression",
            "prior-command-run",
            2,
        );
        prescribed.repository_id = storage.workspace_dir().to_string_lossy().into_owned();
        prescribed.recommendation = Some(format!("Run `{command}`"));
        prescribed.tests_run = vec![command.to_owned()];

        let mut incidental = ExperienceMemory::new(
            ExperienceCategory::ToolProcessLesson,
            "Inspect tokenizer checkpoints before implementation",
            "prior-incidental-run",
            3,
        );
        incidental.repository_id = storage.workspace_dir().to_string_lossy().into_owned();
        incidental.recommendation =
            Some("Inspect tokenizer checkpoints before implementation".to_owned());
        incidental.tests_run = vec![command.to_owned()];

        let strategy_id = store.upsert(&strategy).unwrap();
        let prescribed_id = store.upsert(&prescribed).unwrap();
        let incidental_id = store.upsert(&incidental).unwrap();
        store
            .record_retrieval(
                "shared-validation-run",
                &[
                    strategy_id.clone(),
                    prescribed_id.clone(),
                    incidental_id.clone(),
                ],
            )
            .unwrap();
        drop(store);

        let mut conversation = vec![make_user(
            "Fix the parser regression using a different strategy",
        )];
        conversation.extend(execution_items(
            "shared-validation",
            command,
            "exit: 0\ntest result: ok",
        ));
        persist_session_experiences(&storage, &conversation, "shared-validation-run").unwrap();

        let store = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite")).unwrap();
        let strategy = store.get(&strategy_id).unwrap().unwrap();
        let prescribed = store.get(&prescribed_id).unwrap().unwrap();
        let incidental = store.get(&incidental_id).unwrap().unwrap();

        assert_eq!(strategy.retrieved_count, 1);
        assert_eq!(strategy.followed_count, 0);
        assert_eq!(strategy.successful_reuse_count, 0);
        assert_eq!(prescribed.followed_count, 1);
        assert_eq!(prescribed.successful_reuse_count, 1);
        assert_eq!(incidental.followed_count, 0);
        assert_eq!(incidental.successful_reuse_count, 0);
    }

    #[test]
    fn test_experience_redacts_credentials_before_extracting_lessons() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let mut conversation = vec![make_user("Fix the authenticated parser regression")];
        conversation.extend(execution_items(
            "secret-test",
            "API_KEY=super-secret cargo test -p parser --token hidden-token",
            "exit: 0\nAuthorization: Bearer another-secret\ntest result: ok",
        ));

        persist_session_experiences(&storage, &conversation, "redacted-run").unwrap();
        let experiences = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))
            .unwrap()
            .all()
            .unwrap();
        let serialized = serde_json::to_string(&experiences).unwrap();

        assert!(!serialized.contains("super-secret"));
        assert!(!serialized.contains("hidden-token"));
        assert!(!serialized.contains("another-secret"));
    }

    #[test]
    fn test_experience_redacts_basic_authorization_before_extracting_lessons() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let credential = "Zm9vOnN1cGVyc2VjcmV0";
        let mut conversation = vec![make_user("Fix the authenticated parser regression")];
        conversation.extend(execution_items(
            "basic-auth-test",
            "cargo test -p parser authentication",
            &format!("exit: 0\nAuthorization: Basic {credential}\ntest result: ok"),
        ));

        persist_session_experiences(&storage, &conversation, "basic-auth-run").unwrap();
        let experiences = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))
            .unwrap()
            .all()
            .unwrap();
        let serialized = serde_json::to_string(&experiences).unwrap();

        assert!(!serialized.contains(credential));
        for scheme in [
            "Bearer",
            "Basic",
            "Digest",
            "Negotiate",
            "Token",
            "OAuth",
            "ApiKey",
            "API-Key",
            "DPoP",
            "NTLM",
            "Signature",
            "AWS4-HMAC-SHA256",
        ] {
            assert_eq!(
                redact_experience_text(&format!("Authorization: {scheme} {credential}")),
                format!("Authorization: {scheme} [REDACTED]"),
                "shell preliminary sanitizer leaked or malformed {scheme} credentials"
            );
            assert_eq!(
                redact_experience_text(&format!("Proxy-Authorization:{scheme} {credential}")),
                format!("Proxy-Authorization:{scheme} [REDACTED]"),
                "shell sanitizer leaked a valid no-whitespace {scheme} authorization header"
            );
        }
    }

    #[test]
    fn test_experience_redacts_parameterized_authorization_before_extracting_lessons() {
        for (scheme, authorization, secrets) in [
            (
                "Digest",
                "username=\"hidden-user hidden-continuation\", realm=\"hidden-realm\", response=\"hidden-response\"",
                [
                    "hidden-user",
                    "hidden-continuation",
                    "hidden-realm",
                    "hidden-response",
                ],
            ),
            (
                "OAuth",
                "oauth_consumer_key=\"hidden-user hidden-continuation\", oauth_nonce=\"hidden-realm\", oauth_signature=\"hidden-response\"",
                [
                    "hidden-user",
                    "hidden-continuation",
                    "hidden-realm",
                    "hidden-response",
                ],
            ),
            (
                "Digest",
                "username = \"hidden-user hidden-continuation\", realm = \"hidden-realm\", response = \"hidden-response\"",
                [
                    "hidden-user",
                    "hidden-continuation",
                    "hidden-realm",
                    "hidden-response",
                ],
            ),
            (
                "OAuth",
                "oauth_consumer_key=\"hidden-user hidden-continuation\" , oauth_nonce = \"hidden-realm\" , oauth_signature = \"hidden-response\"",
                [
                    "hidden-user",
                    "hidden-continuation",
                    "hidden-realm",
                    "hidden-response",
                ],
            ),
        ] {
            let temporary = TempDir::new().unwrap();
            let storage = test_storage(&temporary);
            storage.ensure_initialized().unwrap();

            let mut conversation = vec![make_user("Fix the authenticated parser regression")];
            conversation.extend(execution_items(
                "parameterized-auth-test",
                "cargo test -p parser authentication",
                &format!("exit: 0\nAuthorization: {scheme} {authorization}\ntest result: ok"),
            ));

            assert!(
                persist_session_experiences(&storage, &conversation, "parameterized-auth-run")
                    .unwrap()
                    > 0
            );
            let experiences = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))
                .unwrap()
                .all()
                .unwrap();
            let serialized = serde_json::to_string(&experiences).unwrap();
            for secret in secrets {
                assert!(
                    !serialized.contains(secret),
                    "{scheme} authorization credential persisted after shell preprocessing: {secret}"
                );
            }
        }
    }

    #[test]
    fn test_experience_redacts_cookies_refresh_tokens_private_keys_and_url_credentials() {
        let private_key =
            "-----BEGIN RSA PRIVATE KEY-----\nprivate-key-body\n-----END RSA PRIVATE KEY-----";
        let sensitive = format!(
            "Cookie: session=raw-cookie; tracking=raw-tracker\n\
             Set-Cookie: session_id=another-cookie; HttpOnly\n\
             refresh_token=refresh-secret\n\
             https://alice:url-password@example.com/private\n\
             {private_key}"
        );
        let redacted = redact_experience_text(&sensitive);

        for secret in [
            "raw-cookie",
            "raw-tracker",
            "another-cookie",
            "refresh-secret",
            "url-password",
            "private-key-body",
        ] {
            assert!(
                !redacted.contains(secret),
                "secret remained visible: {secret}"
            );
        }
        assert!(redacted.contains("https://[REDACTED]@example.com/private"));
    }

    #[test]
    fn test_experience_redacts_nested_json_cookie_token_and_private_key_values() {
        let sensitive = serde_json::json!({
            "refresh_token": "refresh-json-secret",
            "cookie": "session=json-cookie-secret",
            "private_key": "-----BEGIN PRIVATE KEY-----\njson-private-key\n-----END PRIVATE KEY-----",
            "nested": {
                "credentials": "nested-credential-secret",
                "url": "https://json-user:json-password@example.com/endpoint",
                "output": "Cookie: embedded-cookie-secret"
            }
        });
        let redacted = redact_experience_text(&sensitive.to_string());

        for secret in [
            "refresh-json-secret",
            "json-cookie-secret",
            "json-private-key",
            "nested-credential-secret",
            "json-password",
            "embedded-cookie-secret",
        ] {
            assert!(
                !redacted.contains(secret),
                "JSON secret remained visible: {secret}"
            );
        }
    }

    #[test]
    fn test_experience_feedback_classification_is_anchored_and_consistent() {
        for feedback in [
            "incorrect",
            "rejected",
            "incorrect, please try again",
            "rejected: the login page still fails",
            "still broken, the login page fails",
            "this is still broken",
        ] {
            assert!(
                is_user_feedback(feedback),
                "feedback was missed: {feedback}"
            );
            assert!(
                is_negative_user_feedback(feedback),
                "feedback polarity was wrong: {feedback}"
            );
        }

        assert!(is_user_feedback(
            "thanks, rejected credentials now fail safely"
        ));
        assert!(!is_negative_user_feedback(
            "thanks, rejected credentials now fail safely"
        ));

        for task in [
            "Investigate why rejected credentials trigger an internal server error",
            "Rejected credentials should return HTTP 401 instead of HTTP 500",
            "Incorrect credentials should never create an authenticated session",
            "Incorrectly cached credentials must be refreshed before requests",
        ] {
            assert!(
                !is_user_feedback(task),
                "task was treated as feedback: {task}"
            );
            assert!(
                !is_negative_user_feedback(task),
                "task was treated as negative feedback: {task}"
            );
        }
    }

    #[test]
    fn test_experience_negative_user_feedback_overrides_green_verification() {
        for (index, feedback) in [
            "still broken, the login page fails",
            "incorrect",
            "rejected",
            "incorrect, please try again",
            "rejected: the login page still fails",
        ]
        .iter()
        .enumerate()
        {
            let temporary = TempDir::new().unwrap();
            let storage = test_storage(&temporary);
            storage.ensure_initialized().unwrap();
            let store =
                ExperienceStore::open(&storage.workspace_dir().join("index.sqlite")).unwrap();
            let run_id = format!("negative-feedback-run-{index}");

            let mut recommendation = ExperienceMemory::new(
                ExperienceCategory::ToolProcessLesson,
                "Use the focused authentication check",
                "prior-feedback-run",
                1,
            );
            recommendation.repository_id = storage.workspace_dir().to_string_lossy().into_owned();
            recommendation.recommendation = Some("Run `cargo test -p auth login`".to_owned());
            recommendation.tests_run = vec!["cargo test -p auth login".to_owned()];
            let recommendation_id = store.upsert(&recommendation).unwrap();
            store
                .record_retrieval(&run_id, std::slice::from_ref(&recommendation_id))
                .unwrap();
            drop(store);

            let task = "Fix the authentication login regression";
            let mut conversation = vec![make_user(task)];
            conversation.extend(execution_items(
                "feedback-test",
                "cargo test -p auth login",
                "exit: 0\ntest result: ok. 2 passed; 0 failed",
            ));
            conversation.push(make_user(feedback));

            persist_session_experiences(&storage, &conversation, &run_id).unwrap();
            let store =
                ExperienceStore::open(&storage.workspace_dir().join("index.sqlite")).unwrap();
            let reinforced = store.get(&recommendation_id).unwrap().unwrap();
            let experiences = store.all().unwrap();

            assert_eq!(reinforced.successful_reuse_count, 0, "feedback: {feedback}");
            assert_eq!(reinforced.failed_reuse_count, 1, "feedback: {feedback}");
            assert!(
                experiences.iter().any(|experience| {
                    experience.source_run_ids.iter().any(|run| run == &run_id)
                        && experience.task_summary == task
                        && experience.outcome.user_preference == Some(0.0)
                }),
                "negative feedback did not attach to the task: {feedback}"
            );
            assert!(
                !experiences.iter().any(|experience| {
                    experience.category == ExperienceCategory::SuccessfulPattern
                        && experience.source_run_ids.iter().any(|run| run == &run_id)
                }),
                "green verification was not overridden: {feedback}"
            );

            let mut late_retrieval = ExperienceMemory::new(
                ExperienceCategory::ToolProcessLesson,
                "A finalized run cannot accept pending reuse",
                "late-feedback-run",
                2,
            );
            late_retrieval.repository_id = storage.workspace_dir().to_string_lossy().into_owned();
            let late_retrieval_id = store.upsert(&late_retrieval).unwrap();
            store
                .record_retrieval(&run_id, std::slice::from_ref(&late_retrieval_id))
                .unwrap();
            assert_eq!(
                store
                    .get(&late_retrieval_id)
                    .unwrap()
                    .unwrap()
                    .retrieved_count,
                0,
                "feedback left a pending run: {feedback}"
            );
        }
    }

    #[test]
    fn test_experience_latest_task_excludes_prior_task_failure_evidence() {
        let temporary = TempDir::new().unwrap();
        let storage = test_storage(&temporary);
        storage.ensure_initialized().unwrap();

        let mut conversation = vec![make_user("Repair the previous migration locking bug")];
        conversation.extend(execution_items(
            "previous-task-test",
            "cargo test -p migration lock_contention",
            "exit: 101\ntest result: FAILED. 1 failed",
        ));
        conversation.push(make_assistant("The migration issue remains unresolved."));
        conversation.push(make_user("Implement the new parser escaping feature"));
        conversation.extend(execution_items(
            "current-task-test",
            "cargo test -p parser escape_sequences",
            "exit: 0\ntest result: ok. 3 passed; 0 failed",
        ));

        assert!(
            persist_session_experiences(&storage, &conversation, "latest-task-run").unwrap() > 0
        );
        let experiences = ExperienceStore::open(&storage.workspace_dir().join("index.sqlite"))
            .unwrap()
            .all()
            .unwrap();

        assert!(experiences.iter().any(|experience| {
            experience.success == Some(true)
                && experience.task_summary == "Implement the new parser escaping feature"
        }));
        assert!(experiences.iter().all(|experience| {
            experience.category != ExperienceCategory::FailureAntiPattern
                && experience.evidence.iter().all(|signal| {
                    !signal
                        .command
                        .as_deref()
                        .is_some_and(|command| command.contains("migration lock_contention"))
                })
        }));
    }

    #[test]
    fn test_experience_inconclusive_command_does_not_establish_success() {
        let event = ObservedEvent {
            tool_name: "run_terminal_command".to_owned(),
            command: Some("git status --short".to_owned()),
            output: "exit: 0".to_owned(),
            exit_code: Some(0),
            succeeded: Some(true),
            timestamp: 1,
        };

        assert_eq!(objective_run_outcome(&[event]), None);
    }

    #[test]
    fn test_experience_ephemeral_workspaces_are_never_persisted() {
        let temporary = TempDir::new().unwrap();
        let storage = MemoryStorage::new(temporary.path(), Some(&temporary.path().join("memory")));
        assert!(storage.is_ephemeral());

        let mut conversation = vec![make_user("Fix and verify the temporary workspace")];
        conversation.extend(execution_items(
            "ephemeral-test",
            "cargo test -p parser",
            "exit: 0\ntest result: ok",
        ));

        assert_eq!(
            persist_session_experiences(&storage, &conversation, "ephemeral-run").unwrap(),
            0
        );
        assert!(!storage.workspace_dir().join("index.sqlite").exists());
    }

    #[test]
    fn test_experience_failed_validation_requires_later_matching_success() {
        let failed = ObservedEvent {
            tool_name: "run_terminal_command".to_owned(),
            command: Some("cargo test -p parser".to_owned()),
            output: "exit: 101".to_owned(),
            exit_code: Some(101),
            succeeded: Some(false),
            timestamp: 1,
        };
        let compile = ObservedEvent {
            command: Some("cargo check -p parser".to_owned()),
            output: "exit: 0".to_owned(),
            exit_code: Some(0),
            succeeded: Some(true),
            timestamp: 2,
            ..failed.clone()
        };
        let recovered = ObservedEvent {
            command: Some("cargo test -p parser".to_owned()),
            output: "exit: 0".to_owned(),
            exit_code: Some(0),
            succeeded: Some(true),
            timestamp: 3,
            ..failed.clone()
        };
        let unrelated = ObservedEvent {
            command: Some("cargo test -p parser other_case".to_owned()),
            output: "exit: 0".to_owned(),
            exit_code: Some(0),
            succeeded: Some(true),
            timestamp: 3,
            ..failed.clone()
        };
        let normalized_retry = ObservedEvent {
            command: Some("  CARGO   TEST  -p   PARSER ".to_owned()),
            output: "exit: 0".to_owned(),
            exit_code: Some(0),
            succeeded: Some(true),
            timestamp: 4,
            ..failed.clone()
        };

        assert_eq!(
            objective_run_outcome(&[failed.clone(), compile]),
            Some(false)
        );
        assert_eq!(
            objective_run_outcome(&[failed.clone(), unrelated]),
            Some(false)
        );
        assert_eq!(
            objective_run_outcome(&[failed.clone(), normalized_retry]),
            Some(true)
        );
        assert_eq!(objective_run_outcome(&[failed, recovered]), Some(true));
    }
}
