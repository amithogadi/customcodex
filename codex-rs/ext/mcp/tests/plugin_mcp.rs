use anyhow::Context;
use codex_config::AppToolApproval;
use codex_config::McpServerToolConfig;
use codex_config::test_support::CloudConfigBundleFixture;
use codex_core::EnvironmentConfig;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::windows_sandbox::WindowsSandboxLevelExt;
use codex_core_plugins::ExecutorPluginProvider;
use codex_core_plugins::PluginCatalog;
use codex_core_plugins::PluginCatalogEntry;
use codex_core_plugins::PluginIdentity;
use codex_core_plugins::PluginListQuery;
use codex_core_plugins::PluginProvider;
use codex_core_plugins::PluginProviderError;
use codex_core_plugins::PluginProviderFuture;
use codex_core_plugins::PluginSourceLocation;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecutorCapabilityDiscoveryCache;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::McpServerContributionContext;
use codex_extension_api::SelectedPluginContribution;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_mcp_extension::PluginProviders;
use codex_mcp_extension::PluginsThreadState;
use codex_mcp_extension::install_plugin_providers;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfileSnapshot;
use codex_utils_path_uri::PathUri;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::apps_test_server::recorded_apps_tool_calls;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::run_test_with_large_stack;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Debug, PartialEq, Eq)]
struct ContributionSummary {
    name: String,
    plugin_id: String,
    plugin_display_name: String,
    source_environment_id: String,
    selection_order: usize,
    enabled: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct PackageSummary {
    plugin_id: String,
    plugin_display_name: String,
    source_environment_id: String,
    connector_ids: Vec<String>,
}

#[tokio::test]
async fn selected_plugin_servers_use_managed_requirements_for_the_selected_root_id() -> TestResult {
    let codex_home = tempfile::tempdir()?;
    let plugin_root = tempfile::tempdir()?;
    std::fs::create_dir_all(plugin_root.path().join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"different-manifest-name","interface":{"displayName":"Selected Demo"}}"#,
    )?;
    std::fs::write(
        plugin_root.path().join(".mcp.json"),
        r#"{
  "mcpServers": {
    "allowed": {"command":"allowed-command"},
    "mismatched": {"command":"wrong-command"},
    "unlisted": {"command":"unlisted-command"}
  }
}"#,
    )?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        "[plugins.\"selected-root\".mcp_servers.mismatched]\nenabled = true\n[plugins.\"selected-root\".mcp_servers.unlisted]\nenabled = true",
    )?;
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[plugins."selected-root".mcp_servers.allowed.identity]
command = "allowed-command"

[plugins."selected-root".mcp_servers.mismatched.identity]
command = "expected-command"
"#,
            ),
        )
        .build()
        .await?;

    let contributions = selected_plugin_contributions(&config, plugin_root.path()).await?;

    assert_eq!(
        contributions,
        vec![
            ContributionSummary {
                name: "allowed".to_string(),
                plugin_id: "selected-root".to_string(),
                plugin_display_name: "Selected Demo".to_string(),
                source_environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                selection_order: 0,
                enabled: true,
            },
            ContributionSummary {
                name: "mismatched".to_string(),
                plugin_id: "selected-root".to_string(),
                plugin_display_name: "Selected Demo".to_string(),
                source_environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                selection_order: 0,
                enabled: false,
            },
            ContributionSummary {
                name: "unlisted".to_string(),
                plugin_id: "selected-root".to_string(),
                plugin_display_name: "Selected Demo".to_string(),
                source_environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                selection_order: 0,
                enabled: false,
            },
        ]
    );
    Ok(())
}

#[tokio::test]
async fn selected_plugin_package_is_contributed_without_servers_or_connectors() -> TestResult {
    let codex_home = tempfile::tempdir()?;
    let plugin_root = tempfile::tempdir()?;
    std::fs::create_dir_all(plugin_root.path().join(".codex-plugin"))?;
    std::fs::create_dir_all(plugin_root.path().join("skills/deploy"))?;
    std::fs::write(
        plugin_root.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"skill-only","interface":{"displayName":"Skill Only"}}"#,
    )?;
    std::fs::write(
        plugin_root.path().join("skills/deploy/SKILL.md"),
        "---\nname: deploy\ndescription: Deploy the project.\n---\n",
    )?;
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    let contributions = raw_selected_plugin_contributions(&config, plugin_root.path()).await?;
    let package = contributions
        .into_iter()
        .next()
        .map(|(_, plugin_id, contribution)| PackageSummary {
            plugin_id,
            plugin_display_name: contribution.plugin_display_name,
            source_environment_id: contribution.source_environment_id,
            connector_ids: contribution.connector_ids,
        });

    assert_eq!(
        package,
        Some(PackageSummary {
            plugin_id: "selected-root".to_string(),
            plugin_display_name: "Skill Only".to_string(),
            source_environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            connector_ids: Vec::new(),
        })
    );
    Ok(())
}

