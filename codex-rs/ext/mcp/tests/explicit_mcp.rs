use codex_config::McpServerTransportConfig;
use codex_core::McpManager;
use codex_core::config::{Config, ConfigBuilder};
use codex_core::plugins_manager_for_config;
use codex_extension_api::{
    ExtensionRegistryBuilder, McpServerContribution, McpServerContributionContext,
    McpServerContributor,
};
use codex_login::test_support::auth_manager_from_optional_auth;
use codex_login::{AuthManager, CodexAuth};
use std::sync::Arc;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn runtime_does_not_add_implicit_hosted_servers() -> TestResult {
    let home = tempfile::tempdir()?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .build()
        .await?;
    let manager = McpManager::new(Arc::new(plugins_manager_for_config(
        &config,
        auth_manager_from_optional_auth(None),
    )));
    assert!(manager.runtime_servers(&config).await.is_empty());
    Ok(())
}

#[tokio::test]
async fn explicit_codex_apps_name_preserves_user_url_and_enabled_state() -> TestResult {
    for enabled in [false, true] {
        let home = tempfile::tempdir()?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .fallback_cwd(Some(home.path().to_path_buf()))
            .cli_overrides(vec![
                (
                    "mcp_servers.codex_apps.url".to_string(),
                    "https://example.com/mcp".into(),
                ),
                ("mcp_servers.codex_apps.enabled".to_string(), enabled.into()),
            ])
            .build()
            .await?;
        for auth in [None, Some(CodexAuth::from_api_key("provider-key"))] {
            let manager = McpManager::new(Arc::new(plugins_manager_for_config(
                &config,
                auth_manager_from_optional_auth(auth.clone()),
            )));
            let servers = manager.runtime_servers(&config).await;
            let server = servers
                .get("codex_apps")
                .expect("explicit server remains configured");
            assert_eq!(server.enabled, enabled);
            let McpServerTransportConfig::StreamableHttp { url, .. } = &server.transport else {
                panic!("configured HTTP server should retain its transport");
            };
            assert_eq!(url, "https://example.com/mcp");
        }
    }
    Ok(())
}

#[tokio::test]
async fn extension_can_remove_explicit_server_registration() -> TestResult {
    let home = tempfile::tempdir()?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .cli_overrides(vec![(
            "mcp_servers.codex_apps.url".to_string(),
            "https://example.com/mcp".into(),
        )])
        .build()
        .await?;
    let mut builder = ExtensionRegistryBuilder::new();
    builder.mcp_server_contributor(Arc::new(RemoveExplicitServer));
    let manager = McpManager::new_with_extensions(
        Arc::new(plugins_manager_for_config(
            &config,
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("provider-key")),
        )),
        Arc::new(builder.build()),
        codex_core::CodexAppsToolsCache::default(),
    );
    assert!(
        !manager
            .runtime_servers(&config)
            .await
            .contains_key("codex_apps")
    );
    Ok(())
}

struct RemoveExplicitServer;

impl McpServerContributor<Config> for RemoveExplicitServer {
    fn id(&self) -> &'static str {
        "remove_explicit_server"
    }

    fn contribute<'a>(
        &'a self,
        _context: McpServerContributionContext<'a, Config>,
    ) -> codex_extension_api::ExtensionFuture<'a, Vec<McpServerContribution>> {
        Box::pin(async move {
            vec![McpServerContribution::Remove {
                name: "codex_apps".to_string(),
            }]
        })
    }
}
