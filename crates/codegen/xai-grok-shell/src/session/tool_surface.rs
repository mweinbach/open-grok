//! Provider-aware construction of the model-visible tool surface.
//!
//! Turns, compaction, context accounting, and verbatim forks must derive their
//! tool prefix from the same policy. Keeping that policy here prevents Code
//! Mode transport declarations (notably `exec`) from being omitted or sent in
//! a wire shape the selected Responses dialect does not support.

use xai_grok_sampling_types::{
    ApiBackend, ClientTool, CodeModeTransport, CustomToolSpec, HostedTool, ModelProvider, ToolMode,
    ToolSpec,
};
use xai_grok_tools::types::definition::ToolDefinition;

/// Session-persisted result of tool-mode precedence plus its exact provider,
/// backend, and transport route. Keeping the route makes cold resume
/// deterministic without applying a stale pin to a catalog entry that was
/// rebound after the session was created.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ResolvedToolPolicy {
    pub resolved: crate::agent::config::ResolvedToolMode,
    pub transport: Option<CodeModeTransport>,
    /// Exact route on which this policy was resolved. `None` is accepted only
    /// for legacy summaries; new pins always persist both fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_provider: Option<ModelProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_backend: Option<ApiBackend>,
}

impl ResolvedToolPolicy {
    pub(crate) fn for_route(
        resolved: crate::agent::config::ResolvedToolMode,
        provider: ModelProvider,
        backend: &ApiBackend,
    ) -> Result<Self, String> {
        let transport = resolve_code_mode_transport(resolved.mode, provider, backend)?;
        Ok(Self {
            resolved,
            transport,
            route_provider: Some(provider),
            route_backend: Some(*backend),
        })
    }

    pub(crate) fn is_exact_route(self, provider: ModelProvider, backend: &ApiBackend) -> bool {
        self.route_provider == Some(provider)
            && self.route_backend == Some(*backend)
            && Self::for_route(self.resolved, provider, backend)
                .is_ok_and(|current| current.transport == self.transport)
    }

    pub(crate) fn validate_route(
        self,
        provider: ModelProvider,
        backend: &ApiBackend,
    ) -> Result<Self, String> {
        if let Some(route_provider) = self.route_provider
            && route_provider != provider
        {
            return Err(format!(
                "persisted tool policy provider {} is incompatible with {}",
                route_provider.name(),
                provider.name(),
            ));
        }
        if let Some(route_backend) = self.route_backend
            && route_backend != *backend
        {
            return Err(format!(
                "persisted tool policy backend {route_backend:?} is incompatible with {backend:?}"
            ));
        }
        let current = Self::for_route(self.resolved, provider, backend)?;
        if current.transport != self.transport {
            return Err(format!(
                "persisted Code Mode transport {:?} is incompatible with {} {backend:?} (current {:?})",
                self.transport,
                provider.name(),
                current.transport,
            ));
        }
        Ok(self)
    }

    /// Select a route policy at spawn/switch/fork boundaries. A current hard
    /// model requirement wins; otherwise a persisted or parent pin keeps the
    /// existing session presentation after Settings/catalog drift.
    pub(crate) fn select_for_route(
        current: crate::agent::config::ResolvedToolMode,
        pinned: Option<Self>,
        provider: ModelProvider,
        backend: &ApiBackend,
    ) -> Result<Self, String> {
        if current.source == crate::agent::config::ToolModeSource::ModelRequirement {
            Self::for_route(current, provider, backend)
        } else if let Some(pinned) = pinned {
            pinned.validate_route(provider, backend)
        } else {
            Self::for_route(current, provider, backend)
        }
    }