#[tokio::test]
async fn managed_plugins_requirement_disables_selected_plugin_capabilities() -> TestResult {
    let codex_home = tempfile::tempdir()?;
    let plugin_root = tempfile::tempdir()?;
    std::fs::create_dir_all(plugin_root.path().join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"selected-root","interface":{"displayName":"Selected Root"}}"#,
    )?;
    std::fs::write(
        plugin_root.path().join(".mcp.json"),
        r#"{"mcpServers":{"probe":{"command":"probe-command"}}}"#,
    )?;
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"
[features]
plugins = false
"#,
            ),
        )
        .build()
        .await?;
    assert!(!config.features.enabled(Feature::Plugins));

    let direct = raw_selected_plugin_contributions(&config, plugin_root.path()).await?;
    assert!(
        matches!(
            direct.as_slice(),
            [(selected_root_id, _, contribution)]
                if selected_root_id == "selected-root"
                    && contribution.source_environment_id == LOCAL_ENVIRONMENT_ID
                    && contribution.servers.is_empty()
        ),
        "managed Plugins disable should preserve only the direct selected-root identity"
    );

    config
        .features
        .enable(Feature::ExecutorCapabilityDiscovery)
        .expect("test config should allow feature update");
    let discovered = raw_selected_plugin_contributions(&config, plugin_root.path()).await?;
    assert!(
        matches!(
            discovered.as_slice(),
            [(selected_root_id, _, contribution)]
                if selected_root_id == "selected-root"
                    && contribution.source_environment_id == LOCAL_ENVIRONMENT_ID
                    && contribution.servers.is_empty()
        ),
        "managed Plugins disable should preserve only the discovered selected-root identity"
    );
    Ok(())
}

#[tokio::test]
async fn high_level_discovery_matches_the_existing_plugin_provider() -> TestResult {
    let codex_home = tempfile::tempdir()?;
    let plugin_root = tempfile::tempdir()?;
    std::fs::create_dir_all(plugin_root.path().join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"demo","interface":{"displayName":"Demo"},"mcpServers":"./servers.json"}"#,
    )?;
    std::fs::write(
        plugin_root.path().join("servers.json"),
        r#"{
  "mcpServers": {
    "first": {
      "command": "first",
      "default_tools_approval_mode": "writes",
      "enabled_tools": ["read", "deploy", "trusted", "package-only"],
      "disabled_tools": ["package-denied"],
      "tools": {
        "read": {"approval_mode": "prompt", "output_token_limit": 12000},
        "deploy": {"approval_mode": "approve", "output_token_limit": 4000},
        "trusted": {"approval_mode": "approve"}
      }
    },
    "second": {
      "command": "second",
      "enabled": false,
      "default_tools_approval_mode": "prompt"
    }
  }
}"#,
    )?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
[plugins."selected-root".mcp_servers.first]
enabled = false
default_tools_approval_mode = "prompt"
enabled_tools = ["read", "deploy", "trusted", "host-only"]
disabled_tools = ["write"]

[plugins."selected-root".mcp_servers.first.tools.read]
approval_mode = "approve"
output_token_limit = 8000

[plugins."selected-root".mcp_servers.first.tools.deploy]
output_token_limit = 9000

[plugins."selected-root".mcp_servers.first.tools.trusted]
approval_mode = "approve"

