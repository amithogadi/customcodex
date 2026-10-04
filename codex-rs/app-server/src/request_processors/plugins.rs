use super::config_processor::reload_user_config;
use super::*;
use crate::error_code::{internal_error, invalid_request};
use codex_app_server_protocol::PluginAvailability;
use codex_config::types::McpServerConfig;
use codex_core_plugins::PluginMarketplaceContext;
use codex_core_plugins::loader::load_configured_plugin_mcp_servers;
use codex_core_plugins::manifest::is_agent_plugin_manifest;
use codex_mcp::{
    McpOAuthLoginSupport, McpRuntimeContext, oauth_login_support, resolve_oauth_callback,
    should_retry_without_scopes,
};
use codex_plugin::PluginId;
use codex_rmcp_client::{
    McpOAuthClientRegistration, OAuthDiscoveryTimeout, StreamableHttpRedirectMode,
    perform_oauth_login_silent,
};
mod local;

#[derive(Clone)]
pub(crate) struct PluginRequestProcessor {
    auth_manager: Arc<AuthManager>,
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    config_manager: ConfigManager,
    on_effective_plugins_changed:
        Arc<dyn Fn(codex_core_plugins::EffectivePluginsChange) + Send + Sync>,
}
fn plugin_redirect_mode(plugin_root: &Path) -> StreamableHttpRedirectMode {
    if is_agent_plugin_manifest(plugin_root) {
        StreamableHttpRedirectMode::AgentPluginV1
    } else {
        StreamableHttpRedirectMode::Legacy
    }
}

fn plugin_skills_to_info<'a>(
    skills: impl IntoIterator<Item = &'a codex_skills::SkillMetadata>,
    disabled_skill_paths: &HashSet<AbsolutePathBuf>,
) -> Vec<SkillSummary> {
    skills
        .into_iter()
        .map(|skill| SkillSummary {
            name: skill.name.clone(),
            description: skill.description.clone(),
            short_description: skill.short_description.clone(),
            interface: skill.interface.clone().map(|interface| {
                codex_app_server_protocol::SkillInterface {
                    display_name: interface.display_name,
                    short_description: interface.short_description,
                    icon_small: interface.icon_small,
                    icon_large: interface.icon_large,
                    icon_small_url: None,
                    icon_large_url: None,
                    brand_color: interface.brand_color,
                    default_prompt: interface.default_prompt,
                }
            }),
            path: Some(skill.path_to_skills_md.clone()),
            enabled: !disabled_skill_paths.contains(&skill.path_to_skills_md),
        })
        .collect()
}

fn local_plugin_interface_to_info(interface: PluginManifestInterface) -> PluginInterface {
    PluginInterface {
        display_name: interface.display_name,
        short_description: interface.short_description,
        long_description: interface.long_description,
        developer_name: interface.developer_name,
        category: interface.category,
        capabilities: interface.capabilities,
        website_url: interface.website_url,
        privacy_policy_url: interface.privacy_policy_url,
        terms_of_service_url: interface.terms_of_service_url,
        default_prompt: interface.default_prompt,
        brand_color: interface.brand_color,
        composer_icon: interface.composer_icon,
        composer_icon_url: None,
        logo: interface.logo,
        logo_dark: interface.logo_dark,
        logo_url: None,
        logo_url_dark: None,
        screenshots: interface.screenshots,
        screenshot_urls: Vec::new(),
    }
}

fn marketplace_plugin_source_to_info(source: MarketplacePluginSource) -> PluginSource {
    match source {
        MarketplacePluginSource::Local { path } => PluginSource::Local { path },
        MarketplacePluginSource::Git {
            url,
            path,
            ref_name,
            sha,
        } => PluginSource::Git {
            url,
            path,
            ref_name,
            sha,
        },
        MarketplacePluginSource::Npm {
            package,
            version,
            registry,
        } => PluginSource::Npm {
            package,
            version,
            registry,
        },
    }
}

fn convert_configured_marketplace_plugin_to_plugin_summary(
    plugin: codex_core_plugins::ConfiguredMarketplacePlugin,
) -> PluginSummary {
    PluginSummary {
        id: plugin.id,
        remote_plugin_id: None,
        version: None,
        local_version: plugin.local_version,
        installed: plugin.installed,
        installed_at: None,
        enabled: plugin.enabled,
        name: plugin.name,
        share_context: None,
        source: marketplace_plugin_source_to_info(plugin.source),
        install_policy: plugin.policy.installation.into(),
        install_policy_source: None,
        must_show_installation_interstitial: None,
        auth_policy: plugin.policy.authentication.into(),
        availability: PluginAvailability::Available,
        disabled_reason: None,
        eligible_plan_types: None,
        interface: plugin.interface.map(local_plugin_interface_to_info),
        keywords: plugin.keywords,
    }
}

