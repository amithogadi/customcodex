use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;

use codex_extension_api::ExtensionData;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::SelectedPluginSnapshot;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_extension_api::parse_tool_input_schema;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpResourceClient;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::default_namespace_description;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillSourceKind;
use crate::provider::SkillListQuery;
use crate::provider::attribute_executor_plugins;
use crate::sources::SkillProviders;
use crate::state::SkillsSessionState;
use crate::state::SkillsThreadState;

mod list;
mod read;
mod schema;

const SKILLS_NAMESPACE: &str = "skills";
const MAX_HANDLE_BYTES: usize = 2_048;
const MAX_SKILL_RESPONSE_BYTES: usize = 512 * 1024;

pub(crate) fn skill_tools(
    providers: SkillProviders,
    session_store: &ExtensionData,
    thread_store: &ExtensionData,
    executor_query: Option<SkillListQuery>,
    selected_plugins: Option<Arc<SelectedPluginSnapshot>>,
) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
    let Some(thread_state) = thread_store.get::<SkillsThreadState>() else {
        return Vec::new();
    };
    let cloud_available = providers.has_cloud_provider() && thread_state.cloud_skill_enabled();
    if !cloud_available && executor_query.is_none() {
        return Vec::new();
    }
    let mcp_resources = session_store
        .get::<SkillsSessionState>()
        .and_then(|state| state.mcp_resources.clone());
    let context = SkillToolContext {
        providers,
        mcp_resources,
        thread_state,
        cloud_available,
        executor_query,
        selected_plugins,
        executor_catalog: Arc::new(OnceCell::new()),
    };
    vec![
        Arc::new(list::ListTool {
            context: context.clone(),
        }),
        Arc::new(read::ReadTool { context }),
    ]
}

#[derive(Clone)]
struct SkillToolContext {
    providers: SkillProviders,
    mcp_resources: Option<Arc<McpResourceClient>>,
    thread_state: Arc<SkillsThreadState>,
    cloud_available: bool,
    executor_query: Option<SkillListQuery>,
    selected_plugins: Option<Arc<SelectedPluginSnapshot>>,
    executor_catalog: Arc<OnceCell<SkillCatalog>>,
}

impl SkillToolContext {
    async fn catalog(&self, turn_id: &str, authority: SkillToolAuthoritySelector) -> SkillCatalog {
        match authority {
            SkillToolAuthoritySelector::Cloud => {
                if !self.cloud_available {
                    return SkillCatalog::default();
                }
                self.thread_state.cloud_catalog_snapshot()
            }
            SkillToolAuthoritySelector::Executor => {
                let Some(mut query) = self.executor_query.clone() else {
                    return SkillCatalog::default();
                };
                query.turn_id = turn_id.to_string();
                let mut catalog = self
                    .executor_catalog
                    .get_or_init(|| self.providers.list_executor_for_turn(query))
                    .await
                    .clone();
                if let Some(selected_plugins) = &self.selected_plugins {
                    attribute_executor_plugins(&mut catalog, selected_plugins);
                }
                catalog
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SkillToolAuthoritySelector {
    Cloud,
    Executor,
}

impl SkillToolAuthoritySelector {
    fn matches(self, authority: &SkillAuthority) -> bool {
        match self {
            Self::Cloud => authority.kind == SkillSourceKind::Cloud,
            Self::Executor => authority.kind == SkillSourceKind::Executor,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SkillToolAuthority {
    Cloud,
    Executor { id: String },
}

impl SkillToolAuthority {
    fn selector(&self) -> SkillToolAuthoritySelector {
        match self {
            Self::Cloud => SkillToolAuthoritySelector::Cloud,
            Self::Executor { .. } => SkillToolAuthoritySelector::Executor,
        }
    }

    pub(crate) fn from_authority(authority: &SkillAuthority) -> Option<Self> {
        match &authority.kind {
            SkillSourceKind::Cloud if authority.id == CODEX_APPS_MCP_SERVER_NAME => {
                Some(Self::Cloud)
            }
            SkillSourceKind::Executor => Some(Self::Executor {
                id: authority.id.clone(),
            }),
            SkillSourceKind::Host | SkillSourceKind::Cloud | SkillSourceKind::Custom(_) => None,
        }
    }
}

fn skill_tool_name(name: &str) -> ToolName {
    ToolName::namespaced(SKILLS_NAMESPACE, name)
}

fn skill_function_tool<I: JsonSchema, O: JsonSchema>(name: &str, description: &str) -> ToolSpec {
    let tool = ResponsesApiTool {
        name: name.to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters: parse_tool_input_schema(&schema::input_schema_for::<I>())
            .unwrap_or_else(|err| panic!("generated input schema for {name} should parse: {err}")),
        output_schema: Some(schema::output_schema_for::<O>().into()),
    };

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: SKILLS_NAMESPACE.to_string(),
        description: default_namespace_description(SKILLS_NAMESPACE),
        tools: vec![ResponsesApiNamespaceTool::Function(tool)],
    })
}

fn parse_args<T: for<'de> Deserialize<'de>>(call: &ToolCall<'_>) -> Result<T, FunctionCallError> {
    let arguments = call.function_arguments()?;
    let value = if arguments.trim().is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?
    };
    serde_json::from_value(value).map_err(|err| FunctionCallError::RespondToModel(err.to_string()))
}

fn validate_handle(name: &str, value: &str, max_bytes: usize) -> Result<(), FunctionCallError> {
    if is_bounded_handle(value, max_bytes) {
        return Ok(());
    }

    Err(FunctionCallError::RespondToModel(format!(
        "{name} must be non-empty, contain no control characters, and be at most {max_bytes} bytes"
    )))
}

fn is_bounded_handle(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn pagination_cursor(value: &(impl Hash + ?Sized), offset: usize) -> String {
    format!("{:016x}:{offset}", value_fingerprint(value))
}

fn parse_pagination_cursor(
    cursor: Option<&str>,
    value: &(impl Hash + ?Sized),
    tool: &str,
) -> Result<usize, FunctionCallError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let invalid = || FunctionCallError::RespondToModel(format!("{tool} cursor is invalid"));
    let (fingerprint, offset) = cursor.split_once(':').ok_or_else(invalid)?;
    if u64::from_str_radix(fingerprint, 16).ok() != Some(value_fingerprint(value)) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool} cursor is stale; restart from the first page"
        )));
    }
    offset.parse::<usize>().map_err(|_| invalid())
}

fn value_fingerprint(value: &(impl Hash + ?Sized)) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn serialized_len(value: &impl Serialize) -> Result<usize, FunctionCallError> {
    serde_json::to_vec(value)
        .map(|value| value.len())
        .map_err(|err| FunctionCallError::Fatal(err.to_string()))
}

fn skill_json_output<T: Serialize>(
    value: &T,
    authority: SkillToolAuthoritySelector,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let value = serde_json::to_value(value).map_err(|err| {
        FunctionCallError::Fatal(format!("failed to serialize tool output: {err}"))
    })?;
    let output = JsonToolOutput::new(value);
    Ok(match authority {
        SkillToolAuthoritySelector::Cloud => Box::new(output.with_external_context()),
        SkillToolAuthoritySelector::Executor => Box::new(output),
    })
}
