//! Single-writer subagent coordinator actor.
//!
//! The actor owns the command receiver, the admission queue, pending/active/
//! completed state, concrete blocking waiters, foreground deadlines,
//! cancellation, and the terminal delivery disposition. All hosts drive it
//! through `ChannelBackend`; only their `ChildRunner` implementations differ.
//!
//! There is intentionally no shared mutable state in this module. A runner's
//! associated futures may be `Send` or non-`Send`; the resulting actor future
//! inherits that property naturally on stable Rust.

mod native;
mod query;
mod queue;
mod spawn;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use futures::FutureExt;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{mpsc, oneshot};

use super::admission::Admission;
use super::coordinator_state::{
    ActiveChild, AgentMailboxWaiter, BlockingWaiter, BufferedCompletion, ChildRecord,
    CompletedChild, InternalEvent, ListRequest, PendingChild, ProgressFuture, ProgressTarget,
    ReplyFuture, TaggedFuture, active_summary, background_at_deadline, background_if_caller_gone,
    completed_snapshot, completion_summary, sleep_until, workflow_outstanding,
};
use super::types::{
    AgentMailboxIdentity, AgentMailboxMessage, AgentMessageDeliveryStatus, AgentMessageSendOutput,
    AgentRosterEntry, ListAgentsOutput, SpawnedSubagentRef, SubagentCancelOutcome,
    SubagentCancelTarget, SubagentDescribeOutcome, SubagentEvent, SubagentOutstandingReply,
    SubagentRegistryCounts, SubagentRequest, SubagentResult, SubagentResumeLookup,
    SubagentResumeSource, SubagentValidateTypeOutcome, WaitAgentMessagesOutput,
};

pub use super::coordinator_state::{
    ChildCompletion, ChildControl, ChildReporter, ChildRunOutput, ChildRunRequest, ChildRunner,
    CompletionDisposition, CoordinatorConfig, LimitedSpawnOrigin, LocalBoxFuture,
    MAX_COMPLETED_ENTRIES, SendBoxFuture, StartedChild, SubagentLimitDecision, SubagentLimitNotice,
    SubagentLimitSink, SubagentProgress,
};
use queue::{QUEUED_REAP_INTERVAL, QueuedCaller, SpawnQueue, StartOrigin};

/// Channel-owned subagent lifecycle actor.
pub struct SubagentCoordinator<R: ChildRunner> {
    commands: mpsc::UnboundedReceiver<SubagentEvent>,
    internal_tx: mpsc::UnboundedSender<InternalEvent<R::Control>>,
    internal_rx: mpsc::UnboundedReceiver<InternalEvent<R::Control>>,
    runner: R,
    config: CoordinatorConfig,
    admission: Admission,
    queued: SpawnQueue,
    /// When the queued sweep last ran; bounds how stale an out-of-band
    /// token cancel of a queued spawn can get (see [`Self::next_deadline`]).
    last_queued_reap: tokio::time::Instant,
    /// Re-entry latch: `finish_child` runs the queued sweep, and finishing a
    /// cancelled queued entry routes back through `finish_child`. The inner
    /// sweep is a no-op (a cancelled entry frees no running slot).
    draining_queued: bool,
    pending: HashMap<String, PendingChild>,
    active: HashMap<String, ActiveChild<R::Control>>,
    completed: HashMap<String, CompletedChild>,
    completed_order: VecDeque<String>,
    waiters: HashMap<String, Vec<BlockingWaiter>>,
    mailboxes: HashMap<MailboxKey, VecDeque<AgentMailboxMessage>>,
    mailbox_waiters: HashMap<MailboxKey, AgentMailboxWaiter>,
    native_names: HashMap<MailboxKey, String>,
    native_records: HashMap<MailboxKey, super::types::NativeAgentRecord>,
    native_loaded: HashSet<String>,
    native_activity: HashMap<MailboxKey, Vec<String>>,
    native_waiters: HashMap<MailboxKey, native::NativeWaiter>,
    workflow_cancel_waiters: HashMap<String, Vec<oneshot::Sender<SubagentCancelOutcome>>>,
    /// Per-parent delete-path teardown drain, present only while a
    /// responder-bearing `TeardownSession` (`/delete`) waits for the session's
    /// children to finish. While an entry exists, spawn admission stays closed
    /// and [`SubagentEvent::OpenSpawnAdmission`] cannot reopen it (a racing
    /// next-turn open would reopen Task spawns mid-delete).
    teardown_drains: HashMap<String, TeardownDrain>,
    /// Parent sessions that received `ParentSession` cancel. Non-workflow spawns
    /// are rejected until [`SubagentEvent::OpenSpawnAdmission`] (next turn) or
    /// teardown drain completes, so a detached late `TaskTool` spawn cannot
    /// outrun Stop / delete.
    spawn_blocked_sessions: HashSet<String>,
    usage_not_applied_prompts: HashSet<PromptScope>,
    pending_completions: Vec<BufferedCompletion>,
    runs: FuturesUnordered<
        TaggedFuture<futures::future::CatchUnwind<std::panic::AssertUnwindSafe<R::RunFuture>>>,
    >,
    validations: FuturesUnordered<ReplyFuture<R::ValidateFuture, SubagentValidateTypeOutcome>>,
    descriptions: FuturesUnordered<ReplyFuture<R::DescribeFuture, SubagentDescribeOutcome>>,
    progress: FuturesUnordered<ProgressFuture<<R::Control as ChildControl>::ProgressFuture>>,
    list_requests: HashMap<u64, ListRequest>,
    next_list_request_id: u64,
}

