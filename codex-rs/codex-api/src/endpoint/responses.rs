use crate::auth::SharedAuthProvider;
use crate::common::ResponseStream;
use crate::common::ResponsesApiRequest;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::Compression;
use crate::requests::headers::build_session_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use crate::sse::spawn_response_stream;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestCompression;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use std::sync::OnceLock;
use tracing::instrument;

pub struct ResponsesClient<T: HttpTransport> {
    session: EndpointSession<T>,
}

#[derive(Default)]
pub struct ResponsesOptions {
    /// OpenRouter-only strict upstream allowlist, ordered by preference.
    pub openrouter_providers: Vec<String>,
    /// OpenRouter-only inference ZDR requirement; None enforces true.
    pub openrouter_zdr: Option<bool>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub session_source: Option<SessionSource>,
    pub extra_headers: HeaderMap,
    pub compression: Compression,
    pub turn_state: Option<Arc<OnceLock<String>>>,
}

impl<T: HttpTransport> ResponsesClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
        }
    }

    #[instrument(
        name = "responses.stream_request",
        level = "info",
        skip_all,
        fields(
            transport = "responses_http",
            http.method = "POST",
            api.path = "/responses"
        )
    )]
    pub async fn stream_request(
        &self,
        mut request: ResponsesApiRequest,
        options: ResponsesOptions,
    ) -> Result<ResponseStream, ApiError> {
        let ResponsesOptions {
            openrouter_providers,
            openrouter_zdr,
            session_id,
            thread_id,
            session_source,
            extra_headers,
            compression,
            turn_state,
        } = options;
        let tool_names = if super::openrouter::is_openrouter(&self.session.provider().base_url) {
            Some(super::openrouter::prepare(&mut request)?)
        } else {
            None
        };
        let omit_tool_options = tool_names.as_ref().is_some_and(|names| names.is_empty());
        let zdr = tool_names.as_ref().map(|_| openrouter_zdr.unwrap_or(true));
        let body = encode_request(&request, &openrouter_providers, zdr, omit_tool_options)
            .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))?;

        let mut headers = extra_headers;
        if let Some(ref thread_id) = thread_id {
            insert_header(&mut headers, "x-client-request-id", thread_id);
        }
        headers.extend(build_session_headers(session_id, thread_id));
        if let Some(subagent) = subagent_header(&session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }

        let stream = self
            .stream_encoded(body, headers, compression, turn_state)
            .await?;
        Ok(match tool_names {
            Some(names) => super::openrouter::restore_stream(stream, names),
            None => stream,
        })
    }

    #[instrument(
        name = "responses.stream",
        level = "info",
        skip_all,
        fields(
            transport = "responses_http",
            http.method = "POST",
            api.path = "/responses",
            turn.has_state = turn_state.is_some()
        )
    )]
    pub async fn stream(
        &self,
        mut body: Value,
        extra_headers: HeaderMap,
        compression: Compression,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<ResponseStream, ApiError> {
        if super::openrouter::is_openrouter(&self.session.provider().base_url) {
            let fields = body
                .as_object_mut()
                .ok_or_else(|| ApiError::InvalidRequest {
                    message: "OpenRouter requests must be JSON objects".into(),
                })?;
            let routing = fields
                .entry("provider")
                .or_insert_with(|| serde_json::json!({}));
            let routing = routing
                .as_object_mut()
                .ok_or_else(|| ApiError::InvalidRequest {
                    message: "OpenRouter provider preferences must be a JSON object".into(),
                })?;
            let zdr = routing.entry("zdr").or_insert(Value::Bool(true));
            if !zdr.is_boolean() {
                return Err(ApiError::InvalidRequest {
                    message: "OpenRouter provider.zdr must be a boolean".into(),
                });
            }
        }
        let body = EncodedJsonBody::encode(&body)
            .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))?;
        self.stream_encoded(body, extra_headers, compression, turn_state)
            .await
    }

    async fn stream_encoded(
        &self,
        body: EncodedJsonBody,
        extra_headers: HeaderMap,
        compression: Compression,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<ResponseStream, ApiError> {
        let request_compression = match compression {
            Compression::None => RequestCompression::None,
            Compression::Zstd => RequestCompression::Zstd,
        };

        let stream_response = self
            .session
            .stream_encoded_json_with(
                Method::POST,
                "/responses",
                extra_headers,
                Some(body),
                |req| {
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("text/event-stream"),
                    );
                    req.compression = request_compression;
                },
            )
            .await?;

        Ok(spawn_response_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            turn_state,
        ))
    }
}

// Keep non-OpenRouter requests unchanged when routing is absent, including their
// serialization order. OpenRouter always sends ZDR, independently of its allowlist.
fn encode_request(
    request: &ResponsesApiRequest,
    providers: &[String],
    zdr: Option<bool>,
    omit_tool_options: bool,
) -> Result<EncodedJsonBody, serde_json::Error> {
    #[derive(Serialize)]
    struct Routing<'a> {
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        order: &'a [String],
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        only: &'a [String],
        #[serde(skip_serializing_if = "Option::is_none")]
        zdr: Option<bool>,
    }
    #[derive(Serialize)]
    struct RoutedRequest<'a> {
        #[serde(flatten)]
        request: &'a ResponsesApiRequest,
        provider: Routing<'a>,
    }
    if omit_tool_options {
        // Compaction and other tool-free requests must not ask upstream providers
        // to select tools. In particular Cerebras rejects tool_choice without tools.
        let mut body = serde_json::to_value(request)?;
        if let Some(fields) = body.as_object_mut() {
            fields.remove("tool_choice");
            fields.remove("parallel_tool_calls");
        }
        if !providers.is_empty() || zdr.is_some() {
            body["provider"] = serde_json::to_value(Routing {
                order: providers,
                only: providers,
                zdr,
            })?;
        }
        EncodedJsonBody::encode(&body)
    } else if providers.is_empty() && zdr.is_none() {
        EncodedJsonBody::encode(request)
    } else {
        EncodedJsonBody::encode(&RoutedRequest {
            request,
            provider: Routing {
                order: providers,
                only: providers,
                zdr,
            },
        })
    }
}
