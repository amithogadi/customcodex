pub use binding::McpBinding;
pub use binding::PreparedMcpCall;
pub use client_capabilities::client_mcp_extensions;
pub use codex_rmcp_client::McpProtocolMode;
pub use connection_manager::tool_is_model_visible;
pub use elicitation::ElicitationLifecycle;
pub use elicitation::ElicitationReviewRequest;
pub use elicitation::ElicitationReviewer;
pub use elicitation::ElicitationReviewerHandle;
pub use ema_auth::ema_auth_scope;
pub use resource_client::McpResourceClient;
pub use resource_client::McpResourceClientAuthKey;
pub use resource_client::McpResourceClientCacheKey;
pub use resource_client::McpResourcePage;
pub use resource_client::McpResourceReadResult;
pub use resource_client::McpResourceServerCacheKey;
pub use rmcp::model::ReadResourceRequestParams;
pub use rmcp_client::MCP_SANDBOX_STATE_META_CAPABILITY;
pub use runtime::McpRuntime;
pub use runtime::McpRuntimeContext;
pub use runtime::McpRuntimeInput;
pub use runtime::McpStartupPolicy;
pub use runtime::SandboxState;
pub use runtime::apply_http_headers_helper;
pub use tool_catalog_cache::McpToolCatalogCache;
pub use tools::ToolInfo;

/// Backward-compatible name for the shared Codex Apps tools runtime.
pub type CodexAppsToolsCache = codex_connectors::ConnectorRuntimeManager<ToolInfo>;
/// Backward-compatible name for the Codex Apps runtime context key.
pub type CodexAppsToolsCacheKey = codex_connectors::ConnectorRuntimeContextKey;

pub use catalog::McpCatalogBuilder;
pub use catalog::McpEnvironmentAuthority;
pub use catalog::McpPluginAttribution;
pub use catalog::McpServerConflict;
pub use catalog::McpServerConflictAction;
pub use catalog::McpServerRegistration;
pub use catalog::McpServerSource;
pub use catalog::ResolvedMcpCatalog;
pub use catalog::ResolvedMcpServer;

pub use mcp::CODEX_APPS_MCP_SERVER_NAME;
pub use mcp::DEFAULT_OPTIONAL_MCP_STARTUP_GRACE;
pub use mcp::McpConfig;
pub use mcp::ToolPluginContext;
pub use server::EffectiveMcpServer;

/// Backward-compatible name for the Codex Apps runtime context key builder.
pub use codex_connectors::connector_runtime_context_key as codex_apps_tools_cache_key;
pub use mcp::configured_mcp_servers;
pub use mcp::effective_mcp_servers;
pub use mcp::effective_mcp_servers_from_configured;
pub use mcp::tool_plugin_context;
pub use plugin_config::PluginMcpConfigParseOutcome;
pub use plugin_config::PluginMcpServerParseError;
pub use plugin_config::parse_agent_plugin_mcp_config;
pub use plugin_config::parse_executor_plugin_mcp_config;
pub use plugin_config::parse_plugin_mcp_config;

pub use mcp::McpServerStatusSnapshot;
pub use mcp::McpSnapshotDetail;
pub use mcp::collect_mcp_server_status_snapshot_with_detail;
pub use mcp::read_mcp_resource;

pub use mcp::McpAuthStatusEntry;
pub use mcp::McpOAuthLoginConfig;
pub use mcp::McpOAuthLoginSupport;
pub use mcp::McpOAuthScopesSource;
pub use mcp::ResolvedMcpOAuthScopes;
pub use mcp::compute_auth_statuses;
pub use mcp::discover_supported_scopes;
pub use mcp::oauth_login_support;
pub use mcp::resolve_oauth_callback;
pub use mcp::resolve_oauth_scopes;
pub use mcp::should_retry_without_scopes;

pub use mcp::McpPermissionPromptAutoApproveContext;
pub use mcp::mcp_permission_prompt_is_auto_approved;
pub use mcp::qualified_mcp_tool_name_prefix;

mod auth_changes;
mod binding;
pub(crate) mod binding_clients;
mod catalog;
mod client_capabilities;
mod client_tool_catalog;
pub(crate) mod connection_manager;
pub(crate) mod elicitation;
mod ema_auth;
mod executor_environment_http_client;
pub(crate) mod mcp;
mod pagination;
mod plugin_config;
mod resource_client;
mod resource_origin;
pub(crate) mod rmcp_client;
pub(crate) mod runtime;
pub(crate) mod server;
mod tool_catalog_cache;
pub(crate) mod tools;

/// Legacy server-provided metadata preserved for generic MCP hook routing.
pub const MCP_TOOL_CODEX_APPS_META_KEY: &str = "_codex_apps";