    /// Select a policy for a verbatim child fork. A pin remains valid while
    /// the child stays on the parent's route, but a provider/backend change
    /// must resolve the child's effective mode against its own transport.
    /// This also lets a non-Responses child fall back to Direct instead of
    /// rejecting a parent Responses transport.
    pub(crate) fn select_for_fork_route(
        current: crate::agent::config::ResolvedToolMode,
        pinned: Option<Self>,
        provider: ModelProvider,
        backend: &ApiBackend,
    ) -> Result<Self, String> {
        if current.source == crate::agent::config::ToolModeSource::ModelRequirement {
            return Self::for_route(current, provider, backend);
        }
        if let Some(pinned) = pinned
            && pinned.is_exact_route(provider, backend)
        {
            return Ok(pinned);
        }
        Self::for_route(current, provider, backend)
    }
}

/// Parent snapshot used by verbatim forks. Ordinary tools are rebuilt through
/// [`EffectiveToolSurface`] in the child while the resolved policy pins the
/// parent's current mixed/only presentation across restart-scoped Settings
/// drift.
#[derive(Clone, Debug, Default)]
pub struct ToolSurfaceSnapshot {
    pub function_tools: Vec<ToolSpec>,
    pub resolved_policy: Option<ResolvedToolPolicy>,
}

pub(crate) fn resolve_code_mode_transport(
    tool_mode: ToolMode,
    provider: ModelProvider,
    backend: &ApiBackend,
) -> Result<Option<CodeModeTransport>, String> {
    if tool_mode == ToolMode::Direct {
        return Ok(None);
    }
    if backend != &ApiBackend::Responses {
        return Err(format!(
            "Code Mode requires a Responses-backed model; active backend: {backend:?}"
        ));
    }
    match provider.profile().code_mode_transport {
        CodeModeTransport::NativeCustomGrammar => Ok(Some(CodeModeTransport::NativeCustomGrammar)),
        CodeModeTransport::FunctionEnvelope => Ok(Some(CodeModeTransport::FunctionEnvelope)),
        CodeModeTransport::Unsupported => Err(format!(
            "{} does not provide a Code Mode transport for {backend:?}",
            provider.name()
        )),
    }
}

/// Decode an `exec` call only when its wire kind matches the active provider
/// transport. `Ok(None)` means the call is not an exec control call.
pub(crate) fn code_mode_exec_source(
    call: &xai_grok_sampling_types::ToolCall,
    transport: Option<CodeModeTransport>,
) -> Result<Option<String>, String> {
    if call.name != xai_grok_code_mode_protocol::PUBLIC_TOOL_NAME {
        return Ok(None);
    }
    match transport {
        Some(CodeModeTransport::NativeCustomGrammar) if call.is_custom() => {
            Ok(Some(call.custom_input().unwrap_or_default().to_string()))
        }
        Some(CodeModeTransport::FunctionEnvelope) if !call.is_custom() => {
            let arguments = serde_json::from_str::<serde_json::Value>(call.arguments.as_ref())
                .map_err(|error| format!("exec arguments must be valid JSON: {error}"))?;
            let source = arguments
                .get("source")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "exec arguments must contain a string `source` field".to_string())?;
            Ok(Some(source.to_string()))
        }
        Some(active) => Err(format!(
            "Code Mode received an incompatible `exec` call for the active {active:?} transport"
        )),
        None => Err("received an `exec` call while Code Mode is inactive".to_string()),
    }
}

/// Fully resolved model-facing tools for one provider/backend/mode route.
#[derive(Clone, Debug)]
pub(crate) struct EffectiveToolSurface {
    pub(crate) function_tools: Vec<ToolSpec>,
    pub(crate) hosted_tools: Vec<HostedTool>,
    pub(crate) code_mode_transport: Option<CodeModeTransport>,
    /// Ordinary tool names displaced by the reserved Code Mode controls.
    pub(crate) reserved_name_collisions: Vec<String>,
}

