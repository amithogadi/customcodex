use super::*;
use crate::manifest::load_plugin_manifest;
use crate::manifest::load_plugin_manifest_with_format;
use crate::test_support::test_skill_root_loader;
use crate::test_support::write_file;
use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::ConfigRequirements;
use codex_config::ConfigRequirementsToml;
use codex_plugin::PluginId;
use codex_utils_plugins::AGENT_PLUGIN_SCHEMA_URI;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn user_config_path(temp_dir: &TempDir, file_name: &str) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(temp_dir.path().join(file_name))
        .expect("test user config path should be absolute")
}

fn user_layer(path: AbsolutePathBuf, config: &str) -> ConfigLayerEntry {
    ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: path,
            profile: None,
        },
        toml::from_str(config).expect("user config toml"),
    )
}

#[tokio::test]
async fn agent_plugin_codex_mcp_overlay_only_forwards_matching_stdio_server_env_vars() {
    let temp_dir = TempDir::new().expect("tempdir");
    let plugin_root = temp_dir.path().join("plugin");
    write_file(
        &plugin_root.join("plugin.json"),
        &format!(
            r#"{{"$schema":"{AGENT_PLUGIN_SCHEMA_URI}","name":"plugin","extensions":{{"com.openai":{{"interface":{{"displayName":"Portable"}}}}}}}}"#
        ),
    );
    write_file(
        &plugin_root.join("mcp.json"),
        r#"{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
  "mcpServers": {
    "shared": {
      "type": "stdio",
      "command": "portable-server",
      "args": ["portable"],
      "env": {
        "DB_PASSWORD": "${DB_PASSWORD}",
        "API_TOKEN": "portable-token",
        "REMOTE_ONLY": "${REMOTE_ONLY}",
        "UNLISTED": "${UNLISTED}"
      }
    },
    "portable-only": {"type": "stdio", "command": "portable-only"}
  }
}"#,
    );

    let mut expected = load_plugin_mcp_servers(&plugin_root, /*auth_mode*/ None).await;
    let Some(McpServerConfig {
        transport: McpServerTransportConfig::Stdio { env, env_vars, .. },
        ..
    }) = expected.get_mut("shared")
    else {
        panic!("expected portable stdio server");
    };
    env.as_mut()
        .expect("portable environment")
        .remove("DB_PASSWORD");
    env_vars.extend(["DB_PASSWORD".into(), "API_TOKEN".into()]);

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"legacy-plugin"}"#,
    );
    write_file(
        &plugin_root.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "shared": {
      "command": "legacy-server",
      "args": ["legacy"],
      "env_vars": ["DB_PASSWORD", "API_TOKEN", {"name": "REMOTE_ONLY", "source": "remote"}]
    },
    "legacy-only": {"command": "legacy-only", "env_vars": ["UNLISTED"]}
  }
}"#,
    );

    assert_eq!(
        load_plugin_mcp_servers(&plugin_root, /*auth_mode*/ None).await,
        expected
    );
}

#[tokio::test]
async fn agent_plugin_codex_mcp_overlay_supports_inline_legacy_servers_without_portable_env() {
    let temp_dir = TempDir::new().expect("tempdir");
    let plugin_root = temp_dir.path().join("plugin");
    write_file(
        &plugin_root.join("plugin.json"),
        &format!(r#"{{"$schema":"{AGENT_PLUGIN_SCHEMA_URI}","name":"plugin"}}"#),
    );
    write_file(
        &plugin_root.join("mcp.json"),
        r#"{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
  "mcpServers": {
    "shared": {
      "type": "stdio",
      "command": "portable-server"
    }
  }
}"#,
    );

    let mut expected = load_plugin_mcp_servers(&plugin_root, /*auth_mode*/ None).await;
    let Some(McpServerConfig {
        transport: McpServerTransportConfig::Stdio { env_vars, .. },
        ..
    }) = expected.get_mut("shared")
    else {
        panic!("expected portable stdio server");
    };
    env_vars.push("TOKEN".into());

    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "legacy-plugin",
  "mcpServers": {
    "shared": {"command": "legacy-server", "env_vars": ["TOKEN"]}
  }
}"#,
    );

    assert_eq!(
        load_plugin_mcp_servers(&plugin_root, /*auth_mode*/ None).await,
        expected
    );
}