/// Backstop for a delete-path teardown hold: if a cancelled child never
/// finishes, force-reopen the session's spawn admission after this long (with a
/// warning) rather than blocking spawns for the process lifetime.
const TEARDOWN_DRAIN_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// In-flight delete-path teardown drain: responders to resolve once the last
/// child drains, and the backstop deadline that force-reopens admission.
struct TeardownDrain {
    waiters: Vec<oneshot::Sender<()>>,
    deadline: tokio::time::Instant,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PromptScope {
    parent_session_id: String,
    prompt_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MailboxKey {
    team_scope_id: String,
    agent_id: String,
}

impl From<&AgentMailboxIdentity> for MailboxKey {
    fn from(identity: &AgentMailboxIdentity) -> Self {
        Self {
            team_scope_id: identity.team_scope_id.clone(),
            agent_id: identity.agent_id.clone(),
        }
    }
}

impl PromptScope {
    fn new(parent_session_id: String, prompt_id: String) -> Self {
        Self {
            parent_session_id,
            prompt_id,
        }
    }
}

impl<R: ChildRunner> SubagentCoordinator<R> {
    pub fn new(
        commands: mpsc::UnboundedReceiver<SubagentEvent>,
        runner: R,
        config: CoordinatorConfig,
    ) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        Self {
            commands,
            internal_tx,
            internal_rx,
            runner,
            admission: Admission::new(config.limits),
            queued: SpawnQueue::default(),
            last_queued_reap: tokio::time::Instant::now(),
            draining_queued: false,
            config,
            pending: HashMap::new(),
            active: HashMap::new(),
            completed: HashMap::new(),
            completed_order: VecDeque::new(),
            waiters: HashMap::new(),
            mailboxes: HashMap::new(),
            mailbox_waiters: HashMap::new(),
            native_names: HashMap::new(),
            native_records: HashMap::new(),
            native_loaded: HashSet::new(),
            native_activity: HashMap::new(),
            native_waiters: HashMap::new(),
            workflow_cancel_waiters: HashMap::new(),
            teardown_drains: HashMap::new(),
            spawn_blocked_sessions: HashSet::new(),
            usage_not_applied_prompts: HashSet::new(),
            pending_completions: Vec::new(),
            runs: FuturesUnordered::new(),
            validations: FuturesUnordered::new(),
            descriptions: FuturesUnordered::new(),
            progress: FuturesUnordered::new(),
            list_requests: HashMap::new(),
            next_list_request_id: 0,
        }
    }

    pub async fn run(mut self) {
        let mut commands_open = true;
        loop {
            // Queued spawns need no exit check: queued non-empty means some
            // session is at capacity, so `runs` is non-empty, and the last
            // `finish_child` drains the queue before `runs` empties.
            if !commands_open
                && self.runs.is_empty()
                && self.validations.is_empty()
                && self.descriptions.is_empty()
                && self.progress.is_empty()
            {
                debug_assert!(
                    self.queued.is_empty(),
                    "actor exiting with spawns still queued"
                );
                break;
            }

            let deadline = self.next_deadline();
            tokio::select! {
                biased;
                Some(event) = self.internal_rx.recv() => self.handle_internal(event),
                Some((id, output)) = self.runs.next(), if !self.runs.is_empty() => {
                    match output {
                        Ok(output) => self.finish_child(&id, output),
                        Err(_) => self.finish_panicked_child(&id),
                    }
                }
                Some((respond_to, outcome)) = self.validations.next(), if !self.validations.is_empty() => {
                    let _ = respond_to.send(outcome);
                }
                Some((respond_to, outcome)) = self.descriptions.next(), if !self.descriptions.is_empty() => {
                    let _ = respond_to.send(outcome);
                }
                Some((seed, target, progress)) = self.progress.next(), if !self.progress.is_empty() => {
                    self.finish_progress(seed, target, progress);
                }
                command = self.commands.recv(), if commands_open => {
                    match command {
                        Some(command) => {
                            self.reap_abandoned_callers();
                            self.handle_command(command);
                        }
                        None => commands_open = false,
                    }
                }
                _ = sleep_until(deadline), if deadline.is_some() => self.process_deadlines(),
            }
            while self.completed.len() > MAX_COMPLETED_ENTRIES {
                let Some(id) = self.completed_order.pop_front() else {
                    break;
                };
                if self.native_names.values().any(|current| current == &id) {
                    self.completed_order.push_back(id);
                    if self
                        .completed_order
                        .iter()
                        .all(|id| self.native_names.values().any(|current| current == id))
                    {
                        break;
                    }
                } else {
                    self.completed.remove(&id);
                }
            }
        }

        self.cancel_all_children();
    }

    fn handle_command(&mut self, command: SubagentEvent) {
        match command {
            SubagentEvent::Spawn(command) => self.handle_spawn(command),
            SubagentEvent::NativeAgent(request) => self.handle_native_agent(request),
            SubagentEvent::Query(query) => {
                self.handle_query(
                    query.subagent_id,
                    query.parent_session_id,
                    query.block,
                    query.timeout_ms,
                    query.respond_to,
                );
            }
            SubagentEvent::ListAgents(request) => {
                let _ = request.respond_to.send(self.list_agents(&request.identity));
            }
            SubagentEvent::SendAgentMessage(request) => {
                let result =
                    self.send_agent_message(&request.identity, &request.target, request.message);
                let _ = request.respond_to.send(result);
            }
            SubagentEvent::WaitAgentMessages(request) => {
                self.wait_agent_messages(request.identity, request.timeout_ms, request.respond_to);
            }
            SubagentEvent::Cancel(request) => match request.target {
                SubagentCancelTarget::SubagentId(id) => {
                    let outcome = self.cancel_one(&id, request.parent_session_id.as_deref(), true);
                    let _ = request.respond_to.send(outcome);
                }
                SubagentCancelTarget::ParentPromptId(prompt_id) => {
                    self.cancel_parent_prompt(&prompt_id, request.parent_session_id.as_deref());
                    let _ = request.respond_to.send(SubagentCancelOutcome::Cancelled);
                }
                SubagentCancelTarget::ParentSession => {
                    let outcome = self.cancel_parent_session(request.parent_session_id.as_deref());
                    let _ = request.respond_to.send(outcome);
                }
                SubagentCancelTarget::WorkflowRunId(run_id) => {
                    self.cancel_workflow_children(&run_id, request.parent_session_id.as_deref());
                    if workflow_outstanding(&self.pending, &self.active, &run_id) == 0 {
                        let _ = request.respond_to.send(SubagentCancelOutcome::Cancelled);
                    } else {
                        self.workflow_cancel_waiters
                            .entry(run_id)
                            .or_default()
                            .push(request.respond_to);
                    }
                }
            },
            SubagentEvent::ListActive(request) => {
                let summaries = self
                    .active
                    .values()
                    .filter(|child| {
                        child.request.parent_session_id == request.parent_session_id
                            && !child.request.owner.is_workflow()
                    })
                    .map(active_summary)
                    .collect();
                let _ = request.respond_to.send(summaries);
            }
            SubagentEvent::ListRunning(request) => {
                self.handle_list_running(request.parent_session_id, request.respond_to);
            }
            SubagentEvent::Completions(request) => {
                let (owned, foreign): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut self.pending_completions)
                        .into_iter()
                        .partition(|completion| {
                            request
                                .parent_session_id
                                .as_ref()
                                .is_none_or(|id| completion.parent_session_id == *id)
                        });
                self.pending_completions = foreign;
                let completions = owned
                    .into_iter()
                    .map(|completion| completion.summary)
                    .filter(|summary| !request.suppress_ids.contains(&summary.subagent_id))
                    .collect();
                let _ = request.respond_to.send(completions);
            }
            SubagentEvent::TeardownSession {
                parent_session_id,
                respond_to,
            } => {
                self.pending_completions
                    .retain(|completion| completion.parent_session_id != parent_session_id);
                self.mailboxes
                    .retain(|key, _| key.team_scope_id != parent_session_id);
                self.native_names
                    .retain(|key, _| key.team_scope_id != parent_session_id);
                self.native_records
                    .retain(|key, _| key.team_scope_id != parent_session_id);
                self.native_loaded.remove(&parent_session_id);
                self.native_activity
                    .retain(|key, _| key.team_scope_id != parent_session_id);
                self.native_waiters
                    .retain(|key, _| key.team_scope_id != parent_session_id);
                let closed_waiters: Vec<_> = self
                    .mailbox_waiters
                    .extract_if(|key, _| key.team_scope_id == parent_session_id)
                    .map(|(_, waiter)| waiter)
                    .collect();
                for waiter in closed_waiters {
                    let _ = waiter.respond_to.send(WaitAgentMessagesOutput {
                        messages: Vec::new(),
                        timed_out: true,
                    });
                }
                self.teardown_session_children(&parent_session_id);
                // Only the delete path (responder present) holds spawn
                // admission closed until children drain, so a next-turn
                // OpenSpawnAdmission cannot reopen Task spawns mid-delete.
                // Close / idle unload (no responder) keep the pre-existing
                // behavior: cancel children and leave admission untouched.
                if let Some(respond_to) = respond_to {
                    if self.session_has_children(&parent_session_id) {
                        self.begin_teardown_drain(parent_session_id, respond_to);
                    } else {
                        let _ = respond_to.send(());
                    }
                }
            }
            SubagentEvent::OpenSpawnAdmission { parent_session_id } => {
                // Next-turn reopen after Stop is intentional even while cancelled
                // children finish — but not while a delete-path TeardownSession
                // is draining.
                if !self.teardown_drains.contains_key(&parent_session_id) {
                    self.spawn_blocked_sessions.remove(&parent_session_id);
                }
            }
            SubagentEvent::Outstanding(request) => {
                // Reap again here so turn-freeze / Outstanding polls see
                // ParentGone even if no other command woke the actor first.
                self.reap_abandoned_callers();
                let scoped = |candidate: &SubagentRequest| {
                    candidate.parent_session_id == request.parent_session_id
                        && candidate.parent_prompt_id.as_deref() == Some(&request.prompt_id)
                        && !candidate.owner.is_workflow()
                };
                let mut live_ids: Vec<_> = self
                    .pending
                    .values()
                    .filter(|child| scoped(&child.request) && !child.handle_only)
                    .map(|child| child.request.id.clone())
                    .chain(
                        self.active
                            .values()
                            .filter(|child| {
                                // Definition-declared background children are
                                // background for accounting even while the
                                // spawning tool block-awaits them.
                                scoped(&child.request)
                                    && !child.handle_only
                                    && !child.definition_background
                            })
                            .map(|child| child.request.id.clone()),
                    )
                    .chain(
                        self.queued
                            .iter()
                            .filter(|queued| {
                                scoped(&queued.request)
                                    && !queued.caller.is_backgrounded()
                                    && !queued.request.run_in_background
                            })
                            .map(|queued| queued.request.id.clone()),
                    )
                    .collect();
                live_ids.sort();
                let background_live = self
                    .pending
                    .values()
                    .any(|child| scoped(&child.request) && child.handle_only)
                    || self.active.values().any(|child| {
                        scoped(&child.request) && (child.handle_only || child.definition_background)
                    })
                    || self.queued.iter().any(|queued| {
                        scoped(&queued.request)
                            && (queued.request.run_in_background || queued.caller.is_backgrounded())
                    });
                let scope =
                    PromptScope::new(request.parent_session_id.clone(), request.prompt_id.clone());
                let _ = request.respond_to.send(SubagentOutstandingReply {
                    live_ids,
                    background_live,
                    subagent_usage_not_applied: self.usage_not_applied_prompts.contains(&scope),
                });
            }
            SubagentEvent::ClearUsageNotApplied(request) => {
                self.usage_not_applied_prompts.remove(&PromptScope::new(
                    request.parent_session_id,
                    request.prompt_id,
                ));
            }
            SubagentEvent::MarkUsageNotApplied(request) => {
                self.usage_not_applied_prompts.insert(PromptScope::new(
                    request.parent_session_id,
                    request.prompt_id,
                ));
                let _ = request.respond_to.send(());
            }
            SubagentEvent::RegistryCounts(request) => {
                let _ = request.respond_to.send(SubagentRegistryCounts {
                    pending: self.pending.len(),
                    active: self.active.len(),
                    completed: self.completed.len(),
                    queued: self.queued.len(),
                });
            }
            SubagentEvent::Inspect(request) => {
                self.handle_inspect(
                    request.subagent_id,
                    request.parent_session_id,
                    request.respond_to,
                );
            }
            SubagentEvent::SpawnedRefs(request) => {
                let mut refs: Vec<_> = self
                    .active
                    .values()
                    .filter(|child| {
                        child.request.parent_session_id == request.parent_session_id
                            && child.request.parent_prompt_id.as_deref() == Some(&request.prompt_id)
                    })
                    .map(|child| SpawnedSubagentRef {
                        subagent_id: child.request.id.clone(),
                        child_session_id: child.child_session_id.clone(),
                        subagent_type: child.request.subagent_type.clone(),
                        description: child.request.description.clone(),
                        persona: child.persona.clone(),
                        resumed_from: child.resumed_from.clone(),
                    })
                    .chain(
                        self.completed
                            .values()
                            .filter(|child| {
                                child.request.parent_session_id == request.parent_session_id
                                    && child.request.parent_prompt_id.as_deref()
                                        == Some(&request.prompt_id)
                            })
                            .map(|child| SpawnedSubagentRef {
                                subagent_id: child.request.id.clone(),
                                child_session_id: child.child_session_id.clone(),
                                subagent_type: child.request.subagent_type.clone(),
                                description: child.request.description.clone(),
                                persona: child.persona.clone(),
                                resumed_from: child.resumed_from.clone(),
                            }),
                    )
                    .collect();
                refs.sort_by(|a, b| a.subagent_id.cmp(&b.subagent_id));
                let _ = request.respond_to.send(refs);
            }
            SubagentEvent::ValidateType(request) => {
                self.validations.push(ReplyFuture {
                    future: Box::pin(
                        self.runner
                            .validate_type(request.subagent_type, request.parent_session_id),
                    ),
                    respond_to: Some(request.respond_to),
                });
            }
            SubagentEvent::DescribeType(request) => {
                self.descriptions.push(ReplyFuture {
                    future: Box::pin(self.runner.describe_type(
                        request.subagent_type,
                        request.harness_agent_type,
                        request.parent_session_id,
                    )),
                    respond_to: Some(request.respond_to),
                });
            }
            SubagentEvent::LoopUnitActive(request) => {
                let is_active = self.pending.values().any(|child| {
                    child.request.runtime_overrides.loop_task_id.as_deref()
                        == Some(&request.task_id)
                }) || self.active.values().any(|child| {
                    child.request.runtime_overrides.loop_task_id.as_deref()
                        == Some(&request.task_id)
                }) || self.queued.iter().any(|queued| {
                    queued.request.runtime_overrides.loop_task_id.as_deref()
                        == Some(&request.task_id)
                });
                let _ = request.respond_to.send(is_active);
            }
        }
    }

