use crate::manifest::ManifestCache;
use crate::manifest::PluginManifest;
use crate::manifest::PluginManifestFormat;
use crate::manifest::PluginManifestHooks;
use crate::manifest::PluginManifestMcpServers;
use crate::manifest::PluginManifestPaths;
use crate::manifest::load_plugin_manifest_with_format;
use crate::marketplace::MarketplacePluginSource;
use crate::marketplace_policy::configured_plugins_from_stack;
use crate::store::PluginStore;
use codex_config::ConfigLayerStack;
use codex_config::HooksFile;
use codex_config::SkillConfigRules;
use codex_config::skill_config_rules_from_stack;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_config::types::PluginConfig;
use codex_config::types::PluginMcpServerConfig;
use codex_mcp::parse_agent_plugin_mcp_config;
use codex_mcp::parse_plugin_mcp_config;
use codex_plugin::AppDeclaration;
use codex_plugin::LoadedPlugin;
use codex_plugin::PluginCapabilitySummary;
use codex_plugin::PluginHookSource;
use codex_plugin::PluginId;
use codex_plugin::PluginIdError;
use codex_protocol::auth::AuthMode;
use codex_protocol::protocol::Product;
use codex_skills::SkillMetadata;
use codex_skills::SkillRootLoadRequest;
use codex_skills::SkillRootLoader;
use codex_skills::SkillRootSnapshots;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_plugins::PluginIdentity;
use codex_utils_plugins::PluginSkillRoot;
use codex_utils_plugins::SkillDiscoveryMode;
use codex_utils_plugins::find_plugin_manifest_path;
use codex_utils_plugins::migrated_command_skills_root;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use tempfile::TempDir;
use tracing::instrument;
use tracing::warn;

#[path = "agent_plugin_mcp_overlay.rs"]
mod agent_plugin_mcp_overlay;

const DEFAULT_SKILLS_DIR_NAME: &str = "skills";
const DEFAULT_HOOKS_CONFIG_FILE: &str = "hooks/hooks.json";
const DEFAULT_MCP_CONFIG_FILE: &str = ".mcp.json";
const CONFIG_TOML_FILE: &str = "config.toml";
const CURATED_PLUGIN_CACHE_VERSION_SHA_PREFIX_LEN: usize = 8;

/// Hook declarations and warnings resolved without loading other plugin capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginHookLoadOutcome {
    pub hook_sources: Vec<PluginHookSource>,
    pub hook_load_warnings: Vec<String>,
}

enum PluginLoadScope<'a> {
    AllCapabilities {
        restriction_product: Option<Product>,
        skill_config_rules: &'a SkillConfigRules,
        plugin_skill_snapshots: Option<&'a SkillRootSnapshots<PluginSkillRoot>>,
        skill_root_loader: &'a dyn SkillRootLoader<PluginSkillRoot>,
    },
    HooksOnly,
}

pub(crate) fn log_plugin_load_errors(plugins: &[LoadedPlugin<McpServerConfig>]) {
    for plugin in plugins.iter().filter(|plugin| plugin.error.is_some()) {
        if let Some(error) = plugin.error.as_deref() {
            warn!(
                plugin = plugin.config_name,
                path = %plugin.root.display(),
                "failed to load plugin: {error}"
            );
        }
    }
}