#[cfg(unix)]
#[tokio::test]
async fn agent_plugin_mcp_rejects_config_symlink_outside_plugin_root() {
    let temp_dir = TempDir::new().expect("tempdir");
    let plugin_root = temp_dir.path().join("plugin");
    let outside_config = temp_dir.path().join("outside-mcp.json");
    fs::create_dir_all(&plugin_root).expect("create plugin root");
    fs::write(
        plugin_root.join("plugin.json"),
        format!(r#"{{"$schema":"{AGENT_PLUGIN_SCHEMA_URI}","name":"plugin"}}"#),
    )
    .expect("write Agent Plugins manifest");
    fs::write(
        &outside_config,
        r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json","mcpServers":{"outside":{"type":"stdio","command":"echo"}}}"#,
    )
    .expect("write outside MCP config");
    std::os::unix::fs::symlink(&outside_config, plugin_root.join("mcp.json"))
        .expect("create MCP symlink");
    let config_path = AbsolutePathBuf::from_absolute_path(plugin_root.join("mcp.json"))
        .expect("absolute MCP path");

    let discovered = load_mcp_servers_from_file(
        &plugin_root,
        /*plugin_data_root*/ None,
        PluginManifestFormat::AgentPlugin,
        &config_path,
    )
    .await;

    assert!(discovered.mcp_servers.is_empty());
}

#[tokio::test]
async fn agent_plugin_mcp_rejects_present_nonregular_config() {
    let temp_dir = TempDir::new().expect("tempdir");
    let plugin_root = temp_dir.path().join("plugin");
    let config_path = plugin_root.join("mcp.json");
    fs::create_dir_all(&config_path).expect("create nonregular MCP config");

    let discovered = load_mcp_servers_from_file(
        &plugin_root,
        /*plugin_data_root*/ None,
        PluginManifestFormat::AgentPlugin,
        &AbsolutePathBuf::from_absolute_path(config_path).expect("absolute MCP path"),
    )
    .await;

    assert!(discovered.mcp_servers.is_empty());
}

#[tokio::test]
async fn legacy_manifest_can_point_at_root_mcp_json() {
    let temp_dir = TempDir::new().expect("tempdir");
    let plugin_root = temp_dir.path().join("plugin");
    fs::create_dir_all(plugin_root.join(".codex-plugin")).expect("create manifest directory");
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"plugin","mcpServers":"./mcp.json"}"#,
    )
    .expect("write legacy manifest");
    fs::write(
        plugin_root.join("mcp.json"),
        r#"{"mcpServers":{"legacy":{"command":"echo"}}}"#,
    )
    .expect("write legacy MCP config");
    let manifest = load_plugin_manifest(&plugin_root).expect("load legacy manifest");

    let discovered = load_plugin_mcp_servers_from_manifest_with_format(
        &plugin_root,
        &manifest.paths,
        /*plugin_policy*/ None,
        /*plugin_data_root*/ None,
        PluginManifestFormat::Legacy,
    )
    .await;

    assert_eq!(
        discovered.keys().collect::<Vec<_>>(),
        vec![&"legacy".to_string()]
    );
}

