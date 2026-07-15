pub mod command;
pub mod http;

use std::time::Duration;

use crate::config::HookSpec;
use crate::event::HookEventEnvelope;
use crate::result::{HookDecision, HttpInfo};

/// Context passed to any hook runner for environment setup.
pub struct RunContext<'a> {
    pub session_id: &'a str,
    pub workspace_root: &'a str,
}

/// Result of running a single hook (any handler type).
#[derive(Debug)]
pub enum HookRunnerResult {
    /// Hook ran and produced a decision (for blocking hooks).
    Decision(HookDecision),
    /// Hook ran successfully (for non-blocking hooks).
    Success,
    /// Hook failed — caller should fail-open.
    Failed(String),
}

/// Bundle returned by each runner: the result, wall-clock duration, and
/// optional HTTP metadata for enriched scrollback logging.
pub type HookRunOutput = (HookRunnerResult, Duration, Option<HttpInfo>);

/// Run a hook using the appropriate handler for its type.
///
/// Dispatches to `command::run_command_hook()` or `http::run_http_hook()`
/// based on `spec.handler_type`. Returns the result, elapsed duration, and
/// optional HTTP metadata for scrollback enrichment.
pub async fn run_hook(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
    is_blocking: bool,
) -> HookRunOutput {
    match spec.handler_type.as_str() {
        "command" => {
            let (result, elapsed) =
                command::run_command_hook(spec, envelope, ctx, is_blocking).await;
            (result, elapsed, None)
        }
        "http" => http::run_http_hook(spec, envelope, ctx, is_blocking).await,
        _ => (
            HookRunnerResult::Failed(format!("unsupported handler type '{}'", spec.handler_type)),
            Duration::ZERO,
            None,
        ),
    }
}
