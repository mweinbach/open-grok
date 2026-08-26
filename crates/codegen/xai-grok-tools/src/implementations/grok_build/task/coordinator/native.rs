use super::super::types::{
    NativeAgentOperation, NativeAgentRecord, NativeAgentRequest, NativeAgentSpawn,
    SubagentContextRequest, SubagentOwner, SubagentRuntimeOverrides, SubagentSpawnRequest,
};
use super::*;
use serde_json::{Value, json};

pub(super) struct NativeWaiter {
    pub(super) deadline: tokio::time::Instant,
    respond_to: oneshot::Sender<Result<Value, String>>,
}

pub fn valid_task_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "root"
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

impl<R: ChildRunner> SubagentCoordinator<R> {
    fn ensure_native_registry(&mut self, team: &str) -> Result<(), String> {
        if self.native_loaded.contains(team) {
            return Ok(());
        }
        let records = self.runner.load_native_agents(team)?;
        for record in records.into_iter().take(MAX_COMPLETED_ENTRIES / 2) {
            if !record
                .task_name
                .strip_prefix("/root/")
                .is_some_and(valid_task_name)
                || uuid::Uuid::parse_str(&record.agent_id).is_err()
            {
                continue;
            }
            let key = MailboxKey {
                team_scope_id: team.to_owned(),
                agent_id: record.task_name.clone(),
            };
            let mailbox_key = MailboxKey {
                team_scope_id: team.to_owned(),
                agent_id: record.agent_id.clone(),
            };
            let messages = record
                .mailbox
                .iter()
                .filter(|message| {
                    message.team_scope_id == team
                        && message.to_agent_id == record.agent_id
                        && message.native.is_some()
                })
                .take(128)
                .cloned()
                .collect();
            self.mailboxes.insert(mailbox_key, messages);
            self.native_names
                .insert(key.clone(), record.agent_id.clone());
            self.native_records.insert(key, record);
        }
        self.native_loaded.insert(team.to_owned());
        Ok(())
    }

    pub(super) fn persist_native_registry(&self, team: &str) {
        if !self.native_loaded.contains(team) {
            return;
        }
        let records: Vec<_> = self
            .native_records
            .iter()
            .filter(|(key, _)| key.team_scope_id == team)
            .map(|(_, record)| {
                let mut record = record.clone();
                let key = MailboxKey {
                    team_scope_id: team.to_owned(),
                    agent_id: record.agent_id.clone(),
                };
                record.mailbox = self
                    .mailboxes
                    .get(&key)
                    .map(|mailbox| mailbox.iter().cloned().collect())
                    .unwrap_or_default();
                record
            })
            .collect();
        if let Err(error) = self.runner.save_native_agents(team, &records) {
            tracing::warn!(%error, "Could not persist native agent registry");
        }
    }

    pub(super) fn register_native_spawn(
        &mut self,
        request: &mut SubagentRequest,
    ) -> Result<(), String> {
        if request.runtime_overrides.native_agent.is_none() {
            return Ok(());
        }
        self.ensure_native_registry(&request.parent_session_id)?;
        let Some(options) = request.runtime_overrides.native_agent.as_mut() else {
            return Ok(());
        };
        let name = options
            .task_name
            .strip_prefix("/root/")
            .unwrap_or(&options.task_name);
        if !valid_task_name(name) {
            return Err("task_name must contain lowercase letters, digits, or underscores; root is reserved".to_owned());
        }
        let path = format!("/root/{name}");
        let key = MailboxKey {
            team_scope_id: request.parent_session_id.clone(),
            agent_id: path.clone(),
        };
        if let Some(existing) = self.native_names.get(&key) {
            if request.resume_from.as_deref() != Some(existing) {
                return Err(format!(
                    "Task '{path}' already exists; use followup_task to reuse it"
                ));
            }
        } else if self
            .native_names
            .keys()
            .filter(|key| key.team_scope_id == request.parent_session_id)
            .count()
            >= MAX_COMPLETED_ENTRIES / 2
        {
            return Err("Named agent limit reached for this team".to_owned());
        }
        if let Some(message) = options.message.as_mut() {
            message.to_agent_id = request.id.clone();
            if let Some(native) = message.native.as_mut() {
                native.author = "/root".to_owned();
                native.recipient = path.clone();
            }
        }
        options.task_name = path;
        self.native_records.insert(
            key.clone(),
            NativeAgentRecord {
                task_name: options.task_name.clone(),
                agent_id: request.id.clone(),
                agent_type: request.subagent_type.clone(),
                model: request.runtime_overrides.model.clone(),
                reasoning_effort: request.runtime_overrides.reasoning_effort.clone(),
                service_tier: options.service_tier.clone(),
                cwd: request.cwd.clone(),
                mailbox: Vec::new(),
            },
        );
        self.native_names.insert(key, request.id.clone());
        if request.resume_from.is_none() {
            self.persist_native_registry(&request.parent_session_id);
        }
        Ok(())
    }