#[tokio::test]
async fn installed_agent_plugin_uses_isolated_data_root_for_stdio_mcp() {
    let temp_dir = TempDir::new().expect("tempdir");
    let plugin_root = temp_dir.path().join("plugins/cache/c/a-b/local");
    write_file(
        &plugin_root.join("plugin.json"),
        &format!(r#"{{"$schema":"{AGENT_PLUGIN_SCHEMA_URI}","name":"a-b"}}"#),
    );
    write_file(
        &plugin_root.join("mcp.json"),
        r#"{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
  "mcpServers": {
    "example": {
      "type": "stdio",
      "command": "echo"
    }
  }
}"#,
    );
    let stack = ConfigLayerStack::new(
        vec![user_layer(
            user_config_path(&temp_dir, "config.toml"),
            "[plugins.\"a-b@c\"]\nenabled = true\n",
        )],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .expect("valid config layer stack");
    let store = PluginStore::new(temp_dir.path().to_path_buf());

    let plugins = load_plugins_from_layer_stack(
        &stack,
        &store,
        /*plugin_skill_snapshots*/ None,
        Some(Product::Codex),
        test_skill_root_loader().as_ref(),
    )
    .await;

    let expected_data_root = temp_dir
        .path()
        .join("plugins")
        .join("data")
        .join("agent-plugins")
        .join("6920dd17774030852d11d1b94758fcaae4f894c7b2f36301ed174bc3b33e0743");
    let expected_data_root = AbsolutePathBuf::from_absolute_path(expected_data_root)
        .expect("absolute Agent Plugin data root")
        .canonicalize()
        .expect("canonical Agent Plugin data root");
    let server = plugins
        .first()
        .and_then(|plugin| plugin.mcp_servers.get("example"))
        .expect("Agent plugin stdio MCP server");
    let McpServerTransportConfig::Stdio { env, .. } = &server.transport else {
        panic!("expected stdio MCP server");
    };
    assert_eq!(
        env.as_ref()
            .and_then(|env| env.get("PLUGIN_DATA"))
            .map(String::as_str),
        expected_data_root.as_path().to_str()
    );
    assert!(expected_data_root.as_path().is_dir());
}

#[test]
fn configured_plugins_from_stack_merges_enabled_effective_layers() {
    let temp_dir = TempDir::new().expect("tempdir");
    let stack = ConfigLayerStack::new(
        vec![
            ConfigLayerEntry::new(
                ConfigLayerSource::System {
                    file: user_config_path(&temp_dir, "system.toml"),
                },
                toml::from_str("[plugins.system]\nenabled = true\n").expect("system config toml"),
            ),
            user_layer(
                user_config_path(&temp_dir, "config.toml"),
                "[plugins.base]\nenabled = true\n",
            ),
            user_layer(
                user_config_path(&temp_dir, "work.config.toml"),
                "[plugins.profile]\nenabled = false\n",
            ),
            ConfigLayerEntry::new(
                ConfigLayerSource::Project {
                    dot_codex_folder: user_config_path(&temp_dir, "project/.codex"),
                },
                toml::from_str(
                    "[plugins.profile]\nenabled = true\n[plugins.profile.mcp_servers.example]\nenabled = false\n",
                )
                .expect("project config toml"),
            ),
            ConfigLayerEntry::new_disabled(
                ConfigLayerSource::Project {
                    dot_codex_folder: user_config_path(&temp_dir, "project/untrusted/.codex"),
                },
                toml::from_str("[plugins.untrusted]\nenabled = true\n")
                    .expect("untrusted project config toml"),
                "project is untrusted",
            ),
        ],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .expect("valid config layer stack");

    let project_mcp_servers = HashMap::from([(
        "example".to_string(),
        PluginMcpServerConfig {
            enabled: false,
            ..PluginMcpServerConfig::default()
        },
    )]);
    let plugins = configured_plugins_from_stack(&stack, temp_dir.path());

    assert_eq!(
        plugins,
        HashMap::from([
            (
                "base".to_string(),
                PluginConfig {
                    enabled: true,
                    mcp_servers: HashMap::new(),
                },
            ),
            (
                "profile".to_string(),
                PluginConfig {
                    enabled: true,
                    mcp_servers: project_mcp_servers.clone(),
                },
            ),
            (
                "system".to_string(),
                PluginConfig {
                    enabled: true,
                    mcp_servers: HashMap::new(),
                },
            ),
        ])
    );
    assert_eq!(
        configured_plugin_mcp_server_policies(&stack).get("profile"),
        Some(&project_mcp_servers)
    );
}

#[tokio::test]
async fn legacy_ema_policy_disables_installed_and_selected_plugin_servers() {
    let temp_dir = TempDir::new().expect("tempdir");
    let plugin_root = temp_dir.path().join("plugin");
    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"plugin","mcpServers":"./mcp.json"}"#,
    );
    write_file(
        &plugin_root.join("mcp.json"),
        r#"{"mcpServers":{"example":{"url":"https://resource.example/mcp"}}}"#,
    );
    for endpoint in ["https://resource.example/mcp", "https://other.example/mcp"] {
        let stack = ConfigLayerStack::new(
            vec![user_layer(
                user_config_path(&temp_dir, "config.toml"),
                &format!(
                    "[plugins.\"plugin@market\".mcp_servers.example.ema_auth]\nurl = '{endpoint}'"
                ),
            )],
            ConfigRequirements::default(),
            ConfigRequirementsToml::default(),
        )
        .expect("legacy policy stack");
        let policies = configured_plugin_mcp_server_policies(&stack);
        let policy = policies.get("plugin@market").expect("plugin policy");
        let installed = load_plugin_mcp_servers_with_policy(
            &plugin_root,
            /*auth_mode*/ None,
            Some(policy),
        )
        .await;
        let mut selected = load_plugin_mcp_servers(&plugin_root, /*auth_mode*/ None).await;
        assert!(selected["example"].enabled);
        apply_configured_plugin_mcp_server_policies(policy, &mut selected);
        assert!(!installed["example"].enabled);
        assert_eq!(installed, selected);

        let round_trip: HashMap<String, PluginMcpServerConfig> =
            toml::from_str(&toml::to_string(policy).expect("serialize policy"))
                .expect("deserialize policy");
        assert_eq!(
            &round_trip, policy,
            "serialization must preserve the denial"
        );
    }
}