[plugins."selected-root".mcp_servers.second]
enabled = true
default_tools_approval_mode = "auto"
"#,
    )?;
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    let existing = selected_plugin_contributions(&config, plugin_root.path()).await?;
    let mut servers = raw_selected_plugin_contributions(&config, plugin_root.path())
        .await?
        .into_iter()
        .flat_map(|(_, _, contribution)| contribution.servers)
        .collect::<HashMap<_, _>>();
    let server = servers
        .remove("first")
        .expect("disabled selected server remains registered");
    let declared_disabled_server = servers
        .remove("second")
        .expect("package-disabled server remains registered");
    assert_eq!(
        (
            server.enabled,
            server.default_tools_approval_mode,
            server.enabled_tools,
            server.disabled_tools,
            server.tools,
            declared_disabled_server.enabled,
            declared_disabled_server.default_tools_approval_mode,
        ),
        (
            false,
            Some(AppToolApproval::Prompt),
            Some(vec![
                "read".to_string(),
                "deploy".to_string(),
                "trusted".to_string(),
            ]),
            Some(vec!["package-denied".to_string(), "write".to_string()]),
            HashMap::from([
                (
                    "read".to_string(),
                    McpServerToolConfig {
                        approval_mode: Some(AppToolApproval::Prompt),
                        output_token_limit: std::num::NonZeroUsize::new(8_000),
                    },
                ),
                (
                    "deploy".to_string(),
                    McpServerToolConfig {
                        approval_mode: Some(AppToolApproval::Prompt),
                        output_token_limit: std::num::NonZeroUsize::new(4_000),
                    },
                ),
                (
                    "trusted".to_string(),
                    McpServerToolConfig {
                        approval_mode: Some(AppToolApproval::Approve),
                        ..Default::default()
                    },
                ),
            ]),
            false,
            Some(AppToolApproval::Prompt),
        )
    );
    config
        .features
        .enable(Feature::ExecutorCapabilityDiscovery)
        .expect("test config should allow feature update");
    let high_level = selected_plugin_contributions(&config, plugin_root.path()).await?;

    assert_eq!(
        existing
            .iter()
            .map(|contribution| contribution.source_environment_id.as_str())
            .collect::<Vec<_>>(),
        vec![LOCAL_ENVIRONMENT_ID, LOCAL_ENVIRONMENT_ID]
    );
    assert_eq!(high_level, existing);
    Ok(())
}

async fn selected_plugin_contributions(
    config: &Config,
    plugin_root: &std::path::Path,
) -> Result<Vec<ContributionSummary>, Box<dyn std::error::Error>> {
    Ok(raw_selected_plugin_contributions(config, plugin_root)
        .await?
        .into_iter()
        .enumerate()
        .flat_map(|(selection_order, (_, plugin_id, contribution))| {
            contribution
                .servers
                .into_iter()
                .map(move |(name, config)| ContributionSummary {
                    name,
                    plugin_id: plugin_id.clone(),
                    plugin_display_name: contribution.plugin_display_name.clone(),
                    source_environment_id: contribution.source_environment_id.clone(),
                    selection_order,
                    enabled: config.enabled,
                })
        })
        .collect())
}

async fn raw_selected_plugin_contributions(
    config: &Config,
    plugin_root: &std::path::Path,
) -> Result<Vec<(String, String, SelectedPluginContribution)>, Box<dyn std::error::Error>> {
    let mut builder = ExtensionRegistryBuilder::new();
    let environment_manager = Arc::new(EnvironmentManager::default_for_tests());
    codex_mcp_extension::install_plugins(&mut builder, Arc::clone(&environment_manager));
    let registry = builder.build();
    let thread_init = ExtensionDataInit::new();
    let selected_capability_roots = vec![SelectedCapabilityRoot {
        id: "selected-root".to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            path: PathUri::from_host_native_path(plugin_root)?,
        },
    }];
    let thread_store = ExtensionData::new_with_init("test-thread", thread_init.clone());
    let executor_capability_discovery = if config
        .features
        .enabled(Feature::ExecutorCapabilityDiscovery)
    {
        Some(
            ExecutorCapabilityDiscoveryCache::new(environment_manager)
                .snapshot(&selected_capability_roots, &Default::default())
                .await,
        )
    } else {
        None
    };

    let selected = registry.mcp_server_contributors()[0]
        .selected_plugins(McpServerContributionContext::for_step(
            config,
            &thread_init,
            &thread_store,
            "test_originator",
            &selected_capability_roots,
            executor_capability_discovery.as_ref(),
        ))
        .await;
    let mut contributions = Vec::new();
    for plugin in selected {
        contributions.push((plugin.selected_root_id, plugin.plugin_id, plugin.mcp.await));
    }
    Ok(contributions)
}
