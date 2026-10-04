use std::sync::Arc;
use std::time::Duration;

use codex_api::AuthProvider;
use codex_api::SharedAuthProvider;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::HttpResponse;
use codex_http_client::RouteAwareClientPool;
use futures::FutureExt;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::StatusCode;
use serde::Deserialize;

use crate::EnvironmentRegistryConnectRequest;
use crate::EnvironmentRegistryConnectResponse;
use crate::ExecServerError;
use crate::NoiseChannelPublicKey;
use crate::NoiseRendezvousConnectBundle;
use crate::NoiseRendezvousConnectProvider;
use crate::client_api::DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT;

const ERROR_BODY_PREVIEW_BYTES: usize = 4096;
const NOISE_RELAY_SECURITY_PROFILE: &str = "noise_hybrid_ik_v1";

#[derive(Clone)]
struct EnvironmentRegistryClient {
    base_url: String,
    auth_provider: SharedAuthProvider,
    http: RouteAwareClientPool,
    connect_timeout: Duration,
}

impl std::fmt::Debug for EnvironmentRegistryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvironmentRegistryClient")
            .field("base_url", &self.base_url)
            .field("auth_provider", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl EnvironmentRegistryClient {
    #[cfg(test)]
    fn new(base_url: String, auth_provider: SharedAuthProvider) -> Result<Self, ExecServerError> {
        Self::new_with_options(
            base_url,
            auth_provider,
            HttpClientFactory::new(codex_http_client::OutboundProxyPolicy::ReqwestDefault),
        )
    }

    fn new_with_options(
        base_url: String,
        auth_provider: SharedAuthProvider,
        http_client_factory: HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        let base_url = normalize_base_url(base_url)?;
        Ok(Self {
            base_url,
            auth_provider,
            http: RouteAwareClientPool::new_without_redirects_or_request_logging(
                http_client_factory,
                ClientRouteClass::Api,
            ),
            connect_timeout: DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT,
        })
    }

    /// Authorize one Noise harness key and obtain the full rendezvous bundle.
    #[tracing::instrument(
        name = "codex.exec_server.remote.environment_registry.connect",
        skip_all,
        fields(
            otel.kind = "client",
            otel.name = "codex.exec_server.remote.environment_registry.connect",
            environment_id = %environment_id,
        )
    )]
    async fn connect_environment(
        &self,
        environment_id: &str,
        harness_public_key: NoiseChannelPublicKey,
    ) -> Result<NoiseRendezvousConnectBundle, ExecServerError> {
        let url = endpoint_url(
            &self.base_url,
            &format!("/cloud/environment/{environment_id}/connect"),
        );
        let body = EnvironmentRegistryConnectRequest { harness_public_key };
        let response = self
            .http
            .post(url)
            .headers(self.resolve_auth_headers().await?)
            .headers(http::HeaderMap::new())
            .json(&body)
            .timeout(self.connect_timeout)
            .send()
            .await?;
        let response: EnvironmentRegistryConnectResponse =
            self.parse_json_response(response).await?;
        if response.environment_id != environment_id {
            return Err(ExecServerError::Protocol(
                "environment registry returned a different environment id".to_string(),
            ));
        }
        if response.security_profile != NOISE_RELAY_SECURITY_PROFILE {
            return Err(ExecServerError::Protocol(format!(
                "environment registry returned unsupported security profile `{}`",
                response.security_profile
            )));
        }
        if response.url.trim().is_empty()
            || response.executor_registration_id.trim().is_empty()
            || response.harness_key_authorization.trim().is_empty()
        {
            return Err(ExecServerError::Protocol(
                "environment registry returned incomplete Noise connection data".to_string(),
            ));
        }
        Ok(NoiseRendezvousConnectBundle {
            websocket_url: response.url,
            environment_id: response.environment_id,
            executor_registration_id: response.executor_registration_id,
            executor_public_key: response.executor_public_key,
            harness_key_authorization: response.harness_key_authorization,
        })
    }

    async fn resolve_auth_headers(&self) -> Result<HeaderMap, ExecServerError> {
        self.auth_provider
            .resolve_auth_headers()
            .await
            .map_err(|error| {
                ExecServerError::EnvironmentRegistryAuth(format!(
                    "failed to resolve environment registry authentication: {error}"
                ))
            })
    }

    async fn parse_json_response<R>(&self, response: HttpResponse) -> Result<R, ExecServerError>
    where
        R: for<'de> Deserialize<'de>,
    {
        if response.status().is_success() {
            let body = response
                .text()
                .await
                .map_err(ExecServerError::EnvironmentRegistryRequest)?;
            return serde_json::from_str(&body).map_err(ExecServerError::Json);
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(environment_registry_auth_error(status, &body));
        }

        Err(environment_registry_http_error(status, &body))
    }
}

