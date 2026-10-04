//! Aggregates request-wide plugin settings and repository-scoped marketplaces.
//!
//! Marketplace paths and plugin sources use first-scope precedence, while plugin state is
//! merged across scopes. Cache refresh reuses listed marketplaces and configured plugin identities.

use super::*;

/// Request-wide plugin settings and the ordered repository configurations for one marketplace request.
#[derive(Clone)]
pub struct PluginMarketplaceContext {
    pub global_config: PluginsConfigInput,
    pub scopes: Vec<PluginMarketplaceScope>,
    pub load_errors: Vec<MarketplaceListError>,
}

/// A repository's effective configuration, or non-project configuration when no cwd was supplied.
#[derive(Clone)]
pub struct PluginMarketplaceScope {
    pub cwd: Option<AbsolutePathBuf>,
    pub config: PluginsConfigInput,
}

impl PluginMarketplaceContext {
    pub fn plugins_enabled(&self) -> bool {
        self.global_config.plugins_enabled
            || self.scopes.iter().any(|scope| scope.config.plugins_enabled)
    }

    pub(super) fn list_marketplaces(
        &self,
        manager: &PluginsManager,
        include_openai_curated: bool,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        let mut plugin_states = ConfiguredPluginStates::default();
        for scope in self
            .scopes
            .iter()
            .filter(|scope| scope.config.plugins_enabled)
        {
            let state = manager.configured_plugin_states(&scope.config);
            plugin_states.installed.extend(state.installed);
            plugin_states.enabled.extend(state.enabled);
        }

        let mut combined = ConfiguredMarketplaceListOutcome {
            errors: self.load_errors.clone(),
            ..Default::default()
        };
        let mut seen_marketplace_paths = HashSet::new();
        let mut seen_plugin_ids = HashSet::new();
        let mut seen_error_paths = combined
            .errors
            .iter()
            .map(|error| error.path.clone())
            .collect::<HashSet<_>>();

        for scope in self
            .scopes
            .iter()
            .filter(|scope| scope.config.plugins_enabled)
        {
            let outcome = manager.list_marketplaces_for_config_with_states(
                &scope.config,
                scope.cwd.as_slice(),
                include_openai_curated,
                &plugin_states,
            )?;
            for mut marketplace in outcome.marketplaces {
                if !seen_marketplace_paths.insert(marketplace.path.clone()) {
                    continue;
                }
                marketplace
                    .plugins
                    .retain(|plugin| seen_plugin_ids.insert(plugin.id.clone()));
                if !marketplace.plugins.is_empty() {
                    combined.marketplaces.push(marketplace);
                }
            }
            combined.errors.extend(
                outcome
                    .errors
                    .into_iter()
                    .filter(|error| seen_error_paths.insert(error.path.clone())),
            );
        }

        Ok(combined)
    }
}
