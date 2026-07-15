//! Client-registered hooks for [`SessionActor`].
//!
//! Hooks registered at `session/new` (`_meta["x.ai/hooks"]`) come in two flavors,
//! both matched by the agent ([`xai_grok_hooks::matcher::HookMatcher`], shared with
//! file hooks):
//! - **`PreToolUse` gate**: an awaited reverse *request* `x.ai/hooks/run`; a `deny`
//!   blocks the tool.
//! - **All other events**: fire-and-forget *notifications* `x.ai/hooks/event`,
//!   observe-only (the callback's return is ignored). Sent per matching callback.

use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol as acp;
use agent_client_protocol::Client as _;
use futures::stream::{FuturesUnordered, StreamExt as _};
use serde_json::value::RawValue;
use xai_grok_hooks::event::{HookEventEnvelope, HookEventName, HookPayload};
use xai_grok_telemetry::events::ClientHookGateOutcome;

use super::{SessionActor, ToolLoop};
use crate::extensions::hooks::{
    ClientHookDecision, ClientHookDispatch, ClientHookGroup, ClientHookResponse,
};
use crate::sampling::types::ToolCallResponse;

const HOOK_EVENT_METHOD: &str = "x.ai/hooks/event";
const HOOK_RUN_METHOD: &str = "x.ai/hooks/run";

/// Default per-callback bound for a client's `x.ai/hooks/run` reply; on timeout the gate
/// fails open (the tool proceeds).
///
/// Some external hosts default to 600s per hook; we default to 30s because our gate sits
/// in the interactive tool hot path (a hung hook would otherwise stall a tool call for
/// minutes). Hosts can override per group up to `MAX_HOOK_TIMEOUT_SECS` (300s). To match
/// a longer external default, change this value (and raise/remove the cap in
/// `extensions::hooks`).
const CLIENT_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Outcome of the `x.ai/hooks/run` reverse request, before interpreting it as a
/// decision. Separate so [`classify`] stays pure and unit-testable.
enum ReverseOutcome {
    Responded(Arc<RawValue>),
    Transport(acp::Error),
    Timeout,
}

/// Map a reverse-request outcome to a decision. Malformed / transport / timeout
/// all fail open (the `Default` response = proceed).
fn classify(outcome: ReverseOutcome) -> (ClientHookResponse, ClientHookGateOutcome) {
    match outcome {
        ReverseOutcome::Responded(raw) => {
            match serde_json::from_str::<ClientHookResponse>(raw.get()) {
                Ok(resp) => {
                    // An unknown `decision` fails open like Continue, but is almost always a
                    // client bug (typo / version skew), so surface it rather than allow silently.
                    let label = match resp.decision {
                        ClientHookDecision::Deny => ClientHookGateOutcome::Denied,
                        ClientHookDecision::Other => {
                            tracing::warn!(
                                "x.ai/hooks/run returned an unknown decision value; failing open"
                            );
                            ClientHookGateOutcome::UnknownDecision
                        }
                        ClientHookDecision::Continue => ClientHookGateOutcome::Proceeded,
                    };
                    (resp, label)
                }
                Err(err) => {
                    tracing::warn!(%err, "malformed x.ai/hooks/run response; failing open");
                    (
                        ClientHookResponse::default(),
                        ClientHookGateOutcome::Malformed,
                    )
                }
            }
        }
        ReverseOutcome::Transport(err) => {
            tracing::warn!(%err, "x.ai/hooks/run transport error (no client wired?); failing open");
            (
                ClientHookResponse::default(),
                ClientHookGateOutcome::TransportError,
            )
        }
        ReverseOutcome::Timeout => {
            tracing::warn!("x.ai/hooks/run timed out; failing open");
            (
                ClientHookResponse::default(),
                ClientHookGateOutcome::TimedOut,
            )
        }
    }
}

/// Whether `group` fires for an event on `tool_name`. Mirrors the file-hook matcher rule
/// (dispatcher::dispatch_non_blocking): a group is skipped only when it has a matcher AND
/// there is a tool name AND the matcher doesn't match, so non-tool events
/// (`tool_name == None`) and matcher-less groups always fire.
fn group_matches(group: &ClientHookGroup, tool_name: Option<&str>) -> bool {
    match (group.matcher.as_ref(), tool_name) {
        (Some(matcher), Some(name)) => matcher.is_match(name),
        _ => true,
    }
}

