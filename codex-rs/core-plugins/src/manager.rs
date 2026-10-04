//! Local plugin installation and loading without network discovery.
#[path = "marketplace_context.rs"]
mod marketplace_context;
use super::PluginLoadOutcome;
use crate::installed_marketplaces::installed_marketplace_roots_from_layer_stack;
use crate::loader::{
    load_plugin_hooks, load_plugin_hooks_from_layer_stack,
    load_plugin_mcp_servers_from_manifest_with_format, load_plugin_skill_inventory,
    load_plugins_from_layer_stack, log_plugin_load_errors, materialize_marketplace_plugin_source,
};
use crate::manifest::{PluginManifestFormat, PluginManifestInterface};
use crate::marketplace::{
    MarketplaceError, MarketplaceInterface, MarketplaceListError, MarketplaceListOutcome,
    MarketplacePluginAuthPolicy, MarketplacePluginManifestFallback, MarketplacePluginPolicy,
    MarketplacePluginSource, ResolvedMarketplacePlugin, find_installable_marketplace_plugin,
    find_marketplace_plugin, home_dir, list_marketplaces_with_cache,
    plugin_interface_with_marketplace_category,
};
use crate::marketplace_policy::{MarketplacePolicy, configured_plugins_from_stack};
use crate::skill_snapshots::new_plugin_skill_snapshots;
use crate::store::{PluginStore, PluginStoreError, error_context_sub_error_type};
use codex_config::{
    ConfigLayerStack, clear_user_plugin, set_user_plugin_enabled, skill_config_rules_from_stack,
};
use codex_hooks::plugin_hook_declarations;
use codex_plugin::{
    AppConnectorId, PluginCapabilitySummary, PluginId, PluginIdError,
    prompt_safe_plugin_description,
};
use codex_protocol::protocol::{HookEventName, Product};
use codex_skills::{SkillMetadata, SkillRootLoader, SkillRootSnapshots};
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_plugins::{PluginIdentity, PluginSkillRoot};
pub use marketplace_context::{PluginMarketplaceContext, PluginMarketplaceScope};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::instrument;
#[derive(Debug, Clone)]
pub struct PluginsConfigInput {
    pub config_layer_stack: ConfigLayerStack,
    pub plugins_enabled: bool,
}
impl PluginsConfigInput {
    pub fn new(config_layer_stack: ConfigLayerStack, plugins_enabled: bool) -> Self {
        Self {
            config_layer_stack,
            plugins_enabled,
        }
    }
}
#[derive(Debug, Clone, Default)]
pub struct EffectivePluginsChange {}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallRequest {
    pub plugin_name: String,
    pub marketplace_path: AbsolutePathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginReadRequest {
    pub plugin_name: String,
    pub marketplace_path: AbsolutePathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallOutcome {
    pub plugin_id: PluginId,
    pub plugin_version: String,
    pub installed_path: AbsolutePathBuf,
    pub auth_policy: MarketplacePluginAuthPolicy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginReadOutcome {
    pub marketplace_name: String,
    pub marketplace_path: Option<AbsolutePathBuf>,
    pub plugin: PluginDetail,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginDetail {
    pub id: String,
    pub name: String,
    pub local_version: Option<String>,
    pub description: Option<String>,
    pub source: MarketplacePluginSource,
    pub policy: MarketplacePluginPolicy,
    pub interface: Option<PluginManifestInterface>,
    pub keywords: Vec<String>,
    pub installed: bool,
    pub enabled: bool,
    pub skills: Vec<SkillMetadata>,
    pub disabled_skill_paths: HashSet<AbsolutePathBuf>,
    /// Packaged onboarding path; callers apply visibility and enablement.
    pub onboarding_skill: Option<AbsolutePathBuf>,
    pub hooks: Vec<PluginHookSummary>,
    pub apps: Vec<AppConnectorId>,
    pub app_category_by_id: HashMap<String, String>,
    pub mcp_server_names: Vec<String>,
    pub details_unavailable_reason: Option<PluginDetailsUnavailableReason>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginHookSummary {
    pub key: String,
    pub event_name: HookEventName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginDetailsUnavailableReason {
    InstallRequiredForRemoteSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredMarketplace {
    pub name: String,
    pub path: AbsolutePathBuf,
    pub interface: Option<MarketplaceInterface>,
    pub plugins: Vec<ConfiguredMarketplacePlugin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredMarketplacePlugin {
    pub id: String,
    pub name: String,
    pub local_version: Option<String>,
    pub installed_version: Option<String>,
    pub source: MarketplacePluginSource,
    pub policy: MarketplacePluginPolicy,
    pub interface: Option<PluginManifestInterface>,
    pub keywords: Vec<String>,
    pub manifest_fallback: Option<MarketplacePluginManifestFallback>,
    pub installed: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfiguredMarketplaceListOutcome {
    pub marketplaces: Vec<ConfiguredMarketplace>,
    pub errors: Vec<MarketplaceListError>,
}

#[derive(Default)]
struct ConfiguredPluginStates {
    installed: HashSet<String>,
    enabled: HashSet<String>,
}

impl From<PluginDetail> for PluginCapabilitySummary {
    fn from(value: PluginDetail) -> Self {
        let has_skills = value.skills.iter().any(|skill| {
            !value
                .disabled_skill_paths
                .contains(&skill.path_to_skills_md)
        });
        Self {
            config_name: value.id,
            display_name: value.name,
            plugin_namespace: None,
            description: prompt_safe_plugin_description(value.description.as_deref()),
            has_skills,
            mcp_server_names: value.mcp_server_names,
            app_connector_ids: value.apps,
        }
    }
}

pub struct PluginsManager {
    codex_home: PathBuf,
    store: PluginStore,
    skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    restriction_product: Option<Product>,
    skill_snapshots: Mutex<Option<(String, SkillRootSnapshots<PluginSkillRoot>)>>,
}
impl PluginsManager {
    pub fn new(
        codex_home: PathBuf,
        skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    ) -> Self {
        Self::new_with_options(codex_home, Some(Product::Codex), skill_root_loader)
    }
    pub fn new_with_options(
        codex_home: PathBuf,
        restriction_product: Option<Product>,
        skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    ) -> Self {
        Self {
            store: PluginStore::new(codex_home.clone()),
            codex_home,
            skill_root_loader,
            restriction_product,
            skill_snapshots: Mutex::new(None),
        }
    }
    pub fn clear_cache(&self) {
        *self
            .skill_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
    pub fn plugin_skill_snapshots_for_config(
        &self,
        config: &PluginsConfigInput,
    ) -> Option<SkillRootSnapshots<PluginSkillRoot>> {
        let key = config.config_layer_stack.effective_config().to_string();
        self.skill_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|(stored, _)| config.plugins_enabled && stored == &key)
            .map(|(_, snapshots)| snapshots.clone())
    }
    pub async fn plugins_for_config(&self, config: &PluginsConfigInput) -> PluginLoadOutcome {
        if !config.plugins_enabled {
            return PluginLoadOutcome::default();
        }
        let snapshots = new_plugin_skill_snapshots();
        let plugins = load_plugins_from_layer_stack(
            &config.config_layer_stack,
            &self.store,
            Some(&snapshots),
            self.restriction_product,
            self.skill_root_loader.as_ref(),
        )
        .await;
        log_plugin_load_errors(&plugins);
        *self
            .skill_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((
            config.config_layer_stack.effective_config().to_string(),
            snapshots,
        ));
        PluginLoadOutcome::from_plugins(plugins)
    }
    pub async fn plugin_hooks_for_layer_stack(
        &self,
        stack: &ConfigLayerStack,
        config: &PluginsConfigInput,
    ) -> crate::loader::PluginHookLoadOutcome {
        if !config.plugins_enabled {
            return Default::default();
        }
        load_plugin_hooks_from_layer_stack(stack, &self.store).await
    }
    pub async fn install_plugin(
        &self,
        stack: &ConfigLayerStack,
        request: PluginInstallRequest,
    ) -> Result<PluginInstallOutcome, PluginInstallError> {
        let resolved = self.resolve_installable_plugin(stack, &request)?;
        let auth_policy = resolved.policy.authentication;
        let materialized =
            materialize_marketplace_plugin_source(self.codex_home.as_path(), &resolved.source)
                .map_err(PluginStoreError::Invalid)?;
        let result = match resolved.manifest_fallback.contents_if_has_metadata() {
            Some(contents) => self.store.install_with_fallback_manifest(
                materialized.path,
                resolved.plugin_id,
                &contents,
            ),
            None => self.store.install(materialized.path, resolved.plugin_id),
        }?;
        set_user_plugin_enabled(&self.codex_home, result.plugin_id.as_key(), true)
            .await
            .map_err(anyhow::Error::from)?;
        self.clear_cache();
        Ok(PluginInstallOutcome {
            plugin_id: result.plugin_id,
            plugin_version: result.plugin_version,
            installed_path: result.installed_path,
            auth_policy,
        })
    }
    pub async fn uninstall_plugin(&self, plugin_id: String) -> Result<(), PluginUninstallError> {
        let plugin_id = PluginId::parse(&plugin_id)?;
        self.store.uninstall(&plugin_id)?;
        clear_user_plugin(&self.codex_home, plugin_id.as_key())
            .await
            .map_err(anyhow::Error::from)?;
        self.clear_cache();
        Ok(())
    }
    fn restriction_product_matches(&self, products: Option<&[Product]>) -> bool {
        match products {
            None => true,
            Some([]) => false,
            Some(products) => self
                .restriction_product
                .is_some_and(|product| product.matches_product_restriction(products)),
        }
    }

    fn resolve_installable_plugin(
        &self,
        config_layer_stack: &ConfigLayerStack,
        request: &PluginInstallRequest,
    ) -> Result<ResolvedMarketplacePlugin, PluginInstallError> {
        let resolved = match find_installable_marketplace_plugin(
            &request.marketplace_path,
            &request.plugin_name,
            self.restriction_product,
        ) {
            Ok(resolved) => resolved,
            Err(err) => {
                return Err(err.into());
            }
        };
        if let Err(message) =
            MarketplacePolicy::from_requirements(config_layer_stack.requirements())
                .validate_install(
                    config_layer_stack,
                    self.codex_home.as_path(),
                    &request.marketplace_path,
                    &resolved.plugin_id.marketplace_name,
                )
        {
            let err = MarketplaceError::InvalidMarketplaceFile {
                path: request.marketplace_path.to_path_buf(),
                message,
            };
            return Err(err.into());
        }
        Ok(resolved)
    }

    pub fn list_marketplaces_for_context(
        &self,
        context: &PluginMarketplaceContext,
        include_openai_curated: bool,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        context.list_marketplaces(self, include_openai_curated)
    }

    pub fn list_marketplaces_for_config(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Ok(ConfiguredMarketplaceListOutcome::default());
        }

        let plugin_states = self.configured_plugin_states(config);
        self.list_marketplaces_for_config_with_states(
            config,
            additional_roots,
            include_openai_curated,
            &plugin_states,
        )
    }

    fn list_marketplaces_for_config_with_states(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
        plugin_states: &ConfiguredPluginStates,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        let marketplace_roots =
            self.marketplace_roots(config, additional_roots, include_openai_curated);
        let marketplace_outcome = self.list_marketplaces_with_policy(config, &marketplace_roots)?;
        let mut seen_plugin_keys = HashSet::new();
        let mut marketplaces: Vec<ConfiguredMarketplace> = marketplace_outcome
            .marketplaces
            .into_iter()
            .filter_map(|marketplace| {
                let marketplace_name = marketplace.name.clone();
                let plugins = marketplace
                    .plugins
                    .into_iter()
                    .filter_map(|plugin| {
                        let plugin_key = format!("{}@{marketplace_name}", plugin.name);
                        if !seen_plugin_keys.insert(plugin_key.clone()) {
                            return None;
                        }
                        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
                            return None;
                        }
                        let plugin_id =
                            PluginId::new(plugin.name.clone(), marketplace_name.clone()).ok();
                        let installed = plugin_states.installed.contains(&plugin_key);
                        let installed_version = installed.then_some(()).and_then(|_| {
                            plugin_id
                                .as_ref()
                                .and_then(|plugin_id| self.store.active_plugin_version(plugin_id))
                        });
                        let enabled = plugin_states.enabled.contains(&plugin_key);
                        let mut interface = plugin.interface;
                        let mut local_version = plugin.local_version;
                        let manifest_fallback = plugin.manifest_fallback.clone();
                        if installed
                            && plugin.source.is_install_materialized()
                            && let Some(plugin_id) = plugin_id.as_ref()
                            && let Some(plugin_root) = self.store.active_plugin_root(plugin_id)
                            && let Some(manifest) = self
                                .store
                                .manifest_cache
                                .load(plugin_root.as_path())
                                .map(|loaded| loaded.manifest)
                        {
                            local_version = manifest.version.clone();
                            let marketplace_category = interface
                                .as_ref()
                                .and_then(|interface| interface.category.clone());
                            interface = plugin_interface_with_marketplace_category(
                                manifest.interface,
                                marketplace_category,
                            );
                        }

                        Some(ConfiguredMarketplacePlugin {
                            // Enabled state is keyed by `<plugin>@<marketplace>`, so duplicate
                            // plugin entries from duplicate marketplace files intentionally
                            // resolve to the first discovered source.
                            id: plugin_key,
                            installed_version,
                            installed,
                            enabled,
                            name: plugin.name,
                            local_version,
                            source: plugin.source,
                            policy: plugin.policy,
                            keywords: plugin.keywords,
                            interface,
                            manifest_fallback,
                        })
                    })
                    .collect::<Vec<_>>();

                (!plugins.is_empty()).then_some(ConfiguredMarketplace {
                    name: marketplace.name,
                    path: marketplace.path,
                    interface: marketplace.interface,
                    plugins,
                })
            })
            .collect();

        // Installed bundles can outlive their original online catalog. Expose only
        // configured cached identities, using their local directory as the read handle.
        let mut cached_ids = plugin_states.installed.iter().collect::<Vec<_>>();
        cached_ids.sort_unstable();
        for plugin_key in cached_ids {
            if seen_plugin_keys.contains(plugin_key) {
                continue;
            }
            let Ok(plugin_id) = PluginId::parse(plugin_key) else {
                continue;
            };
            let Some(plugin) = self.cached_plugin(&plugin_id, plugin_states) else {
                continue;
            };
            let path = self.store.root().join(&plugin_id.marketplace_name);
            if let Some(marketplace) = marketplaces.iter_mut().find(|item| item.path == path) {
                marketplace.plugins.push(plugin);
            } else {
                marketplaces.push(ConfiguredMarketplace {
                    name: plugin_id.marketplace_name,
                    path,
                    interface: None,
                    plugins: vec![plugin],
                });
            }
        }

        Ok(ConfiguredMarketplaceListOutcome {
            marketplaces,
            errors: marketplace_outcome.errors,
        })
    }

    pub fn discover_marketplaces_for_config(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
    ) -> Result<MarketplaceListOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Ok(MarketplaceListOutcome::default());
        }

        let marketplace_roots = self.marketplace_roots(
            config,
            additional_roots,
            /*include_openai_curated*/ true,
        );
        self.list_marketplaces_with_policy(config, &marketplace_roots)
    }

    pub async fn read_plugin_for_config(
        &self,
        config: &PluginsConfigInput,
        request: &PluginReadRequest,
    ) -> Result<PluginReadOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Err(MarketplaceError::PluginsDisabled);
        }

        let states = self.configured_plugin_states(config);
        for plugin_key in &states.installed {
            let Ok(plugin_id) = PluginId::parse(plugin_key) else {
                continue;
            };
            if plugin_id.plugin_name == request.plugin_name
                && request.marketplace_path == self.store.root().join(&plugin_id.marketplace_name)
                && let Some(plugin) = self.cached_plugin(&plugin_id, &states)
            {
                let detail = self
                    .read_plugin_detail_for_marketplace_plugin(
                        config,
                        &plugin_id.marketplace_name,
                        plugin,
                    )
                    .await?;
                return Ok(PluginReadOutcome {
                    marketplace_name: plugin_id.marketplace_name,
                    marketplace_path: Some(request.marketplace_path.clone()),
                    plugin: detail,
                });
            }
        }

        let plugin = find_marketplace_plugin(&request.marketplace_path, &request.plugin_name)?;
        MarketplacePolicy::from_requirements(config.config_layer_stack.requirements())
            .validate_install(
                &config.config_layer_stack,
                self.codex_home.as_path(),
                &request.marketplace_path,
                &plugin.plugin_id.marketplace_name,
            )
            .map_err(|message| MarketplaceError::InvalidMarketplaceFile {
                path: request.marketplace_path.to_path_buf(),
                message,
            })?;
        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
            return Err(MarketplaceError::PluginNotFound {
                plugin_name: plugin.plugin_id.plugin_name,
                marketplace_name: plugin.plugin_id.marketplace_name,
            });
        }

        let marketplace_name = plugin.plugin_id.marketplace_name.clone();
        let plugin_key = plugin.plugin_id.as_key();
        let manifest_fallback = plugin
            .manifest_fallback
            .contents_if_has_metadata()
            .map(|_| plugin.manifest_fallback.clone());
        let plugin_states = self.configured_plugin_states(config);
        let installed = plugin_states.installed.contains(&plugin_key);
        let installed_version = if installed {
            self.store.active_plugin_version(&plugin.plugin_id)
        } else {
            None
        };
        let plugin = self
            .read_plugin_detail_for_marketplace_plugin(
                config,
                &marketplace_name,
                ConfiguredMarketplacePlugin {
                    id: plugin_key.clone(),
                    name: plugin.plugin_id.plugin_name,
                    local_version: plugin
                        .manifest
                        .as_ref()
                        .and_then(|manifest| manifest.version.clone()),
                    installed_version,
                    source: plugin.source,
                    policy: plugin.policy,
                    interface: plugin.interface,
                    keywords: plugin
                        .manifest
                        .as_ref()
                        .map(|manifest| manifest.keywords.clone())
                        .unwrap_or_default(),
                    manifest_fallback,
                    installed,
                    enabled: plugin_states.enabled.contains(&plugin_key),
                },
            )
            .await?;

        Ok(PluginReadOutcome {
            marketplace_name,
            marketplace_path: Some(request.marketplace_path.clone()),
            plugin,
        })
    }

    pub async fn read_plugin_detail_for_marketplace_plugin(
        &self,
        config: &PluginsConfigInput,
        marketplace_name: &str,
        plugin: ConfiguredMarketplacePlugin,
    ) -> Result<PluginDetail, MarketplaceError> {
        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
            return Err(MarketplaceError::PluginNotFound {
                plugin_name: plugin.name,
                marketplace_name: marketplace_name.to_string(),
            });
        }

        let plugin_id =
            PluginId::new(plugin.name.clone(), marketplace_name.to_string()).map_err(|err| {
                match err {
                    PluginIdError::Invalid(message) => MarketplaceError::InvalidPlugin(message),
                }
            })?;
        let plugin_key = plugin_id.as_key();
        if plugin.source.is_install_materialized() && !plugin.installed {
            let description = remote_plugin_install_required_description(&plugin.source);
            return Ok(PluginDetail {
                id: plugin_key,
                name: plugin.name,
                local_version: None,
                description: Some(description),
                source: plugin.source,
                policy: plugin.policy,
                interface: plugin.interface,
                keywords: plugin.keywords,
                installed: plugin.installed,
                enabled: plugin.enabled,
                skills: Vec::new(),
                disabled_skill_paths: HashSet::new(),
                onboarding_skill: None,
                hooks: Vec::new(),
                apps: Vec::new(),
                app_category_by_id: HashMap::new(),
                mcp_server_names: Vec::new(),
                details_unavailable_reason: Some(
                    PluginDetailsUnavailableReason::InstallRequiredForRemoteSource,
                ),
            });
        }

        let source_path = if plugin.source.is_install_materialized() && plugin.installed {
            self.store.active_plugin_root(&plugin_id).ok_or_else(|| {
                MarketplaceError::InvalidPlugin(format!(
                    "installed plugin cache entry is missing for {plugin_key}"
                ))
            })?
        } else {
            let codex_home = self.codex_home.clone();
            let source = plugin.source.clone();
            let materialized = tokio::task::spawn_blocking(move || {
                materialize_marketplace_plugin_source(codex_home.as_path(), &source)
            })
            .await
            .map_err(|err| {
                MarketplaceError::InvalidPlugin(format!(
                    "failed to materialize plugin source: {err}"
                ))
            })?
            .map_err(MarketplaceError::InvalidPlugin)?;
            materialized.path.clone()
        };
        if !source_path.as_path().is_dir() {
            return Err(MarketplaceError::InvalidPlugin(
                "path does not exist or is not a directory".to_string(),
            ));
        }
        let loaded_manifest =
            if codex_utils_plugins::find_plugin_manifest_path(source_path.as_path()).is_some() {
                self.store.manifest_cache.load(source_path.as_path())
            } else {
                plugin
                    .manifest_fallback
                    .as_ref()
                    .and_then(|fallback| fallback.parse_for_plugin_root(source_path.as_path()))
                    .map(|manifest| crate::manifest::LoadedPluginManifest {
                        manifest,
                        format: PluginManifestFormat::Legacy,
                    })
            }
            .ok_or_else(|| {
                MarketplaceError::InvalidPlugin("missing or invalid plugin.json".to_string())
            })?;
        let manifest_format = loaded_manifest.format;
        let manifest = loaded_manifest.manifest;
        let description = manifest.description.clone();
        let marketplace_category = plugin
            .interface
            .as_ref()
            .and_then(|interface| interface.category.clone());
        let interface = plugin_interface_with_marketplace_category(
            manifest.interface.clone(),
            marketplace_category,
        );
        let plugin_identity = PluginIdentity {
            plugin_id: plugin_id.as_key(),
            remote_plugin_id: None,
        };
        let skill_config_rules = skill_config_rules_from_stack(&config.config_layer_stack);
        let resolved_skills = load_plugin_skill_inventory(
            &source_path,
            &plugin_identity,
            &manifest,
            manifest_format,
            self.restriction_product,
            /*plugin_skill_snapshots*/ None,
            self.skill_root_loader.as_ref(),
        )
        .await
        .resolve(&skill_config_rules);
        let onboarding_skill = manifest.paths.onboarding_skill.as_ref().and_then(|path| {
            let plugin_root = source_path.canonicalize().ok()?;
            let path = path.canonicalize().ok()?;
            if !path.as_path().starts_with(plugin_root.as_path()) {
                return None;
            }
            resolved_skills
                .skills
                .iter()
                .any(|skill| skill.path_to_skills_md == path)
                .then_some(path)
        });
        let plugin_data_root = self.store.plugin_data_root(&plugin_id);
        let (hook_sources, _hook_load_warnings) = if manifest_format == PluginManifestFormat::Legacy
        {
            load_plugin_hooks(&source_path, &plugin_id, &plugin_data_root, &manifest.paths)
        } else {
            (Vec::new(), Vec::new())
        };
        let hooks = plugin_hook_declarations(&hook_sources)
            .into_iter()
            .map(|hook| PluginHookSummary {
                key: hook.key,
                event_name: hook.event_name,
            })
            .collect();
        let mcp_data_root = (manifest_format == PluginManifestFormat::AgentPlugin)
            .then(|| self.store.mcp_data_root(&plugin_id, manifest_format));
        let mcp_servers = load_plugin_mcp_servers_from_manifest_with_format(
            source_path.as_path(),
            &manifest.paths,
            /*plugin_policy*/ None,
            mcp_data_root.as_deref(),
            manifest_format,
        )
        .await;
        let apps = Vec::new();
        let app_category_by_id = HashMap::new();
        let mut mcp_server_names = mcp_servers.into_keys().collect::<Vec<_>>();
        mcp_server_names.sort_unstable();
        mcp_server_names.dedup();

        Ok(PluginDetail {
            id: plugin.id,
            name: plugin.name,
            local_version: manifest.version.clone(),
            description,
            source: plugin.source,
            policy: plugin.policy,
            interface,
            keywords: manifest.keywords,
            installed: plugin.installed,
            enabled: plugin.enabled,
            skills: resolved_skills.skills,
            disabled_skill_paths: resolved_skills.disabled_skill_paths,
            onboarding_skill,
            hooks,
            apps,
            app_category_by_id,
            mcp_server_names,
            details_unavailable_reason: None,
        })
    }

    fn cached_plugin(
        &self,
        id: &PluginId,
        states: &ConfiguredPluginStates,
    ) -> Option<ConfiguredMarketplacePlugin> {
        let path = self.store.active_plugin_root(id)?;
        let manifest = self.store.manifest_cache.load(path.as_path())?.manifest;
        Some(ConfiguredMarketplacePlugin {
            id: id.as_key(),
            name: id.plugin_name.clone(),
            local_version: manifest.version.clone(),
            installed_version: self.store.active_plugin_version(id),
            source: MarketplacePluginSource::Local { path },
            policy: MarketplacePluginPolicy {
                installation: crate::marketplace::MarketplacePluginInstallPolicy::Available,
                authentication: MarketplacePluginAuthPolicy::OnUse,
                products: None,
            },
            interface: manifest.interface,
            keywords: manifest.keywords,
            manifest_fallback: None,
            installed: true,
            enabled: states.enabled.contains(&id.as_key()),
        })
    }

    fn configured_plugin_states(&self, config: &PluginsConfigInput) -> ConfiguredPluginStates {
        let configured_plugins =
            configured_plugins_from_stack(&config.config_layer_stack, self.codex_home.as_path());
        let installed = configured_plugins
            .keys()
            .filter(|plugin_key| {
                PluginId::parse(plugin_key)
                    .ok()
                    .is_some_and(|plugin_id| self.store.is_installed(&plugin_id))
            })
            .cloned()
            .collect::<HashSet<_>>();
        let enabled = configured_plugins
            .into_iter()
            .filter_map(|(plugin_key, plugin)| plugin.enabled.then_some(plugin_key))
            .collect::<HashSet<_>>();
        ConfiguredPluginStates { installed, enabled }
    }

    fn list_marketplaces_with_policy(
        &self,
        config: &PluginsConfigInput,
        roots: &[AbsolutePathBuf],
    ) -> Result<MarketplaceListOutcome, MarketplaceError> {
        let mut outcome =
            list_marketplaces_with_cache(roots, home_dir().as_deref(), &self.store.manifest_cache)?;
        let policy = MarketplacePolicy::from_requirements(config.config_layer_stack.requirements());
        outcome.marketplaces.retain(|marketplace| {
            policy
                .validate_install(
                    &config.config_layer_stack,
                    self.codex_home.as_path(),
                    &marketplace.path,
                    &marketplace.name,
                )
                .is_ok()
        });
        Ok(outcome)
    }

    fn marketplace_roots(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        _include_legacy_cached: bool,
    ) -> Vec<AbsolutePathBuf> {
        let mut roots = additional_roots.to_vec();
        roots.extend(installed_marketplace_roots_from_layer_stack(
            &config.config_layer_stack,
            self.codex_home.as_path(),
        ));
        roots.sort_unstable();
        roots.dedup();
        roots
    }
}
pub(crate) fn remote_plugin_install_required_description(
    source: &MarketplacePluginSource,
) -> String {
    let source_description = match source {
        MarketplacePluginSource::Git {
            url,
            path,
            ref_name,
            sha,
        } => {
            let mut parts = vec![url.clone()];
            if let Some(path) = path {
                parts.push(format!("path `{path}`"));
            }
            if let Some(ref_name) = ref_name {
                parts.push(format!("ref `{ref_name}`"));
            }
            if let Some(sha) = sha {
                parts.push(format!("sha `{sha}`"));
            }
            parts.join(", ")
        }
        MarketplacePluginSource::Local { path } => path.as_path().display().to_string(),
        MarketplacePluginSource::Npm {
            package,
            version,
            registry,
        } => {
            let mut parts = vec![package.clone()];
            if let Some(version) = version {
                parts.push(format!("version `{version}`"));
            }
            if let Some(registry) = registry {
                parts.push(format!("registry `{registry}`"));
            }
            parts.join(", ")
        }
    };

    let source_kind = if matches!(source, MarketplacePluginSource::Npm { .. }) {
        "an npm plugin"
    } else {
        "a cross-repo plugin"
    };
    format!(
        "This is {source_kind}. Online installation is unsupported; install a local copy to view its contents. The source of the plugin is {source_description}."
    )
}
#[derive(Debug, thiserror::Error)]
pub enum PluginInstallError {
    #[error("{0}")]
    Marketplace(#[from] MarketplaceError),

    #[error("{0}")]
    Store(#[from] PluginStoreError),

    #[error("{0}")]
    Config(#[from] anyhow::Error),

    #[error("failed to join plugin install task: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl PluginInstallError {
    fn join(source: tokio::task::JoinError) -> Self {
        Self::Join(source)
    }

    pub fn is_invalid_request(&self) -> bool {
        matches!(
            self,
            Self::Marketplace(
                MarketplaceError::MarketplaceNotFound { .. }
                    | MarketplaceError::InvalidMarketplaceFile { .. }
                    | MarketplaceError::PluginNotFound { .. }
                    | MarketplaceError::PluginNotAvailable { .. }
                    | MarketplaceError::InvalidPlugin(_)
            ) | Self::Store(PluginStoreError::Invalid(_))
        )
    }

    pub fn sub_error_type(&self) -> Option<String> {
        match self {
            Self::Marketplace(err) => marketplace_error_sub_error_type(err),
            Self::Store(err) => err.sub_error_type(),
            Self::Config(_) => Some("failed_to_enable_plugin".to_string()),
            Self::Join(_) => Some("plugin_install_task_failed".to_string()),
        }
    }
}

fn marketplace_error_sub_error_type(err: &MarketplaceError) -> Option<String> {
    match err {
        MarketplaceError::Io { context, .. } => Some(error_context_sub_error_type(context)),
        MarketplaceError::MarketplaceNotFound { .. }
        | MarketplaceError::InvalidMarketplaceFile { .. }
        | MarketplaceError::PluginNotFound { .. }
        | MarketplaceError::PluginNotAvailable { .. }
        | MarketplaceError::PluginsDisabled
        | MarketplaceError::InvalidPlugin(_) => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PluginUninstallError {
    #[error("{0}")]
    InvalidPluginId(#[from] PluginIdError),

    #[error("{0}")]
    Store(#[from] PluginStoreError),

    #[error("{0}")]
    Config(#[from] anyhow::Error),

    #[error("failed to join plugin uninstall task: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl PluginUninstallError {
    fn join(source: tokio::task::JoinError) -> Self {
        Self::Join(source)
    }

    pub fn is_invalid_request(&self) -> bool {
        matches!(self, Self::InvalidPluginId(_))
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
