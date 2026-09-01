use super::*;

pub(super) struct TurnSubagentScopeGuard {
    current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    prompt_id: String,
}

impl TurnSubagentScopeGuard {
    pub(super) fn new(
        current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        prompt_id: String,
    ) -> Self {
        Self {
            current_prompt_id,
            prompt_id,
        }
    }
}

impl Drop for TurnSubagentScopeGuard {
    fn drop(&mut self) {
        let mut current_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned");
        if current_prompt_id.as_deref() == Some(self.prompt_id.as_str()) {
            *current_prompt_id = None;
        }
    }
}

pub(super) struct TurnActiveGuard(Option<Arc<std::sync::atomic::AtomicBool>>);

impl TurnActiveGuard {
    pub(super) fn activate(flag: Option<&Arc<std::sync::atomic::AtomicBool>>) -> Self {
        if let Some(flag) = flag {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Self(flag.cloned())
    }
}

impl Drop for TurnActiveGuard {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub(crate) struct TurnInputRequest {
    pub(crate) prompt_id: String,
    pub(crate) input_origin: InputOrigin,
    pub(crate) prompt_blocks: Vec<ContentBlock>,
    pub(crate) prompt_mode: PromptMode,
    pub(crate) trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
    pub(crate) artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
    pub(crate) client_identifier: Option<String>,
    pub(crate) screen_mode: Option<String>,
    pub(crate) verbatim: bool,
    pub(crate) send_now: bool,
    pub(crate) json_schema: Option<serde_json::Value>,
    pub(crate) persist_ack: Option<oneshot::Sender<()>>,
    pub(crate) parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
    pub(crate) traceparent: Option<String>,
}

pub(super) type TaskIdentity = std::rc::Rc<()>;

pub(crate) struct TurnCompletionMsg {
    pub(crate) prompt_id: String,
    pub(crate) epoch: TurnEpoch,
    pub(super) task_identity: TaskIdentity,
    pub(crate) result: PromptTurnResult,
    pub(crate) elapsed_ms: Option<u64>,
    #[cfg(test)]
    pub(crate) processed: Option<tokio::sync::oneshot::Sender<()>>,
}

#[derive(Debug)]
pub(super) enum FinalizationBinding {
    Task {
        prompt_id: String,
        epoch: TurnEpoch,
        task_identity: TaskIdentity,
    },
    NoTask(Option<String>),
}

impl FinalizationBinding {
    pub(super) fn epoch(&self) -> Option<TurnEpoch> {
        match self {
            Self::Task { epoch, .. } => Some(*epoch),
            Self::NoTask(_) => None,
        }
    }

    fn matches_task(&self, task: &AgentTask) -> bool {
        matches!(self, Self::Task { prompt_id, epoch, task_identity }
            if task.prompt_id == *prompt_id && task.epoch == *epoch
                && TaskIdentity::ptr_eq(&task.identity, task_identity))
    }
}

#[derive(Default)]
pub(in crate::session) struct FinalizationGate {
    next_token: u64,
    active: Option<u64>,
}

#[cfg(test)]
mod finalization_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn stale_spawn_identity_cannot_finalize_reused_prompt_and_epoch() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (actor, _) = super::super::support::build_actor().await;
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(super::super::support::user_item("same", "owner"));
                let old = super::super::support::running_task_stub("same");
                let old_identity = old.identity.clone();
                let epoch = old.epoch;
                state.running_task = Some(super::super::support::running_task_stub("same"));
                assert!(
                    state
                        .claim_task_finalization("same", epoch, &old_identity)
                        .is_none()
                );
                assert!(!state.finalization_gate.is_active());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completion_and_cancel_share_one_exclusive_finalization_lease() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (actor, _) = super::super::support::build_actor().await;
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(super::super::support::user_item("turn", "owner"));
                let task = super::super::support::running_task_stub("turn");
                let identity = task.identity.clone();
                let epoch = task.epoch;
                state.running_task = Some(task);
                let mut lease = state
                    .claim_task_finalization("turn", epoch, &identity)
                    .unwrap();
                assert!(state.claim_cancel_finalization().is_none());
                assert!(state.finalization_binding_is_current(&lease));
                assert!(!state.finish_finalization(&mut lease));
                assert!(state.release_finalizing_task(&lease));
                state.pending_inputs.pop_front();
                assert!(state.finish_finalization(&mut lease));
                assert!(!state.finalization_gate.is_active());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn finalization_cannot_release_a_replacement_task() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (actor, _) = super::super::support::build_actor().await;
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(super::super::support::user_item("turn", "owner"));
                state.running_task = Some(super::super::support::running_task_stub("turn"));
                let mut lease = state.claim_cancel_finalization().unwrap();
                let original = state.running_task.take();
                state.running_task = Some(super::super::support::running_task_stub("turn"));
                assert!(!state.finalization_binding_is_current(&lease));
                assert!(!state.release_finalizing_task(&lease));
                assert!(state.running_task.is_some());
                state.running_task = original;
                assert!(state.release_finalizing_task(&lease));
                assert!(state.finish_finalization(&mut lease));
            })
            .await;
    }
}