/// Callback ids that fire for an event, in registration order.
fn matching_callback_ids<'a>(
    groups: &'a [ClientHookGroup],
    tool_name: Option<&str>,
) -> Vec<&'a str> {
    groups
        .iter()
        .filter(|group| group_matches(group, tool_name))
        .flat_map(|group| group.callback_ids.iter().map(String::as_str))
        .collect()
}

/// Serialize a [`ClientHookDispatch`] to reverse-RPC params, or `None` (logged) on the
/// should-never-happen serialization failure; callers then skip that callback (fail open)
/// rather than panic the session actor.
fn dispatch_params(dispatch: &ClientHookDispatch<'_>) -> Option<Arc<RawValue>> {
    serde_json::value::to_raw_value(dispatch)
        .inspect_err(|err| tracing::warn!(%err, "failed to serialize client hook dispatch"))
        .ok()
        .map(Into::into)
}

impl SessionActor {
    /// Build a [`HookEventEnvelope`] with this session's common fields filled (session id,
    /// cwd, workspace root, timestamp). Single source of truth for envelope shape; every
    /// fire site goes through here.
    pub(super) fn make_hook_envelope(
        &self,
        hook_event_name: HookEventName,
        prompt_id: Option<String>,
        payload: HookPayload,
    ) -> HookEventEnvelope {
        HookEventEnvelope {
            hook_event_name,
            session_id: self.session_id_string(),
            cwd: self.session_info.cwd.clone(),
            workspace_root: self.hook_workspace_root(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            transcript_path: self.get_transcript_path(),
            client_identifier: None,
            prompt_id,
            payload,
        }
    }

    /// Whether any hook would consume `event`: the on-disk file registry, or a registered
    /// client hook. Lets the hot path skip building/serializing a payload (e.g. a large tool
    /// output) when nothing is listening, so the feature stays inert when unused.
    pub(super) fn hook_event_active(&self, event: HookEventName) -> bool {
        self.hook_registry.borrow().is_some()
            || self.client_hooks.borrow().contains_key(&event.canonical())
    }

    /// Build the envelope for an observe-only event, fire observe client hooks for it, and
    /// return the envelope for any subsequent file-hook dispatch. One call so a fire site
    /// can't build the envelope but forget to notify.
    pub(super) fn fire_hook(
        &self,
        hook_event_name: HookEventName,
        prompt_id: Option<String>,
        payload: HookPayload,
    ) -> HookEventEnvelope {
        let envelope = self.make_hook_envelope(hook_event_name, prompt_id, payload);
        self.notify_client_hooks(&envelope);
        envelope
    }

    /// Block a tool call denied by a `PreToolUse` hook (file- or client-side),
    /// emitting the shared telemetry + UI side-effects and returning the
    /// [`ToolLoop::HookDenied`] the caller should propagate.
    pub(super) async fn deny_tool(
        &self,
        model_call_id: &str,
        tool_call_id: &acp::ToolCallId,
        tool_name: String,
        hook_name: String,
        reason: String,
    ) -> Result<ToolLoop, acp::Error> {
        tracing::info!(%tool_name, %hook_name, %reason, "tool call denied by pre_tool_use hook");
        xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::HookBlocked {
            hook_name: hook_name.clone(),
        });
        self.handle_tool_not_executed(
            model_call_id,
            tool_call_id,
            format!("Hook denied: {reason}"),
        )
        .await?;
        self.send_hook_annotation(&format!(
            "\u{26a0} `{tool_name}` blocked by hook `{hook_name}`: {reason}"
        ))
        .await;
        Ok(ToolLoop::HookDenied { hook_name })
    }