    fn list_agents(&self, identity: &AgentMailboxIdentity) -> ListAgentsOutput {
        let mut agents = vec![AgentRosterEntry {
            agent_id: identity.team_scope_id.clone(),
            is_root: true,
            status: "running".to_string(),
            subagent_type: None,
            description: Some("Root agent".to_string()),
            resumed_from: None,
            worktree_path: None,
        }];
        agents.extend(
            self.pending
                .values()
                .filter(|child| child.request.parent_session_id == identity.team_scope_id)
                .map(|child| AgentRosterEntry {
                    agent_id: child.request.id.clone(),
                    is_root: false,
                    status: "pending".to_string(),
                    subagent_type: Some(child.request.subagent_type.clone()),
                    description: Some(child.request.description.clone()),
                    resumed_from: child.request.resume_from.clone(),
                    worktree_path: None,
                }),
        );
        agents.extend(
            self.active
                .values()
                .filter(|child| child.request.parent_session_id == identity.team_scope_id)
                .map(|child| AgentRosterEntry {
                    agent_id: child.request.id.clone(),
                    is_root: false,
                    status: "running".to_string(),
                    subagent_type: Some(child.request.subagent_type.clone()),
                    description: Some(child.request.description.clone()),
                    resumed_from: child.resumed_from.clone(),
                    worktree_path: child.worktree_path.clone(),
                }),
        );
        agents.extend(
            self.completed
                .values()
                .filter(|child| child.request.parent_session_id == identity.team_scope_id)
                .map(|child| AgentRosterEntry {
                    agent_id: child.request.id.clone(),
                    is_root: false,
                    status: child.result.status().to_string(),
                    subagent_type: Some(child.request.subagent_type.clone()),
                    description: Some(child.request.description.clone()),
                    resumed_from: child.resumed_from.clone(),
                    worktree_path: child.worktree_path.clone(),
                }),
        );
        agents[1..].sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        ListAgentsOutput {
            team_scope_id: identity.team_scope_id.clone(),
            agents,
        }
    }