pub(crate) async fn load_plugins_from_layer_stack(
    stack: &ConfigLayerStack,
    store: &PluginStore,
    snapshots: Option<&SkillRootSnapshots<PluginSkillRoot>>,
    restriction_product: Option<Product>,
    skill_root_loader: &dyn SkillRootLoader<PluginSkillRoot>,
) -> Vec<LoadedPlugin<McpServerConfig>> {
    let rules = skill_config_rules_from_stack(stack);
    load_plugins_from_layer_stack_with_scope(
        stack,
        store,
        PluginLoadScope::AllCapabilities {
            restriction_product,
            skill_config_rules: &rules,
            plugin_skill_snapshots: snapshots,
            skill_root_loader,
        },
    )
    .await
}
async fn load_plugins_from_layer_stack_with_scope(
    stack: &ConfigLayerStack,
    store: &PluginStore,
    scope: PluginLoadScope<'_>,
) -> Vec<LoadedPlugin<McpServerConfig>> {
    let mut configured = configured_plugins_from_stack(stack, store.codex_home().as_path())
        .into_iter()
        .collect::<Vec<_>>();
    configured.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut plugins = Vec::new();
    for (id, config) in configured {
        plugins.push(load_plugin(id, &config, store, &scope).await);
    }
    plugins
}
pub async fn load_plugin_hooks_from_layer_stack(
    stack: &ConfigLayerStack,
    store: &PluginStore,
) -> PluginHookLoadOutcome {
    let plugins =
        load_plugins_from_layer_stack_with_scope(stack, store, PluginLoadScope::HooksOnly).await;
    PluginHookLoadOutcome {
        hook_sources: plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| plugin.hook_sources.iter().cloned())
            .collect(),
        hook_load_warnings: plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| plugin.hook_load_warnings.iter().cloned())
            .collect(),
    }
}
async fn load_plugin(
    config_name: String,
    plugin: &PluginConfig,
    store: &PluginStore,
    scope: &PluginLoadScope<'_>,
) -> LoadedPlugin<McpServerConfig> {
    let plugin_id = PluginId::parse(&config_name);
    let active_plugin_installation = plugin_id
        .as_ref()
        .ok()
        .and_then(|plugin_id| store.active_plugin_installation(plugin_id));
    let root = active_plugin_installation
        .as_ref()
        .map(|installation| installation.root.clone())
        .unwrap_or_else(|| match &plugin_id {
            Ok(plugin_id) => store.plugin_base_root(plugin_id),
            Err(_) => store.root().clone(),
        });
    let mut loaded_plugin = LoadedPlugin {
        config_name,
        remote_plugin_id: None,
        manifest_name: None,
        plugin_namespace: None,
        manifest_description: None,
        root,
        enabled: plugin.enabled,
        skill_roots: Vec::new(),
        skill_discovery_mode: SkillDiscoveryMode::Recursive,
        disabled_skill_paths: HashSet::new(),
        has_enabled_skills: false,
        mcp_servers: HashMap::new(),
        apps: Vec::new(),
        hook_sources: Vec::new(),
        hook_load_warnings: Vec::new(),
        error: None,
    };

    if !plugin.enabled {
        return loaded_plugin;
    }

    let (loaded_plugin_id, installation) = match plugin_id {
        Ok(plugin_id) => {
            let Some(installation) = active_plugin_installation else {
                loaded_plugin.error = Some("plugin is not installed".to_string());
                return loaded_plugin;
            };
            (plugin_id, installation)
        }
        Err(err) => {
            loaded_plugin.error = Some(err.to_string());
            return loaded_plugin;
        }
    };

    let plugin_root = installation.root;

    if !plugin_root.as_path().is_dir() {
        loaded_plugin.error = Some("path does not exist or is not a directory".to_string());
        return loaded_plugin;
    }

    let Some(loaded_manifest) = store.manifest_cache.load(plugin_root.as_path()) else {
        loaded_plugin.error = Some("missing or invalid plugin.json".to_string());
        return loaded_plugin;
    };
    loaded_plugin.skill_discovery_mode = match loaded_manifest.format {
        PluginManifestFormat::Legacy => SkillDiscoveryMode::Recursive,
        PluginManifestFormat::AgentPlugin => SkillDiscoveryMode::DirectChildren,
    };
    let manifest = loaded_manifest.manifest;

    let manifest_paths = &manifest.paths;
    let plugin_data_root = store.plugin_data_root(&loaded_plugin_id);
    let mcp_plugin_data_root = store.mcp_data_root(&loaded_plugin_id, loaded_manifest.format);
    loaded_plugin.plugin_namespace = Some(manifest.name.clone());
    match scope {
        PluginLoadScope::AllCapabilities {
            restriction_product,
            skill_config_rules,
            plugin_skill_snapshots,
            skill_root_loader,
        } => {
            loaded_plugin.manifest_name = Some(manifest.display_name().to_string());
            loaded_plugin.manifest_description = manifest.description.clone();
            loaded_plugin.skill_roots =
                plugin_skill_roots(&plugin_root, manifest_paths, loaded_manifest.format);
            let plugin_identity = PluginIdentity {
                plugin_id: loaded_plugin_id.as_key(),
                remote_plugin_id: loaded_plugin.remote_plugin_id.clone(),
            };
            let resolved_skills = load_plugin_skill_inventory(
                &plugin_root,
                &plugin_identity,
                &manifest,
                loaded_manifest.format,
                *restriction_product,
                *plugin_skill_snapshots,
                *skill_root_loader,
            )
            .await
            .resolve(skill_config_rules);
            let has_enabled_skills = resolved_skills.has_enabled_skills();
            loaded_plugin.disabled_skill_paths = resolved_skills.disabled_skill_paths;
            loaded_plugin.has_enabled_skills = has_enabled_skills;
            loaded_plugin.mcp_servers = load_plugin_mcp_servers_from_manifest_with_format(
                plugin_root.as_path(),
                manifest_paths,
                Some(&plugin.mcp_servers),
                Some(mcp_plugin_data_root.as_path()),
                loaded_manifest.format,
            )
            .await;
        }
        PluginLoadScope::HooksOnly => {}
    }
    let (hook_sources, hook_load_warnings) =
        if loaded_manifest.format == PluginManifestFormat::AgentPlugin {
            (Vec::new(), Vec::new())
        } else {
            load_plugin_hooks(
                &plugin_root,
                &loaded_plugin_id,
                &plugin_data_root,
                manifest_paths,
            )
        };
    loaded_plugin.hook_sources = hook_sources;
    loaded_plugin.hook_load_warnings = hook_load_warnings;
    loaded_plugin
}

