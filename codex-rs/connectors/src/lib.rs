use std::time::Duration;

pub mod accessible;
mod app_info;
mod app_tool_policy;
mod connector_runtime;
pub mod filter;
pub mod merge;
pub mod metadata;
mod metadata_store;
mod plugin_config;
mod runtime_projection;
mod snapshot;

pub use app_info::AppBranding;
pub use app_info::AppInfo;
pub use app_info::AppMetadata;
pub use app_info::AppReview;
pub use app_info::AppScreenshot;
pub use app_tool_policy::AppToolPolicy;
pub use app_tool_policy::AppToolPolicyEvaluator;
pub use app_tool_policy::AppToolPolicyInput;
pub use app_tool_policy::app_is_enabled;
pub use app_tool_policy::apps_config_from_layer_stack;
pub use connector_runtime::ConnectorRuntimeContext;
pub use connector_runtime::ConnectorRuntimeContextKey;
pub use connector_runtime::ConnectorRuntimeFetchSource;
pub use connector_runtime::ConnectorRuntimeFetchTicket;
pub use connector_runtime::ConnectorRuntimeManager;
pub use connector_runtime::ConnectorRuntimePayload;
pub use connector_runtime::ConnectorRuntimeSnapshot;
pub use connector_runtime::connector_runtime_cache_path;
pub use connector_runtime::connector_runtime_context_key;
pub use metadata_store::ConnectorMetadata;
pub use metadata_store::ConnectorMetadataStore;
pub use metadata_store::ConnectorToolSummary;
pub use plugin_config::parse_plugin_app_config;
pub use plugin_config::parse_plugin_app_config_value;
pub use runtime_projection::ConnectorRuntimeTool;
pub use runtime_projection::InstalledConnectorRuntime;
pub use runtime_projection::connector_tool_is_synthetic;
pub use runtime_projection::installed_connector_runtime;
pub use snapshot::ConnectorSnapshot;
pub use snapshot::PluginConnectorSource;

pub const CONNECTOR_METADATA_CACHE_TTL: Duration = Duration::from_secs(3600);

fn connector_name_slug(name: &str) -> String {
    let normalized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let normalized = normalized.trim_matches('-');
    if normalized.is_empty() {
        "app".to_string()
    } else {
        normalized.to_string()
    }
}

fn normalize_connector_value(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}