    fn resolve_message_target(
        &self,
        identity: &AgentMailboxIdentity,
        target: &str,
    ) -> Result<String, String> {
        let target = target.trim();
        if target.is_empty() {
            return Err("target must not be empty".to_string());
        }
        let target = if target.eq_ignore_ascii_case("root") || target == identity.team_scope_id {
            identity.team_scope_id.clone()
        } else {
            target.to_string()
        };
        if target == identity.agent_id {
            return Err("Cannot send an agent message to the calling agent itself. \
                 Use list_agents to see teammates, or target \"root\" for the team root."
                .to_string());
        }
        if target == identity.team_scope_id {
            return Ok(target);
        }
        let belongs_to_team = self
            .pending
            .get(&target)
            .is_some_and(|child| child.request.parent_session_id == identity.team_scope_id)
            || self
                .active
                .get(&target)
                .is_some_and(|child| child.request.parent_session_id == identity.team_scope_id)
            || self
                .completed
                .get(&target)
                .is_some_and(|child| child.request.parent_session_id == identity.team_scope_id);
        if belongs_to_team {
            Ok(target)
        } else {
            Err(format!(
                "Agent '{target}' was not found in team '{}'",
                identity.team_scope_id
            ))
        }
    }

    fn send_agent_message(
        &mut self,
        identity: &AgentMailboxIdentity,
        target: &str,
        mut message: AgentMailboxMessage,
    ) -> Result<AgentMessageSendOutput, String> {
        if message.team_scope_id != identity.team_scope_id
            || message.from_agent_id != identity.agent_id
        {
            return Err("Agent message identity did not match the calling session".to_string());
        }
        let target = self.resolve_message_target(identity, target)?;
        message.to_agent_id = target.clone();

        if target != identity.team_scope_id
            && self
                .completed
                .get(&target)
                .is_some_and(|child| child.request.parent_session_id == identity.team_scope_id)
        {
            return Err(format!(
                "Agent '{target}' has finished. Continue it with task(resume_from=\"{target}\") \
                 before sending more work."
            ));
        }

        let key = MailboxKey {
            team_scope_id: identity.team_scope_id.clone(),
            agent_id: target.clone(),
        };
        // Delivery preference: a blocked `wait_agent` waiter first. Then
        // steering messages are pushed into the recipient session live (a
        // running turn consumes them at an interjection boundary; an idle
        // session starts an agent-message turn) and only queue when the
        // recipient cannot receive yet — passive mail that nobody polls is
        // how urgent peer messages rot unread. Follow-up tasks are passive
        // by design and always queue for `wait_agent`.
        let status = if let Some(waiter) = self.mailbox_waiters.remove(&key) {
            let _ = waiter.respond_to.send(WaitAgentMessagesOutput {
                messages: vec![message.clone()],
                timed_out: false,
            });
            AgentMessageDeliveryStatus::Delivered
        } else if message.kind.steers_recipient() {
            let delivered = if target == identity.team_scope_id {
                self.runner.deliver_root_followup(&target, &message)
            } else {
                self.active
                    .get(&target)
                    .is_some_and(|child| child.control.deliver_followup(&message))
            };
            if delivered {
                AgentMessageDeliveryStatus::Delivered
            } else if target == identity.team_scope_id || self.pending.contains_key(&target) {
                self.enqueue_agent_message(key, message.clone())?;
                AgentMessageDeliveryStatus::Queued
            } else {
                return Err(format!(
                    "Agent '{target}' is not currently able to receive messages"
                ));
            }
        } else {
            self.enqueue_agent_message(key, message.clone())?;
            AgentMessageDeliveryStatus::Queued
        };

        self.runner.on_agent_message(&message, status);
        Ok(AgentMessageSendOutput {
            message_id: message.message_id,
            target_agent_id: target,
            status,
        })
    }