fn apply_plugin_mcp_server_policy(config: &mut McpServerConfig, policy: &PluginMcpServerConfig) {
    config.enabled = policy.enabled && !policy.has_unsupported_ema_auth;
    if policy.has_unsupported_ema_auth {
        config.auth = codex_config::McpServerAuth::EmaAuth;
    }
    if let Some(approval_mode) = policy.default_tools_approval_mode {
        config.default_tools_approval_mode = Some(approval_mode);
    }
    if let Some(enabled_tools) = &policy.enabled_tools {
        config.enabled_tools = Some(enabled_tools.clone());
    }
    if let Some(disabled_tools) = &policy.disabled_tools {
        config.disabled_tools = Some(disabled_tools.clone());
    }
    for (tool_name, tool_policy) in &policy.tools {
        let tool_config = config.tools.entry(tool_name.clone()).or_default();
        if let Some(approval_mode) = tool_policy.approval_mode {
            tool_config.approval_mode = Some(approval_mode);
        }
        tool_config.restrict_output_token_limit(tool_policy.output_token_limit);
    }
}

pub(crate) struct PluginSkillInventory {
    skills: Vec<SkillMetadata>,
    had_errors: bool,
}

impl PluginSkillInventory {
    pub(crate) fn has_enabled_skills(&self, skill_config_rules: &SkillConfigRules) -> bool {
        contains_enabled_skill(
            &self.skills,
            &skill_config_rules.resolve_disabled_paths(
                self.skills
                    .iter()
                    .map(|skill| (skill.name.as_str(), &skill.path_to_skills_md)),
            ),
        )
    }