impl FinalizationGate {
    fn claim(&mut self, binding: FinalizationBinding) -> Option<FinalizationLease> {
        if self.active.is_some() {
            return None;
        }
        self.next_token = self.next_token.wrapping_add(1);
        let token = self.next_token;
        self.active = Some(token);
        Some(FinalizationLease {
            token,
            binding,
            is_finished: false,
        })
    }

    pub(super) fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

impl State {
    fn front_prompt_id(&self) -> Option<&str> {
        self.pending_inputs
            .front()
            .map(|item| item.prompt_id.as_str())
    }

    pub(super) fn claim_task_finalization(
        &mut self,
        prompt_id: &str,
        epoch: TurnEpoch,
        task_identity: &TaskIdentity,
    ) -> Option<FinalizationLease> {
        let binding = FinalizationBinding::Task {
            prompt_id: prompt_id.to_owned(),
            epoch,
            task_identity: task_identity.clone(),
        };
        if self.front_prompt_id() != Some(prompt_id)
            || !self
                .running_task
                .as_ref()
                .is_some_and(|task| binding.matches_task(task))
        {
            return None;
        }
        self.finalization_gate.claim(binding)
    }

    pub(super) fn claim_cancel_finalization(&mut self) -> Option<FinalizationLease> {
        let binding = match self.running_task.as_ref() {
            Some(task) => {
                if self.front_prompt_id() != Some(task.prompt_id.as_str()) {
                    return None;
                }
                FinalizationBinding::Task {
                    prompt_id: task.prompt_id.clone(),
                    epoch: task.epoch,
                    task_identity: task.identity.clone(),
                }
            }
            None => FinalizationBinding::NoTask(self.front_prompt_id().map(str::to_owned)),
        };
        self.finalization_gate.claim(binding)
    }

    fn finalization_matches(&self, lease: &FinalizationLease) -> bool {
        self.finalization_gate.active == Some(lease.token)
    }

    pub(super) fn finalization_binding_is_current(&self, lease: &FinalizationLease) -> bool {
        self.finalization_matches(lease)
            && match &lease.binding {
                FinalizationBinding::Task { prompt_id, .. } => {
                    self.front_prompt_id() == Some(prompt_id.as_str())
                        && self
                            .running_task
                            .as_ref()
                            .is_some_and(|task| lease.binding.matches_task(task))
                }
                FinalizationBinding::NoTask(prompt_id) => {
                    self.running_task.is_none() && self.front_prompt_id() == prompt_id.as_deref()
                }
            }
    }

