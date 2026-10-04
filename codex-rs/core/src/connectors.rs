//! Local metadata helpers retained while generic MCP approvals use the configured reviewer.
use codex_config::types::ApprovalsReviewer;
pub use codex_connectors::{AppBranding, AppInfo, AppMetadata};
use codex_mcp::{CODEX_APPS_MCP_SERVER_NAME, ToolInfo, ToolPluginContext};
pub(crate) fn accessible_connectors_from_mcp_tools(mcp_tools: &[ToolInfo]) -> Vec<AppInfo> {
    collect_accessible_connectors_from_mcp_tools(mcp_tools.iter())
}

fn collect_accessible_connectors_from_mcp_tools<'a>(
    mcp_tools: impl Iterator<Item = &'a ToolInfo>,
) -> Vec<AppInfo> {
    // ToolInfo already carries plugin provenance, so app-level plugin sources
    // can be derived here instead of requiring a separate enrichment pass.
    let tools = mcp_tools.filter_map(|tool| {
        if tool.server_name != CODEX_APPS_MCP_SERVER_NAME {
            return None;
        }
        let connector_id = tool.connector_id.as_deref()?;
        Some(codex_connectors::accessible::AccessibleConnectorTool {
            connector_id: connector_id.to_string(),
            connector_name: tool.connector_name.clone(),
            connector_description: tool.namespace_description.clone(),
            plugin_display_names: tool.plugin_display_names.clone(),
        })
    });
    codex_connectors::accessible::collect_accessible_connectors(tools)
}

pub fn with_app_plugin_sources(
    mut connectors: Vec<AppInfo>,
    tool_plugin_context: &ToolPluginContext,
) -> Vec<AppInfo> {
    for connector in &mut connectors {
        connector.plugin_display_names = tool_plugin_context
            .plugin_display_names_for_connector_id(connector.id.as_str())
            .to_vec();
    }
    connectors
}

pub(crate) fn mcp_approvals_reviewer_from_layers(
    config_layer_stack: &codex_config::ConfigLayerStack,
    default_reviewer: ApprovalsReviewer,
    model: Option<&str>,
    _server_name: &str,
    _connector_id: Option<&str>,
    _link_id: Option<&str>,
) -> ApprovalsReviewer {
    if model.is_some_and(|model| {
        config_layer_stack
            .requirements()
            .auto_review_required_for_model(model)
    }) {
        ApprovalsReviewer::AutoReview
    } else {
        default_reviewer
    }
}