#[tokio::test]
async fn hooks_only_scope_shares_plugin_resolution_without_loading_other_capabilities() {
    let temp_dir = TempDir::new().expect("tempdir");
    let plugin_root = temp_dir.path().join("plugins/cache/test/valid/local");
    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"valid"}"#,
    );
    write_file(
        &plugin_root.join("skills/example/SKILL.md"),
        "---\nname: example\ndescription: example skill\n---\n",
    );
    write_file(
        &plugin_root.join(".mcp.json"),
        r#"{"mcpServers":{"example":{"command":"echo"}}}"#,
    );
    write_file(
        &plugin_root.join(".app.json"),
        r#"{"apps":{"example":{"id":"connector_example"}}}"#,
    );
    write_file(
        &plugin_root.join("hooks/hooks.json"),
        r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo startup"
          }
        ]
      }
    ]
  }
}"#,
    );

    let disabled_root = temp_dir.path().join("plugins/cache/test/disabled/local");
    write_file(
        &disabled_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"disabled"}"#,
    );
    write_file(
        &disabled_root.join("hooks/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo disabled"}]}]}}"#,
    );

    let malformed_root = temp_dir.path().join("plugins/cache/test/malformed/local");
    write_file(
        &malformed_root.join(".codex-plugin/plugin.json"),
        "not valid json",
    );

    let warning_root = temp_dir.path().join("plugins/cache/test/warning/local");
    write_file(
        &warning_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"warning"}"#,
    );
    write_file(&warning_root.join("hooks/hooks.json"), "not valid json");

    let stack = ConfigLayerStack::new(
        vec![user_layer(
            user_config_path(&temp_dir, "config.toml"),
            r#"
[plugins."valid@test"]
enabled = true

[plugins."disabled@test"]
enabled = false

[plugins.invalid]
enabled = true

[plugins."malformed@test"]
enabled = true

[plugins."missing@test"]
enabled = true

[plugins."warning@test"]
enabled = true
"#,
        )],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .expect("valid config layer stack");
    let store = PluginStore::new(temp_dir.path().to_path_buf());

    let full = load_plugins_from_layer_stack(
        &stack,
        &store,
        /*plugin_skill_snapshots*/ None,
        Some(Product::Codex),
        test_skill_root_loader().as_ref(),
    )
    .await;
    let hooks_only =
        load_plugins_from_layer_stack_with_scope(&stack, &store, PluginLoadScope::HooksOnly).await;

    let validation_state = |plugins: &[LoadedPlugin<McpServerConfig>]| {
        plugins
            .iter()
            .map(|plugin| {
                (
                    plugin.config_name.clone(),
                    plugin.enabled,
                    plugin.root.clone(),
                    plugin.error.clone(),
                    plugin.hook_sources.clone(),
                    plugin.hook_load_warnings.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(validation_state(&hooks_only), validation_state(&full));

    let full_valid = full
        .iter()
        .find(|plugin| plugin.config_name == "valid@test")
        .expect("full load should include valid plugin");
    assert!(full_valid.manifest_name.is_some());
    assert!(!full_valid.skill_roots.is_empty());
    assert!(!full_valid.mcp_servers.is_empty());
    assert!(full_valid.apps.is_empty());

    let hooks_only_valid = hooks_only
        .iter()
        .find(|plugin| plugin.config_name == "valid@test")
        .expect("hooks-only load should include valid plugin");
    assert_eq!(hooks_only_valid.manifest_name, None);
    assert!(hooks_only_valid.skill_roots.is_empty());
    assert!(hooks_only_valid.mcp_servers.is_empty());
    assert!(hooks_only_valid.apps.is_empty());
}

fn plugin_id() -> PluginId {
    PluginId::parse("demo-plugin@test-marketplace").expect("plugin id")
}

fn plugin_root() -> (tempfile::TempDir, AbsolutePathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plugin_root =
        AbsolutePathBuf::try_from(tmp.path().join("demo-plugin")).expect("plugin root");
    fs::create_dir_all(plugin_root.join(".codex-plugin")).expect("create manifest dir");
    fs::create_dir_all(plugin_root.join("hooks")).expect("create hooks dir");
    (tmp, plugin_root)
}

fn write_manifest(plugin_root: &AbsolutePathBuf, manifest: &str) {
    fs::write(plugin_root.join(".codex-plugin/plugin.json"), manifest).expect("write manifest");
}

fn write_hook_file(plugin_root: &AbsolutePathBuf, relative_path: &str, event: &str, command: &str) {
    fs::write(
        plugin_root.join(relative_path),
        format!(
            r#"{{
  "hooks": {{
    "{event}": [
      {{
        "hooks": [{{ "type": "command", "command": "{command}" }}]
      }}
    ]
  }}
}}"#
        ),
    )
    .expect("write hooks");
}

fn load_sources(plugin_root: &AbsolutePathBuf) -> (Vec<PluginHookSource>, Vec<String>) {
    let loaded_manifest =
        load_plugin_manifest_with_format(plugin_root.as_path()).expect("manifest");
    let plugin_data_root = AbsolutePathBuf::try_from(
        plugin_root
            .as_path()
            .parent()
            .expect("plugin root parent")
            .join("plugin-data"),
    )
    .expect("plugin data root");
    load_plugin_hooks(
        plugin_root,
        &plugin_id(),
        &plugin_data_root,
        &loaded_manifest.manifest.paths,
    )
}

fn assert_sources(sources: &[PluginHookSource], expected_relative_paths: &[&str]) {
    assert_eq!(
        sources
            .iter()
            .map(|source| source.plugin_id.clone())
            .collect::<Vec<_>>(),
        vec![plugin_id(); expected_relative_paths.len()]
    );
    assert_eq!(
        sources
            .iter()
            .map(|source| source.source_relative_path.as_str())
            .collect::<Vec<_>>(),
        expected_relative_paths
    );
    assert_eq!(
        sources
            .iter()
            .map(|source| source.hooks.handler_count())
            .collect::<Vec<_>>(),
        vec![1; expected_relative_paths.len()]
    );
}

#[test]
fn load_plugin_hooks_discovers_default_hooks_file() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(&plugin_root, r#"{ "name": "demo-plugin" }"#);
    fs::write(
        plugin_root.join("hooks/hooks.json"),
        r#"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "echo default" }]
      }
    ]
  }
}"#,
    )
    .expect("write hooks");

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(warnings, Vec::<String>::new());
    assert_sources(&sources, &["hooks/hooks.json"]);
}