    pub(super) fn release_finalizing_task(&mut self, lease: &FinalizationLease) -> bool {
        if !self.finalization_matches(lease) {
            return false;
        }
        self.running_task
            .take_if(|task| lease.binding.matches_task(task))
            .is_some()
    }

    pub(super) fn finish_finalization(&mut self, lease: &mut FinalizationLease) -> bool {
        if !self.finalization_matches(lease) || self.running_task.is_some() {
            return false;
        }
        self.finalization_gate.active = None;
        lease.is_finished = true;
        true
    }
}

#[must_use = "a committed finalization lease must be finished"]
pub(super) struct FinalizationLease {
    token: u64,
    pub(super) binding: FinalizationBinding,
    is_finished: bool,
}

impl Drop for FinalizationLease {
    fn drop(&mut self) {
        if !self.is_finished {
            tracing::error!(
                token = self.token,
                binding = ?self.binding,
                "committed turn finalization dropped before exact release"
            );
        }
    }
}

pub(crate) struct AgentTask {
    pub(crate) prompt_id: String,
    pub(super) epoch: TurnEpoch,
    pub(super) identity: TaskIdentity,
    pub(crate) handle: tokio::task::AbortHandle,
    pub(crate) started_at: std::time::Instant,
}

pub(crate) fn elapsed_ms_saturating(start: std::time::Instant, now: std::time::Instant) -> u64 {
    u64::try_from(now.saturating_duration_since(start).as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod elapsed_ms_tests {
    use super::elapsed_ms_saturating;

    #[test]
    fn elapsed_ms_saturating_uses_injected_instants() {
        let start = std::time::Instant::now();
        let later = start + std::time::Duration::from_millis(42);
        assert_eq!(elapsed_ms_saturating(start, later), 42);
        assert_eq!(elapsed_ms_saturating(later, start), 0);
    }
}

impl SessionActor {
    pub(super) async fn clear_pinned_prompt_if_current(&self, prompt_id: &str) {
        let cleared = {
            let mut current = self
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned");
            if current.as_deref() == Some(prompt_id) {
                *current = None;
                true
            } else {
                false
            }
        };
        if !cleared {
            return;
        }
        let prompt_id = prompt_id.to_owned();
        self.agent
            .borrow()
            .tool_bridge()
            .update_resources_with(move |resources| {
                use xai_grok_tools::implementations::grok_build::task::types::CurrentPromptIdResource;
                if resources
                    .get::<CurrentPromptIdResource>()
                    .is_some_and(|current| current.0 == prompt_id)
                {
                    resources.insert(CurrentPromptIdResource(String::new()));
                }
            })
            .await;
    }

    pub(super) async fn clear_exact_turn_resources(&self, prompt_id: &str) {
        {
            let mut current = self
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned");
            if current.as_deref() == Some(prompt_id) {
                *current = None;
            }
        }
        self.tool_context
            .goal_loop_active_gate
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let prompt_id = prompt_id.to_owned();
        self.agent
            .borrow()
            .tool_bridge()
            .update_resources_with(move |resources| {
                use xai_grok_tools::implementations::grok_build::task::types::{
                    CurrentPromptIdResource, GoalLoopActive,
                };
                if resources
                    .get::<CurrentPromptIdResource>()
                    .is_some_and(|current| current.0 == prompt_id)
                {
                    resources.insert(CurrentPromptIdResource(String::new()));
                }
                resources.insert(GoalLoopActive(false));
            })
            .await;
    }

    pub(super) async fn finish_finalization_lease(&self, lease: &mut FinalizationLease) -> bool {
        if matches!(lease.binding, FinalizationBinding::Task { .. })
            && !self.state.lock().await.release_finalizing_task(lease)
        {
            return false;
        }
        if let FinalizationBinding::Task { prompt_id, .. } = &lease.binding {
            self.clear_exact_turn_resources(prompt_id).await;
        } else if let FinalizationBinding::NoTask(Some(prompt_id)) = &lease.binding {
            self.clear_pinned_prompt_if_current(prompt_id).await;
        }
        self.state.lock().await.finish_finalization(lease)
    }
}

impl AgentTask {
    #[cfg(test)]
    pub(crate) fn new(prompt_id: &str, handle: tokio::task::AbortHandle) -> Self {
        Self::new_at_epoch(prompt_id, TurnEpoch::default(), handle)
    }

    #[cfg(test)]
    pub(super) fn new_at_epoch(
        prompt_id: &str,
        epoch: TurnEpoch,
        handle: tokio::task::AbortHandle,
    ) -> Self {
        Self {
            prompt_id: prompt_id.to_string(),
            epoch,
            identity: TaskIdentity::new(()),
            handle,
            started_at: std::time::Instant::now(),
        }
    }

    pub(super) fn new_prompt(
        session: Arc<SessionActor>,
        request: TurnInputRequest,
        epoch: TurnEpoch,
        completion_tx: mpsc::UnboundedSender<TurnCompletionMsg>,
    ) -> Self {
        let started_at = std::time::Instant::now();
        let identity = TaskIdentity::new(());
        Self {
            prompt_id: request.prompt_id.clone(),
            epoch,
            identity: identity.clone(),
            handle: xai_grok_telemetry::session_ctx::spawn_local_in_session_ctx(run_task(
                session,
                request,
                epoch,
                identity,
                completion_tx,
                started_at,
            ))
            .abort_handle(),
            started_at,
        }
    }

    pub(super) fn abort(&self) {
        if !self.handle.is_finished() {
            self.handle.abort();
        }
    }
}

pub(crate) struct TaskSlot<Output> {
    handle: std::cell::Cell<Option<tokio::task::JoinHandle<Output>>>,
}

impl<Output> TaskSlot<Output> {
    pub(crate) fn new() -> Self {
        Self {
            handle: std::cell::Cell::new(None),
        }
    }

    pub(crate) fn arm(&self, handle: tokio::task::JoinHandle<Output>) {
        if let Some(old) = self.handle.take() {
            old.abort();
        }
        self.handle.set(Some(handle));
    }

    pub(crate) fn take(&self) -> Option<tokio::task::JoinHandle<Output>> {
        self.handle.take()
    }

    pub(crate) fn cancel(&self) {
        if let Some(old) = self.handle.take() {
            old.abort();
        }
    }
}

async fn run_task(
    session: Arc<SessionActor>,
    request: TurnInputRequest,
    epoch: TurnEpoch,
    task_identity: TaskIdentity,
    completion_tx: mpsc::UnboundedSender<TurnCompletionMsg>,
    started_at: std::time::Instant,
) {
    let prompt_id = request.prompt_id.clone();
    let result = session.handle_turn_input(request).await;
    let elapsed_ms = elapsed_ms_saturating(started_at, std::time::Instant::now());
    let _ = completion_tx.send(TurnCompletionMsg {
        prompt_id,
        epoch,
        task_identity,
        result,
        elapsed_ms: Some(elapsed_ms),
        #[cfg(test)]
        processed: None,
    });
}

#[cfg(test)]
mod task_slot_tests {
    use super::TaskSlot;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn arm_counter(slot: &TaskSlot<()>, fired: &Arc<AtomicUsize>, by: usize) {
        let flag = Arc::clone(fired);
        slot.arm(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            flag.fetch_add(by, Ordering::SeqCst);
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn armed_task_fires_after_delay() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;

        assert_eq!(fired.load(Ordering::SeqCst), 1, "armed task must fire");
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_aborts_pending_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        tokio::time::advance(Duration::from_secs(30)).await;
        slot.cancel();
        assert!(slot.take().is_none(), "cancel must clear the slot");
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            0,
            "cancelled task must not fire"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rearm_aborts_previous_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);
        arm_counter(&slot, &fired, 10);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            10,
            "only the re-armed task fires"
        );
    }
}
