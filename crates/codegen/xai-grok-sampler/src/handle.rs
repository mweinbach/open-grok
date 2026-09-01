//! Public handle for talking to the sampler actor.

use std::sync::{Arc, OnceLock, RwLock};

use tokio::sync::{mpsc, oneshot};

use xai_grok_sampling_types::{ConversationRequest, ConversationResponse, SamplingError};

use crate::actor::request_task::CompletionResult;
use crate::commands::SamplerCommand;
use crate::config::SamplerConfig;
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

/// Shared owner for Codex's turn-scoped sticky-routing token.
///
/// Each logical turn swaps in a fresh [`OnceLock`]. Requests snapshot that
/// generation before they start, so a late response from an older turn can
/// never seed the next turn's routing state.
#[derive(Clone, Debug)]
pub(crate) struct CodexTurnState {
    current: Arc<RwLock<Arc<OnceLock<String>>>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_binds_turn_generation_before_actor_dequeue() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let handle = SamplerHandle::new(cmd_tx, CodexTurnState::default());

        handle.begin_codex_turn();
        handle.submit(RequestId::from("turn-a"), ConversationRequest::default());
        handle.begin_codex_turn();
        handle.submit(RequestId::from("turn-b"), ConversationRequest::default());

        let first = cmd_rx.try_recv().expect("first submit must be queued");
        let second = cmd_rx.try_recv().expect("second submit must be queued");
        let SamplerCommand::Submit {
            codex_turn_state: first_state,
            ..
        } = first
        else {
            panic!("expected first submit command")
        };
        let SamplerCommand::Submit {
            codex_turn_state: second_state,
            ..
        } = second
        else {
            panic!("expected second submit command")
        };

        assert!(!Arc::ptr_eq(&first_state, &second_state));
        first_state.set("turn-a-state".into()).unwrap();
        assert_eq!(first_state.get().map(String::as_str), Some("turn-a-state"));
        assert_eq!(second_state.get(), None);
    }
}

impl Default for CodexTurnState {
    fn default() -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(OnceLock::new()))),
        }
    }
}

impl CodexTurnState {
    fn begin_turn(&self) {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = Arc::new(OnceLock::new());
    }

