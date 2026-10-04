use super::*;
use crate::test_support::{load_plugins_config, test_plugins_manager, write_file};
use codex_config::{CONFIG_TOML_FILE, ConfigLayerStack};
use std::{fs, path::Path};
fn unrestricted_config_layer_stack() -> ConfigLayerStack {
    ConfigLayerStack::default()
}

fn write_plugin_with_version(
    root: &Path,
    dir_name: &str,
    manifest_name: &str,
    manifest_version: Option<&str>,
) {
    let plugin_root = root.join(dir_name);
    fs::create_dir_all(plugin_root.join(".codex-plugin")).unwrap();
    fs::create_dir_all(plugin_root.join("skills")).unwrap();
    let version = manifest_version
        .map(|manifest_version| format!(r#","version":"{manifest_version}""#))
        .unwrap_or_default();
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        format!(r#"{{"name":"{manifest_name}"{version}}}"#),
    )
    .unwrap();
    fs::write(
        plugin_root.join("skills/SKILL.md"),
        format!("---\nname: {manifest_name}-skill\ndescription: test skill\n---\n\n# Test skill\n"),
    )
    .unwrap();
    fs::write(plugin_root.join(".mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
}

fn write_plugin(root: &Path, dir_name: &str, manifest_name: &str) {
    write_plugin_with_version(
        root,
        dir_name,
        manifest_name,
        /*manifest_version*/ None,
    );
}

#[tokio::test]
async fn install_plugin_updates_config_with_relative_path_and_plugin_key() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(repo_root.join(".git")).unwrap();
    fs::create_dir_all(repo_root.join(".agents/plugins")).unwrap();
    write_plugin(&repo_root, "sample-plugin", "sample-plugin");
    fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": {
        "source": "local",
        "path": "./sample-plugin"
      },
      "policy": {
        "authentication": "ON_USE"
      }
    }
  ]
}"#,
    )
    .unwrap();

    let result = test_plugins_manager(tmp.path().to_path_buf())
        .install_plugin(
            &unrestricted_config_layer_stack(),
            PluginInstallRequest {
                plugin_name: "sample-plugin".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    repo_root.join(".agents/plugins/marketplace.json"),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();

    let installed_path = tmp.path().join("plugins/cache/debug/sample-plugin/local");
    assert_eq!(
        result,
        PluginInstallOutcome {
            plugin_id: PluginId::new("sample-plugin".to_string(), "debug".to_string()).unwrap(),
            plugin_version: "local".to_string(),
            installed_path: AbsolutePathBuf::try_from(installed_path).unwrap(),
            auth_policy: MarketplacePluginAuthPolicy::OnUse,
        }
    );

    let config = fs::read_to_string(tmp.path().join("config.toml")).unwrap();
    assert!(config.contains(r#"[plugins."sample-plugin@debug"]"#));
    assert!(config.contains("enabled = true"));
}

#[tokio::test]
async fn uninstall_plugin_removes_cache_and_config_entry() {
    let tmp = tempfile::tempdir().unwrap();
    write_plugin(
        &tmp.path().join("plugins/cache/debug"),
        "sample-plugin/local",
        "sample-plugin",
    );
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."sample-plugin@debug"]
enabled = true
"#,
    );

    let manager = test_plugins_manager(tmp.path().to_path_buf());
    manager
        .uninstall_plugin("sample-plugin@debug".to_string())
        .await
        .unwrap();
    manager
        .uninstall_plugin("sample-plugin@debug".to_string())
        .await
        .unwrap();

    assert!(
        !tmp.path()
            .join("plugins/cache/debug/sample-plugin")
            .exists()
    );
    let config = fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).unwrap();
    assert!(!config.contains(r#"[plugins."sample-plugin@debug"]"#));
}

#[tokio::test]
async fn cached_plugins_require_explicit_enablement_and_keep_direct_mcp() {
    let home = tempfile::tempdir().unwrap();
    let root = home
        .path()
        .join("plugins/cache/openai-curated-remote/sample/1.0.0");
    write_file(
        &root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample","version":"1.0.0"}"#,
    );
    write_file(
        &root.join("skills/query/SKILL.md"),
        "---\nname: query\ndescription: query data\n---\n",
    );
    write_file(
        &root.join(".mcp.json"),
        r#"{"mcpServers":{"shared":{"command":"local-tool"}}}"#,
    );
    write_file(
        &root.join(".app.json"),
        r#"{"apps":{"shared":{"id":"connector_shared"}}}"#,
    );
    let manager = test_plugins_manager(home.path().to_path_buf());
    for (entry, expected) in [
        ("", false),
        ("[plugins.\"sample@openai-curated-remote\"]", false),
        (
            "[plugins.\"sample@openai-curated-remote\"]\nenabled=false",
            false,
        ),
        (
            "[plugins.\"sample@openai-curated-remote\"]\nenabled=true",
            true,
        ),
    ] {
        write_file(&home.path().join("config.toml"), entry);
        let config = load_plugins_config(home.path(), home.path()).await;
        let loaded = manager.plugins_for_config(&config).await;
        assert_eq!(
            !loaded.effective_mcp_servers().is_empty(),
            expected,
            "{entry}"
        );
        assert!(loaded.effective_apps().is_empty());
        if !entry.is_empty() {
            let listed = manager
                .list_marketplaces_for_config(&config, &[], false)
                .unwrap();
            let cached = &listed.marketplaces[0];
            assert_eq!(cached.plugins[0].enabled, expected);
            let detail = manager
                .read_plugin_for_config(
                    &config,
                    &PluginReadRequest {
                        plugin_name: "sample".into(),
                        marketplace_path: cached.path.clone(),
                    },
                )
                .await
                .unwrap();
            assert_eq!(detail.plugin.mcp_server_names, vec!["shared"]);
        }
        if expected {
            assert!(loaded.plugins()[0].has_enabled_skills);
            assert!(loaded.plugins()[0].mcp_servers.contains_key("shared"));
        }
    }
}

#[tokio::test]
async fn local_marketplace_rejects_online_plugin_sources_without_materialization() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("marketplace");
    let manifest = root.join(".agents/plugins/marketplace.json");
    write_file(
        &manifest,
        r#"{"name":"local","plugins":[{"name":"remote","source":{"source":"url","url":"https://example.invalid/plugin.git"}}]}"#,
    );
    let result = test_plugins_manager(home.path().to_path_buf())
        .install_plugin(
            &unrestricted_config_layer_stack(),
            PluginInstallRequest {
                plugin_name: "remote".into(),
                marketplace_path: AbsolutePathBuf::try_from(manifest).unwrap(),
            },
        )
        .await;
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("Online plugin installation is unsupported"),
        "{error}"
    );
    assert!(!home.path().join("plugins/cache").exists());
    assert!(!home.path().join("config.toml").exists());
}