    /// Steering mail accepted while the recipient was still pending is
    /// delivered to the live session as soon as its control exists,
    /// preserving FIFO order among steering messages. Follow-up tasks (and
    /// anything the control refuses) stay queued for `wait_agent`.
    fn flush_mailbox_to_started_child(&mut self, key: &MailboxKey) {
        let Some(mailbox) = self.mailboxes.remove(key) else {
            return;
        };
        let Some(child) = self.active.get(&key.agent_id) else {
            self.mailboxes.insert(key.clone(), mailbox);
            return;
        };
        let mut retained = VecDeque::new();
        let mut steering_live = true;
        for mut message in mailbox {
            if message.native.is_some() && !child.control.accepts_native_message(&message) {
                self.runner
                    .on_agent_message(&message, AgentMessageDeliveryStatus::Rejected);
                continue;
            }
            if matches!(
                message.kind,
                super::types::AgentMailboxMessageKind::NativeFollowup
            ) {
                message.kind = super::types::AgentMailboxMessageKind::NativeMessage;
            }
            if steering_live
                && message.kind.steers_recipient()
                && child.control.deliver_initial_message(&message)
            {
                continue;
            }
            if message.kind.steers_recipient() {
                // The control refused a live delivery; keep later steering
                // mail queued behind it so FIFO order survives a retry.
                steering_live = false;
            }
            retained.push_back(message);
        }
        if !retained.is_empty() {
            self.mailboxes.insert(key.clone(), retained);
        }
        self.persist_native_registry(&key.team_scope_id);
    }

    fn enqueue_agent_message(
        &mut self,
        key: MailboxKey,
        message: AgentMailboxMessage,
    ) -> Result<(), String> {
        const MAX_MAILBOX_MESSAGES: usize = 128;
        let mailbox = self.mailboxes.entry(key).or_default();
        if mailbox.len() >= MAX_MAILBOX_MESSAGES {
            return Err(format!(
                "Recipient mailbox is full (maximum {MAX_MAILBOX_MESSAGES} messages)"
            ));
        }
        mailbox.push_back(message);
        Ok(())
    }

    fn wait_agent_messages(
        &mut self,
        identity: AgentMailboxIdentity,
        timeout_ms: u64,
        respond_to: oneshot::Sender<WaitAgentMessagesOutput>,
    ) {
        const MAX_DRAIN_MESSAGES: usize = 20;
        let key = MailboxKey::from(&identity);
        if let Some(mailbox) = self.mailboxes.get_mut(&key)
            && mailbox.iter().any(|message| message.native.is_none())
        {
            let mut messages = Vec::new();
            while messages.len() < MAX_DRAIN_MESSAGES {
                let Some(index) = mailbox.iter().position(|message| message.native.is_none())
                else {
                    break;
                };
                if let Some(message) = mailbox.remove(index) {
                    messages.push(message);
                }
            }
            if mailbox.is_empty() {
                self.mailboxes.remove(&key);
            }
            let _ = respond_to.send(WaitAgentMessagesOutput {
                messages,
                timed_out: false,
            });
            return;
        }
        if timeout_ms == 0 {
            let _ = respond_to.send(WaitAgentMessagesOutput {
                messages: Vec::new(),
                timed_out: true,
            });
            return;
        }
        if let Some(previous) = self.mailbox_waiters.insert(
            key,
            AgentMailboxWaiter {
                deadline: tokio::time::Instant::now()
                    + std::time::Duration::from_millis(timeout_ms),
                respond_to,
            },
        ) {
            let _ = previous.respond_to.send(WaitAgentMessagesOutput {
                messages: Vec::new(),
                timed_out: false,
            });
        }
    }

    fn handle_internal(&mut self, event: InternalEvent<R::Control>) {
        match event {
            InternalEvent::Started {
                subagent_id,
                child,
                respond_to,
            } => {
                let Some(mut pending) = self.pending.remove(&subagent_id) else {
                    let _ = respond_to.send(false);
                    return;
                };
                if pending.cancellation.is_cancelled() {
                    self.pending.insert(subagent_id, pending);
                    let _ = respond_to.send(false);
                    return;
                }
                let mailbox_key = MailboxKey {
                    team_scope_id: pending.request.parent_session_id.clone(),
                    agent_id: subagent_id.clone(),
                };
                if pending.request.runtime_overrides.native_agent.is_some()
                    && pending.handle_only
                    && let Some(reply) = pending.spawn_reply.take()
                {
                    let _ = reply.send(SubagentResult {
                        success: true,
                        backgrounded: true,
                        subagent_id: subagent_id.clone(),
                        child_session_id: child.child_session_id.clone(),
                        ..Default::default()
                    });
                }
                self.active.insert(
                    subagent_id,
                    ActiveChild {
                        request: pending.request,
                        started_at: pending.started_at,
                        cancellation: pending.cancellation,
                        spawn_reply: pending.spawn_reply,
                        foreground_deadline: pending.foreground_deadline,
                        handle_only: pending.handle_only,
                        definition_background: child.definition_background,
                        explicitly_killed: pending.explicitly_killed,
                        child_session_id: child.child_session_id,
                        persona: child.persona,
                        resumed_from: child.resumed_from,
                        child_cwd: child.child_cwd,
                        worktree_path: child.worktree_path,
                        effective_model_id: child.effective_model_id,
                        control: child.control,
                    },
                );
                let _ = respond_to.send(true);
                self.flush_mailbox_to_started_child(&mailbox_key);
            }
            InternalEvent::ResumeSource {
                source_id,
                parent_session_id,
                respond_to,
            } => {
                let source_is_active = self.pending
                        .get(&source_id)
                        .is_some_and(|child| child.request.parent_session_id == parent_session_id)
                        || self.active.get(&source_id).is_some_and(|child| {
                            child.request.parent_session_id == parent_session_id
                        })
                        // Queued spawns resolve as "still running", matching
                        // the query path's Initializing, not as missing.
                        || self.queued.iter().any(|queued| {
                            queued.request.id == source_id
                                && queued.request.parent_session_id == parent_session_id
                        });
                let lookup = if source_is_active {
                    SubagentResumeLookup::Active
                } else if let Some(child) = self.completed.get(&source_id)
                    && child.request.parent_session_id == parent_session_id
                {
                    SubagentResumeLookup::Completed(SubagentResumeSource {
                        subagent_id: child.request.id.clone(),
                        child_session_id: child.child_session_id.clone(),
                        child_cwd: child.child_cwd.clone(),
                        worktree_path: child.worktree_path.clone(),
                        snapshot_ref: child.snapshot_ref.clone(),
                        subagent_type: child.request.subagent_type.clone(),
                        persona: child.persona.clone(),
                        model_id: Some(child.effective_model_id.clone()),
                    })
                } else {
                    SubagentResumeLookup::Missing
                };
                let _ = respond_to.send(lookup);
            }
        }
    }