    /// Run the client-registered `PreToolUse` hooks for `call`, firing
    /// `x.ai/hooks/run` once per matching callback with the shared `envelope` (the
    /// same payload file hooks and observe events receive).
    ///
    /// Returns `Some(ToolLoop::HookDenied)` on the first deny, else `None`.
    /// Timeout, transport error, and malformed replies all fail open.
    pub(super) async fn run_pre_tool_use_client_hook(
        &self,
        call: &ToolCallResponse,
        tool_call_id: &acp::ToolCallId,
        envelope: &HookEventEnvelope,
    ) -> Result<Option<ToolLoop>, acp::Error> {
        // Clone the matched groups so we don't hold the `client_hooks` borrow across the
        // dispatch awaits below.
        let Some(groups) = self
            .client_hooks
            .borrow()
            .get(&HookEventName::PreToolUse)
            .cloned()
        else {
            return Ok(None);
        };
        // Match on the resolved target (in the envelope) so a client deny matcher
        // keyed on the real MCP tool gates a meta-dispatch call, matching the
        // observe path (`notify_client_hooks`). Equals `function.name` otherwise.
        let tool_name = xai_grok_hooks::dispatcher::extract_tool_name(envelope)
            .unwrap_or_else(|| call.function.name.clone());
        let tool_name = tool_name.as_str();

        // Dispatch every matching callback concurrently, each bounded by its group's
        // timeout (else `CLIENT_HOOK_TIMEOUT`), and act on the first deny. Independent
        // timeouts mean a slow or hung callback can't erode another's budget (so a later
        // deny can't be starved into a fail-open), and concurrency keeps total gate latency
        // bounded to ~one timeout regardless of count.
        let mut pending: FuturesUnordered<_> = groups
            .iter()
            .filter(|group| group_matches(group, Some(tool_name)))
            .flat_map(|group| {
                let timeout = group.timeout.unwrap_or(CLIENT_HOOK_TIMEOUT);
                group
                    .callback_ids
                    .iter()
                    .map(move |callback_id| (callback_id.as_str(), timeout))
            })
            .map(|(callback_id, timeout)| {
                let dispatch = ClientHookDispatch {
                    hook_callback_id: callback_id,
                    envelope,
                };
                async move {
                    let started = tokio::time::Instant::now();
                    let (response, gate_outcome) =
                        classify(self.send_hook_run(&dispatch, timeout).await);
                    xai_grok_telemetry::session_ctx::log_event(
                        xai_grok_telemetry::events::ClientHookGate {
                            callback_id: callback_id.to_string(),
                            tool_name: Some(tool_name.to_string()),
                            outcome: gate_outcome,
                            duration_ms: started.elapsed().as_millis() as u64,
                        },
                    );
                    (callback_id, response)
                }
            })
            .collect();

        while let Some((callback_id, response)) = pending.next().await {
            if response.decision == ClientHookDecision::Deny {
                let reason = response
                    .system_message
                    .unwrap_or_else(|| "blocked by client hook".to_string());
                return Ok(Some(
                    self.deny_tool(
                        &call.id,
                        tool_call_id,
                        tool_name.to_owned(),
                        // Name the specific callback so telemetry / the UI annotation can
                        // attribute the block, not collapse every client hook to "client".
                        format!("client:{callback_id}"),
                        reason,
                    )
                    .await?,
                ));
            }
        }
        Ok(None)
    }

    /// Issue one `x.ai/hooks/run` reverse request, bounded by a per-callback `timeout`.
    async fn send_hook_run(
        &self,
        dispatch: &ClientHookDispatch<'_>,
        timeout: Duration,
    ) -> ReverseOutcome {
        let Some(params) = dispatch_params(dispatch) else {
            return ReverseOutcome::Transport(acp::Error::internal_error());
        };
        let ext_request = acp::ExtRequest::new(HOOK_RUN_METHOD, params);
        match tokio::time::timeout(timeout, self.notifications.gateway.ext_method(ext_request))
            .await
        {
            Ok(Ok(raw)) => ReverseOutcome::Responded(raw.0),
            Ok(Err(err)) => ReverseOutcome::Transport(err),
            Err(_) => ReverseOutcome::Timeout,
        }
    }

