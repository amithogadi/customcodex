mod catalog;
mod command_migration;
mod error_subtype;
mod executor_hooks;
mod executor_provider;
pub mod installed_marketplaces;
pub mod loader;
mod manager;
pub mod manifest;
pub mod marketplace;
pub mod marketplace_add;
mod marketplace_policy;
pub mod marketplace_remove;
mod provider;
mod skill_snapshots;
pub mod store;
pub mod toggles;

pub const OPENAI_CURATED_MARKETPLACE_NAME: &str = "openai-curated";
pub const OPENAI_API_CURATED_MARKETPLACE_NAME: &str = "openai-api-curated";
pub const OPENAI_BUNDLED_MARKETPLACE_NAME: &str = "openai-bundled";
pub(crate) const OPENAI_BUNDLED_ALPHA_MARKETPLACE_NAME: &str = "openai-bundled-alpha";
pub(crate) const OPENAI_PRIMARY_RUNTIME_MARKETPLACE_NAME: &str = "openai-primary-runtime";

pub fn is_openai_curated_marketplace_name(marketplace_name: &str) -> bool {
    marketplace_name == OPENAI_CURATED_MARKETPLACE_NAME
        || marketplace_name == OPENAI_API_CURATED_MARKETPLACE_NAME
}

pub type LoadedPlugin = codex_plugin::LoadedPlugin<codex_config::McpServerConfig>;
pub type PluginLoadOutcome = codex_plugin::PluginLoadOutcome<codex_config::McpServerConfig>;

pub use catalog::PluginCatalog;
pub use catalog::PluginCatalogEntry;
pub use catalog::PluginIdentity;
pub use catalog::PluginSourceLocation;
pub use command_migration::CommandDescriptionMode;
pub use command_migration::CommandMigrationProfile;
pub use command_migration::RewriteProfile as CommandRewriteProfile;
pub use command_migration::count_missing_commands_with_profile;
pub use command_migration::import_commands_with_profile;
pub use command_migration::missing_command_names_with_profile;
pub use executor_hooks::executor_plugin_hook_sources;
pub use executor_provider::ExecutorPluginProvider;
pub use executor_provider::ExecutorPluginProviderError;
pub use executor_provider::ResolvedExecutorPlugin;
pub use loader::PluginHookLoadOutcome;
pub use manager::ConfiguredMarketplace;
pub use manager::ConfiguredMarketplaceListOutcome;
pub use manager::ConfiguredMarketplacePlugin;
pub use manager::EffectivePluginsChange;
pub use manager::PluginDetail;
pub use manager::PluginDetailsUnavailableReason;
pub use manager::PluginInstallError;
pub use manager::PluginInstallOutcome;
pub use manager::PluginInstallRequest;
pub use manager::PluginMarketplaceContext;
pub use manager::PluginMarketplaceScope;
pub use manager::PluginReadOutcome;
pub use manager::PluginReadRequest;
pub use manager::PluginUninstallError;
pub use manager::PluginsConfigInput;
pub use manager::PluginsManager;
pub use marketplace_policy::allowed_configured_marketplace_names;
pub use provider::PluginListQuery;
pub use provider::PluginProvider;
pub use provider::PluginProviderError;
pub use provider::PluginProviderFuture;
pub use provider::PluginProviderResult;

#[cfg(test)]
mod test_support;