    fn start_child(
        &mut self,
        request: SubagentRequest,
        spawn_reply: Option<oneshot::Sender<SubagentResult>>,
        origin: StartOrigin,
    ) {
        let id = request.id.clone();
        let cancellation = request.cancel_token.clone();
        // `spawn_reply: None`: the caller was auto-backgrounded while queued.
        let handle_only = request.run_in_background || spawn_reply.is_none();
        let (queued_for, foreground_deadline) = match origin {
            StartOrigin::Direct => (
                None,
                (spawn_reply.is_some() && request.awaits_in_foreground())
                    .then(|| tokio::time::Instant::now() + self.config.foreground_budget),
            ),
            StartOrigin::Dequeued {
                queued_for,
                deadline,
            } => (Some(queued_for), deadline),
        };
        self.pending.insert(
            id.clone(),
            PendingChild {
                request: request.clone(),
                started_at: std::time::Instant::now(),
                cancellation: cancellation.clone(),
                spawn_reply,
                foreground_deadline,
                handle_only,
                explicitly_killed: false,
            },
        );
        self.running_count_changed();
        // Computed after the pending insert, so a non-workflow spawn counts
        // itself; max over launches gives a session's peak concurrency.
        let session_running = self.session_running_count(&request.parent_session_id);
        let reporter = ChildReporter {
            subagent_id: id.clone(),
            tx: self.internal_tx.clone(),
        };
        self.runs.push(TaggedFuture {
            subagent_id: id,
            future: Box::pin(
                std::panic::AssertUnwindSafe(self.runner.run(ChildRunRequest {
                    request,
                    cancellation,
                    reporter,
                    queued_for,
                    session_running,
                }))
                .catch_unwind(),
            ),
        });
    }

    /// Workflow agents draw from their run's own pool, not session slots.
    fn session_running_count(&self, parent_session_id: &str) -> usize {
        self.pending
            .values()
            .map(|child| &child.request)
            .chain(self.active.values().map(|child| &child.request))
            .filter(|request| {
                request.parent_session_id == parent_session_id && !request.owner.is_workflow()
            })
            .count()
    }

    fn finish_child(&mut self, id: &str, output: ChildRunOutput<R::CompletionData>) {
        let record = if let Some(child) = self.active.remove(id) {
            ChildRecord::Active(child)
        } else if let Some(child) = self.pending.remove(id) {
            ChildRecord::Pending(child)
        } else {
            return;
        };

        let request = record.request().clone();
        let explicitly_killed = record.explicitly_killed();
        let (
            started_at,
            child_session_id,
            persona,
            resumed_from,
            child_cwd,
            worktree_path,
            effective_model_id,
            mut spawn_reply,
            mut handle_only,
        ) = match record {
            ChildRecord::Pending(child) => (
                child.started_at,
                output.result.child_session_id.clone(),
                child.request.runtime_overrides.persona.clone(),
                child.request.resume_from.clone(),
                child.request.cwd.clone().unwrap_or_default(),
                output.result.worktree_path.clone(),
                String::new(),
                child.spawn_reply,
                child.handle_only,
            ),
            ChildRecord::Active(child) => (
                child.started_at,
                child.child_session_id,
                child.persona,
                child.resumed_from,
                child.child_cwd,
                child.worktree_path,
                child.effective_model_id,
                child.spawn_reply,
                child.handle_only,
            ),
        };

        let persisted_output_ref = self.runner.persisted_output_ref(&output.completion_data);
        let mut completed = CompletedChild {
            request: request.clone(),
            started_at,
            child_session_id,
            persona,
            resumed_from,
            child_cwd,
            worktree_path,
            snapshot_ref: output.snapshot_ref,
            persisted_output_ref,
            effective_model_id,
            result: output.result.clone(),
        };
        let snapshot = completed_snapshot(&completed, None);

        let mut waiter_delivered = false;
        for waiter in self.waiters.remove(id).unwrap_or_default() {
            waiter_delivered |= waiter.respond_to.send(Some(snapshot.clone())).is_ok();
        }

        let mut foreground_delivered = false;
        if let Some(respond_to) = spawn_reply.take() {
            let sent = respond_to.send(output.result.clone()).is_ok();
            if !handle_only {
                foreground_delivered = sent;
                handle_only = !sent;
            }
        } else if !handle_only {
            handle_only = true;
        }

        if self.config.buffer_completions
            && request.surface_completion
            && !request.owner.is_workflow()
        {
            let mut summary = completion_summary(&request, &output.result);
            if let Some(cap) = self.config.buffered_completion_output_cap {
                summary.output = super::cap_completion_output(&summary.output, cap);
            }
            self.pending_completions.push(BufferedCompletion {
                parent_session_id: request.parent_session_id.clone(),
                summary,
            });
            // Bound the buffer (drop oldest): sessions unloaded without a
            // TeardownSession cannot grow it unboundedly.
            const MAX_PENDING_COMPLETIONS: usize = 256;
            if self.pending_completions.len() > MAX_PENDING_COMPLETIONS {
                let excess = self.pending_completions.len() - MAX_PENDING_COMPLETIONS;
                self.pending_completions.drain(..excess);
            }
        }
        if completed.persisted_output_ref.is_some() {
            completed.result.output = Arc::from("");
        }

        let should_surface = request.surface_completion
            && handle_only
            && !output.result.cancelled
            && !waiter_delivered
            && !explicitly_killed;
        let disposition = CompletionDisposition {
            foreground_delivered,
            backgrounded: handle_only,
            waiter_delivered,
            explicitly_killed,
            should_surface,
        };
        self.completed.insert(id.to_owned(), completed);
        self.completed_order.push_back(id.to_owned());
        self.notify_native_completion(&request);
        self.running_count_changed();
        let workflow_run_id = request.owner.workflow_run_id().map(str::to_owned);
        let parent_session_id = request.parent_session_id.clone();
        self.runner.on_completed(ChildCompletion {
            request,
            result: output.result,
            completion_data: output.completion_data,
            disposition,
        });
        if let Some(run_id) = workflow_run_id {
            self.resolve_workflow_cancel_waiters(&run_id);
        }
        self.resolve_teardown_drain_waiters(&parent_session_id);
        self.start_queued_within_capacity();
    }