impl EffectiveToolSurface {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build(
        mut function_tools: Vec<ToolSpec>,
        nested_definitions: &[ToolDefinition],
        hosted_tools: &[HostedTool],
        tool_mode: ToolMode,
        provider: ModelProvider,
        backend: &ApiBackend,
        suppress_native_web_search: bool,
    ) -> Result<Self, String> {
        let code_mode_transport = resolve_code_mode_transport(tool_mode, provider, backend)?;
        let code_mode_only = tool_mode == ToolMode::CodeModeOnly;
        let mut hosted_tools = crate::session::code_mode::hosted_tools_for_code_mode(
            hosted_tools,
            tool_mode,
            provider,
        );
        // A non-native web-search source was selected (and resolved) for this
        // provider: drop the provider's native hosted declaration so the
        // client `web_search` tool is the one the model sees.
        if suppress_native_web_search {
            hosted_tools.retain(|tool| {
                !matches!(tool, xai_grok_sampling_types::HostedTool::WebSearch { .. })
            });
        }
        let mut reserved_name_collisions = Vec::new();

        if let Some(transport) = code_mode_transport {
            function_tools.retain(|tool| {
                let reserved = crate::session::code_mode::is_code_mode_transport_tool(&tool.name);
                if reserved {
                    reserved_name_collisions.push(tool.name.clone());
                }
                !reserved
            });
            reserved_name_collisions.sort();
            reserved_name_collisions.dedup();

            if code_mode_only {
                function_tools.retain(|tool| {
                    crate::session::code_mode::is_code_mode_direct_only_tool(&tool.name)
                });
            }

            let nested_definitions =
                crate::session::code_mode::nested_tool_definitions_for_provider(
                    nested_definitions,
                    provider,
                    &hosted_tools,
                );
            match transport {
                CodeModeTransport::NativeCustomGrammar => {
                    let exec_tool = crate::session::code_mode::create_exec_tool(
                        &nested_definitions,
                        code_mode_only,
                    );
                    let ClientTool::Custom {
                        name,
                        description,
                        format,
                    } = exec_tool
                    else {
                        unreachable!("Code Mode exec helper must create a custom tool")
                    };
                    hosted_tools.push(HostedTool::ClientCustom(CustomToolSpec {
                        name,
                        description,
                        format,
                    }));
                }
                CodeModeTransport::FunctionEnvelope => {
                    function_tools.push(crate::session::code_mode::create_exec_function_tool(
                        &nested_definitions,
                        code_mode_only,
                    ));
                }
                CodeModeTransport::Unsupported => {
                    unreachable!("unsupported transport rejected during resolution")
                }
            }
            function_tools.push(crate::session::code_mode::create_wait_tool());
        }

        Ok(Self {
            function_tools,
            hosted_tools,
            code_mode_transport,
            reserved_name_collisions,
        })
    }

    /// Tool-prefix estimate including native custom declarations. Hosted
    /// provider tools are intentionally excluded, matching the existing
    /// context accounting, but client custom tools occupy the same prefix as
    /// function definitions and must be counted.
    pub(crate) fn estimated_definition_tokens(&self) -> u64 {
        let function_tokens = self
            .function_tools
            .iter()
            .map(|tool| {
                let definition = ToolDefinition::function(
                    tool.name.clone(),
                    tool.description.clone(),
                    tool.parameters.clone(),
                );
                xai_chat_state::estimate_tool_definition_tokens(&definition)
            })
            .sum::<u64>();
        let custom_tokens = self
            .hosted_tools
            .iter()
            .filter_map(|tool| match tool {
                HostedTool::ClientCustom(tool) => Some(tool),
                HostedTool::WebSearch { .. } | HostedTool::XSearch { .. } => None,
            })
            .map(|tool| {
                let bytes = tool.name.len()
                    + tool.description.as_deref().map_or(0, str::len)
                    + serde_json::to_string(&tool.format).map_or(0, |value| value.len());
                (bytes as u64) / xai_token_estimation::BYTES_PER_TOKEN
            })
            .sum::<u64>();
        function_tokens.saturating_add(custom_tokens)
    }
}

