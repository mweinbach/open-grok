use super::*;
use xai_agent_lifecycle::{InputPolicy, QueuePolicy, ShutdownPolicy};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InputOrigin(PromptOrigin);

impl InputOrigin {
    pub(crate) fn from_prompt_id(prompt_id: &str) -> Self {
        Self(PromptOrigin::from_prompt_id(prompt_id))
    }

    pub(crate) const fn new(origin: PromptOrigin) -> Self {
        Self(origin)
    }

    pub(crate) fn policy(&self) -> InputPolicy {
        self.0.policy()
    }

    pub(crate) const fn as_prompt_origin(&self) -> &PromptOrigin {
        &self.0
    }

    pub(crate) fn is_synthetic(&self) -> bool {
        self.0.is_synthetic()
    }

    pub(crate) fn is_preemptible_runtime_wake(&self) -> bool {
        !matches!(self.policy().shutdown, ShutdownPolicy::Drain)
    }

    pub(crate) fn completion_id(&self) -> Option<&str> {
        self.0.completion_id()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QueueMutationPolicy {
    visible: bool,
    editable: bool,
}

impl QueueMutationPolicy {
    pub(crate) const fn new(visible: bool, editable: bool) -> Self {
        Self { visible, editable }
    }

    pub(crate) const fn editable() -> Self {
        Self::new(true, true)
    }

    pub(crate) const fn hidden() -> Self {
        Self::new(false, false)
    }

    pub(crate) fn is_visible(self) -> bool {
        self.visible
    }

    pub(crate) fn is_editable(self) -> bool {
        self.editable
    }

    pub(crate) fn is_protected(self) -> bool {
        self.visible && !self.editable
    }

    pub(crate) fn from_input_origin(origin: &InputOrigin) -> Self {
        match origin.policy().queue {
            QueuePolicy::VisibleProtected => Self::new(true, false),
            QueuePolicy::VisibleEditable => Self::editable(),
            QueuePolicy::Hidden => Self::hidden(),
        }
    }
}

impl InputItem {
    fn queue_mutation_policy(&self) -> QueueMutationPolicy {
        QueueMutationPolicy::from_input_origin(&InputOrigin::new(self.origin.clone()))
    }
    pub(crate) fn is_queue_visible(&self) -> bool {
        self.queue_mutation_policy().is_visible()
    }

    pub(crate) fn is_queue_editable(&self) -> bool {
        self.queue_mutation_policy().is_editable()
    }

    pub(crate) fn is_queue_protected(&self) -> bool {
        self.queue_mutation_policy().is_protected()
    }

    pub(crate) fn editable_queue_meta_matches(
        &self,
        id: &str,
        expected_version: u64,
        owner: Option<&str>,
    ) -> bool {
        self.is_queue_editable()
            && self.queue_meta.as_ref().is_some_and(|meta| {
                meta.id == id
                    && meta.version == expected_version
                    && owner.is_none_or(|expected| meta.owner.as_deref() == Some(expected))
            })
    }

    pub(crate) fn has_queue_id(&self, id: &str) -> bool {
        self.queue_meta.as_ref().is_some_and(|meta| meta.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_message_rows_are_visible_but_never_user_editable() {
        let origin = InputOrigin::new(PromptOrigin::ParentAgentMessage {
            message_id: "message".into(),
            sender_session_id: "parent".into(),
        });
        let policy = QueueMutationPolicy::from_input_origin(&origin);
        assert!(policy.is_visible());
        assert!(policy.is_protected());
        assert!(!policy.is_editable());
        assert!(!origin.is_preemptible_runtime_wake());
    }

    #[test]
    fn user_rows_remain_editable_and_agent_mail_stays_untrusted() {
        let user = QueueMutationPolicy::from_input_origin(&InputOrigin::new(PromptOrigin::User));
        assert!(user.is_editable());
        assert!(!user.is_protected());
        let peer = InputOrigin::new(PromptOrigin::AgentMessage {
            message_id: "peer".into(),
        });
        assert!(peer.is_synthetic());
        assert!(!QueueMutationPolicy::from_input_origin(&peer).is_editable());
        assert!(peer.completion_id().is_none());
    }
}