    fn finish_panicked_child(&mut self, id: &str) {
        let request = self
            .active
            .get(id)
            .map(|child| child.request.clone())
            .or_else(|| self.pending.get(id).map(|child| child.request.clone()));
        let Some(request) = request else {
            return;
        };
        tracing::error!(subagent_id = id, "subagent child runner panicked");
        self.finish_child(
            id,
            ChildRunOutput {
                result: SubagentResult {
                    success: false,
                    error: Some("Subagent runtime panicked".to_owned()),
                    subagent_id: request.id.clone(),
                    child_session_id: request.id,
                    ..Default::default()
                },
                completion_data: R::CompletionData::default(),
                snapshot_ref: None,
            },
        );
    }

    fn cancel_one(
        &mut self,
        id: &str,
        parent_session_id: Option<&str>,
        explicit: bool,
    ) -> SubagentCancelOutcome {
        if let Some(child) = self.active.get_mut(id)
            && belongs_to_session(&child.request, parent_session_id)
        {
            child.explicitly_killed |= explicit;
            child.cancellation.cancel();
            child.control.cancel();
            return SubagentCancelOutcome::Cancelled;
        }
        if let Some(child) = self.pending.get_mut(id)
            && belongs_to_session(&child.request, parent_session_id)
        {
            child.explicitly_killed |= explicit;
            child.cancellation.cancel();
            return SubagentCancelOutcome::Cancelled;
        }
        if self.remove_queued(|request| {
            request.id == id && belongs_to_session(request, parent_session_id)
        }) > 0
        {
            return SubagentCancelOutcome::Cancelled;
        }
        if let Some(child) = self.completed.get(id)
            && belongs_to_session(&child.request, parent_session_id)
        {
            return SubagentCancelOutcome::AlreadyFinished {
                status: child.result.status().to_owned(),
            };
        }
        SubagentCancelOutcome::NotFound
    }

    fn cancel_parent_prompt(&mut self, parent_prompt_id: &str, parent_session_id: Option<&str>) {
        for child in self.active.values() {
            if child.request.parent_prompt_id.as_deref() == Some(parent_prompt_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
                child.control.cancel();
            }
        }
        for child in self.pending.values() {
            if child.request.parent_prompt_id.as_deref() == Some(parent_prompt_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
            }
        }
        self.remove_queued(|request| {
            request.parent_prompt_id.as_deref() == Some(parent_prompt_id)
                && belongs_to_session(request, parent_session_id)
        });
    }

    /// All non-workflow children for the parent session (user Stop / Esc).
    ///
    /// Requires a concrete session id — unbound (`None`) is rejected so a
    /// wildcard cannot cancel every session on a shared coordinator.
    fn cancel_parent_session(&mut self, parent_session_id: Option<&str>) -> SubagentCancelOutcome {
        let Some(parent_session_id) = parent_session_id else {
            return SubagentCancelOutcome::NotFound;
        };
        self.spawn_blocked_sessions
            .insert(parent_session_id.to_owned());
        for child in self.active.values() {
            if child.request.parent_session_id == parent_session_id
                && !child.request.owner.is_workflow()
            {
                child.cancellation.cancel();
                child.control.cancel();
            }
        }
        for child in self.pending.values() {
            if child.request.parent_session_id == parent_session_id
                && !child.request.owner.is_workflow()
            {
                child.cancellation.cancel();
            }
        }
        self.remove_queued(|request| {
            request.parent_session_id == parent_session_id && !request.owner.is_workflow()
        });
        SubagentCancelOutcome::Cancelled
    }

    fn teardown_session_children(&mut self, parent_session_id: &str) {
        let mut cancelled = 0;
        for child in self.active.values_mut() {
            if child.request.parent_session_id == parent_session_id {
                // Parent is gone: do not rebuffer this completion for a later
                // resume of the same session id.
                child.request.surface_completion = false;
                child.cancellation.cancel();
                child.control.cancel();
                cancelled += 1;
            }
        }
        for child in self.pending.values_mut() {
            if child.request.parent_session_id == parent_session_id {
                child.request.surface_completion = false;
                child.cancellation.cancel();
                cancelled += 1;
            }
        }
        // Parent is gone here too: a queued spawn's cancelled completion must
        // not be rebuffered for a later resume of the same session id.
        for queued in self.queued.iter_mut() {
            if queued.request.parent_session_id == parent_session_id {
                queued.request.surface_completion = false;
            }
        }
        cancelled += self.remove_queued(|request| request.parent_session_id == parent_session_id);
        if cancelled > 0 {
            tracing::info!(
                parent_session_id,
                cancelled,
                "cancelled subagents on session teardown"
            );
        }
    }

    /// Whether any child (active, pending, or queued) still belongs to the
    /// session. Unlike [`Self::session_running_count`] it counts every owner,
    /// including workflow, since teardown drains all children.
    fn session_has_children(&self, parent_session_id: &str) -> bool {
        self.active
            .values()
            .map(|child| &child.request)
            .chain(self.pending.values().map(|child| &child.request))
            .chain(self.queued.iter().map(|queued| queued.request.as_ref()))
            .any(|request| request.parent_session_id == parent_session_id)
    }

    /// Latch spawn admission closed for a delete-path teardown and park the
    /// responder until the last child drains (or the backstop deadline fires).
    fn begin_teardown_drain(&mut self, parent_session_id: String, respond_to: oneshot::Sender<()>) {
        let deadline = tokio::time::Instant::now() + TEARDOWN_DRAIN_MAX;
        self.spawn_blocked_sessions
            .insert(parent_session_id.clone());
        self.teardown_drains
            .entry(parent_session_id)
            .or_insert_with(|| TeardownDrain {
                waiters: Vec::new(),
                deadline,
            })
            .waiters
            .push(respond_to);
    }

    /// Clear a delete-path hold: reopen the session's spawn admission and
    /// resolve every parked drain responder.
    fn clear_teardown_drain(&mut self, parent_session_id: &str) {
        self.spawn_blocked_sessions.remove(parent_session_id);
        if let Some(drain) = self.teardown_drains.remove(parent_session_id) {
            for respond_to in drain.waiters {
                let _ = respond_to.send(());
            }
        }
    }