pub(crate) fn tool_specs_as_definitions(tools: &[ToolSpec]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .map(|tool| {
            ToolDefinition::function(
                tool.name.clone(),
                tool.description.clone(),
                tool.parameters.clone(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: Some(format!("{name} description")),
            parameters: json!({"type": "object"}),
        }
    }

    fn definitions(tools: &[ToolSpec]) -> Vec<ToolDefinition> {
        tool_specs_as_definitions(tools)
    }

    #[test]
    fn direct_mode_preserves_ordinary_exec_and_wait_tools() {
        let tools = vec![tool("exec"), tool("wait"), tool("read_file")];
        let surface = EffectiveToolSurface::build(
            tools.clone(),
            &definitions(&tools),
            &[],
            ToolMode::Direct,
            ModelProvider::Xai,
            &ApiBackend::Responses,
            false,
        )
        .unwrap();
        assert_eq!(
            surface
                .function_tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["exec", "wait", "read_file"]
        );
        assert!(surface.reserved_name_collisions.is_empty());
    }

    #[test]
    fn xai_mixed_mode_uses_function_exec_and_keeps_ordinary_tools() {
        let tools = vec![tool("read_file")];
        let surface = EffectiveToolSurface::build(
            tools.clone(),
            &definitions(&tools),
            &[],
            ToolMode::CodeMode,
            ModelProvider::Xai,
            &ApiBackend::Responses,
            false,
        )
        .unwrap();
        assert_eq!(
            surface.code_mode_transport,
            Some(CodeModeTransport::FunctionEnvelope)
        );
        assert!(
            surface
                .hosted_tools
                .iter()
                .all(|tool| !matches!(tool, HostedTool::ClientCustom(_)))
        );
        assert_eq!(
            surface
                .function_tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read_file", "exec", "wait"]
        );
        let exec = surface
            .function_tools
            .iter()
            .find(|tool| tool.name == "exec")
            .unwrap();
        assert_eq!(exec.parameters["required"], json!(["source"]));
        let description = exec.description.as_deref().unwrap();
        assert!(description.contains(r#"`{"source":"<raw JavaScript>"}`"#));
        assert!(!description.contains("Accepts raw JavaScript source text, not JSON"));
    }

    #[test]
    fn xai_only_uses_function_exec_and_direct_only_functions() {
        let tools = vec![tool("read_file"), tool("request_user_input")];
        let surface = EffectiveToolSurface::build(
            tools.clone(),
            &definitions(&tools),
            &[],
            ToolMode::CodeModeOnly,
            ModelProvider::Xai,
            &ApiBackend::Responses,
            false,
        )
        .unwrap();
        assert_eq!(
            surface.code_mode_transport,
            Some(CodeModeTransport::FunctionEnvelope)
        );
        assert!(
            surface
                .hosted_tools
                .iter()
                .all(|tool| !matches!(tool, HostedTool::ClientCustom(_)))
        );
        assert_eq!(
            surface
                .function_tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["request_user_input", "exec", "wait"]
        );
        let exec = surface
            .function_tools
            .iter()
            .find(|tool| tool.name == "exec")
            .unwrap();
        assert_eq!(exec.parameters["required"], json!(["source"]));
        let description = exec.description.as_deref().unwrap();
        assert!(description.contains(r#"`{"source":"<raw JavaScript>"}`"#));
        assert!(!description.contains("Accepts raw JavaScript source text, not JSON"));
    }

    #[test]
    fn codex_mixed_uses_native_custom_exec_and_keeps_ordinary_tools() {
        let tools = vec![tool("read_file")];
        let surface = EffectiveToolSurface::build(
            tools.clone(),
            &definitions(&tools),
            &[],
            ToolMode::CodeMode,
            ModelProvider::Codex,
            &ApiBackend::Responses,
            false,
        )
        .unwrap();
        assert_eq!(
            surface.code_mode_transport,
            Some(CodeModeTransport::NativeCustomGrammar)
        );
        assert_eq!(
            surface
                .function_tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read_file", "wait"]
        );
        let exec = surface
            .hosted_tools
            .iter()
            .find_map(|tool| match tool {
                HostedTool::ClientCustom(tool) if tool.name == "exec" => Some(tool),
                HostedTool::ClientCustom(_)
                | HostedTool::WebSearch { .. }
                | HostedTool::XSearch { .. } => None,
            })
            .expect("Codex mixed Code Mode must expose native custom exec");
        let description = exec.description.as_deref().unwrap();
        assert!(description.contains("Accepts raw JavaScript source text, not JSON"));
        assert!(!description.contains(r#"`{"source":"<raw JavaScript>"}`"#));
    }

    #[test]
    fn codex_only_uses_native_custom_exec_and_direct_only_functions() {
        let tools = vec![tool("read_file"), tool("request_user_input")];
        let surface = EffectiveToolSurface::build(
            tools.clone(),
            &definitions(&tools),
            &[],
            ToolMode::CodeModeOnly,
            ModelProvider::Codex,
            &ApiBackend::Responses,
            false,
        )
        .unwrap();
        assert_eq!(
            surface.code_mode_transport,
            Some(CodeModeTransport::NativeCustomGrammar)
        );
        assert_eq!(
            surface
                .function_tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["request_user_input", "wait"]
        );
        assert!(surface.hosted_tools.iter().any(|tool| matches!(
            tool,
            HostedTool::ClientCustom(tool) if tool.name == "exec"
        )));
        assert!(surface.estimated_definition_tokens() > 0);
    }

    #[test]
    fn code_mode_reserves_exec_and_wait_without_duplicate_names() {
        let tools = vec![tool("exec"), tool("wait"), tool("read_file")];
        let surface = EffectiveToolSurface::build(
            tools.clone(),
            &definitions(&tools),
            &[],
            ToolMode::CodeMode,
            ModelProvider::Xai,
            &ApiBackend::Responses,
            false,
        )
        .unwrap();
        assert_eq!(
            surface.reserved_name_collisions,
            vec!["exec".to_string(), "wait".to_string()]
        );
        for reserved in ["exec", "wait"] {
            assert_eq!(
                surface
                    .function_tools
                    .iter()
                    .filter(|tool| tool.name == reserved)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn code_mode_fails_closed_without_responses_transport() {
        let error = EffectiveToolSurface::build(
            vec![tool("read_file")],
            &[],
            &[],
            ToolMode::CodeMode,
            ModelProvider::Kimi,
            &ApiBackend::ChatCompletions,
            false,
        )
        .unwrap_err();
        assert!(error.contains("Responses-backed"));
    }

    #[test]
    fn persisted_policy_rejects_transport_drift() {
        let policy = ResolvedToolPolicy::for_route(
            crate::agent::config::ResolvedToolMode {
                mode: ToolMode::CodeMode,
                source: crate::agent::config::ToolModeSource::UserPreference,
            },
            ModelProvider::Xai,
            &ApiBackend::Responses,
        )
        .unwrap();
        assert_eq!(policy.transport, Some(CodeModeTransport::FunctionEnvelope));
        assert!(
            policy
                .validate_route(ModelProvider::Codex, &ApiBackend::Responses)
                .is_err()
        );
    }

    #[test]
    fn fork_policy_rebinds_codex_native_transport_to_xai_function_envelope() {
        let current = crate::agent::config::ResolvedToolMode {
            mode: ToolMode::CodeMode,
            source: crate::agent::config::ToolModeSource::UserPreference,
        };
        let parent =
            ResolvedToolPolicy::for_route(current, ModelProvider::Codex, &ApiBackend::Responses)
                .unwrap();

        let child = ResolvedToolPolicy::select_for_fork_route(
            current,
            Some(parent),
            ModelProvider::Xai,
            &ApiBackend::Responses,
        )
        .unwrap();

        assert_eq!(child.resolved.mode, ToolMode::CodeMode);
        assert_eq!(child.transport, Some(CodeModeTransport::FunctionEnvelope));
    }

    #[test]
    fn fork_policy_rebinds_xai_function_envelope_to_codex_native_transport() {
        let current = crate::agent::config::ResolvedToolMode {
            mode: ToolMode::CodeMode,
            source: crate::agent::config::ToolModeSource::UserPreference,
        };
        let parent =
            ResolvedToolPolicy::for_route(current, ModelProvider::Xai, &ApiBackend::Responses)
                .unwrap();

        let child = ResolvedToolPolicy::select_for_fork_route(
            current,
            Some(parent),
            ModelProvider::Codex,
            &ApiBackend::Responses,
        )
        .unwrap();

        assert_eq!(child.resolved.mode, ToolMode::CodeMode);
        assert_eq!(
            child.transport,
            Some(CodeModeTransport::NativeCustomGrammar)
        );
    }

    #[test]
    fn fork_policy_rebinds_responses_parent_to_direct_kimi_route() {
        let parent = ResolvedToolPolicy::for_route(
            crate::agent::config::ResolvedToolMode {
                mode: ToolMode::CodeMode,
                source: crate::agent::config::ToolModeSource::UserPreference,
            },
            ModelProvider::Xai,
            &ApiBackend::Responses,
        )
        .unwrap();
        let current = crate::agent::config::ResolvedToolMode {
            mode: ToolMode::Direct,
            source: crate::agent::config::ToolModeSource::Default,
        };

        let child = ResolvedToolPolicy::select_for_fork_route(
            current,
            Some(parent),
            ModelProvider::Kimi,
            &ApiBackend::ChatCompletions,
        )
        .unwrap();

        assert_eq!(child.resolved.mode, ToolMode::Direct);
        assert_eq!(child.transport, None);
    }

    #[test]
    fn fork_policy_child_model_requirement_overrides_parent_pin() {
        let parent = ResolvedToolPolicy::for_route(
            crate::agent::config::ResolvedToolMode {
                mode: ToolMode::CodeMode,
                source: crate::agent::config::ToolModeSource::UserPreference,
            },
            ModelProvider::Xai,
            &ApiBackend::Responses,
        )
        .unwrap();
        let required = crate::agent::config::ResolvedToolMode {
            mode: ToolMode::CodeModeOnly,
            source: crate::agent::config::ToolModeSource::ModelRequirement,
        };

        let child = ResolvedToolPolicy::select_for_fork_route(
            required,
            Some(parent),
            ModelProvider::Codex,
            &ApiBackend::Responses,
        )
        .unwrap();

        assert_eq!(child.resolved, required);
        assert_eq!(
            child.transport,
            Some(CodeModeTransport::NativeCustomGrammar)
        );
    }

    #[test]
    fn exec_source_decodes_only_the_active_transport_shape() {
        let native = xai_grok_sampling_types::ToolCall::custom(
            "call-native",
            "item-native",
            "exec",
            "text('native')",
        );
        assert_eq!(
            code_mode_exec_source(&native, Some(CodeModeTransport::NativeCustomGrammar)).unwrap(),
            Some("text('native')".to_string())
        );
        assert!(code_mode_exec_source(&native, Some(CodeModeTransport::FunctionEnvelope)).is_err());

        let function = xai_grok_sampling_types::ToolCall {
            id: std::sync::Arc::from("call-function"),
            name: "exec".to_string(),
            arguments: std::sync::Arc::from(r#"{"source":"text('function')"}"#),
        };
        assert_eq!(
            code_mode_exec_source(&function, Some(CodeModeTransport::FunctionEnvelope)).unwrap(),
            Some("text('function')".to_string())
        );
        assert!(
            code_mode_exec_source(&function, Some(CodeModeTransport::NativeCustomGrammar)).is_err()
        );
    }
}
