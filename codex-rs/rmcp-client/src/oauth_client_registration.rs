use std::sync::Arc;

use anyhow::Result;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::AuthorizationRequest;
use rmcp::transport::AuthorizationSession;
use rmcp::transport::auth::OAuthHttpClient;
use rmcp::transport::auth::OAuthState;

use crate::oauth::validate_authorization_server_endpoints;
use crate::oauth_callback::McpOAuthCallbackMode;
use crate::oauth_callback::append_callback_id_to_redirect_uri;
use crate::oauth_callback::callback_mode;
use crate::oauth_callback::validate_callback_redirect;

/// OAuth client-registration strategy for one interactive HTTP MCP login.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpOAuthClientRegistration {
    /// Use the configured server's Dynamic Client Registration endpoint.
    #[default]
    Auto,
    /// Require the authorization server's Dynamic Client Registration endpoint.
    Dcr,
}

/// OAuth state prepared from one authorization-server metadata resolution.
pub(crate) struct PreparedOAuthLogin {
    pub(crate) oauth_state: OAuthState,
    pub(crate) authorization_server_issuer: Option<String>,
    pub(crate) redirect_uri: String,
}

pub(crate) async fn start_authorization(
    server_url: &str,
    http_client: Arc<dyn OAuthHttpClient>,
    scopes: &[&str],
    redirect_uri: &str,
    callback_id: &str,
    _client_registration: McpOAuthClientRegistration,
) -> Result<PreparedOAuthLogin> {
    let mut auth_manager =
        AuthorizationManager::new_with_oauth_http_client(server_url, http_client).await?;
    auth_manager.set_allow_missing_issuer(true);
    let metadata = auth_manager.resolve_metadata().await?.metadata;
    validate_authorization_server_endpoints(&metadata)?;
    let authorization_server_issuer = metadata.issuer.clone();
    let callback_mode = callback_mode(&metadata)?;

    let uses_shared_callback = callback_mode == McpOAuthCallbackMode::IssuerBound;
    let redirect_uri = if uses_shared_callback {
        redirect_uri.to_string()
    } else {
        append_callback_id_to_redirect_uri(redirect_uri, callback_id)?
    };
    validate_callback_redirect(&redirect_uri, callback_id, callback_mode)?;
    auth_manager.set_metadata(metadata);
    let request = AuthorizationRequest::new(redirect_uri.clone())
        .with_scopes(scopes.iter().copied())
        .with_client_name("Codex");
    let session = AuthorizationSession::new(auth_manager, request)
        .await
        .map_err(|(_auth_manager, error)| error)?;

    Ok(PreparedOAuthLogin {
        oauth_state: OAuthState::Session(session),
        authorization_server_issuer,
        redirect_uri,
    })
}

#[cfg(test)]
#[path = "oauth_client_registration_tests.rs"]
mod tests;
