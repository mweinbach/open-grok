//! Fail-closed model switching while a session turn is still live.

use super::support::*;
use super::*;

fn replacement_config(model: &str) -> xai_grok_sampler::SamplerConfig {
    xai_grok_sampler::SamplerConfig {
        model: model.to_string(),
        base_url: "https://replacement.invalid/v1".to_string(),
        context_window: 128_000,
        supports_backend_search: true,
        ..Default::default()
    }
}

async fn assert_switch_rejected_without_mutation(actor: &SessionActor) {
    let before_config = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .expect("test actor must have sampling config");
    let before_tool_mode = actor.agent.borrow().tool_mode();
    let before_threshold = actor.compaction.threshold_percent.get();
    let before_runtime_generation = actor.rebuild_spec.code_mode_runtime.generation();

    let error = actor
        .handle_set_session_model(
            acp::ModelId::new("replacement-model"),
            replacement_config("replacement-model"),
            false,
            true,
            false,
            42,
            None,
            None,
        )
        .await
        .expect_err("an active turn must reject model mutation");

    assert_eq!(
        error.data.as_ref().and_then(serde_json::Value::as_str),
        Some("Cannot switch models while a turn is active; cancel it or wait for it to finish.")
    );
    let after_config = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .expect("test actor must retain sampling config");
    assert_eq!(after_config.model, before_config.model);
    assert_eq!(after_config.base_url, before_config.base_url);
    assert_eq!(after_config.provider, before_config.provider);
    assert_eq!(after_config.api_backend, before_config.api_backend);
    assert_eq!(actor.agent.borrow().tool_mode(), before_tool_mode);
    assert_eq!(actor.compaction.threshold_percent.get(), before_threshold);
    assert_eq!(
        actor.rebuild_spec.code_mode_runtime.generation(),
        before_runtime_generation,
        "a rejected switch must not replace the runtime"
    );
}

#[tokio::test]
async fn model_switch_rejects_an_occupied_running_task_slot() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            actor.state.lock().await.running_task = Some(running_task_stub("active-turn"));

            assert_switch_rejected_without_mutation(&actor).await;
            if let Some(task) = actor.state.lock().await.running_task.take() {
                task.handle.abort();
            }
        })
        .await;
}

#[tokio::test]
async fn model_switch_rejects_the_send_now_cancellation_window() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            assert!(actor.state.lock().await.running_task.is_none());
            actor
                .session_turn_active
                .store(true, std::sync::atomic::Ordering::Release);

            assert_switch_rejected_without_mutation(&actor).await;

            actor
                .session_turn_active
                .store(false, std::sync::atomic::Ordering::Release);
        })
        .await;
}

#[tokio::test]
async fn harness_rebuild_rejects_the_send_now_cancellation_window() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            let before_agent = actor.agent.borrow().definition().name.clone();
            actor
                .session_turn_active
                .store(true, std::sync::atomic::Ordering::Release);

            let error = actor
                .handle_rebuild_agent_for_definition(
                    xai_grok_agent::AgentDefinition::default_grok_build(),
                    false,
                )
                .await
                .expect_err("a send-now cancellation window must reject harness rebuild");

            assert_eq!(
                error.data.as_ref().and_then(serde_json::Value::as_str),
                Some(
                    "Cannot switch models while a turn is active; cancel it or wait for it to finish."
                )
            );
            assert_eq!(actor.agent.borrow().definition().name, before_agent);
            actor
                .session_turn_active
                .store(false, std::sync::atomic::Ordering::Release);
        })
        .await;
}

#[tokio::test]
async fn idle_incompatible_model_transition_replaces_the_runtime_generation() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use crate::agent::config::{EndpointsConfig, ModelEntry};
            use xai_grok_sampling_types::{ApiBackend, ModelProvider, ToolMode};

            let (actor, _gateway_rx) = build_actor().await;
            let mut entry = ModelEntry::fallback("replacement-model", &EndpointsConfig::default());
            entry.info.provider = ModelProvider::Codex;
            entry.info.api_backend = ApiBackend::Responses;
            entry.info.tool_mode = Some(ToolMode::CodeModeOnly);
            actor
                .models_manager
                .insert_test_entry("replacement-model", entry);

            let previous_runtime = actor.rebuild_spec.code_mode_runtime.current();
            let previous_generation = actor.rebuild_spec.code_mode_runtime.generation();
            let mut config = replacement_config("replacement-model");
            config.provider = ModelProvider::Codex;
            config.api_backend = ApiBackend::Responses;

            let updated = actor
                .handle_set_session_model(
                    acp::ModelId::new("replacement-model"),
                    config,
                    false,
                    false,
                    true,
                    42,
                    None,
                    None,
                )
                .await
                .expect("an idle compatible route must switch successfully");

            assert_eq!(updated.0.as_ref(), "replacement-model");
            assert_eq!(actor.agent.borrow().tool_mode(), ToolMode::CodeModeOnly);
            assert_eq!(
                actor.rebuild_spec.code_mode_runtime.generation(),
                previous_generation + 1
            );
            assert!(!std::sync::Arc::ptr_eq(
                &previous_runtime,
                &actor.rebuild_spec.code_mode_runtime.current()
            ));
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_sampling_config()
                    .await
                    .expect("updated sampling config")
                    .provider,
                ModelProvider::Codex
            );
        })
        .await;
}

#[tokio::test]
async fn forked_tool_snapshot_is_revalidated_for_the_active_provider() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use xai_grok_sampling_types::{ModelProvider, ToolSpec};

            let (actor, _gateway_rx) = build_actor().await;
            let tools = ["read_file", "image_gen", "image_edit", "image_to_video"]
                .into_iter()
                .map(|name| ToolSpec {
                    name: name.to_owned(),
                    description: None,
                    parameters: serde_json::json!({"type": "object"}),
                })
                .collect::<Vec<_>>();

            let filtered = actor.provider_filtered_tool_specs(&tools, ModelProvider::Codex);
            assert_eq!(
                filtered
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["read_file"],
                "a forked xAI tool snapshot must not leak media tools after switching provider"
            );
        })
        .await;
}

#[tokio::test]
async fn fork_snapshot_preserves_codex_model_requirement_source() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use crate::agent::config::{EndpointsConfig, ModelEntry, ToolModeSource};
            use xai_grok_sampling_types::{ApiBackend, ModelProvider, ToolMode};

            let (actor, _gateway_rx) = build_actor().await;
            let mut entry = ModelEntry::fallback("required-codex", &EndpointsConfig::default());
            entry.info.provider = ModelProvider::Codex;
            entry.info.api_backend = ApiBackend::Responses;
            entry.info.tool_mode = Some(ToolMode::CodeModeOnly);
            actor
                .models_manager
                .insert_test_entry("required-codex", entry);
            actor
                .agent
                .borrow_mut()
                .set_tool_mode(ToolMode::CodeModeOnly);

            let mut sampling_config = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor sampling config");
            sampling_config.model = "required-codex".to_string();
            sampling_config.provider = ModelProvider::Codex;
            sampling_config.api_backend = ApiBackend::Responses;
            let policy = actor
                .snapshot_resolved_tool_policy(&sampling_config)
                .expect("Codex Responses route supports the required policy");

            assert_eq!(policy.resolved.mode, ToolMode::CodeModeOnly);
            assert_eq!(policy.resolved.source, ToolModeSource::ModelRequirement);
        })
        .await;
}
