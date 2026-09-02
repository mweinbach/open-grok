//! Grok Build Mission tools.

pub mod dismiss_handoff_items;
pub mod end_feature_run;
pub mod inspect_mission_readiness;
pub mod propose_mission;
pub mod start_mission_run;

pub use dismiss_handoff_items::{
    DISMISS_HANDOFF_ITEMS_TOOL_NAME, DismissHandoffItemsInput, DismissHandoffItemsOutput,
    DismissHandoffItemsTool,
};
pub use end_feature_run::{
    END_FEATURE_RUN_TOOL_NAME, EndFeatureRunInput, EndFeatureRunOutput, EndFeatureRunTool,
};
pub use inspect_mission_readiness::{
    INSPECT_MISSION_READINESS_TOOL_NAME, InspectMissionReadinessInput,
    InspectMissionReadinessOutput, InspectMissionReadinessTool,
};
pub use propose_mission::{
    PROPOSE_MISSION_TOOL_NAME, ProposeMissionInput, ProposeMissionOutput, ProposeMissionTool,
};
pub use start_mission_run::{
    START_MISSION_RUN_TOOL_NAME, StartMissionRunInput, StartMissionRunOutput, StartMissionRunTool,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::types::{Feature, FeatureStatus, FeaturesFile, WorkerHandoff};
    use crate::mission::storage::MissionFileService;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_mission_tools_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let mission_dir = tmp.path().to_path_buf();
        let mission_id = "test-mission-lifecycle".to_string();

        let service = MissionFileService::new(&mission_dir);
        service.initialize_mission_dir().unwrap();
        service.ensure_default_skills().unwrap();

        // Write initial state and working dir
        let state = crate::mission::types::MissionStateFile {
            mission_id: format!("mis_{}", &mission_id[..8]),
            state: crate::mission::types::MissionState::AwaitingInput,
            working_directory: tmp.path().display().to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            initial_feature_count: None,
            last_reviewed_handoff_count: None,
            feature_retry_budget_bonus: Default::default(),
        };
        service.write_state(&state).unwrap();
        service.write_working_directory(&tmp.path().display().to_string()).unwrap();
        service.write_mission_md("Test Mission", "Overview and objectives").unwrap();

        let f1 = Feature {
            id: "feature-alpha".to_string(),
            description: "Implement alpha feature".to_string(),
            skill_name: "foundation-worker".to_string(),
            preconditions: vec!["Hermetic environment".to_string()],
            expected_behavior: vec!["Passes cargo check".to_string()],
            fulfills: vec!["VAL-001".to_string()],
            milestone: "m1".to_string(),
            status: FeatureStatus::Pending,
            worker_session_ids: vec![],
            current_worker_session_id: None,
            completed_worker_session_id: None,
        };
        service.write_features(&FeaturesFile { features: vec![f1] }).unwrap();

        // 1. Test InspectMissionReadiness
        let inspect_tool = InspectMissionReadinessTool;
        let inspect_input = InspectMissionReadinessInput {
            mission_id: Some(mission_dir.display().to_string()),
        };
        let ctx = xai_tool_runtime::ToolCallContext::new(xai_tool_protocol::ToolCallId::new("call-1").unwrap());
        use xai_tool_runtime::Tool;
        let readiness = inspect_tool.run(ctx.clone(), inspect_input).await.unwrap();
        assert!(readiness.ready);
        assert_eq!(readiness.total_features, 1);

        // 2. Test StartMissionRun
        let start_tool = StartMissionRunTool;
        let start_input = StartMissionRunInput {
            mission_id: Some(mission_dir.display().to_string()),
            resume_worker_session_id: None,
            restart_feature: None,
            message_to_worker: Some("Be careful with invariants".to_string()),
        };
        let run_output = start_tool.run(ctx.clone(), start_input).await.unwrap();
        assert!(run_output.success);
        assert_eq!(run_output.status, "worker_ready");
        assert_eq!(run_output.active_feature_id, Some("feature-alpha".to_string()));
        let prompt = run_output.worker_prompt.unwrap();
        assert!(prompt.contains("Be careful with invariants"));
        assert!(prompt.contains("feature-alpha"));

        // 3. Test EndFeatureRun
        let end_tool = EndFeatureRunTool;
        let handoff = WorkerHandoff {
            salient_summary: "Completed alpha feature successfully".to_string(),
            what_was_implemented: "Created core components".to_string(),
            what_was_left_undone: String::new(),
            verification: Default::default(),
            tests: Default::default(),
            discovered_issues: vec![],
            skill_feedback: None,
        };
        let end_input = EndFeatureRunInput {
            feature_id: "feature-alpha".to_string(),
            success_state: "success".to_string(),
            return_to_orchestrator: Some(false),
            commit_id: Some("commit123".to_string()),
            exit_code: Some(0),
            handoff,
            mission_id: Some(mission_dir.display().to_string()),
        };
        let end_output = end_tool.run(ctx.clone(), end_input).await.unwrap();
        assert!(end_output.success);

        // 4. Test DismissHandoffItems
        let dismiss_tool = DismissHandoffItemsTool;
        let dismiss_input = DismissHandoffItemsInput {
            mission_id: Some(mission_dir.display().to_string()),
            dismissals: vec![dismiss_handoff_items::DismissItemInput {
                r#type: "discovered_issue".to_string(),
                source_feature_id: "feature-alpha".to_string(),
                summary: "Minor whitespace discrepancy".to_string(),
                justification: "Not a functional issue".to_string(),
            }],
        };
        let dismiss_output = dismiss_tool.run(ctx.clone(), dismiss_input).await.unwrap();
        assert!(dismiss_output.success);
        assert_eq!(dismiss_output.dismissed_count, 1);

        // Verify progress log has entries
        let logs = service.read_progress_log().unwrap();
        assert!(!logs.is_empty());
    }
}