    pub(crate) fn snapshot(&self) -> Arc<OnceLock<String>> {
        Arc::clone(
            &self
                .current
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// One doom-loop recovery action performed during a request.
pub struct DoomLoopRecoveryAttempt {
    pub triggers: Vec<String>,
    pub aborted_at_chunk: Option<u64>,
}

/// Awaited sampling result plus request-level detector metadata.
pub struct CollectedSamplingResult {
    /// Terminal response and metrics, or the rich sampling error.
    pub result: Result<(ConversationResponse, InferenceLatencyStats), SamplingError>,
    /// Whether a terminal event was successfully queued on the event rail.
    pub terminal_event_queued: bool,
    /// Deduplicated raw labels observed across every attempt in the request.
    pub doom_loop_signals: Vec<String>,
    /// Doom-loop recovery actions performed for this request.
    pub doom_loop_recovery_attempts: Vec<DoomLoopRecoveryAttempt>,
}

/// Cheaply-cloneable handle to the sampler actor.
///
/// Internally just an `mpsc::UnboundedSender<SamplerCommand>`. All
/// methods are non-blocking (fire-and-forget) except for the
/// `*_async` queries which return a future awaiting an
/// `oneshot::Receiver`.
#[derive(Clone)]
pub struct SamplerHandle {
    cmd_tx: mpsc::UnboundedSender<SamplerCommand>,
    codex_turn_state: CodexTurnState,
}

impl SamplerHandle {
    /// Construct a handle from a command sender. `pub(crate)` because
    /// only [`SamplerActor::spawn`](crate::actor::SamplerActor::spawn)
    /// produces one of these.
    pub(crate) fn new(
        cmd_tx: mpsc::UnboundedSender<SamplerCommand>,
        codex_turn_state: CodexTurnState,
    ) -> Self {
        Self {
            cmd_tx,
            codex_turn_state,
        }
    }

    /// Create a no-op handle that discards all commands.
    ///
    /// Useful for tests and callers that need a `SamplerHandle` field
    /// before the actor is wired up. Mirrors
    /// [`HunkTrackerHandle::noop`](https://docs.rs/xai-hunk-tracker).
    pub fn noop() -> Self {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        // Receiver is dropped immediately; sends will fail but every
        // send-site uses `let _ = ...` so that is fine.
        Self {
            cmd_tx,
            codex_turn_state: CodexTurnState::default(),
        }
    }

    /// Start a new logical user turn for Codex sticky routing.
    ///
    /// The first successful Codex Responses or compact response may seed this
    /// turn's token. All later Codex requests in the turn replay that first
    /// value; the next call here replaces it with a fresh empty generation.
    pub fn begin_codex_turn(&self) {
        self.codex_turn_state.begin_turn();
    }

    /// Snapshot the current turn's sticky-routing cell for a direct Codex
    /// client (notably `/responses/compact`).
    pub fn codex_turn_state(&self) -> Arc<OnceLock<String>> {
        self.codex_turn_state.snapshot()
    }

    /// Submit a sampling request. Fire-and-forget -- results arrive
    /// via the shared event channel.
    pub fn submit(&self, request_id: RequestId, request: ConversationRequest) {
        let _ = self.cmd_tx.send(SamplerCommand::Submit {
            request_id,
            request: Box::new(request),
            config: None,
            codex_turn_state: self.codex_turn_state.snapshot(),
            completion_tx: None,
        });
    }

    /// Submit a sampling request with an explicit per-request config
    /// override (e.g., a different model than the actor's default).
    pub fn submit_with_config(
        &self,
        request_id: RequestId,
        request: ConversationRequest,
        config: SamplerConfig,
    ) {
        let _ = self.cmd_tx.send(SamplerCommand::Submit {
            request_id,
            request: Box::new(request),
            config: Some(Box::new(config)),
            codex_turn_state: self.codex_turn_state.snapshot(),
            completion_tx: None,
        });
    }

    /// Cancel an in-flight request. No-op if the request id is
    /// unknown (already finished or never submitted).
    pub fn cancel(&self, request_id: RequestId) {
        let _ = self.cmd_tx.send(SamplerCommand::Cancel { request_id });
    }

    /// Update the default sampling config (e.g., after model switch
    /// or auth refresh). The next request submitted without an
    /// override will use it.
    pub fn update_config(&self, config: SamplerConfig) {
        let _ = self.cmd_tx.send(SamplerCommand::UpdateConfig {
            config: Box::new(config),
        });
    }

    /// Query whether a request is still in flight. Returns `false`
    /// for unknown / finished / cancelled ids.
    pub async fn is_active(&self, request_id: RequestId) -> bool {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.cmd_tx.send(SamplerCommand::IsActive {
            request_id,
            reply: reply_tx,
        });
        reply_rx.await.unwrap_or(false)
    }

    /// Query the number of in-flight requests. Returns 0 if the
    /// actor has been shut down.
    pub async fn active_count(&self) -> usize {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .send(SamplerCommand::ActiveCount { reply: reply_tx });
        reply_rx.await.unwrap_or(0)
    }

    /// Submit a request and await its completion. Events still flow
    /// to the shared channel for live UI updates -- this method just
    /// additionally awaits the per-request completion oneshot so the
    /// caller gets a clean `Result` without filtering events.
    ///
    /// Used by sequential callers like compaction / summary /
    /// `/btw` side questions.
    pub async fn submit_and_collect(
        &self,
        request_id: RequestId,
        request: ConversationRequest,
    ) -> CompletionResult {
        self.submit_and_collect_with_metadata(request_id, request)
            .await
            .result
    }

    /// Submit and also return detector labels observed on discarded attempts.
    pub async fn submit_and_collect_with_metadata(
        &self,
        request_id: RequestId,
        request: ConversationRequest,
    ) -> CollectedSamplingResult {
        struct CancelOnDrop {
            cmd_tx: mpsc::UnboundedSender<SamplerCommand>,
            request_id: RequestId,
        }
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                // fire-and-forget the send.
                let _ = self.cmd_tx.send(SamplerCommand::Cancel {
                    request_id: self.request_id.clone(),
                });
            }
        }

        let (completion_tx, completion_rx) = oneshot::channel();
        let cancel_id = request_id.clone();

        // Only arm the guard if Submit actually reached the actor.
        let _guard = self
            .cmd_tx
            .send(SamplerCommand::Submit {
                request_id,
                request: Box::new(request),
                config: None,
                codex_turn_state: self.codex_turn_state.snapshot(),
                completion_tx: Some(completion_tx),
            })
            .ok()
            .map(|_| CancelOnDrop {
                cmd_tx: self.cmd_tx.clone(),
                request_id: cancel_id,
            });
        completion_rx
            .await
            .unwrap_or_else(|_| CollectedSamplingResult {
                result: Err(SamplingError::auth_unknown(
                    "sampler actor dropped before completion",
                )),
                terminal_event_queued: false,
                doom_loop_signals: Vec::new(),
                doom_loop_recovery_attempts: Vec::new(),
            })
    }
}