    fn native_path(&self, identity: &AgentMailboxIdentity, id: &str) -> String {
        if id == identity.team_scope_id {
            return "/root".to_owned();
        }
        self.native_names
            .iter()
            .find(|(key, current)| {
                key.team_scope_id == identity.team_scope_id && current.as_str() == id
            })
            .map(|(key, _)| key.agent_id.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    fn native_target(
        &self,
        identity: &AgentMailboxIdentity,
        target: &str,
    ) -> Result<String, String> {
        if target == "/root" {
            return self.resolve_message_target(identity, "root");
        }
        let path = if target.starts_with('/') {
            target.to_owned()
        } else {
            format!(
                "{}/{}",
                self.native_path(identity, &identity.agent_id),
                target
            )
        };
        let key = MailboxKey {
            team_scope_id: identity.team_scope_id.clone(),
            agent_id: path,
        };
        let resolved = self
            .native_names
            .get(&key)
            .map(String::as_str)
            .unwrap_or(target);
        if resolved != identity.agent_id
            && self.native_records.iter().any(|(key, record)| {
                key.team_scope_id == identity.team_scope_id && record.agent_id == resolved
            })
        {
            return Ok(resolved.to_owned());
        }
        self.resolve_message_target(identity, resolved)
    }

    pub(super) fn handle_native_agent(&mut self, request: NativeAgentRequest) {
        let NativeAgentRequest {
            identity,
            operation,
            respond_to,
        } = request;
        let known = identity.agent_id == identity.team_scope_id
            || self
                .active
                .get(&identity.agent_id)
                .is_some_and(|child| child.request.parent_session_id == identity.team_scope_id);
        if !known {
            let _ = respond_to.send(Err("Calling agent is not active in this team".to_owned()));
            return;
        }
        if let Err(error) = self.ensure_native_registry(&identity.team_scope_id) {
            let _ = respond_to.send(Err(error));
            return;
        }
        let result = match operation {
            NativeAgentOperation::List { path_prefix } => {
                let prefix = path_prefix.as_deref().unwrap_or("/root");
                if prefix != "/root" && !prefix.strip_prefix("/root/").is_some_and(valid_task_name)
                {
                    Err(
                        "path_prefix must be a canonical task path without a trailing slash"
                            .to_owned(),
                    )
                } else {
                    let mut agents: Vec<_> = self.list_agents(&identity).agents.into_iter().filter_map(|agent| {
                        let path = self.native_path(&identity, &agent.agent_id);
                        if path != prefix && !path.strip_prefix(prefix).is_some_and(|tail| tail.starts_with('/')) {
                            return None;
                        }
                        if !agent.is_root && !self.native_names.values().any(|id| id == &agent.agent_id) {
                            return None;
                        }
                        Some(json!({"task_name": path, "agent_id": agent.agent_id, "status": agent.status, "agent_type": agent.subagent_type, "worktree_path": agent.worktree_path}))
                    }).collect();
                    for (key, record) in &self.native_records {
                        if key.team_scope_id == identity.team_scope_id
                            && (record.task_name == prefix
                                || record
                                    .task_name
                                    .strip_prefix(prefix)
                                    .is_some_and(|tail| tail.starts_with('/')))
                            && !agents
                                .iter()
                                .any(|agent| agent["agent_id"].as_str() == Some(&record.agent_id))
                        {
                            agents.push(json!({"task_name": record.task_name, "agent_id": record.agent_id, "agent_type": record.agent_type, "status": "unloaded"}));
                        }
                    }
                    Ok(json!({"agents": agents}))
                }
            }
            NativeAgentOperation::Message { target, message } => {
                match self.send_native_message(&identity, &target, message) {
                    Ok((value, Some(started))) => {
                        tokio::spawn(async move {
                            let result = match started.await {
                                Ok(result) if result.success => Ok(value),
                                Ok(result) => Err(result
                                    .error
                                    .unwrap_or_else(|| "Agent could not be resumed".to_owned())),
                                Err(_) => {
                                    Err("Agent initialization response was dropped".to_owned())
                                }
                            };
                            let _ = respond_to.send(result);
                        });
                        return;
                    }
                    Ok((value, None)) => Ok(value),
                    Err(error) => Err(error),
                }
            }
            NativeAgentOperation::Interrupt { target } => {
                self.native_target(&identity, &target).and_then(|target| {
                    if target == identity.team_scope_id {
                        return Err("The root agent cannot be interrupted by an agent".to_owned());
                    }
                    let previous_status = if let Some(child) = self.active.get(&target) {
                        if !child.control.interrupt() {
                            return Err(
                                "This host cannot interrupt an agent without terminating it"
                                    .to_owned(),
                            );
                        }
                        "running".to_owned()
                    } else if self.pending.contains_key(&target) {
                        "pending".to_owned()
                    } else {
                        self.completed
                            .get(&target)
                            .map(|child| child.result.status().to_owned())
                            .unwrap_or_else(|| "unloaded".to_owned())
                    };
                    Ok(json!({"previous_status": previous_status}))
                })
            }
            NativeAgentOperation::Wait { timeout_ms } => {
                let key = MailboxKey::from(&identity);
                if let Some(updates) = self.native_activity.remove(&key) {
                    Ok(json!({"updates": updates, "timed_out": false}))
                } else if timeout_ms == 0 {
                    Ok(json!({"updates": [], "timed_out": true}))
                } else {
                    if let Some(previous) = self.native_waiters.insert(
                        key,
                        NativeWaiter {
                            deadline: tokio::time::Instant::now()
                                + std::time::Duration::from_millis(timeout_ms.min(600_000)),
                            respond_to,
                        },
                    ) {
                        let _ = previous.respond_to.send(Ok(
                            json!({"updates": [], "interrupted": true, "timed_out": false}),
                        ));
                    }
                    return;
                }
            }
        };
        let _ = respond_to.send(result);
    }

    fn send_native_message(
        &mut self,
        identity: &AgentMailboxIdentity,
        target: &str,
        mut message: AgentMailboxMessage,
    ) -> Result<(Value, Option<oneshot::Receiver<SubagentResult>>), String> {
        let mut started = None;
        if message.team_scope_id != identity.team_scope_id
            || message.from_agent_id != identity.agent_id
            || message.native.is_none()
        {
            return Err("Native message identity does not match the calling session".to_owned());
        }
        let mut target = self.native_target(identity, target)?;
        if target == identity.team_scope_id && message.kind.triggers_turn() {
            return Err("Follow-up tasks cannot target the root agent".to_owned());
        }
        let author = self.native_path(identity, &identity.agent_id);
        let recipient = self.native_path(identity, &target);
        if let Some(native) = message.native.as_mut() {
            native.author = author.clone();
            native.recipient = recipient;
        }
        let reusable = self
            .completed
            .get(&target)
            .map(|completed| completed.request.clone())
            .or_else(|| {
                if self.pending.contains_key(&target)
                    || self.active.contains_key(&target)
                    || self.queued.contains_id(&target)
                {
                    return None;
                }
                self.native_records
                    .iter()
                    .find(|(key, record)| {
                        key.team_scope_id == identity.team_scope_id && record.agent_id == target
                    })
                    .map(|(_, record)| SubagentRequest {
                        id: target.clone(),
                        parent_session_id: identity.team_scope_id.clone(),
                        parent_prompt_id: None,
                        prompt: String::new(),
                        description: record.task_name.clone(),
                        subagent_type: record.agent_type.clone(),
                        swarm: None,
                        resume_from: None,
                        cwd: record.cwd.clone(),
                        runtime_overrides: SubagentRuntimeOverrides {
                            model: record.model.clone(),
                            reasoning_effort: record.reasoning_effort.clone(),
                            native_agent: Some(NativeAgentSpawn {
                                task_name: record.task_name.clone(),
                                service_tier: record.service_tier.clone(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        run_in_background: true,
                        surface_completion: true,
                        await_to_completion: false,
                        context: SubagentContextRequest::FRESH,
                        owner: SubagentOwner::Task,
                        cancel_token: tokio_util::sync::CancellationToken::new(),
                    })
            });
        if let Some(reusable) = reusable {
            if message.kind.triggers_turn() {
                if reusable.runtime_overrides.native_agent.is_none() {
                    return Err(
                        "Only named native agents can be reused with followup_task".to_owned()
                    );
                }
                if self
                    .spawn_blocked_sessions
                    .contains(&identity.team_scope_id)
                {
                    return Err("The parent session is stopped".to_owned());
                }
                let mut resumed = reusable;
                resumed.id = uuid::Uuid::now_v7().to_string();
                resumed.resume_from = Some(target.clone());
                resumed.prompt =
                    "Continue with the new follow-up task in your agent inbox.".to_owned();
                if identity.agent_id == identity.team_scope_id {
                    resumed.parent_prompt_id = message
                        .native
                        .as_ref()
                        .and_then(|native| native.trigger_prompt_id.clone());
                }
                resumed.cancel_token = tokio_util::sync::CancellationToken::new();
                resumed.run_in_background = true;
                if let Some(native) = resumed.runtime_overrides.native_agent.as_mut() {
                    native.message = None;
                }
                let new_target = resumed.id.clone();
                let (result_tx, response_rx) = oneshot::channel();
                started = Some(response_rx);
                self.handle_spawn(SubagentSpawnRequest {
                    request: Box::new(resumed),
                    result_tx,
                });
                if !self.pending.contains_key(&new_target)
                    && !self.active.contains_key(&new_target)
                    && !self.queued.contains_id(&new_target)
                {
                    return Err("Agent could not be resumed".to_owned());
                }
                let old_key = MailboxKey {
                    team_scope_id: identity.team_scope_id.clone(),
                    agent_id: target,
                };
                let new_key = MailboxKey {
                    team_scope_id: identity.team_scope_id.clone(),
                    agent_id: new_target.clone(),
                };
                if let Some(mailbox) = self.mailboxes.remove(&old_key) {
                    self.mailboxes
                        .entry(new_key)
                        .or_default()
                        .extend(mailbox.into_iter().map(|mut message| {
                            message.to_agent_id = new_target.clone();
                            message
                        }));
                }
                target = new_target;
            }
        }
        message.to_agent_id = target.clone();
        let key = MailboxKey {
            team_scope_id: identity.team_scope_id.clone(),
            agent_id: target.clone(),
        };
        let delivered = if target == identity.team_scope_id {
            self.runner.deliver_root_followup(&target, &message)
        } else if let Some(child) = self.active.get(&target) {
            if !child.control.accepts_native_message(&message) {
                return Err(
                    "Encrypted agent messages cannot cross the target provider boundary".to_owned(),
                );
            }
            child.control.deliver_followup(&message)
        } else {
            false
        };
        if !delivered {
            if target == identity.team_scope_id {
                return Err(
                    "The root agent is unavailable or cannot receive this provider-private message"
                        .to_owned(),
                );
            }
            self.enqueue_agent_message(key.clone(), message.clone())?;
        }
        let status = if delivered {
            AgentMessageDeliveryStatus::Delivered
        } else {
            AgentMessageDeliveryStatus::Queued
        };
        if delivered && let Some(waiter) = self.mailbox_waiters.remove(&key) {
            let _ = waiter.respond_to.send(WaitAgentMessagesOutput {
                messages: Vec::new(),
                timed_out: false,
            });
        }
        self.runner.on_agent_message(&message, status);
        self.record_native_activity(key, author);
        self.persist_native_registry(&identity.team_scope_id);
        Ok((
            json!({"message_id": message.message_id, "status": status}),
            started,
        ))
    }

    fn record_native_activity(&mut self, key: MailboxKey, source: String) {
        if let Some(waiter) = self.native_waiters.remove(&key) {
            let result = Ok(json!({"updates": [source.clone()], "timed_out": false}));
            if waiter.respond_to.send(result).is_ok() {
                return;
            }
        }
        let activity = self.native_activity.entry(key).or_default();
        if !activity.contains(&source) {
            activity.push(source);
        }
    }

    pub(super) fn notify_native_completion(&mut self, request: &SubagentRequest) {
        if let Some(native) = &request.runtime_overrides.native_agent {
            self.record_native_activity(
                MailboxKey {
                    team_scope_id: request.parent_session_id.clone(),
                    agent_id: request.parent_session_id.clone(),
                },
                native.task_name.clone(),
            );
            if self
                .completed
                .get(&request.id)
                .is_some_and(|child| child.effective_model_id.is_empty())
            {
                let name_key = MailboxKey {
                    team_scope_id: request.parent_session_id.clone(),
                    agent_id: native.task_name.clone(),
                };
                let mailbox_key = MailboxKey {
                    team_scope_id: request.parent_session_id.clone(),
                    agent_id: request.id.clone(),
                };
                let messages = self.mailboxes.remove(&mailbox_key).unwrap_or_default();
                if let Some(source) = &request.resume_from {
                    self.native_names.insert(name_key.clone(), source.clone());
                    if let Some(record) = self.native_records.get_mut(&name_key) {
                        record.agent_id = source.clone();
                    }
                    let key = MailboxKey {
                        team_scope_id: request.parent_session_id.clone(),
                        agent_id: source.clone(),
                    };
                    for mut message in messages {
                        if message.kind.triggers_turn() {
                            self.runner
                                .on_agent_message(&message, AgentMessageDeliveryStatus::Rejected);
                        } else {
                            message.to_agent_id = source.clone();
                            self.mailboxes
                                .entry(key.clone())
                                .or_default()
                                .push_back(message);
                        }
                    }
                } else {
                    self.native_names.remove(&name_key);
                    self.native_records.remove(&name_key);
                    for message in messages {
                        self.runner
                            .on_agent_message(&message, AgentMessageDeliveryStatus::Rejected);
                    }
                }
                self.persist_native_registry(&request.parent_session_id);
                return;
            }
            let key = MailboxKey {
                team_scope_id: request.parent_session_id.clone(),
                agent_id: request.id.clone(),
            };
            let followup = self.mailboxes.get_mut(&key).and_then(|mailbox| {
                let index = mailbox
                    .iter()
                    .rposition(|message| message.kind.triggers_turn())?;
                mailbox.remove(index)
            });
            if let Some(message) = followup {
                let identity = AgentMailboxIdentity {
                    team_scope_id: request.parent_session_id.clone(),
                    agent_id: message.from_agent_id.clone(),
                };
                if let Err(error) = self.send_native_message(&identity, &request.id, message) {
                    tracing::warn!(%error, "Could not resume agent for queued follow-up");
                }
            }
            self.persist_native_registry(&request.parent_session_id);
        }
    }

    pub(super) fn expire_native_waiters(&mut self) {
        let now = tokio::time::Instant::now();
        for (_, waiter) in self
            .native_waiters
            .extract_if(|_, waiter| waiter.deadline <= now || waiter.respond_to.is_closed())
        {
            let _ = waiter
                .respond_to
                .send(Ok(json!({"updates": [], "timed_out": true})));
        }
    }
}
