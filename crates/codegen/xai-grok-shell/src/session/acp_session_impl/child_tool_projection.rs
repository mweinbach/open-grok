use xai_grok_sampling_types::ToolSpec;
use xai_grok_tools::implementations::grok_build::SEND_SUBAGENT_MESSAGE_TOOL_NAME;
use xai_grok_tools::types::tool::ToolKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildToolProjection {
    Rebuilt,
    VerbatimMirror,
}

pub(super) fn child_safe_tool_specs(
    specs: Vec<ToolSpec>,
    projection: ChildToolProjection,
    kind_for_name: impl Fn(&str) -> Option<ToolKind>,
) -> Vec<ToolSpec> {
    match projection {
        ChildToolProjection::Rebuilt | ChildToolProjection::VerbatimMirror => specs
            .into_iter()
            .filter(|spec| {
                !matches!(
                    kind_for_name(&spec.name),
                    Some(ToolKind::ActiveAgentMessage | ToolKind::AskUser | ToolKind::Workflow)
                ) && !matches!(spec.name.as_str(), "ask_user_question" | "workflow")
                    && spec.name != SEND_SUBAGENT_MESSAGE_TOOL_NAME
            })
            .collect(),
    }
}

#[cfg(test)]
#[path = "child_tool_projection_tests.rs"]
mod tests;