/// Noise connection configuration for a Codex harness.
///
/// Configuration stays inert until the effective outbound HTTP policy is known.
/// Its connection provider then holds the authenticated registry client so every
/// reconnect receives fresh URL and harness-key authorization material.
#[derive(Clone)]
pub(crate) struct NoiseRendezvousEnvironmentConfig {
    base_url: String,
    environment_id: String,
    auth_provider: SharedAuthProvider,
}

impl std::fmt::Debug for NoiseRendezvousEnvironmentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoiseRendezvousEnvironmentConfig")
            .field("base_url", &"<redacted>")
            .field("environment_id", &self.environment_id)
            .field("auth_provider", &"<redacted>")
            .finish()
    }
}

impl NoiseRendezvousEnvironmentConfig {
    pub(crate) fn new(
        base_url: String,
        environment_id: String,
        bearer_token: String,
        chatgpt_account_id: Option<String>,
    ) -> Result<Self, ExecServerError> {
        let base_url = normalize_base_url(base_url)?;
        let environment_id = normalize_environment_id(environment_id)?;
        let auth_provider = static_bearer_auth_provider(bearer_token, chatgpt_account_id)?;
        Ok(Self {
            base_url,
            environment_id,
            auth_provider,
        })
    }

    pub(crate) fn into_connect_provider(
        self,
        http_client_factory: HttpClientFactory,
    ) -> Result<Arc<dyn NoiseRendezvousConnectProvider>, ExecServerError> {
        let client = EnvironmentRegistryClient::new_with_options(
            self.base_url,
            self.auth_provider,
            http_client_factory,
        )?;
        Ok(Arc::new(EnvironmentRegistryNoiseConnectProvider {
            client,
            environment_id: self.environment_id,
        }))
    }
}

#[derive(Clone, Debug)]
struct EnvironmentRegistryNoiseConnectProvider {
    client: EnvironmentRegistryClient,
    environment_id: String,
}

impl NoiseRendezvousConnectProvider for EnvironmentRegistryNoiseConnectProvider {
    fn connect_bundle(
        &self,
        harness_public_key: NoiseChannelPublicKey,
    ) -> futures::future::BoxFuture<'_, Result<NoiseRendezvousConnectBundle, ExecServerError>> {
        async move {
            self.client
                .connect_environment(&self.environment_id, harness_public_key)
                .await
        }
        .boxed()
    }
}

#[derive(Clone)]
struct StaticBearerAuthProvider {
    authorization: HeaderValue,
    chatgpt_account_id: Option<HeaderValue>,
}

impl std::fmt::Debug for StaticBearerAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticBearerAuthProvider")
            .field("authorization", &"<redacted>")
            .field(
                "chatgpt_account_id",
                &self.chatgpt_account_id.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl AuthProvider for StaticBearerAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        headers.insert(http::header::AUTHORIZATION, self.authorization.clone());
        if let Some(chatgpt_account_id) = &self.chatgpt_account_id {
            headers.insert(
                HeaderName::from_static("chatgpt-account-id"),
                chatgpt_account_id.clone(),
            );
        }
    }
}

fn static_bearer_auth_provider(
    bearer_token: String,
    chatgpt_account_id: Option<String>,
) -> Result<SharedAuthProvider, ExecServerError> {
    let bearer_token = bearer_token.trim();
    if bearer_token.is_empty() {
        return Err(ExecServerError::EnvironmentRegistryConfig(
            "environment registry bearer token is required".to_string(),
        ));
    }
    let authorization =
        HeaderValue::try_from(format!("Bearer {bearer_token}")).map_err(|error| {
            ExecServerError::EnvironmentRegistryConfig(format!(
                "environment registry bearer token is not a valid HTTP header: {error}"
            ))
        })?;
    let chatgpt_account_id = chatgpt_account_id
        .as_deref()
        .map(str::trim)
        .filter(|account_id| !account_id.is_empty())
        .map(|account_id| {
            HeaderValue::try_from(account_id).map_err(|error| {
                ExecServerError::EnvironmentRegistryConfig(format!(
                    "ChatGPT account id is not a valid HTTP header: {error}"
                ))
            })
        })
        .transpose()?;
    Ok(Arc::new(StaticBearerAuthProvider {
        authorization,
        chatgpt_account_id,
    }))
}

