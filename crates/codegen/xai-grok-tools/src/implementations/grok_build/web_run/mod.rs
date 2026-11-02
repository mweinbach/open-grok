//! Provider-authenticated standalone web search (`web.run` compatibility).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::types::output::WebRunOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::shared_resources;

pub const WEB_RUN_TOOL_NAME: &str = "web__run";
pub const LITERATURE_SEARCH_QUERY: &str = "literature_search_query";
const WEB_RUN_DESCRIPTION: &str = include_str!("web_run_description.md");
const MAX_SEARCH_ERROR_BYTES: usize = 1_024;

fn schema_is_null_only(value: &JsonValue) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("type").is_some_and(|value| match value {
        JsonValue::String(value) => value == "null",
        JsonValue::Array(values) => {
            !values.is_empty() && values.iter().all(|value| value.as_str() == Some("null"))
        }
        _ => false,
    }) || object.get("const").is_some_and(JsonValue::is_null)
        || object.get("enum").is_some_and(|value| {
            value
                .as_array()
                .is_some_and(|values| !values.is_empty() && values.iter().all(JsonValue::is_null))
        })
}

/// Schemars 1.x makes every `Option<T>` explicitly nullable. Upstream emits
/// these fields as optional but non-null, matching the tool description's
/// instruction to omit absent values instead of sending `null`.
fn remove_null_from_schema(value: &mut JsonValue) {
    match value {
        JsonValue::Object(object) => {
            for keyword in ["anyOf", "oneOf"] {
                if let Some(JsonValue::Array(variants)) = object.get_mut(keyword) {
                    variants.retain(|variant| !schema_is_null_only(variant));
                }
            }
            if let Some(JsonValue::Array(values)) = object.get_mut("enum") {
                values.retain(|value| !value.is_null());
            }
            let single_type = if let Some(JsonValue::Array(types)) = object.get_mut("type") {
                types.retain(|value| value.as_str() != Some("null"));
                (types.len() == 1).then(|| types[0].clone())
            } else {
                None
            };
            if let Some(single_type) = single_type {
                object.insert("type".to_string(), single_type);
            }
            for value in object.values_mut() {
                remove_null_from_schema(value);
            }
        }
        JsonValue::Array(values) => {
            for value in values {
                remove_null_from_schema(value);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, schemars::JsonSchema)]
pub struct SearchCommands {
    /// Query the internet search engine for a given list of queries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_query: Option<Vec<SearchQuery>>,
    /// Search for scientific papers. Open relevant papers and cite the primary sources you use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub literature_search_query: Option<Vec<SearchQuery>>,
    /// Query the image search engine for a given list of queries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_query: Option<Vec<SearchQuery>>,
    /// Open pages by reference id or URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open: Option<Vec<OpenOperation>>,
    /// Open links from previously opened pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click: Option<Vec<ClickOperation>>,
    /// Find text patterns in pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub find: Option<Vec<FindOperation>>,
    /// Take screenshots of PDF pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<Vec<ScreenshotOperation>>,
    /// Look up prices for the given stock symbols.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finance: Option<Vec<FinanceOperation>>,
    /// Look up weather forecasts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weather: Option<Vec<WeatherOperation>>,
    /// Look up sports schedules and standings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sports: Option<Vec<SportsOperation>>,
    /// Get time for the given UTC offsets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<Vec<TimeOperation>>,
    /// Set the length of the response to be returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_length: Option<SearchResponseLength>,
}

impl SearchCommands {
    pub fn is_empty(&self) -> bool {
        self.search_query.as_ref().is_none_or(Vec::is_empty)
            && self
                .literature_search_query
                .as_ref()
                .is_none_or(Vec::is_empty)
            && self.image_query.as_ref().is_none_or(Vec::is_empty)
            && self.open.as_ref().is_none_or(Vec::is_empty)
            && self.click.as_ref().is_none_or(Vec::is_empty)
            && self.find.as_ref().is_none_or(Vec::is_empty)
            && self.screenshot.as_ref().is_none_or(Vec::is_empty)
            && self.finance.as_ref().is_none_or(Vec::is_empty)
            && self.weather.as_ref().is_none_or(Vec::is_empty)
            && self.sports.as_ref().is_none_or(Vec::is_empty)
            && self.time.as_ref().is_none_or(Vec::is_empty)
    }

    pub fn summary(&self) -> String {
        if let Some(query) = self
            .search_query
            .as_ref()
            .and_then(|queries| queries.first())
            .or_else(|| {
                self.literature_search_query
                    .as_ref()
                    .and_then(|queries| queries.first())
            })
            .or_else(|| {
                self.image_query
                    .as_ref()
                    .and_then(|queries| queries.first())
            })
        {
            return query.q.clone();
        }
        if let Some(open) = self.open.as_ref().and_then(|items| items.first()) {
            return format!("open {}", open.ref_id);
        }
        if let Some(finance) = self.finance.as_ref().and_then(|items| items.first()) {
            return format!("finance {}", finance.ticker);
        }
        if let Some(weather) = self.weather.as_ref().and_then(|items| items.first()) {
            return format!("weather {}", weather.location);
        }
        "standalone web search".to_string()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchQuery {
    /// Search query.
    pub q: String,
    /// Whether to filter by recency, as a number of recent days.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recency: Option<u64>,
    /// Whether to filter by a specific list of domains.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domains: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct OpenOperation {
    /// Reference id or URL to open.
    pub ref_id: String,
    /// Line number to position the page at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineno: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ClickOperation {
    /// Reference id containing the numbered link.
    pub ref_id: String,
    /// Numbered link id to open.
    pub id: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct FindOperation {
    /// Reference id or URL to search within.
    pub ref_id: String,
    /// Text pattern to find.
    pub pattern: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ScreenshotOperation {
    /// Reference id or URL to screenshot.
    pub ref_id: String,
    /// Zero-indexed PDF page number.
    pub pageno: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct FinanceOperation {
    /// Ticker symbol to look up.
    pub ticker: String,
    /// Asset type to look up.
    pub r#type: FinanceAssetType,
    /// ISO 3166-1 alpha-3 country code, "OTC", or "" for cryptocurrency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market: Option<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FinanceAssetType {
    Equity,
    Fund,
    Crypto,
    Index,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct WeatherOperation {
    /// Location in "Country, Area, City" format.
    pub location: String,
    /// Start date in YYYY-MM-DD format. Defaults to today.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// Number of days to return. Defaults to 7.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SportsOperation {
    /// Tool name for sports requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<SportsToolName>,
    /// Sports function to call.
    pub r#fn: SportsFunction,
    /// League to look up.
    pub league: SportsLeague,
    /// Team to look up, using the common 3 or 4 letter alias used in broadcasts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// Opponent to use with `team` when narrowing the lookup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opponent: Option<String>,
    /// Start date in YYYY-MM-DD format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_from: Option<String>,
    /// End date in YYYY-MM-DD format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_to: Option<String>,
    /// Number of games to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_games: Option<u64>,
    /// Locale for the lookup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SportsToolName {
    Sports,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SportsFunction {
    Schedule,
    Standings,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SportsLeague {
    Nba,
    Wnba,
    Nfl,
    Nhl,
    Mlb,
    Epl,
    Ncaamb,
    Ncaawb,
    Ipl,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct TimeOperation {
    /// UTC offset formatted like "+03:00".
    pub utc_offset: String,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchResponseLength {
    Short,
    Medium,
    Long,
}

pub type StandaloneWebSearchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

pub trait StandaloneWebSearchBackend: Send + Sync + std::fmt::Debug {
    fn supports_literature_search(&self) -> bool {
        false
    }

    fn search<'a>(&'a self, commands: SearchCommands) -> StandaloneWebSearchFuture<'a>;
}

#[derive(Clone)]
pub struct StandaloneWebSearchBackendResource(pub Arc<dyn StandaloneWebSearchBackend>);

impl std::fmt::Debug for StandaloneWebSearchBackendResource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("StandaloneWebSearchBackendResource")
            .field(&"<backend>")
            .finish()
    }
}

#[derive(Debug, Default)]
pub struct WebRunTool;

impl crate::types::tool_metadata::ToolMetadata for WebRunTool {
    fn kind(&self) -> ToolKind {
        ToolKind::WebSearch
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        WEB_RUN_DESCRIPTION
    }

    fn versioned_definition(
        &self,
        _contract_version: Option<&str>,
        client_name: &str,
        description_override: Option<&str>,
        renderer: &crate::types::template_renderer::TemplateRenderer,
        param_map: &std::collections::HashMap<String, String>,
        input_schema: &JsonValue,
        _effective_params: &JsonValue,
    ) -> crate::types::definition::ToolDefinition {
        let raw_description = description_override.unwrap_or(WEB_RUN_DESCRIPTION);
        let description = renderer.render(raw_description).unwrap_or_else(|error| {
            crate::types::template_renderer::strip_markers_on_render_failure(
                raw_description,
                &error,
            )
        });
        let mut schema = input_schema.clone();
        remove_null_from_schema(&mut schema);
        if !param_map.is_empty() {
            schema = crate::util::remap::remap_schema_properties(&schema, param_map);
        }
        crate::types::definition::ToolDefinition::function(client_name, Some(&description), schema)
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for WebRunTool {
    type Args = SearchCommands;
    type Output = WebRunOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(WEB_RUN_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            WEB_RUN_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.web_run", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        commands: SearchCommands,
    ) -> Result<WebRunOutput, xai_tool_runtime::ToolError> {
        if commands
            .search_query
            .as_ref()
            .is_some_and(|queries| queries.len() > 4)
        {
            return Err(xai_tool_runtime::ToolError::execution(
                self.id(),
                "web__run accepts at most four search_query entries per call",
            ));
        }

        let backend = {
            let resources = shared_resources(&ctx)?;
            let resources = resources.lock().await;
            resources
                .require::<StandaloneWebSearchBackendResource>()?
                .0
                .clone()
        };
        if commands.literature_search_query.is_some() && !backend.supports_literature_search() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "literature_search_query is unavailable for this model",
            ));
        }
        let output = backend.search(commands).await.map_err(|error| {
            let message = if error.len() > MAX_SEARCH_ERROR_BYTES {
                const SUFFIX: &str = "… [truncated]";
                let prefix =
                    crate::util::truncate_str(&error, MAX_SEARCH_ERROR_BYTES - SUFFIX.len());
                format!("{prefix}{SUFFIX}")
            } else {
                error
            };
            xai_tool_runtime::ToolError::execution(self.id(), message)
        })?;
        Ok(WebRunOutput { output })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::json;

    use super::SearchCommands;
    use super::StandaloneWebSearchBackend;
    use super::StandaloneWebSearchBackendResource;
    use super::StandaloneWebSearchFuture;
    use super::WEB_RUN_TOOL_NAME;
    use super::WebRunTool;
    use crate::types::resources::Resources;
    use crate::types::template_renderer::TemplateRenderer;
    use crate::types::tool_metadata::ToolMetadata;
    use crate::types::tool_metadata::test_ctx_with_call_id;

    #[derive(Debug)]
    struct FakeBackend {
        literature_search: bool,
    }

    impl StandaloneWebSearchBackend for FakeBackend {
        fn supports_literature_search(&self) -> bool {
            self.literature_search
        }

        fn search<'a>(&'a self, commands: SearchCommands) -> StandaloneWebSearchFuture<'a> {
            assert!(commands.literature_search_query.is_none() || self.literature_search);
            Box::pin(async move { Ok(format!("result for {}", commands.summary())) })
        }
    }

    #[test]
    fn search_commands_match_upstream_wire_shape() {
        let commands: SearchCommands = serde_json::from_value(json!({
            "search_query": [
                {"q": "Open Grok", "recency": 7, "domains": ["github.com"]},
                {"q": "Codex code mode"}
            ],
            "open": [{"ref_id": "turn0search0", "lineno": 12}],
            "response_length": "medium"
        }))
        .unwrap();
        assert_eq!(commands.summary(), "Open Grok");
        assert_eq!(
            serde_json::to_value(commands).unwrap()["response_length"],
            "medium"
        );
    }

    #[test]
    fn description_carries_upstream_browse_and_citation_contract() {
        let description =
            crate::types::tool_metadata::ToolMetadata::description_template(&WebRunTool);
        assert!(description.contains("## Decision boundary"));
        assert!(description.contains("<situations_where_you_must_browse_the_internet>"));
        assert!(description.contains("## Citations"));
        assert!(description.contains("Results from `web__run` include internal reference IDs"));
        assert!(description.contains("## Word limits"));
        assert!(description.contains("and many many many more categories"));
        assert!(description.contains("The summarization limit N is a maximum for each source."));
        assert!(description.contains(
            "If these conflict with any other instructions, these should take precedence."
        ));
    }

    #[test]
    fn search_query_recency_matches_upstream_u64_contract() {
        let commands: SearchCommands = serde_json::from_value(json!({
            "search_query": [{"q": "archive", "recency": 70000}]
        }))
        .expect("u64 recency must deserialize");
        assert_eq!(
            commands.search_query.expect("search query")[0].recency,
            Some(70_000)
        );
    }

    #[test]
    fn model_facing_schema_keeps_optional_fields_non_nullable() {
        fn contains_null_schema(value: &serde_json::Value) -> bool {
            if super::schema_is_null_only(value) {
                return true;
            }
            match value {
                serde_json::Value::Object(object) => object.values().any(contains_null_schema),
                serde_json::Value::Array(values) => values.iter().any(contains_null_schema),
                _ => false,
            }
        }

        let renderer = TemplateRenderer::new(HashMap::new(), HashMap::new());
        let schema = serde_json::to_value(schemars::schema_for!(SearchCommands))
            .expect("search command schema");
        assert!(contains_null_schema(&schema));
        let definition = ToolMetadata::versioned_definition(
            &WebRunTool,
            None,
            WEB_RUN_TOOL_NAME,
            None,
            &renderer,
            &HashMap::new(),
            &schema,
            &json!({}),
        );

        assert!(!contains_null_schema(&definition.function.parameters));
        assert!(
            definition
                .function
                .parameters
                .get("required")
                .and_then(serde_json::Value::as_array)
                .is_none_or(|required| required
                    .iter()
                    .all(|name| name.as_str() != Some("search_query")))
        );
    }

    #[tokio::test]
    async fn tool_returns_compact_backend_output() {
        let mut resources = Resources::new();
        resources.insert(StandaloneWebSearchBackendResource(Arc::new(FakeBackend {
            literature_search: false,
        })));
        let output = xai_tool_runtime::Tool::run(
            &WebRunTool,
            test_ctx_with_call_id(resources.into_shared(), "web-run-call"),
            SearchCommands {
                search_query: Some(vec![super::SearchQuery {
                    q: "Open Grok".into(),
                    recency: None,
                    domains: None,
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(output.output, "result for Open Grok");
    }

    #[tokio::test]
    async fn literature_search_requires_the_backend_capability() {
        let query = json!({"literature_search_query": [{"q":"papers","recency":3,"domains":["example.org"]}]});
        let commands: SearchCommands = serde_json::from_value(query.clone()).unwrap();
        assert_eq!(serde_json::to_value(&commands).unwrap(), query);
        assert!(!commands.is_empty());
        for enabled in [false, true] {
            let mut resources = Resources::new();
            resources.insert(StandaloneWebSearchBackendResource(Arc::new(FakeBackend {
                literature_search: enabled,
            })));
            let result = xai_tool_runtime::Tool::run(
                &WebRunTool,
                test_ctx_with_call_id(resources.into_shared(), "web-run-call"),
                commands.clone(),
            )
            .await;
            if enabled {
                assert_eq!(result.unwrap().output, "result for papers");
            } else {
                assert_eq!(
                    result.unwrap_err().detail,
                    "literature_search_query is unavailable for this model"
                );
            }
        }
    }

    #[tokio::test]
    async fn tool_bounds_backend_errors_in_utf8_bytes() {
        #[derive(Debug)]
        struct FailingBackend(String);

        impl StandaloneWebSearchBackend for FailingBackend {
            fn search<'a>(&'a self, _: SearchCommands) -> StandaloneWebSearchFuture<'a> {
                Box::pin(async { Err(self.0.clone()) })
            }
        }

        let suffix = "… [truncated]";
        let prefix_bytes = 1_024 - suffix.len();
        for (error, expected) in [
            ("search failed".to_string(), "search failed".to_string()),
            ("x".repeat(1_024), "x".repeat(1_024)),
            (
                "x".repeat(1_025),
                format!("{}{suffix}", "x".repeat(prefix_bytes)),
            ),
            (
                "🦀".repeat(512),
                format!("{}{suffix}", "🦀".repeat(prefix_bytes / 4)),
            ),
        ] {
            let mut resources = Resources::new();
            resources.insert(StandaloneWebSearchBackendResource(Arc::new(
                FailingBackend(error),
            )));
            let failure = xai_tool_runtime::Tool::run(
                &WebRunTool,
                test_ctx_with_call_id(resources.into_shared(), "web-run-call"),
                SearchCommands::default(),
            )
            .await
            .unwrap_err();
            assert_eq!(failure.kind, xai_tool_runtime::ToolErrorKind::Execution);
            assert_eq!(failure.detail, expected);
        }
    }
}