    /// Fire observe-only client hooks for `envelope`'s event: send an
    /// `x.ai/hooks/event` notification to each matching registered callback.
    /// Fire-and-forget (no decision is consumed); independent of file hooks, so it
    /// runs even when no on-disk hook registry exists. No-op when nothing is registered.
    pub(super) fn notify_client_hooks(&self, envelope: &HookEventEnvelope) {
        let hooks = self.client_hooks.borrow();
        let Some(groups) = hooks.get(&envelope.hook_event_name.canonical()) else {
            return;
        };
        let tool_name = xai_grok_hooks::dispatcher::extract_tool_name(envelope);
        for callback_id in matching_callback_ids(groups, tool_name.as_deref()) {
            let dispatch = ClientHookDispatch {
                hook_callback_id: callback_id,
                envelope,
            };
            if let Some(params) = dispatch_params(&dispatch) {
                self.notifications
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(HOOK_EVENT_METHOD, params));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(value: serde_json::Value) -> Arc<RawValue> {
        serde_json::value::to_raw_value(&value).unwrap().into()
    }

    /// Only an explicit `deny` blocks; malformed/transport/timeout all proceed. The second
    /// tuple element is the telemetry outcome, distinct per fail-open mode.
    #[test]
    fn classify_only_deny_blocks() {
        let (denied, outcome) = classify(ReverseOutcome::Responded(raw(
            serde_json::json!({ "decision": "deny" }),
        )));
        assert_eq!(denied.decision, ClientHookDecision::Deny);
        assert!(matches!(outcome, ClientHookGateOutcome::Denied));

        let (cont, outcome) = classify(ReverseOutcome::Responded(raw(
            serde_json::json!({ "decision": "continue" }),
        )));
        assert_eq!(cont.decision, ClientHookDecision::Continue);
        assert!(matches!(outcome, ClientHookGateOutcome::Proceeded));

        // An unrecognized decision string fails open (proceeds) but is reported distinctly
        // from a normal proceed so client bugs (typo / version skew) stay observable.
        let (unknown, outcome) = classify(ReverseOutcome::Responded(raw(
            serde_json::json!({ "decision": "maybe_later" }),
        )));
        assert_ne!(unknown.decision, ClientHookDecision::Deny);
        assert!(matches!(outcome, ClientHookGateOutcome::UnknownDecision));

        // Every failure mode falls open to Continue, but reports a distinct outcome.
        let (malformed, outcome) = classify(ReverseOutcome::Responded(raw(
            serde_json::json!({ "decision": 123 }),
        )));
        assert_eq!(malformed.decision, ClientHookDecision::Continue);
        assert!(matches!(outcome, ClientHookGateOutcome::Malformed));

        let (transport, outcome) =
            classify(ReverseOutcome::Transport(acp::Error::internal_error()));
        assert_eq!(transport.decision, ClientHookDecision::Continue);
        assert!(matches!(outcome, ClientHookGateOutcome::TransportError));

        let (timeout, outcome) = classify(ReverseOutcome::Timeout);
        assert_eq!(timeout.decision, ClientHookDecision::Continue);
        assert!(matches!(outcome, ClientHookGateOutcome::TimedOut));
    }

    /// `hook_event_active` is the inert-when-unused guard: `false` with no file registry and
    /// no client hook for the event; `true` once a client hook for that event is registered;
    /// and `true` for any event whenever a file registry is present.
    #[tokio::test(flavor = "current_thread")]
    async fn hook_event_active_inert_vs_active() {
        use xai_grok_hooks::event::HookEventName;

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let actor = crate::session::acp_session::support::create_test_actor(
                    0,
                    256_000,
                    85,
                    gateway_tx,
                    persistence_tx,
                )
                .await;

                // Inert: no file registry, no client hooks.
                assert!(actor.hook_registry.borrow().is_none());
                assert!(actor.client_hooks.borrow().is_empty());
                assert!(!actor.hook_event_active(HookEventName::PreToolUse));

                // A registered client hook activates exactly its event.
                actor.client_hooks.borrow_mut().insert(
                    HookEventName::PreToolUse,
                    vec![ClientHookGroup {
                        matcher: None,
                        callback_ids: vec!["cb_0".to_string()],
                        timeout: None,
                    }],
                );
                assert!(actor.hook_event_active(HookEventName::PreToolUse));
                assert!(!actor.hook_event_active(HookEventName::Stop));

                // A present file registry activates every event, even ones with no client hook.
                *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(
                    xai_grok_hooks::discovery::HookRegistry::default(),
                ));
                assert!(actor.hook_event_active(HookEventName::Stop));
                assert!(actor.hook_event_active(HookEventName::PostCompact));
            })
            .await;
    }

    /// Tool events filter by matcher (matcher-less groups always fire); a non-tool
    /// event (`None`) fires every group regardless of its matcher.
    #[test]
    fn matching_callback_ids_filters_by_matcher() {
        use xai_grok_hooks::matcher::HookMatcher;

        let groups = vec![
            ClientHookGroup {
                matcher: Some(HookMatcher::new("run_terminal_command").unwrap()),
                callback_ids: vec!["bash_only".to_string()],
                timeout: None,
            },
            ClientHookGroup {
                matcher: None,
                callback_ids: vec!["all_a".to_string(), "all_b".to_string()],
                timeout: None,
            },
            ClientHookGroup {
                matcher: Some(HookMatcher::new("read_file").unwrap()),
                callback_ids: vec!["read_only".to_string()],
                timeout: None,
            },
        ];

        assert_eq!(
            matching_callback_ids(&groups, Some("run_terminal_command")),
            ["bash_only", "all_a", "all_b"]
        );
        assert_eq!(
            matching_callback_ids(&groups, Some("list_dir")),
            ["all_a", "all_b"]
        );
        assert_eq!(
            matching_callback_ids(&groups, None),
            ["bash_only", "all_a", "all_b", "read_only"]
        );
    }
}