    fn resolve_teardown_drain_waiters(&mut self, parent_session_id: &str) {
        // Cheap precondition (one lookup) before the three-collection scan on
        // every child completion: only a delete-path teardown holds a drain.
        if !self.teardown_drains.contains_key(parent_session_id) {
            return;
        }
        if self.session_has_children(parent_session_id) {
            return;
        }
        self.clear_teardown_drain(parent_session_id);
    }

    fn cancel_workflow_children(&mut self, run_id: &str, parent_session_id: Option<&str>) {
        for child in self.active.values() {
            if child.request.owner.workflow_run_id() == Some(run_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
                child.control.cancel();
            }
        }
        for child in self.pending.values() {
            if child.request.owner.workflow_run_id() == Some(run_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
            }
        }
    }

    fn resolve_workflow_cancel_waiters(&mut self, run_id: &str) {
        if workflow_outstanding(&self.pending, &self.active, run_id) != 0 {
            return;
        }
        for respond_to in self
            .workflow_cancel_waiters
            .remove(run_id)
            .unwrap_or_default()
        {
            let _ = respond_to.send(SubagentCancelOutcome::Cancelled);
        }
    }

    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        self.pending
            .values()
            .filter_map(|child| child.foreground_deadline)
            .chain(
                self.active
                    .values()
                    .filter_map(|child| child.foreground_deadline),
            )
            .chain(
                self.queued
                    .iter()
                    .filter_map(|queued| queued.caller.deadline()),
            )
            .chain(
                // Anchored to the last sweep, not `now`: a stream of other
                // wakes must not keep pushing the next sweep further out.
                (!self.queued.is_empty()).then(|| self.last_queued_reap + QUEUED_REAP_INTERVAL),
            )
            .chain(
                self.waiters
                    .values()
                    .flatten()
                    .map(|waiter| waiter.deadline),
            )
            .chain(self.mailbox_waiters.values().map(|waiter| waiter.deadline))
            .chain(self.native_waiters.values().map(|waiter| waiter.deadline))
            .chain(self.teardown_drains.values().map(|drain| drain.deadline))
            .min()
    }

    fn reap_abandoned_callers(&mut self) {
        self.last_queued_reap = tokio::time::Instant::now();
        for child in self.pending.values_mut() {
            background_if_caller_gone(child);
        }
        for child in self.active.values_mut() {
            background_if_caller_gone(child);
        }
        // The queued leg of the same sweep.
        for queued in self.queued.iter_mut() {
            if let QueuedCaller::Awaiting { result_tx, .. } = &queued.caller
                && result_tx.is_closed()
            {
                queued.caller = QueuedCaller::Backgrounded;
            }
        }
        self.remove_queued(|request| request.cancel_token.is_cancelled());
    }

    fn process_deadlines(&mut self) {
        self.expire_native_waiters();
        self.reap_abandoned_callers();
        let now = tokio::time::Instant::now();
        // Backstop: a delete-path hold whose drain deadline elapsed force-clears
        // so a child that never finishes cannot block spawns forever.
        let stale: Vec<String> = self
            .teardown_drains
            .iter()
            .filter(|(_, drain)| drain.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            tracing::warn!(
                parent_session_id = %id,
                still_draining = self.session_has_children(&id),
                "teardown drain deadline elapsed; force-reopening spawn admission",
            );
            self.clear_teardown_drain(&id);
        }
        for child in self.pending.values_mut() {
            background_at_deadline(child, now, self.config.foreground_budget);
        }
        for child in self.active.values_mut() {
            background_at_deadline(child, now, self.config.foreground_budget);
        }
        // The spawn stays queued; only its caller is handed off.
        for queued in self.queued.iter_mut() {
            if queued
                .caller
                .deadline()
                .is_none_or(|deadline| deadline > now)
            {
                continue;
            }
            let caller = std::mem::replace(&mut queued.caller, QueuedCaller::Backgrounded);
            let Some(result_tx) = caller.into_spawn_reply() else {
                continue;
            };
            tracing::warn!(
                subagent_id = %queued.request.id,
                budget_ms = self.config.foreground_budget.as_millis() as u64,
                "queued subagent exceeded await budget; auto-backgrounding (spawn stays queued)",
            );
            let _ = result_tx.send(SubagentResult {
                backgrounded: true,
                subagent_id: queued.request.id.clone(),
                child_session_id: queued.request.id.clone(),
                ..Default::default()
            });
        }

        let ids: Vec<_> = self.waiters.keys().cloned().collect();
        for id in ids {
            let waiters = self.waiters.remove(&id).unwrap_or_default();
            let (due, live): (Vec<_>, Vec<_>) = waiters
                .into_iter()
                .partition(|waiter| waiter.deadline <= now);
            if !live.is_empty() {
                self.waiters.insert(id.clone(), live);
            }
            for waiter in due {
                if waiter.respond_to.is_closed() {
                    continue;
                }
                if self.active.contains_key(&id) {
                    self.queue_active_progress(&id, ProgressTarget::Query(waiter.respond_to));
                } else {
                    let _ = waiter.respond_to.send(self.ready_snapshot(&id));
                }
            }
        }
        let expired_mailboxes: Vec<_> = self
            .mailbox_waiters
            .iter()
            .filter(|(_, waiter)| waiter.deadline <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired_mailboxes {
            if let Some(waiter) = self.mailbox_waiters.remove(&key) {
                let _ = waiter.respond_to.send(WaitAgentMessagesOutput {
                    messages: Vec::new(),
                    timed_out: true,
                });
            }
        }
    }

    fn running_count_changed(&self) {
        self.runner
            .running_count_changed(self.pending.len() + self.active.len());
    }

    fn cancel_all_children(&self) {
        for child in self.active.values() {
            child.cancellation.cancel();
            child.control.cancel();
        }
        for child in self.pending.values() {
            child.cancellation.cancel();
        }
    }
}

fn belongs_to_session(request: &SubagentRequest, parent_session_id: Option<&str>) -> bool {
    parent_session_id.is_none_or(|id| request.parent_session_id == id)
}

impl<R: ChildRunner> Drop for SubagentCoordinator<R> {
    fn drop(&mut self) {
        // Not `remove_queued`: that routes through `finish_child`, which runs
        // host completion callbacks — off-limits from a destructor (the
        // host's storage may already be tearing down).
        self.resolve_queued_at_drop();
        self.cancel_all_children();
    }
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;