    pub(crate) fn resolve(self, skill_config_rules: &SkillConfigRules) -> ResolvedPluginSkills {
        let disabled_skill_paths = skill_config_rules.resolve_disabled_paths(
            self.skills
                .iter()
                .map(|skill| (skill.name.as_str(), &skill.path_to_skills_md)),
        );
        ResolvedPluginSkills {
            skills: self.skills,
            disabled_skill_paths,
            had_errors: self.had_errors,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPluginSkills {
    pub skills: Vec<SkillMetadata>,
    pub disabled_skill_paths: HashSet<AbsolutePathBuf>,
    pub had_errors: bool,
}

impl ResolvedPluginSkills {
    pub fn has_enabled_skills(&self) -> bool {
        self.had_errors || contains_enabled_skill(&self.skills, &self.disabled_skill_paths)
    }
}

fn contains_enabled_skill(
    skills: &[SkillMetadata],
    disabled_skill_paths: &HashSet<AbsolutePathBuf>,
) -> bool {
    skills
        .iter()
        .any(|skill| !disabled_skill_paths.contains(&skill.path_to_skills_md))
}

pub(crate) async fn load_plugin_skill_inventory(
    plugin_root: &AbsolutePathBuf,
    plugin_identity: &PluginIdentity,
    manifest: &PluginManifest,
    manifest_format: PluginManifestFormat,
    restriction_product: Option<Product>,
    plugin_skill_snapshots: Option<&SkillRootSnapshots<PluginSkillRoot>>,
    skill_root_loader: &dyn SkillRootLoader<PluginSkillRoot>,
) -> PluginSkillInventory {
    let discovery_mode = match manifest_format {
        PluginManifestFormat::Legacy => SkillDiscoveryMode::Recursive,
        PluginManifestFormat::AgentPlugin => SkillDiscoveryMode::DirectChildren,
    };
    let roots = plugin_skill_roots(plugin_root, &manifest.paths, manifest_format)
        .into_iter()
        .map(|path| PluginSkillRoot {
            path,
            plugin_identity: plugin_identity.clone(),
            plugin_namespace: manifest.name.clone(),
            plugin_root: plugin_root.clone(),
            discovery_mode,
        })
        .collect();
    let outcome = skill_root_loader
        .load_roots(SkillRootLoadRequest {
            roots,
            restriction_product,
            snapshots: plugin_skill_snapshots.cloned(),
        })
        .await;

    PluginSkillInventory {
        skills: outcome.skills,
        had_errors: !outcome.errors.is_empty(),
    }
}

pub(crate) fn plugin_skill_roots(
    plugin_root: &AbsolutePathBuf,
    manifest_paths: &PluginManifestPaths,
    manifest_format: PluginManifestFormat,
) -> Vec<AbsolutePathBuf> {
    let mut paths = if manifest_paths.skills.is_empty() {
        default_skill_roots(plugin_root)
    } else {
        manifest_paths.skills.clone()
    };
    if manifest_format == PluginManifestFormat::Legacy {
        let migrated_command_skills = migrated_command_skills_root(plugin_root);
        if migrated_command_skills.is_dir() {
            paths.push(migrated_command_skills);
        }
    }
    paths.sort_unstable();
    paths.dedup();
    paths
}

fn default_skill_roots(plugin_root: &AbsolutePathBuf) -> Vec<AbsolutePathBuf> {
    let skills_dir = plugin_root.join(DEFAULT_SKILLS_DIR_NAME);
    if skills_dir.is_dir() {
        vec![skills_dir]
    } else {
        Vec::new()
    }
}

fn plugin_mcp_config_paths(
    plugin_root: &Path,
    manifest_paths: &PluginManifestPaths,
) -> Vec<AbsolutePathBuf> {
    if let Some(PluginManifestMcpServers::Path(path)) = &manifest_paths.mcp_servers {
        return vec![path.clone()];
    }
    default_mcp_config_paths(plugin_root)
}

fn default_mcp_config_paths(plugin_root: &Path) -> Vec<AbsolutePathBuf> {
    let mut paths = Vec::new();
    let default_path = plugin_root.join(DEFAULT_MCP_CONFIG_FILE);
    if default_path.is_file()
        && let Ok(default_path) = AbsolutePathBuf::try_from(default_path)
    {
        paths.push(default_path);
    }
    paths.sort_unstable_by(|left, right| left.as_path().cmp(right.as_path()));
    paths.dedup_by(|left, right| left.as_path() == right.as_path());
    paths
}

// Discover plugin-bundled hooks from manifest `hooks` entries when present
// (path, paths, inline object, or inline objects), otherwise from the default
// `hooks/hooks.json` file.
pub fn load_plugin_hooks(
    plugin_root: &AbsolutePathBuf,
    plugin_id: &PluginId,
    plugin_data_root: &AbsolutePathBuf,
    manifest_paths: &PluginManifestPaths,
) -> (Vec<PluginHookSource>, Vec<String>) {
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    match &manifest_paths.hooks {
        Some(PluginManifestHooks::Paths(paths)) => {
            for path in paths {
                append_plugin_hook_file(
                    plugin_root,
                    plugin_id,
                    plugin_data_root,
                    path,
                    &mut sources,
                    &mut warnings,
                );
            }
        }
        Some(PluginManifestHooks::Inline(hooks_files)) => {
            let manifest_path = find_plugin_manifest_path(plugin_root.as_path())
                .and_then(|path| AbsolutePathBuf::try_from(path).ok())
                .unwrap_or_else(|| plugin_root.join(".codex-plugin/plugin.json"));
            for (index, hooks_file) in hooks_files.iter().enumerate() {
                if hooks_file.hooks.is_empty() {
                    continue;
                }
                sources.push(PluginHookSource {
                    plugin_id: plugin_id.clone(),
                    plugin_root: plugin_root.clone(),
                    plugin_data_root: plugin_data_root.clone(),
                    source_path: manifest_path.clone(),
                    source_relative_path: format!("plugin.json#hooks[{index}]"),
                    hooks: hooks_file.hooks.clone(),
                });
            }
        }
        None => {
            let default_path = plugin_root.join(DEFAULT_HOOKS_CONFIG_FILE);
            if default_path.as_path().is_file() {
                append_plugin_hook_file(
                    plugin_root,
                    plugin_id,
                    plugin_data_root,
                    &default_path,
                    &mut sources,
                    &mut warnings,
                );
            }
        }
    }
    (sources, warnings)
}

// Append one resolved plugin hook file, keeping source metadata for runtime
// reporting and collecting load warnings for startup surfacing.
fn append_plugin_hook_file(
    plugin_root: &AbsolutePathBuf,
    plugin_id: &PluginId,
    plugin_data_root: &AbsolutePathBuf,
    path: &AbsolutePathBuf,
    sources: &mut Vec<PluginHookSource>,
    warnings: &mut Vec<String>,
) {
    let contents = match fs::read_to_string(path.as_path()) {
        Ok(contents) => contents,
        Err(err) => {
            warnings.push(format!(
                "failed to read plugin hooks config {}: {err}",
                path.display()
            ));
            return;
        }
    };
    let parsed = match serde_json::from_str::<HooksFile>(&contents) {
        Ok(parsed) => parsed,
        Err(err) => {
            warnings.push(format!(
                "failed to parse plugin hooks config {}: {err}",
                path.display()
            ));
            return;
        }
    };
    if parsed.hooks.is_empty() {
        return;
    }

    let source_relative_path = path
        .as_path()
        .strip_prefix(plugin_root.as_path())
        .unwrap_or(path.as_path())
        .to_string_lossy()
        .replace('\\', "/");

    sources.push(PluginHookSource {
        plugin_id: plugin_id.clone(),
        plugin_root: plugin_root.clone(),
        plugin_data_root: plugin_data_root.clone(),
        source_path: path.clone(),
        source_relative_path,
        hooks: parsed.hooks,
    });
}

pub(crate) async fn plugin_capability_summary_from_root(
    plugin_id: &PluginId,
    plugin_root: &AbsolutePathBuf,
    skill_root_loader: &dyn SkillRootLoader<PluginSkillRoot>,
    manifest_cache: &ManifestCache,
) -> Option<PluginCapabilitySummary> {
    let loaded_manifest = manifest_cache.load(plugin_root.as_path())?;
    let manifest_format = loaded_manifest.format;
    let manifest = loaded_manifest.manifest;
    let plugin_identity = PluginIdentity {
        plugin_id: plugin_id.as_key(),
        remote_plugin_id: None,
    };

    let manifest_paths = &manifest.paths;
    let has_skills = match manifest_format {
        PluginManifestFormat::Legacy => {
            !plugin_skill_roots(plugin_root, manifest_paths, manifest_format).is_empty()
        }
        PluginManifestFormat::AgentPlugin => {
            !load_plugin_skill_inventory(
                plugin_root,
                &plugin_identity,
                &manifest,
                manifest_format,
                /*restriction_product*/ None,
                /*plugin_skill_snapshots*/ None,
                skill_root_loader,
            )
            .await
            .skills
            .is_empty()
        }
    };
    let mut mcp_server_names = load_plugin_mcp_servers_from_manifest_with_format(
        plugin_root.as_path(),
        manifest_paths,
        /*plugin_policy*/ None,
        /*plugin_data_root*/ None,
        manifest_format,
    )
    .await
    .into_keys()
    .collect::<Vec<_>>();
    mcp_server_names.sort_unstable();
    mcp_server_names.dedup();

    Some(PluginCapabilitySummary {
        config_name: plugin_id.as_key(),
        display_name: plugin_id.plugin_name.clone(),
        plugin_namespace: Some(manifest.name.clone()),
        description: None,
        has_skills,
        mcp_server_names,
        app_connector_ids: Vec::new(),
    })
}

/// Loads plugin MCP servers without applying user-specific policy overrides.
pub async fn load_plugin_mcp_servers(
    plugin_root: &Path,
    auth_mode: Option<AuthMode>,
) -> HashMap<String, McpServerConfig> {
    load_plugin_mcp_servers_with_policy(plugin_root, auth_mode, /*plugin_policy*/ None).await
}

/// Loads plugin MCP servers with the effective configuration policy for an installed plugin.
pub async fn load_configured_plugin_mcp_servers(
    plugin_root: &Path,
    auth_mode: Option<AuthMode>,
    plugin_id: &PluginId,
    config_layer_stack: &ConfigLayerStack,
    codex_home: &Path,
) -> HashMap<String, McpServerConfig> {
    let configured_plugins = configured_plugins_from_stack(config_layer_stack, codex_home);
    let plugin_id = plugin_id.as_key();
    let plugin_policy = configured_plugins
        .get(&plugin_id)
        .map(|plugin| &plugin.mcp_servers);

    load_plugin_mcp_servers_with_policy(plugin_root, auth_mode, plugin_policy).await
}

/// Resolves effective per-plugin MCP policies without validating opaque selected-root IDs.
pub fn configured_plugin_mcp_server_policies(
    config_layer_stack: &ConfigLayerStack,
) -> HashMap<String, HashMap<String, PluginMcpServerConfig>> {
    configured_plugins_from_config_value(&config_layer_stack.effective_config())
        .into_iter()
        .map(|(plugin_id, plugin)| (plugin_id, plugin.mcp_servers))
        .collect()
}

/// Applies user policy without widening the selected plugin's declared restrictions.
pub fn apply_configured_plugin_mcp_server_policies(
    policies: &HashMap<String, PluginMcpServerConfig>,
    servers: &mut HashMap<String, McpServerConfig>,
) {
    for (name, server) in servers {
        if let Some(policy) = policies.get(name) {
            server.enabled &= policy.enabled && !policy.has_unsupported_ema_auth;
            if policy.has_unsupported_ema_auth {
                server.auth = codex_config::McpServerAuth::EmaAuth;
            }
            let declared_approval_mode = server.default_tools_approval_mode.unwrap_or_default();

            if let Some(approval_mode) = policy.default_tools_approval_mode {
                server.default_tools_approval_mode =
                    Some(declared_approval_mode.restrict_to(approval_mode));
            }
            if let Some(enabled_tools) = &policy.enabled_tools {
                match &mut server.enabled_tools {
                    Some(declared_tools) => {
                        declared_tools.retain(|tool| enabled_tools.contains(tool));
                    }
                    None => server.enabled_tools = Some(enabled_tools.clone()),
                }
            }
            if let Some(disabled_tools) = &policy.disabled_tools {
                let declared_tools = server.disabled_tools.get_or_insert_default();
                for tool in disabled_tools {
                    if !declared_tools.contains(tool) {
                        declared_tools.push(tool.clone());
                    }
                }
            }
            for (tool_name, tool_policy) in &policy.tools {
                if tool_policy.approval_mode.is_some() || tool_policy.output_token_limit.is_some() {
                    server.tools.entry(tool_name.clone()).or_default();
                }
            }
            for (tool_name, tool_config) in &mut server.tools {
                if let Some(approval_mode) = policy
                    .tools
                    .get(tool_name)
                    .and_then(|tool_policy| tool_policy.approval_mode)
                    .or(policy.default_tools_approval_mode)
                {
                    tool_config.approval_mode = Some(
                        tool_config
                            .approval_mode
                            .unwrap_or(declared_approval_mode)
                            .restrict_to(approval_mode),
                    );
                }
                tool_config.restrict_output_token_limit(
                    policy
                        .tools
                        .get(tool_name)
                        .and_then(|tool_policy| tool_policy.output_token_limit),
                );
            }
        }
    }
}

async fn load_plugin_mcp_servers_with_policy(
    plugin_root: &Path,
    _auth_mode: Option<AuthMode>,
    plugin_policy: Option<&HashMap<String, PluginMcpServerConfig>>,
) -> HashMap<String, McpServerConfig> {
    load_declared_plugin_mcp_servers(plugin_root, plugin_policy).await
}

async fn load_declared_plugin_mcp_servers(
    plugin_root: &Path,
    plugin_policy: Option<&HashMap<String, PluginMcpServerConfig>>,
) -> HashMap<String, McpServerConfig> {
    let Some(loaded_manifest) = load_plugin_manifest_with_format(plugin_root) else {
        return HashMap::new();
    };

    load_plugin_mcp_servers_from_manifest_with_format(
        plugin_root,
        &loaded_manifest.manifest.paths,
        plugin_policy,
        /*plugin_data_root*/ None,
        loaded_manifest.format,
    )
    .await
}

pub(crate) async fn load_plugin_mcp_servers_from_manifest_with_format(
    plugin_root: &Path,
    manifest_paths: &PluginManifestPaths,
    plugin_policy: Option<&HashMap<String, PluginMcpServerConfig>>,
    plugin_data_root: Option<&Path>,
    manifest_format: PluginManifestFormat,
) -> HashMap<String, McpServerConfig> {
    let mut mcp_servers = HashMap::new();
    match &manifest_paths.mcp_servers {
        Some(PluginManifestMcpServers::Object(object_servers)) => {
            let plugin_mcp = load_mcp_servers_from_manifest_object(plugin_root, object_servers);
            for (name, mut config) in plugin_mcp.mcp_servers {
                if let Some(policy) = plugin_policy.and_then(|policy| policy.get(&name)) {
                    apply_plugin_mcp_server_policy(&mut config, policy);
                }
                if mcp_servers.insert(name.clone(), config).is_some() {
                    warn!(
                        plugin = %plugin_root.display(),
                        server = name,
                        "plugin manifest MCP object overwrote an earlier server definition"
                    );
                }
            }
        }
        Some(PluginManifestMcpServers::Path(_)) | None => {
            for mcp_config_path in plugin_mcp_config_paths(plugin_root, manifest_paths) {
                let plugin_mcp = load_mcp_servers_from_file(
                    plugin_root,
                    plugin_data_root,
                    manifest_format,
                    &mcp_config_path,
                )
                .await;
                for (name, mut config) in plugin_mcp.mcp_servers {
                    if let Some(policy) = plugin_policy.and_then(|policy| policy.get(&name)) {
                        apply_plugin_mcp_server_policy(&mut config, policy);
                    }
                    if mcp_servers.insert(name.clone(), config).is_some() {
                        warn!(
                            plugin = %plugin_root.display(),
                            path = %mcp_config_path.display(),
                            server = name,
                            "plugin MCP file overwrote an earlier server definition"
                        );
                    }
                }
            }
        }
    }

    if manifest_format == PluginManifestFormat::AgentPlugin {
        agent_plugin_mcp_overlay::apply_codex_env_overlay(plugin_root, &mut mcp_servers).await;
    }

    mcp_servers
}

async fn load_mcp_servers_from_file(
    plugin_root: &Path,
    plugin_data_root: Option<&Path>,
    manifest_format: PluginManifestFormat,
    mcp_config_path: &AbsolutePathBuf,
) -> PluginMcpDiscovery {
    let is_agent_plugin_mcp = manifest_format == PluginManifestFormat::AgentPlugin;
    if is_agent_plugin_mcp {
        match tokio::fs::symlink_metadata(mcp_config_path.as_path()).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                warn!(
                    path = %mcp_config_path.display(),
                    "Agent Plugins MCP config is not a regular file; disabling MCP"
                );
                return PluginMcpDiscovery::default();
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return PluginMcpDiscovery::default();
            }
            Err(err) => {
                warn!(
                    path = %mcp_config_path.display(),
                    "failed to inspect Agent Plugins MCP config; disabling MCP: {err}"
                );
                return PluginMcpDiscovery::default();
            }
        }
        let resolved_root = match tokio::fs::canonicalize(plugin_root).await {
            Ok(path) => path,
            Err(err) => {
                warn!(
                    plugin = %plugin_root.display(),
                    "failed to resolve Agent Plugins root; disabling MCP: {err}"
                );
                return PluginMcpDiscovery::default();
            }
        };
        let resolved_config = match tokio::fs::canonicalize(mcp_config_path.as_path()).await {
            Ok(path) => path,
            Err(err) => {
                warn!(
                    path = %mcp_config_path.display(),
                    "failed to resolve Agent Plugins MCP config; disabling MCP: {err}"
                );
                return PluginMcpDiscovery::default();
            }
        };
        if !resolved_config.starts_with(&resolved_root) {
            warn!(
                plugin = %plugin_root.display(),
                path = %mcp_config_path.display(),
                "Agent Plugins MCP config resolves outside the plugin root; disabling MCP"
            );
            return PluginMcpDiscovery::default();
        }
    }
    let Ok(contents) = tokio::fs::read_to_string(mcp_config_path.as_path()).await else {
        return PluginMcpDiscovery::default();
    };
    let fallback_data_root = plugin_root.join(".plugin-data");
    let mut parsed = match if is_agent_plugin_mcp {
        parse_agent_plugin_mcp_config(
            plugin_root,
            plugin_data_root.unwrap_or(&fallback_data_root),
            &contents,
        )
    } else {
        parse_plugin_mcp_config(plugin_root, &contents)
    } {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!(
                path = %mcp_config_path.display(),
                "failed to parse plugin MCP config: {err}"
            );
            return PluginMcpDiscovery::default();
        }
    };
    if is_agent_plugin_mcp
        && let Some(plugin_data_root) = plugin_data_root
        && parsed
            .servers
            .values()
            .any(|server| matches!(&server.transport, McpServerTransportConfig::Stdio { .. }))
        && let Err(err) = tokio::fs::create_dir_all(plugin_data_root).await
    {
        warn!(
            plugin = %plugin_root.display(),
            path = %plugin_data_root.display(),
            "failed to create Agent Plugins data directory; disabling stdio MCP servers: {err}"
        );
        parsed.servers.retain(|_, server| {
            !matches!(&server.transport, McpServerTransportConfig::Stdio { .. })
        });
    }
    for error in parsed.errors {
        warn!(
            plugin = %plugin_root.display(),
            server = error.name,
            path = %mcp_config_path.display(),
            error = error.message,
            "failed to parse plugin MCP server"
        );
    }
    PluginMcpDiscovery {
        mcp_servers: parsed.servers.into_iter().collect(),
    }
}

