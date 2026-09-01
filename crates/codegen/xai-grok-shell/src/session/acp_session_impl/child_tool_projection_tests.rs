use super::*;

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: Some(format!("description for {name}")),
        parameters: serde_json::json!({"type": "object", "required": ["text"]}),
    }
}

#[test]
fn both_projections_strip_canonical_and_renamed_root_only_tools() {
    for projection in [
        ChildToolProjection::Rebuilt,
        ChildToolProjection::VerbatimMirror,
    ] {
        let projected = child_safe_tool_specs(
            [
                "read_file",
                "ask_user_question",
                "workflow",
                SEND_SUBAGENT_MESSAGE_TOOL_NAME,
                "renamed_parent_message",
                "renamed_question",
                "renamed_workflow",
            ]
            .map(tool)
            .into(),
            projection,
            |name| match name {
                "renamed_parent_message" => Some(ToolKind::ActiveAgentMessage),
                "renamed_question" => Some(ToolKind::AskUser),
                "renamed_workflow" => Some(ToolKind::Workflow),
                _ => None,
            },
        );
        assert_eq!(
            serde_json::to_vec(&projected).unwrap(),
            serde_json::to_vec(&vec![tool("read_file")]).unwrap(),
        );
    }
}

#[test]
fn projection_preserves_flat_team_and_native_agent_tools_byte_for_byte() {
    let parent = [
        "read_file",
        "list_agents",
        "send_message",
        "wait_agent",
        "followup_task",
    ]
    .map(tool)
    .to_vec();
    let projected =
        child_safe_tool_specs(parent.clone(), ChildToolProjection::VerbatimMirror, |_| {
            None
        });
    assert_eq!(
        serde_json::to_vec(&projected).unwrap(),
        serde_json::to_vec(&parent).unwrap(),
    );
}