fn normalize_environment_id(environment_id: String) -> Result<String, ExecServerError> {
    let environment_id = environment_id.trim().to_string();
    if environment_id.is_empty() {
        return Err(ExecServerError::EnvironmentRegistryConfig(
            "environment id is required for remote exec-server registration".to_string(),
        ));
    }
    Ok(environment_id)
}

#[derive(Deserialize)]
struct RegistryErrorBody {
    error: Option<RegistryError>,
    detail: Option<serde_json::Value>,
}

impl RegistryErrorBody {
    fn into_error(self) -> Option<RegistryError> {
        // Keep the canonical error authoritative when both envelopes are present.
        self.error
            .or_else(|| serde_json::from_value(self.detail?).ok())
    }
}

#[derive(Deserialize)]
struct RegistryError {
    code: Option<String>,
    message: Option<String>,
}

fn normalize_base_url(base_url: String) -> Result<String, ExecServerError> {
    let trimmed = base_url.trim().trim_end_matches('/').to_string();
    if trimmed.is_empty() {
        return Err(ExecServerError::EnvironmentRegistryConfig(
            "environment registry base URL is required".to_string(),
        ));
    }
    Ok(trimmed)
}

fn endpoint_url(base_url: &str, path: &str) -> String {
    format!("{base_url}/{}", path.trim_start_matches('/'))
}

fn environment_registry_auth_error(status: StatusCode, body: &str) -> ExecServerError {
    let message = registry_error_message(body).unwrap_or_else(|| "empty error body".to_string());
    ExecServerError::EnvironmentRegistryAuth(format!(
        "environment registry authentication failed ({status}): {message}"
    ))
}

fn environment_registry_http_error(status: StatusCode, body: &str) -> ExecServerError {
    let parsed = serde_json::from_str::<RegistryErrorBody>(body).ok();
    let (code, message) = parsed
        .and_then(RegistryErrorBody::into_error)
        .map(|error| {
            (
                error.code,
                error.message.unwrap_or_else(|| {
                    preview_error_body(body).unwrap_or_else(|| "empty error body".to_string())
                }),
            )
        })
        .unwrap_or_else(|| {
            (
                None,
                preview_error_body(body)
                    .unwrap_or_else(|| "empty or malformed error body".to_string()),
            )
        });
    ExecServerError::EnvironmentRegistryHttp {
        status,
        code,
        message,
    }
}

fn registry_error_message(body: &str) -> Option<String> {
    serde_json::from_str::<RegistryErrorBody>(body)
        .ok()
        .and_then(RegistryErrorBody::into_error)
        .and_then(|error| error.message)
        .or_else(|| preview_error_body(body))
}

