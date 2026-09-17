use std::time::Duration;

use xai_grok_tools::types::output::{ToolOutput as ToolsToolOutput, ToolRunResult};
use xai_interjection_core::InterjectionBuffer;
use xai_tool_types::{TaskOutputOutput, TaskOutputResult};

use crate::tools::tool_context::{BlockingWaitState, ParentInterjectSignal};

/// Why an interruptible wait tool was aborted before it produced its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WaitInterruptCause {
    HumanInterjection,
    ParentInterject,
}

const WAIT_INTERRUPT_POLL: Duration = Duration::from_millis(50);

/// Human first, then parent. Parent Interject uses a dedicated flag so the
/// follow-up is not also pushed into `pending_interjections`.
pub(super) async fn wait_for_wait_interrupt(
    human: &InterjectionBuffer<agent_client_protocol::ImageContent>,
    parent: &ParentInterjectSignal,
) -> WaitInterruptCause {
    loop {
        if !human.is_empty() {
            return WaitInterruptCause::HumanInterjection;
        }
        if parent.is_pending() {
            return WaitInterruptCause::ParentInterject;
        }
        tokio::time::sleep(WAIT_INTERRUPT_POLL).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InterruptedWaitFilter {
    Rewritten { kept: Vec<String>, requested: usize },
    Unchanged,
}

pub(super) fn wait_task_ids_from_args(args: &serde_json::Value) -> Vec<String> {
    let raw = args
        .get("task_ids")
        .or_else(|| args.get("task_id"))
        .and_then(xai_tool_types::serde_lenient::lenient_string_list_from_json)
        .unwrap_or_default();
    xai_tool_types::resolve_task_ids(&raw)
}

fn wait_ids_after_interrupt(interrupted: &[String], requested: &[String]) -> Option<Vec<String>> {
    if interrupted.is_empty() || requested.is_empty() {
        return None;
    }
    let extra = requested.iter().any(|id| !interrupted.contains(id));
    let missing = interrupted.iter().any(|id| !requested.contains(id));
    if extra && !missing {
        Some(interrupted.to_vec())
    } else {
        None
    }
}

fn write_rewritten_task_ids(parsed_args: &mut serde_json::Value, kept: &[String]) {
    if let Some(obj) = parsed_args.as_object_mut() {
        obj.insert("task_ids".to_string(), serde_json::json!(kept));
        obj.remove("task_id");
    }
}

pub(super) fn apply_interrupted_wait_filter(
    state: &BlockingWaitState,
    parsed_args: &mut serde_json::Value,
) -> InterruptedWaitFilter {
    let requested = wait_task_ids_from_args(parsed_args);
    if requested.is_empty() {
        return InterruptedWaitFilter::Unchanged;
    }
    state
        .update_interrupted_wait(state.generation(), |remembered| {
            let Some(interrupted) = remembered.as_ref() else {
                return InterruptedWaitFilter::Unchanged;
            };
            match wait_ids_after_interrupt(interrupted, &requested) {
                Some(kept) => {
                    write_rewritten_task_ids(parsed_args, &kept);
                    InterruptedWaitFilter::Rewritten {
                        requested: requested.len(),
                        kept,
                    }
                }
                None => InterruptedWaitFilter::Unchanged,
            }
        })
        .unwrap_or(InterruptedWaitFilter::Unchanged)
}

fn is_terminal_wait_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "timed_out" | "not_found"
    )
}

pub(super) fn finished_wait_ids(result: &ToolRunResult) -> Vec<String> {
    match &result.output {
        ToolsToolOutput::TaskOutput(TaskOutputOutput::Result(res))
            if is_terminal_wait_status(&res.status) && !res.task_id.is_empty() =>
        {
            vec![res.task_id.clone()]
        }
        ToolsToolOutput::TaskOutput(TaskOutputOutput::TaskNotFound(id)) if !id.is_empty() => {
            vec![id.clone()]
        }
        ToolsToolOutput::TaskOutput(TaskOutputOutput::MultiResult(multi)) => multi
            .results
            .iter()
            .filter(|res| is_terminal_wait_status(&res.status) && !res.task_id.is_empty())
            .map(|res| res.task_id.clone())
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn record_interruptible_wait_outcome(
    state: &BlockingWaitState,
    generation: u64,
    waited_ids: Vec<String>,
    aborted: bool,
    finished_ids: &[String],
) {
    if waited_ids.is_empty() && finished_ids.is_empty() {
        return;
    }
    let _ = state.update_interrupted_wait(generation, |remembered| {
        if aborted {
            match remembered {
                Some(existing) => {
                    for id in waited_ids {
                        if !existing.contains(&id) {
                            existing.push(id);
                        }
                    }
                }
                None => *remembered = Some(waited_ids),
            }
        } else if let Some(existing) = remembered {
            existing.retain(|id| !finished_ids.contains(id));
            if existing.is_empty() {
                *remembered = None;
            }
        }
    });
}

pub(super) const WAIT_INTERRUPTED_HEAD: &str = "Wait interrupted: the user sent a message.";

/// Sender-neutral: the parent lane also carries a human's message relayed through the parent.
pub(super) const WAIT_INTERRUPTED_BY_AGENT_MESSAGE_HEAD: &str =
    "Wait interrupted: an agent message is pending.";

pub(super) fn interrupted_wait_prompt(has_ids: bool, cause: WaitInterruptCause) -> String {
    let head = match cause {
        WaitInterruptCause::HumanInterjection => WAIT_INTERRUPTED_HEAD,
        WaitInterruptCause::ParentInterject => WAIT_INTERRUPTED_BY_AGENT_MESSAGE_HEAD,
    };
    if !has_ids {
        return head.to_owned();
    }
    format!(
        "{head}\n\n\
         If you choose to wait on these tasks again, resume with the called task_ids only. \
         Newly spawned background work auto-wakes when it finishes and doesn't need to be added to this wait."
    )
}

pub(super) fn interrupted_wait_tool_result(
    args: &serde_json::Value,
    cause: WaitInterruptCause,
) -> ToolRunResult {
    let ids = wait_task_ids_from_args(args);
    let msg = interrupted_wait_prompt(!ids.is_empty(), cause);
    let task_id = ids.first().cloned().unwrap_or_default();
    let result = TaskOutputResult {
        task_id,
        command: String::new(),
        status: "cancelled".to_string(),
        exit_code: None,
        started: String::new(),
        ended: None,
        duration_secs: 0.0,
        output: msg.clone(),
        output_file: String::new(),
        truncated: false,
        truncation_hint: String::new(),
        raw_output_bytes: msg.len(),
    };
    ToolRunResult {
        output: ToolsToolOutput::TaskOutput(TaskOutputOutput::Result(result)),
        prompt_text: msg,
        effective_tool_name: None,
    }
}

#[cfg(test)]
#[path = "wait_interrupt_tests.rs"]
mod wait_interrupt_tests;
