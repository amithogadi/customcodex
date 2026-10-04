use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;

use codex_mcp::McpBinding;
use codex_mcp::ToolInfo as McpToolInfo;
use codex_mcp::tool_is_model_visible;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use tracing::instrument;
use tracing::warn;

const MAX_AGENT_PLUGIN_MCP_SPEC_BYTES: usize = 8_000;
const MAX_AGENT_PLUGIN_MCP_TOTAL_BYTES: usize = 64_000;

use crate::config::Config;
use crate::tools::handlers::McpHandler;
use crate::tools::registry::ToolRegistry;

#[derive(Default)]
pub(crate) struct McpHandlerCache {
    handlers: Mutex<HashMap<ToolName, CachedMcpHandler>>,
}

struct CachedMcpHandler {
    tool_info: McpToolInfo,
    agent_plugin: bool,
    schema_max_bytes: Option<NonZeroUsize>,
    handler: Arc<McpHandler>,
}

impl McpHandlerCache {
    pub(crate) fn append_mcp_tools(
        &self,
        binding: &Arc<McpBinding>,
        config: &Config,
        apps_enabled: bool,
        mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
        search_tool_enabled: bool,
        registry: &mut ToolRegistry,
    ) -> HashSet<ToolName> {
        let mut handlers = self
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        append_mcp_tools(
            binding.tools(),
            config,
            apps_enabled,
            mcp_server_catalog,
            search_tool_enabled,
            &mut handlers,
            registry,
        )
    }
}

#[instrument(level = "trace", skip_all)]
fn append_mcp_tools(
    all_mcp_tools: &[McpToolInfo],
    _config: &Config,
    _apps_enabled: bool,
    mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
    search_tool_enabled: bool,
    handlers: &mut HashMap<ToolName, CachedMcpHandler>,
    registry: &mut ToolRegistry,
) -> HashSet<ToolName> {
    let current_tools: HashSet<_> = all_mcp_tools
        .iter()
        .map(McpToolInfo::canonical_tool_name)
        .collect();
    handlers.retain(|name, _| current_tools.contains(name));

    let exposure = if search_tool_enabled {
        ToolExposure::Deferred
    } else {
        ToolExposure::Direct
    };
    let mut registered_tools = HashSet::new();
    let mut agent_plugin_bytes = 0usize;
    for tool in all_mcp_tools
        .iter()
        .filter(|tool| tool_is_model_visible(tool))
    {
        let tool_name = tool.canonical_tool_name();
        let server = mcp_server_catalog.server(&tool.server_name);
        let agent_plugin = server.is_some_and(|server| server.source().is_agent_plugin());
        let tool_input_schema_max_bytes =
            server.and_then(|server| server.config().tool_input_schema_max_bytes);
        // Handlers contain immutable tool metadata, not a connection or authorization snapshot.
        // Preserve their identity across equivalent bindings so the search index can also be reused.
        let handler = if let Some(cached) = handlers.get(&tool_name).filter(|cached| {
            cached.tool_info == *tool
                && cached.agent_plugin == agent_plugin
                && cached.schema_max_bytes == tool_input_schema_max_bytes
        }) {
            Arc::clone(&cached.handler)
        } else {
            handlers.remove(&tool_name);
            let handler = if agent_plugin {
                McpHandler::new_agent_plugin(tool.clone())
            } else if let Some(budget) = tool_input_schema_max_bytes {
                McpHandler::new_with_schema_max_bytes(tool.clone(), budget.get())
            } else {
                McpHandler::new(tool.clone())
            };

            let handler = match handler {
                Ok(handler) => Arc::new(handler),
                Err(err) => {
                    warn!("Skipping MCP tool `{tool_name}`: failed to build tool spec: {err}");
                    continue;
                }
            };
            handlers.insert(
                tool_name.clone(),
                CachedMcpHandler {
                    tool_info: tool.clone(),
                    agent_plugin,
                    schema_max_bytes: tool_input_schema_max_bytes,
                    handler: Arc::clone(&handler),
                },
            );
            handler
        };

        let fits_agent_budget = if agent_plugin {
            handler.model_spec_bytes().is_ok_and(|bytes| {
                if bytes > MAX_AGENT_PLUGIN_MCP_SPEC_BYTES {
                    return false;
                }
                let next = agent_plugin_bytes.saturating_add(bytes);
                if next <= MAX_AGENT_PLUGIN_MCP_TOTAL_BYTES {
                    agent_plugin_bytes = next;
                    true
                } else {
                    false
                }
            })
        } else {
            true
        };
        let tool_exposure = if fits_agent_budget {
            exposure
        } else {
            ToolExposure::Hidden
        };
        if registry.register_external_with_exposure(handler, tool_exposure) && fits_agent_budget {
            registered_tools.insert(tool_name);
        }
    }
    registered_tools
}

#[cfg(test)]
#[path = "mcp_tool_exposure_test.rs"]
mod tests;