fn preview_error_body(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(ERROR_BODY_PREVIEW_BYTES).collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codex_api::AuthProvider;
    use codex_http_client::OutboundProxyPolicy;
    use http::HeaderMap;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tracing::Instrument;
    use tracing_subscriber::prelude::*;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_partial_json;
    use wiremock::matchers::header;
    use wiremock::matchers::header_regex;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::*;

    #[derive(Debug)]
    struct StaticRegistryAuthProvider;

    impl AuthProvider for StaticRegistryAuthProvider {
        fn add_auth_headers(&self, _headers: &mut HeaderMap) {}

        fn resolve_auth_headers(&self) -> codex_api::AuthHeadersFuture<'_> {
            Box::pin(async {
                let mut headers = HeaderMap::new();
                let _ = headers.insert(
                    http::header::AUTHORIZATION,
                    HeaderValue::from_static("Bearer registry-token"),
                );
                let _ = headers.insert(
                    "ChatGPT-Account-ID",
                    HeaderValue::from_static("workspace-123"),
                );
                Ok(headers)
            })
        }
    }

    fn static_registry_auth_provider() -> SharedAuthProvider {
        Arc::new(StaticRegistryAuthProvider)
    }

    #[tokio::test]
    async fn noise_connect_provider_requests_and_validates_a_full_bundle() {
        let server = MockServer::start().await;
        let harness_public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();
        let executor_public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();
        Mock::given(method("POST"))
            .and(path("/cloud/environment/environment-requested/connect"))
            .and(header("authorization", "Bearer registry-token"))
            .and(header("chatgpt-account-id", "workspace-123"))
            .and(body_partial_json(serde_json::json!({
                "harness_public_key": harness_public_key.clone(),
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "environment_id": "environment-requested",
                "url": "wss://rendezvous.test/cloud-agent/default/ws/environment/environment-requested?role=harness&sig=abc",
                "security_profile": NOISE_RELAY_SECURITY_PROFILE,
                "executor_registration_id": "registration-1",
                "executor_public_key": executor_public_key.clone(),
                "harness_key_authorization": "authorization-1",
            })))
            .mount(&server)
            .await;
        let config = NoiseRendezvousEnvironmentConfig::new(
            server.uri(),
            "environment-requested".to_string(),
            "registry-token".to_string(),
            Some("workspace-123".to_string()),
        )
        .expect("noise configuration");

        let bundle = config
            .into_connect_provider(HttpClientFactory::new(
                codex_http_client::OutboundProxyPolicy::ReqwestDefault,
            ))
            .expect("Noise connect provider")
            .connect_bundle(harness_public_key)
            .await
            .expect("Noise connect bundle");

        assert_eq!(
            bundle.websocket_url,
            "wss://rendezvous.test/cloud-agent/default/ws/environment/environment-requested?role=harness&sig=abc"
        );
        assert_eq!(bundle.environment_id, "environment-requested");
        assert_eq!(bundle.executor_registration_id, "registration-1");
        assert_eq!(bundle.executor_public_key, executor_public_key);
        assert_eq!(bundle.harness_key_authorization, "authorization-1");
    }

    #[tokio::test]
    async fn connect_environment_times_out_when_registry_stalls() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cloud/environment/environment-requested/connect"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
            .mount(&server)
            .await;
        let mut client =
            EnvironmentRegistryClient::new(server.uri(), static_registry_auth_provider())
                .expect("client");
        client.connect_timeout = Duration::from_millis(50);
        let harness_public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();

        let error = match client
            .connect_environment("environment-requested", harness_public_key)
            .await
        {
            Ok(_) => panic!("stalled connect response should time out"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            ExecServerError::EnvironmentRegistryRequest(error) if error.is_timeout()
        ));
    }

    #[tokio::test]
    async fn connect_environment_times_out_when_registry_response_body_stalls() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("registry listener should bind");
        let registry_url = format!(
            "http://{}",
            listener
                .local_addr()
                .expect("registry listener should have an address")
        );
        tokio::spawn(async move {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("registry request should connect");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 256\r\n\r\n{",
                )
                .await
                .expect("registry response headers should write");
            sleep(Duration::from_secs(1)).await;
        });
        let mut client =
            EnvironmentRegistryClient::new(registry_url, static_registry_auth_provider())
                .expect("client");
        client.connect_timeout = Duration::from_millis(50);
        let harness_public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();

        let error = match client
            .connect_environment("environment-requested", harness_public_key)
            .await
        {
            Ok(_) => panic!("stalled connect response body should time out"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            ExecServerError::EnvironmentRegistryRequest(error) if error.is_timeout()
        ));
    }

    #[tokio::test]
    async fn connect_environment_retries_interrupted_registry_response_bodies() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("registry listener should bind");
        let registry_url = format!(
            "http://{}",
            listener
                .local_addr()
                .expect("registry listener should have an address")
        );
        tokio::spawn(async move {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("registry request should connect");
            let mut request = [0_u8; 4096];
            let bytes_read = stream
                .read(&mut request)
                .await
                .expect("registry request should arrive before the response");
            assert_ne!(bytes_read, 0, "registry request should not be empty");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 256\r\n\r\n{",
                )
                .await
                .expect("registry response headers should write");
            stream
                .shutdown()
                .await
                .expect("registry connection should close");
        });
        let client = EnvironmentRegistryClient::new(registry_url, static_registry_auth_provider())
            .expect("client");
        let harness_public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();

        let error = client
            .connect_environment("environment-requested", harness_public_key)
            .await
            .err()
            .expect("interrupted response body must fail");

        assert!(
            crate::client::is_retryable_registry_error(&error),
            "interrupted registry response body should be retryable: {error:?}"
        );
        assert!(matches!(
            error,
            ExecServerError::EnvironmentRegistryRequest(_)
        ));
    }

    #[tokio::test]
    async fn connect_environment_does_not_retry_malformed_successful_responses() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cloud/environment/environment-requested/connect"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{"))
            .mount(&server)
            .await;
        let client = EnvironmentRegistryClient::new(server.uri(), static_registry_auth_provider())
            .expect("client");
        let harness_public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();

        let error = client
            .connect_environment("environment-requested", harness_public_key)
            .await
            .err()
            .expect("malformed response must fail");

        assert!(!crate::client::is_retryable_registry_error(&error));
        assert!(matches!(error, ExecServerError::Json(_)));
    }
}