fn load_mcp_servers_from_manifest_object(
    plugin_root: &Path,
    object_config: &str,
) -> PluginMcpDiscovery {
    let parsed = match parse_plugin_mcp_config(plugin_root, object_config) {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!(
                plugin = %plugin_root.display(),
                "failed to parse plugin manifest MCP object: {err}"
            );
            return PluginMcpDiscovery::default();
        }
    };
    for error in parsed.errors {
        warn!(
            plugin = %plugin_root.display(),
            server = error.name,
            error = error.message,
            "failed to parse plugin manifest MCP object server"
        );
    }
    PluginMcpDiscovery {
        mcp_servers: parsed.servers.into_iter().collect(),
    }
}

#[derive(Debug, Default)]
struct PluginMcpDiscovery {
    mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug)]
pub struct MaterializedMarketplacePluginSource {
    pub path: AbsolutePathBuf,
    _tempdir: Option<TempDir>,
}

pub fn materialize_marketplace_plugin_source(
    _codex_home: &Path,
    source: &MarketplacePluginSource,
) -> Result<MaterializedMarketplacePluginSource, String> {
    match source {
        MarketplacePluginSource::Local { path } => Ok(MaterializedMarketplacePluginSource {
            path: path.clone(),
            _tempdir: None,
        }),
        MarketplacePluginSource::Git { .. } | MarketplacePluginSource::Npm { .. } => Err(
            "Online plugin installation is unsupported; provide a local plugin directory."
                .to_string(),
        ),
    }
}

fn configured_plugins_from_config_value(
    user_config: &toml::Value,
) -> HashMap<String, PluginConfig> {
    let Some(plugins_value) = user_config.get("plugins") else {
        return HashMap::new();
    };
    match plugins_value.clone().try_into() {
        Ok(plugins) => plugins,
        Err(err) => {
            warn!("invalid plugins config: {err}");
            HashMap::new()
        }
    }
}

#[cfg(test)]
#[path = "loader_tests.rs"]
mod tests;