#[test]
fn load_plugin_hooks_supports_manifest_hook_path() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(
        &plugin_root,
        r#"{
  "name": "demo-plugin",
  "hooks": "./hooks/one.json"
}"#,
    );
    write_hook_file(&plugin_root, "hooks/one.json", "PreToolUse", "echo one");

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(warnings, Vec::<String>::new());
    assert_sources(&sources, &["hooks/one.json"]);
}

#[test]
fn load_plugin_hooks_manifest_paths_replace_default_hooks_file() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(
        &plugin_root,
        r#"{
  "name": "demo-plugin",
  "hooks": ["./hooks/one.json", "./hooks/two.json"]
}"#,
    );
    write_hook_file(
        &plugin_root,
        "hooks/hooks.json",
        "PreToolUse",
        "echo ignored",
    );
    write_hook_file(&plugin_root, "hooks/one.json", "PreToolUse", "echo one");
    write_hook_file(&plugin_root, "hooks/two.json", "PostToolUse", "echo two");

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(warnings, Vec::<String>::new());
    assert_sources(&sources, &["hooks/one.json", "hooks/two.json"]);
}

#[test]
fn load_plugin_hooks_supports_inline_manifest_hooks() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(
        &plugin_root,
        r#"{
  "name": "demo-plugin",
  "hooks": {
    "hooks": {
      "SessionStart": [
        {
          "matcher": "startup",
          "hooks": [{ "type": "command", "command": "echo inline" }]
        }
      ]
    }
  }
}"#,
    );

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(warnings, Vec::<String>::new());
    assert_sources(&sources, &["plugin.json#hooks[0]"]);
}

#[test]
fn load_plugin_hooks_reports_invalid_hook_file() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(&plugin_root, r#"{ "name": "demo-plugin" }"#);
    fs::write(plugin_root.join("hooks/hooks.json"), "{ not-json").expect("write invalid hooks");

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(sources, Vec::<PluginHookSource>::new());
    assert_eq!(
        warnings,
        vec![format!(
            "failed to parse plugin hooks config {}: key must be a string at line 1 column 3",
            plugin_root.join("hooks/hooks.json").display()
        )]
    );
}

#[test]
fn load_plugin_hooks_supports_inline_manifest_hook_list() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(
        &plugin_root,
        r#"{
  "name": "demo-plugin",
  "hooks": [
    {
      "hooks": {
        "SessionStart": [
          {
            "hooks": [{ "type": "command", "command": "echo inline one" }]
          }
        ]
      }
    },
    {
      "hooks": {
        "Stop": [
          {
            "hooks": [{ "type": "command", "command": "echo inline two" }]
          }
        ]
      }
    }
  ]
}"#,
    );

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(warnings, Vec::<String>::new());
    assert_sources(&sources, &["plugin.json#hooks[0]", "plugin.json#hooks[1]"]);
}