impl PluginRequestProcessor {
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        config_manager: ConfigManager,
        on_effective_plugins_changed: Arc<
            dyn Fn(codex_core_plugins::EffectivePluginsChange) + Send + Sync,
        >,
    ) -> Self {
        Self {
            auth_manager,
            thread_manager,
            outgoing,
            config_manager,
            on_effective_plugins_changed,
        }
    }

    pub(crate) async fn plugin_list(
        &self,
        params: PluginListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.plugin_list_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn plugin_installed(
        &self,
        params: PluginInstalledParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.plugin_installed_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn plugin_read(
        &self,
        params: PluginReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.plugin_read_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn plugin_install(
        &self,
        params: PluginInstallParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.plugin_install_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn plugin_uninstall(
        &self,
        params: PluginUninstallParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.plugin_uninstall_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) fn effective_plugins_changed_callback(
        &self,
    ) -> Arc<dyn Fn(codex_core_plugins::EffectivePluginsChange) + Send + Sync> {
        Arc::clone(&self.on_effective_plugins_changed)
    }

    async fn on_effective_plugins_changed(&self) {
        self.clear_plugin_related_caches();
        self.thread_manager.invalidate_mcp_runtimes().await;
        self.thread_manager.refresh_hook_runtimes().await;
    }

    fn clear_plugin_related_caches(&self) {
        self.thread_manager.plugins_manager().clear_cache();
        self.thread_manager.skills_service().clear_cache();
    }

    async fn load_latest_config(
        &self,
        fallback_cwd: Option<PathBuf>,
    ) -> Result<Config, JSONRPCErrorError> {
        self.config_manager
            .load_latest_config(fallback_cwd)
            .await
            .map_err(|err| internal_error(format!("failed to reload config: {err}")))
    }

    async fn start_plugin_mcp_oauth_logins(
        &self,
        config: &Config,
        plugin_id: &PluginId,
        mut plugin_mcp_servers: HashMap<String, McpServerConfig>,
        redirect_mode: StreamableHttpRedirectMode,
    ) {
        let plugin_id = plugin_id.as_key();
        config.apply_plugin_mcp_server_requirements(&plugin_id, &mut plugin_mcp_servers);
        let runtime_context = McpRuntimeContext::new(
            self.thread_manager.environment_manager(),
            config.cwd.to_path_buf(),
        );
        for (name, server) in plugin_mcp_servers {
            // EMA uses the account's enterprise grant, never per-plugin OAuth fallback.
            if !server.enabled || matches!(server.auth, codex_config::types::McpServerAuth::EmaAuth)
            {
                continue;
            }
            if !server.is_local_environment() {
                warn!(
                    plugin = %plugin_id,
                    server = %name,
                    environment_id = %server.environment_id,
                    "skipping plugin MCP OAuth for an unowned environment"
                );
                continue;
            }
            let http_client = match runtime_context.resolve_http_client(&name, &server) {
                Ok(http_client) => http_client,
                Err(err) => {
                    warn!("failed to resolve MCP runtime for plugin install {name}: {err}");
                    continue;
                }
            };
            let login_support = oauth_login_support(
                &server.transport,
                Arc::clone(&http_client),
                OAuthDiscoveryTimeout::LOCAL,
                redirect_mode,
            )
            .await;
            let oauth_config = match login_support {
                McpOAuthLoginSupport::Supported(config) => config,
                McpOAuthLoginSupport::Unsupported => continue,
                McpOAuthLoginSupport::Unknown(err) => {
                    warn!(
                        "MCP server may or may not require login for plugin install {name}: {err}"
                    );
                    continue;
                }
            };

            let resolved_scopes = resolve_oauth_scopes(
                /*explicit_scopes*/ None,
                server.scopes.clone(),
                oauth_config.discovered_scopes.clone(),
            );

            let store_mode = config.mcp_oauth_credentials_store_mode;
            let keyring_backend_kind = config.auth_keyring_backend_kind();
            let callback_port = server.oauth_callback_port(config.mcp_oauth_callback_port);
            let callback_url = match resolve_oauth_callback(
                &server,
                &oauth_config.url,
                config.mcp_oauth_callback_url.as_deref(),
            ) {
                Ok(callback_url) => callback_url,
                Err(error) => {
                    warn!(
                        "failed to resolve MCP OAuth callback for plugin install {name}: {error}"
                    );
                    continue;
                }
            };
            let outgoing = Arc::clone(&self.outgoing);
            let notification_name = name.clone();
            let oauth_credential_name = server.oauth_credential_name(&name).into_owned();
            let thread_manager = Arc::clone(&self.thread_manager);
            let http_client = Arc::clone(&http_client);
            let global_callback_url = config.mcp_oauth_callback_url.clone();

            tokio::spawn(async move {
                let oauth_client_config = server.oauth.as_ref();
                let first_attempt = perform_oauth_login_silent(
                    &oauth_credential_name,
                    &oauth_config.url,
                    store_mode,
                    keyring_backend_kind,
                    oauth_config.http_headers.clone(),
                    oauth_config.env_http_headers.clone(),
                    &resolved_scopes.scopes,
                    oauth_client_config,
                    McpOAuthClientRegistration::Auto,
                    server.oauth_resource.as_deref(),
                    callback_port,
                    callback_url.as_deref(),
                    global_callback_url.as_deref(),
                    Arc::clone(&http_client),
                    redirect_mode,
                )
                .await;

                let final_result = match first_attempt {
                    Err(err) if should_retry_without_scopes(&resolved_scopes, &err) => {
                        perform_oauth_login_silent(
                            &oauth_credential_name,
                            &oauth_config.url,
                            store_mode,
                            keyring_backend_kind,
                            oauth_config.http_headers,
                            oauth_config.env_http_headers,
                            &[],
                            oauth_client_config,
                            McpOAuthClientRegistration::Auto,
                            server.oauth_resource.as_deref(),
                            callback_port,
                            callback_url.as_deref(),
                            global_callback_url.as_deref(),
                            http_client,
                            redirect_mode,
                        )
                        .await
                    }
                    result => result,
                };

                let (success, error) = match final_result {
                    Ok(()) => (true, None),
                    Err(err) => (false, Some(err.to_string())),
                };
                if success {
                    thread_manager.invalidate_mcp_runtimes().await;
                }

                let notification = ServerNotification::McpServerOauthLoginCompleted(
                    McpServerOauthLoginCompletedNotification {
                        name: notification_name,
                        thread_id: None,
                        login_id: None,
                        success,
                        error,
                    },
                );
                outgoing.send_server_notification(notification).await;
            });
        }
    }

    fn marketplace_error(err: MarketplaceError, action: &str) -> JSONRPCErrorError {
        match err {
            MarketplaceError::MarketplaceNotFound { .. }
            | MarketplaceError::InvalidMarketplaceFile { .. }
            | MarketplaceError::PluginNotFound { .. }
            | MarketplaceError::PluginNotAvailable { .. }
            | MarketplaceError::PluginsDisabled
            | MarketplaceError::InvalidPlugin(_) => invalid_request(err.to_string()),
            MarketplaceError::Io { .. } => internal_error(format!("failed to {action}: {err}")),
        }
    }

    async fn plugin_list_response(
        &self,
        params: PluginListParams,
    ) -> Result<PluginListResponse, JSONRPCErrorError> {
        let roots = params.cwds.unwrap_or_default();
        let config = self.load_catalog_config(&roots).await?;
        let context = self.load_marketplace_context(roots, &config).await;
        let outcome = self
            .thread_manager
            .plugins_manager()
            .list_marketplaces_for_context(&context, false)
            .map_err(|err| Self::marketplace_error(err, "list local plugins"))?;
        Ok(PluginListResponse {
            marketplaces: outcome
                .marketplaces
                .into_iter()
                .map(|marketplace| PluginMarketplaceEntry {
                    name: marketplace.name,
                    path: Some(marketplace.path),
                    interface: marketplace.interface.map(|interface| MarketplaceInterface {
                        display_name: interface.display_name,
                    }),
                    plugins: marketplace
                        .plugins
                        .into_iter()
                        .map(convert_configured_marketplace_plugin_to_plugin_summary)
                        .collect(),
                })
                .collect(),
            marketplace_load_errors: outcome
                .errors
                .into_iter()
                .map(|err| codex_app_server_protocol::MarketplaceLoadErrorInfo {
                    marketplace_path: err.path,
                    message: err.message,
                })
                .collect(),
            featured_plugin_ids: Vec::new(),
        })
    }
    async fn plugin_installed_response(
        &self,
        params: PluginInstalledParams,
    ) -> Result<PluginInstalledResponse, JSONRPCErrorError> {
        let mut result = self
            .plugin_list_response(PluginListParams {
                cwds: params.cwds,
                marketplace_kinds: None,
                force_refetch: false,
            })
            .await?;
        for marketplace in &mut result.marketplaces {
            marketplace.plugins.retain(|plugin| plugin.installed);
        }
        result
            .marketplaces
            .retain(|marketplace| !marketplace.plugins.is_empty());
        Ok(PluginInstalledResponse {
            marketplaces: result.marketplaces,
            marketplace_load_errors: result.marketplace_load_errors,
        })
    }

    async fn plugin_read_response(
        &self,
        params: PluginReadParams,
    ) -> Result<PluginReadResponse, JSONRPCErrorError> {
        if params.remote_marketplace_name.is_some() {
            return Err(invalid_request(
                "Online plugin catalogs are unsupported; supply marketplacePath.",
            ));
        }
        let marketplace_path = params
            .marketplace_path
            .ok_or_else(|| invalid_request("marketplacePath is required"))?;
        let config = self
            .load_latest_config(marketplace_path.as_path().parent().map(Path::to_path_buf))
            .await?;
        let outcome = self
            .thread_manager
            .plugins_manager()
            .read_plugin_for_config(
                &config.plugins_config_input(),
                &PluginReadRequest {
                    plugin_name: params.plugin_name,
                    marketplace_path,
                },
            )
            .await
            .map_err(|err| Self::marketplace_error(err, "read local plugin"))?;
        let visible_skills = outcome.plugin.skills.iter().filter(|skill| {
            skill.matches_product_restriction_for_product(
                self.thread_manager.session_source().restriction_product(),
            )
        });
        let skills = plugin_skills_to_info(visible_skills, &outcome.plugin.disabled_skill_paths);
        let onboarding_skill = if outcome.plugin.enabled
            && let Some(path) = outcome.plugin.onboarding_skill.as_ref()
        {
            skills
                .iter()
                .find(|skill| skill.enabled && skill.path.as_ref() == Some(path))
                .cloned()
        } else {
            None
        };
        let plugin = PluginDetail {
            marketplace_name: outcome.marketplace_name,
            marketplace_path: outcome.marketplace_path,
            summary: PluginSummary {
                id: outcome.plugin.id,
                remote_plugin_id: None,
                version: None,
                local_version: outcome.plugin.local_version,
                name: outcome.plugin.name,
                share_context: None,
                source: marketplace_plugin_source_to_info(outcome.plugin.source),
                installed: outcome.plugin.installed,
                installed_at: None,
                enabled: outcome.plugin.enabled,
                install_policy: outcome.plugin.policy.installation.into(),
                install_policy_source: None,
                must_show_installation_interstitial: None,
                auth_policy: outcome.plugin.policy.authentication.into(),
                availability: PluginAvailability::Available,
                disabled_reason: None,
                eligible_plan_types: None,
                interface: outcome.plugin.interface.map(local_plugin_interface_to_info),
                keywords: outcome.plugin.keywords,
            },
            share_url: None,
            description: outcome.plugin.description,
            skills,
            onboarding_skill,
            hooks: outcome
                .plugin
                .hooks
                .into_iter()
                .map(|hook| codex_app_server_protocol::PluginHookSummary {
                    key: hook.key,
                    event_name: hook.event_name.into(),
                })
                .collect(),
            apps: Vec::new(),
            app_templates: Vec::new(),
            mcp_servers: outcome.plugin.mcp_server_names,
            scheduled_tasks: None,
        };
        Ok(PluginReadResponse { plugin })
    }
    async fn plugin_install_response(
        &self,
        params: PluginInstallParams,
    ) -> Result<PluginInstallResponse, JSONRPCErrorError> {
        let PluginInstallParams {
            marketplace_path,
            remote_marketplace_name,
            install_attempt_id: _,
            plugin_name,
        } = params;
        let marketplace_path = match (marketplace_path, remote_marketplace_name) {
            (Some(marketplace_path), None) => marketplace_path,
            _ => {
                return Err(invalid_request(
                    "Online installation is unsupported; supply marketplacePath",
                ));
            }
        };
        let config_cwd = marketplace_path.as_path().parent().map(Path::to_path_buf);
        let config = self.load_latest_config(config_cwd.clone()).await?;
        let plugins_manager = self.thread_manager.plugins_manager();
        let marketplace_display = marketplace_path.display().to_string();
        let plugin_name_for_log = plugin_name.clone();
        let request = PluginInstallRequest {
            plugin_name,
            marketplace_path,
        };

        let result = match plugins_manager
            .install_plugin(&config.config_layer_stack, request)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                warn!(
                    marketplace = %marketplace_display,
                    plugin_name = %plugin_name_for_log,
                    "failed to install plugin: {err}"
                );
                return Err(Self::plugin_install_error(err));
            }
        };
        let config = match self.load_latest_config(config_cwd).await {
            Ok(mut reloaded_config) => {
                // Keep the policy captured with this installation's auth snapshot.
                reloaded_config.application_network_policy = config.application_network_policy;
                reloaded_config
            }
            Err(err) => {
                warn!(
                    "failed to reload config after plugin install, using current config: {err:?}"
                );
                config
            }
        };

        self.clear_plugin_related_caches();
        reload_user_config(&self.config_manager, &self.thread_manager).await;
        self.thread_manager.invalidate_mcp_runtimes().await;
        self.thread_manager.refresh_hook_runtimes().await;

        let plugin_mcp_servers = load_configured_plugin_mcp_servers(
            result.installed_path.as_path(),
            None,
            &result.plugin_id,
            &config.config_layer_stack,
            config.codex_home.as_path(),
        )
        .await;
        if !plugin_mcp_servers.is_empty() {
            let redirect_mode = plugin_redirect_mode(result.installed_path.as_path());
            self.start_plugin_mcp_oauth_logins(
                &config,
                &result.plugin_id,
                plugin_mcp_servers,
                redirect_mode,
            )
            .await;
        }

        Ok(PluginInstallResponse {
            auth_policy: result.auth_policy.into(),
            apps_needing_auth: Vec::new(),
        })
    }

    async fn plugin_uninstall_response(
        &self,
        params: PluginUninstallParams,
    ) -> Result<PluginUninstallResponse, JSONRPCErrorError> {
        let PluginUninstallParams { plugin_id } = params;
        PluginId::parse(&plugin_id).map_err(|err| invalid_request(err.to_string()))?;
        let plugins_manager = self.thread_manager.plugins_manager();

        plugins_manager
            .uninstall_plugin(plugin_id)
            .await
            .map_err(Self::plugin_uninstall_error)?;
        match self.load_latest_config(/*fallback_cwd*/ None).await {
            Ok(_) => self.on_effective_plugins_changed().await,
            Err(err) => {
                warn!(
                    "failed to reload config after plugin uninstall, clearing plugin-related caches only: {err:?}"
                );
                self.clear_plugin_related_caches();
            }
        }
        Ok(PluginUninstallResponse {})
    }

    fn plugin_install_error(err: CorePluginInstallError) -> JSONRPCErrorError {
        if err.is_invalid_request() {
            return invalid_request(err.to_string());
        }

        match err {
            CorePluginInstallError::Marketplace(err) => {
                Self::marketplace_error(err, "install plugin")
            }
            CorePluginInstallError::Config(err) => {
                internal_error(format!("failed to persist installed plugin config: {err}"))
            }
            CorePluginInstallError::Join(err) => {
                internal_error(format!("failed to install plugin: {err}"))
            }
            CorePluginInstallError::Store(err) => {
                internal_error(format!("failed to install plugin: {err}"))
            }
        }
    }

    fn plugin_uninstall_error(err: CorePluginUninstallError) -> JSONRPCErrorError {
        if err.is_invalid_request() {
            return invalid_request(err.to_string());
        }

        match err {
            CorePluginUninstallError::Config(err) => {
                internal_error(format!("failed to clear plugin config: {err}"))
            }
            CorePluginUninstallError::Join(err) => {
                internal_error(format!("failed to uninstall plugin: {err}"))
            }
            CorePluginUninstallError::Store(err) => {
                internal_error(format!("failed to uninstall plugin: {err}"))
            }
            CorePluginUninstallError::InvalidPluginId(_) => {
                unreachable!("invalid plugin ids are handled above");
            }
        }
    }
}
