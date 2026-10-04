//! Source-neutral plugin discovery types.

use codex_utils_path_uri::PathUri;
use std::collections::BTreeMap;

/// One source's complete discovery snapshot; no entries means no plugins were found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginCatalog {
    pub entries: Vec<PluginCatalogEntry>,
    pub warnings: Vec<String>,
}

/// Manifest metadata and every location that can supply the same logical plugin.
#[derive(Clone, PartialEq, Eq)]
pub struct PluginCatalogEntry {
    pub id: PluginIdentity,
    pub display_name: String,
    pub version: Option<String>,
    /// Serialized `.mcp.json` server declarations keyed by their authored names.
    pub mcp_servers: BTreeMap<String, String>,
    pub connector_ids: Vec<String>,
    pub locations: Vec<PluginSourceLocation>,
}

impl std::fmt::Debug for PluginCatalogEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginCatalogEntry")
            .field("id", &self.id)
            .field("display_name", &self.display_name)
            .field("version", &self.version)
            // Declaration values can contain secrets, so only log their keys.
            .field(
                "mcp_server_names",
                &self.mcp_servers.keys().collect::<Vec<_>>(),
            )
            .field("connector_ids", &self.connector_ids)
            .field("locations", &self.locations)
            .finish()
    }
}

/// Stable key used to merge discoveries of the same logical plugin.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PluginIdentity {
    /// Local `name@marketplace` config key for a plugin without a remote ID.
    Local { plugin_id: String },
}

impl PluginIdentity {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Local { plugin_id } => plugin_id,
        }
    }
}

/// A local plugin source owned by a configured executor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginSourceLocation {
    Executor {
        /// Executor environment that owns `root`.
        environment_id: String,
        /// Local `name@marketplace` key used for this installation in configuration.
        plugin_id: String,
        root: PathUri,
    },
}
