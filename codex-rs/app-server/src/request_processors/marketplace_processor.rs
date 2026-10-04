use super::*;

#[derive(Clone)]
pub(crate) struct MarketplaceRequestProcessor {
    config: Arc<Config>,
    config_manager: ConfigManager,
    thread_manager: Arc<ThreadManager>,
}

impl MarketplaceRequestProcessor {
    pub(crate) fn new(
        config: Arc<Config>,
        config_manager: ConfigManager,
        thread_manager: Arc<ThreadManager>,
    ) -> Self {
        Self {
            config,
            config_manager,
            thread_manager,
        }
    }

    pub(crate) async fn marketplace_add(
        &self,
        params: MarketplaceAddParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.marketplace_add_inner(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn marketplace_remove(
        &self,
        params: MarketplaceRemoveParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.marketplace_remove_inner(params)
            .await
            .map(|response| Some(response.into()))
    }

    async fn marketplace_remove_inner(
        &self,
        params: MarketplaceRemoveParams,
    ) -> Result<MarketplaceRemoveResponse, JSONRPCErrorError> {
        let config = self.load_latest_config(/*fallback_cwd*/ None).await?;
        remove_marketplace(
            config.codex_home.to_path_buf(),
            config.config_layer_stack,
            CoreMarketplaceRemoveRequest {
                marketplace_name: params.marketplace_name,
            },
        )
        .await
        .map(|outcome| MarketplaceRemoveResponse {
            marketplace_name: outcome.marketplace_name,
            installed_root: outcome.removed_installed_root,
        })
        .map_err(|err| match err {
            MarketplaceRemoveError::InvalidRequest(message) => invalid_request(message),
            MarketplaceRemoveError::Internal(message) => internal_error(message),
        })
    }

    async fn marketplace_add_inner(
        &self,
        params: MarketplaceAddParams,
    ) -> Result<MarketplaceAddResponse, JSONRPCErrorError> {
        let config = self.load_latest_config(/*fallback_cwd*/ None).await?;
        add_marketplace_to_codex_home(
            self.config.codex_home.to_path_buf(),
            config.config_layer_stack.requirements().clone(),
            MarketplaceAddRequest {
                source: params.source,
                ref_name: params.ref_name,
                sparse_paths: params.sparse_paths.unwrap_or_default(),
            },
        )
        .await
        .map(|outcome| MarketplaceAddResponse {
            marketplace_name: outcome.marketplace_name,
            installed_root: outcome.installed_root,
            already_added: outcome.already_added,
        })
        .map_err(|err| match err {
            MarketplaceAddError::InvalidRequest(message) => invalid_request(message),
            MarketplaceAddError::Internal(message) => internal_error(message),
        })
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
}
