#![allow(clippy::expect_used)]
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use codex_api::ApiError;
use codex_api::AuthError;
use codex_api::AuthProvider;
use codex_api::Compression;
use codex_api::Provider;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesClient;
use codex_api::ResponsesOptions;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::RequestBody;
use codex_client::Response;
use codex_client::StreamResponse;
use codex_client::TransportError;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use http::HeaderMap;
use http::HeaderValue;
use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::value::RawValue;

fn assert_path_ends_with(requests: &[Request], suffix: &str) {
    assert_eq!(requests.len(), 1);
    let url = &requests[0].url;
    assert!(
        url.ends_with(suffix),
        "expected url to end with {suffix}, got {url}"
    );
}

fn empty_tools() -> Arc<RawValue> {
    Arc::from(RawValue::from_string("[]".to_string()).expect("valid tool JSON"))
}

fn request_body_bytes(request: &Request) -> &[u8] {
    let Some(RequestBody::EncodedJson(body)) = request.body.as_ref() else {
        panic!("expected a prepared request body");
    };
    body.as_bytes()
}

#[derive(Debug, Default, Clone)]
struct RecordingState {
    stream_requests: Arc<Mutex<Vec<Request>>>,
}

impl RecordingState {
    fn record(&self, req: Request) {
        let mut guard = self
            .stream_requests
            .lock()
            .expect("stream requests mutex should not be poisoned");
        guard.push(req);
    }

    fn take_stream_requests(&self) -> Vec<Request> {
        let mut guard = self
            .stream_requests
            .lock()
            .expect("stream requests mutex should not be poisoned");
        std::mem::take(&mut *guard)
    }
}

#[derive(Clone)]
struct RecordingTransport {
    state: RecordingState,
}

impl RecordingTransport {
    fn new(state: RecordingState) -> Self {
        Self { state }
    }
}

impl HttpTransport for RecordingTransport {
    async fn execute(&self, _req: Request) -> Result<Response, TransportError> {
        Err(TransportError::Build("execute should not run".to_string()))
    }

    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError> {
        self.state.record(req);

        let stream = futures::stream::iter(Vec::<Result<Bytes, TransportError>>::new());
        Ok(StreamResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            bytes: Box::pin(stream),
        })
    }
}

#[derive(Clone, Default)]
struct NoAuth;

impl AuthProvider for NoAuth {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) {}
}

#[derive(Clone)]
struct StaticAuth {
    token: String,
    account_id: String,
}

impl StaticAuth {
    fn new(token: &str, account_id: &str) -> Self {
        Self {
            token: token.to_string(),
            account_id: account_id.to_string(),
        }
    }
}

impl AuthProvider for StaticAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        let token = &self.token;
        if let Ok(header) = HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert(http::header::AUTHORIZATION, header);
        }
        if let Ok(header) = HeaderValue::from_str(&self.account_id) {
            headers.insert("ChatGPT-Account-ID", header);
        }
    }
}

fn provider(name: &str) -> Provider {
    Provider {
        name: name.to_string(),
        base_url: "https://example.com/v1".to_string(),
        query_params: None,
        headers: HeaderMap::new(),
        retry: codex_api::RetryConfig {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            retry_429: false,
            retry_5xx: false,
            retry_transport: true,
        },
        stream_idle_timeout: Duration::from_millis(10),
    }
}

#[derive(Debug, Default)]
struct FlakyTransportState {
    attempts: i64,
    requests: Vec<(RequestBody, HeaderMap, codex_client::RequestCompression)>,
}

#[derive(Clone)]
struct FlakyTransport {
    state: Arc<Mutex<FlakyTransportState>>,
}

impl Default for FlakyTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl FlakyTransport {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FlakyTransportState::default())),
        }
    }

    fn attempts(&self) -> i64 {
        self.state
            .lock()
            .expect("flaky transport state mutex should not be poisoned")
            .attempts
    }

    fn requests(&self) -> Vec<(RequestBody, HeaderMap, codex_client::RequestCompression)> {
        self.state
            .lock()
            .expect("flaky transport state mutex should not be poisoned")
            .requests
            .clone()
    }
}

#[derive(Clone)]
struct FailsOnceAuth {
    attempts: Arc<Mutex<i64>>,
    error: Arc<AuthError>,
}

impl FailsOnceAuth {
    fn transient() -> Self {
        Self {
            attempts: Arc::new(Mutex::new(0)),
            error: Arc::new(AuthError::Transient(
                "sts temporarily unavailable".to_string(),
            )),
        }
    }

    fn build() -> Self {
        Self {
            attempts: Arc::new(Mutex::new(0)),
            error: Arc::new(AuthError::Build("invalid auth configuration".to_string())),
        }
    }

    fn attempts(&self) -> i64 {
        *self
            .attempts
            .lock()
            .expect("auth attempts mutex should not be poisoned")
    }

    async fn apply_auth(&self, request: Request) -> Result<Request, AuthError> {
        let mut attempts = self
            .attempts
            .lock()
            .expect("auth attempts mutex should not be poisoned");
        *attempts += 1;

        if *attempts == 1 {
            return match self.error.as_ref() {
                AuthError::Build(message) => Err(AuthError::Build(message.clone())),
                AuthError::Transient(message) => Err(AuthError::Transient(message.clone())),
            };
        }

        Ok(request)
    }
}

impl AuthProvider for FailsOnceAuth {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) {}

    fn apply_auth(&self, request: Request) -> codex_api::AuthProviderFuture<'_> {
        Box::pin(FailsOnceAuth::apply_auth(self, request))
    }
}

impl HttpTransport for FlakyTransport {
    async fn execute(&self, _req: Request) -> Result<Response, TransportError> {
        Err(TransportError::Build("execute should not run".to_string()))
    }

    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError> {
        let Some(body) = req.body.clone() else {
            panic!("request should have a body");
        };
        let mut state = self
            .state
            .lock()
            .expect("flaky transport state mutex should not be poisoned");
        state.attempts += 1;
        state
            .requests
            .push((body, req.headers.clone(), req.compression));

        if state.attempts == 1 {
            return Err(TransportError::Network("first attempt fails".to_string()));
        }

        let stream = futures::stream::iter(vec![Ok(Bytes::from(
            r#"event: message
data: {"id":"resp-1","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}]}

"#,
        ))]);

        Ok(StreamResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            bytes: Box::pin(stream),
        })
    }
}

#[tokio::test]
async fn responses_client_uses_responses_path() -> Result<()> {
    let state = RecordingState::default();
    let transport = RecordingTransport::new(state.clone());
    let client = ResponsesClient::new(transport, provider("openai"), Arc::new(NoAuth));

    let body = serde_json::json!({ "echo": true });
    let _stream = client
        .stream(
            body,
            HeaderMap::new(),
            Compression::None,
            /*turn_state*/ None,
        )
        .await?;

    let requests = state.take_stream_requests();
    assert_path_ends_with(&requests, "/responses");
    Ok(())
}

#[tokio::test]
async fn responses_client_sends_extra_headers() -> Result<()> {
    let state = RecordingState::default();
    let transport = RecordingTransport::new(state.clone());
    let client = ResponsesClient::new(transport, provider("openai"), Arc::new(NoAuth));
    let headers = HeaderMap::from_iter([(
        http::HeaderName::from_static("x-custom-request"),
        HeaderValue::from_static("example"),
    )]);
    let _stream = client
        .stream(
            serde_json::json!({ "echo": true }),
            headers,
            Compression::None,
            /*turn_state*/ None,
        )
        .await?;
    let requests = state.take_stream_requests();
    assert_path_ends_with(&requests, "/responses");
    assert_eq!(
        requests[0].headers.get("x-custom-request"),
        Some(&HeaderValue::from_static("example")),
    );
    Ok(())
}

#[tokio::test]
async fn responses_client_stream_request_preserves_item_ids() -> Result<()> {
    let state = RecordingState::default();
    let transport = RecordingTransport::new(state.clone());
    let client = ResponsesClient::new(transport, provider("openai"), Arc::new(NoAuth));
    let request = ResponsesApiRequest {
        model: "gpt-test".into(),
        instructions: "Say hi".into(),
        input: vec![ResponseItem::Message {
            id: Some(ResponseItemId::with_suffix("msg", "1")),
            role: "user".into(),
            content: vec![ContentItem::InputText { text: "hi".into() }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        tools: Some(empty_tools().into()),
        tool_choice: "auto".into(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        access_programs: None,
    };
    let expected = serde_json::to_value(&request)?;

    let _stream = client
        .stream_request(request, ResponsesOptions::default())
        .await?;

    let requests = state.take_stream_requests();
    assert_eq!(requests.len(), 1);
    let prepared = requests[0]
        .prepare_body_for_send()
        .expect("body should prepare");
    let body: serde_json::Value =
        serde_json::from_slice(prepared.body.as_deref().expect("body should be JSON"))?;
    assert_eq!(body, expected);
    assert_eq!(body["input"][0]["id"], "msg_1");
    assert_eq!(body.get("service_tier"), None);
    assert_eq!(
        prepared.headers.get(http::header::CONTENT_TYPE),
        Some(&HeaderValue::from_static("application/json"))
    );
    Ok(())
}

#[tokio::test]
async fn responses_client_stream_request_sends_routing_fields_ahead_of_large_input() -> Result<()> {
    let state = RecordingState::default();
    let transport = RecordingTransport::new(state.clone());
    let client = ResponsesClient::new(transport, provider("openai"), Arc::new(NoAuth));
    // Exercise the customer gateway case where routing fields must be available without
    // buffering a potentially multi-megabyte prompt first.
    let large_input = "x".repeat(2 * 1024 * 1024);
    let request = ResponsesApiRequest {
        model: "gpt-test".into(),
        instructions: "Say hi".into(),
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText { text: large_input }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        tools: None,
        tool_choice: "auto".into(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: Some("priority".into()),
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        access_programs: None,
    };
    let expected = serde_json::to_value(&request)?;

    let _stream = client
        .stream_request(request, ResponsesOptions::default())
        .await?;

    let requests = state.take_stream_requests();
    assert_eq!(requests.len(), 1);
    let body = std::str::from_utf8(request_body_bytes(&requests[0]))?;
    let input_position = body.find(r#""input":"#).expect("input should be present");
    for routing_field in [r#""model":"#, r#""stream":"#, r#""service_tier":"#] {
        assert!(
            body.find(routing_field)
                .is_some_and(|position| position < input_position),
            "{routing_field} should precede input"
        );
    }
    assert!(body.starts_with(
        r#"{"model":"gpt-test","stream":true,"service_tier":"priority","instructions":"Say hi","input":[{"type":"message""#
    ));
    assert!(body.len() > 2 * 1024 * 1024);
    assert_eq!(serde_json::from_str::<serde_json::Value>(body)?, expected);
    Ok(())
}

#[tokio::test]
async fn streaming_client_adds_auth_headers() -> Result<()> {
    let state = RecordingState::default();
    let transport = RecordingTransport::new(state.clone());
    let auth = Arc::new(StaticAuth::new("secret-token", "acct-1"));
    let client = ResponsesClient::new(transport, provider("openai"), auth);

    let body = serde_json::json!({ "model": "gpt-test" });
    let _stream = client
        .stream(
            body,
            HeaderMap::new(),
            Compression::None,
            /*turn_state*/ None,
        )
        .await?;

    let requests = state.take_stream_requests();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];

    let auth_header = req.headers.get(http::header::AUTHORIZATION);
    assert!(auth_header.is_some(), "missing auth header");
    assert_eq!(
        auth_header.unwrap().to_str().ok(),
        Some("Bearer secret-token")
    );

    let account_header = req.headers.get("ChatGPT-Account-ID");
    assert!(account_header.is_some(), "missing account header");
    assert_eq!(account_header.unwrap().to_str().ok(), Some("acct-1"));

    let accept_header = req.headers.get(http::header::ACCEPT);
    assert!(accept_header.is_some(), "missing Accept header");
    assert_eq!(
        accept_header.unwrap().to_str().ok(),
        Some("text/event-stream")
    );
    Ok(())
}

#[tokio::test]
async fn streaming_client_retries_on_transport_error() -> Result<()> {
    let transport = FlakyTransport::new();

    let mut provider = provider("openai");
    provider.retry.max_attempts = 2;

    let request = ResponsesApiRequest {
        model: "gpt-test".into(),
        instructions: "Say hi".into(),
        input: Vec::new(),
        tools: Some(empty_tools().into()),
        tool_choice: "auto".into(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        access_programs: None,
    };
    let client = ResponsesClient::new(transport.clone(), provider, Arc::new(NoAuth));

    let _stream = client
        .stream_request(
            request,
            ResponsesOptions {
                compression: Compression::Zstd,
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(transport.attempts(), 2);
    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    let RequestBody::EncodedJson(first_body) = &requests[0].0 else {
        panic!("expected an encoded JSON body");
    };
    let RequestBody::EncodedJson(second_body) = &requests[1].0 else {
        panic!("expected an encoded JSON body");
    };
    assert_eq!(
        first_body.as_bytes().as_ptr(),
        second_body.as_bytes().as_ptr()
    );
    assert_eq!(
        requests[0].1.get(http::header::CONTENT_ENCODING),
        Some(&HeaderValue::from_static("zstd"))
    );
    assert_eq!(requests[0].2, codex_client::RequestCompression::None);
    Ok(())
}

#[tokio::test]
async fn streaming_client_retries_on_transient_auth_error() -> Result<()> {
    let state = RecordingState::default();
    let transport = RecordingTransport::new(state.clone());
    let auth = FailsOnceAuth::transient();

    let mut provider = provider("openai");
    provider.retry.max_attempts = 2;

    let client = ResponsesClient::new(transport, provider, Arc::new(auth.clone()));
    let body = serde_json::json!({ "model": "gpt-test" });
    let _stream = client
        .stream(
            body,
            HeaderMap::new(),
            Compression::None,
            /*turn_state*/ None,
        )
        .await?;

    assert_eq!(auth.attempts(), 2);
    assert_eq!(state.take_stream_requests().len(), 1);
    Ok(())
}

#[tokio::test]
async fn streaming_client_does_not_retry_auth_build_error() -> Result<()> {
    let state = RecordingState::default();
    let transport = RecordingTransport::new(state.clone());
    let auth = FailsOnceAuth::build();

    let mut provider = provider("openai");
    provider.retry.max_attempts = 2;

    let client = ResponsesClient::new(transport, provider, Arc::new(auth.clone()));
    let body = serde_json::json!({ "model": "gpt-test" });
    let result = client
        .stream(
            body,
            HeaderMap::new(),
            Compression::None,
            /*turn_state*/ None,
        )
        .await;
    let err = result
        .err()
        .expect("auth build errors should fail without retry");

    assert!(matches!(
        err,
        ApiError::Transport(TransportError::Build(message))
            if message == "invalid auth configuration"
    ));
    assert_eq!(auth.attempts(), 1);
    assert_eq!(state.take_stream_requests().len(), 0);
    Ok(())
}

#[tokio::test]
async fn azure_store_sends_ids_and_headers() -> Result<()> {
    let state = RecordingState::default();
    let transport = RecordingTransport::new(state.clone());
    let client = ResponsesClient::new(transport, provider("azure"), Arc::new(NoAuth));

    let request = ResponsesApiRequest {
        model: "gpt-test".into(),
        instructions: "Say hi".into(),
        input: vec![ResponseItem::Message {
            id: Some(ResponseItemId::with_suffix("msg", "1")),
            role: "user".into(),
            content: vec![ContentItem::InputText { text: "hi".into() }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        tools: Some(empty_tools().into()),
        tool_choice: "auto".into(),
        parallel_tool_calls: false,
        reasoning: None,
        store: true,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        access_programs: None,
    };

    let mut extra_headers = HeaderMap::new();
    extra_headers.insert("x-test-header", HeaderValue::from_static("present"));
    let _stream = client
        .stream_request(
            request,
            ResponsesOptions {
                session_id: Some("sess_123".into()),
                openrouter_providers: Vec::new(),
                openrouter_zdr: None,
                thread_id: Some("thread_123".into()),
                session_source: Some(SessionSource::SubAgent(SubAgentSource::Review)),
                extra_headers,
                compression: Compression::None,
                turn_state: None,
            },
        )
        .await?;

    let requests = state.take_stream_requests();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];

    assert_eq!(
        req.headers.get("session-id").and_then(|v| v.to_str().ok()),
        Some("sess_123")
    );
    assert_eq!(
        req.headers.get("thread-id").and_then(|v| v.to_str().ok()),
        Some("thread_123")
    );
    assert_eq!(
        req.headers
            .get("x-client-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("thread_123")
    );
    assert_eq!(
        req.headers
            .get("x-openai-subagent")
            .and_then(|v| v.to_str().ok()),
        Some("review")
    );
    assert_eq!(
        req.headers
            .get("x-test-header")
            .and_then(|v| v.to_str().ok()),
        Some("present")
    );

    let body: serde_json::Value = serde_json::from_slice(request_body_bytes(req))?;
    let input_id = body
        .get("input")
        .and_then(|input| input.get(0))
        .and_then(|item| item.get("id"))
        .and_then(|id| id.as_str());
    assert_eq!(input_id, Some("msg_1"));

    Ok(())
}

#[tokio::test]
async fn openrouter_allowlist_is_optional_and_preserves_provider_order() -> Result<()> {
    let state = RecordingState::default();
    let client = ResponsesClient::new(
        RecordingTransport::new(state.clone()),
        provider("openrouter"),
        Arc::new(NoAuth),
    );
    let request = ResponsesApiRequest {
        model: "qwen/qwen3.8-27b".into(),
        instructions: "Say hi".into(),
        input: vec![],
        tools: Some(empty_tools().into()),
        tool_choice: "auto".into(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: vec![],
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        access_programs: None,
    };
    for providers in [
        vec![],
        vec!["cerebras".to_owned()],
        vec!["cerebras".to_owned(), "deepinfra".to_owned()],
    ] {
        let _stream = client
            .stream_request(
                request.clone(),
                ResponsesOptions {
                    openrouter_providers: providers.clone(),
                    ..Default::default()
                },
            )
            .await?;
        let requests = state.take_stream_requests();
        assert_path_ends_with(&requests, "/responses");
        let bytes = request_body_bytes(&requests[0]);
        let body: serde_json::Value = serde_json::from_slice(bytes)?;
        if providers.is_empty() {
            assert_eq!(bytes, serde_json::to_vec(&request)?);
            assert!(body.get("provider").is_none());
        } else {
            assert_eq!(
                body["provider"],
                serde_json::json!({"order":providers,"only":providers})
            );
        }
        assert_eq!(body["model"], "qwen/qwen3.8-27b");
        assert_eq!(body["store"], false);
        assert!(body.get("previous_response_id").is_none());
    }
    let mut request = request;
    request.tools = Some(Arc::<RawValue>::from(serde_json::value::to_raw_value(&serde_json::json!([
        {"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","parameters":{"type":"object"}}]}
    ]))?).into());
    request.input = serde_json::from_value(serde_json::json!([
        {"type":"function_call","call_id":"a","name":"spawn_agent","namespace":"collaboration","arguments":"{}"},
        {"type":"function_call_output","call_id":"a","output":"child started"},
        {"type":"agent_message","author":"/root/child","recipient":"/root","content":[{"type":"input_text","text":"child-ok"}]}
    ]))?;
    let mut openrouter = provider("openrouter");
    openrouter.base_url = "https://openrouter.ai/api/v1".into();
    let client = ResponsesClient::new(
        RecordingTransport::new(state.clone()),
        openrouter,
        Arc::new(NoAuth),
    );
    let _stream = client
        .stream_request(request.clone(), ResponsesOptions::default())
        .await?;
    let requests = state.take_stream_requests();
    let body: serde_json::Value = serde_json::from_slice(request_body_bytes(&requests[0]))?;
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "collaboration__spawn_agent");
    assert_eq!(body["input"][0]["name"], "collaboration__spawn_agent");
    assert!(body["input"][0].get("namespace").is_none());
    assert_eq!(body["input"][1]["call_id"], "a");
    assert_eq!(body["input"][2]["role"], "user");
    assert!(
        body["input"][2]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("child-ok")
    );
    assert!(
        matches!(&request.input[0], ResponseItem::FunctionCall { namespace: Some(ns), .. } if ns == "collaboration")
    );
    request.tools = None;
    request.input.clear();
    let _stream = client
        .stream_request(
            request,
            ResponsesOptions {
                openrouter_providers: vec!["cerebras".into()],
                ..Default::default()
            },
        )
        .await?;
    let requests = state.take_stream_requests();
    let body: serde_json::Value = serde_json::from_slice(request_body_bytes(&requests[0]))?;
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("parallel_tool_calls").is_none());
    assert_eq!(body["provider"]["only"], serde_json::json!(["cerebras"]));
    Ok(())
}

fn openrouter_zdr_request(with_tools: bool) -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "test-model".into(),
        instructions: "Say hi".into(),
        input: vec![],
        tools: with_tools.then(|| {
            Arc::<RawValue>::from(serde_json::value::to_raw_value(
            &serde_json::json!([{"type":"function","name":"read","parameters":{"type":"object"}}])
        ).unwrap()).into()
        }),
        tool_choice: "auto".into(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: vec![],
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        access_programs: None,
    }
}

#[tokio::test]
async fn openrouter_zdr_defaults_true_and_combines_with_optional_allowlists() -> Result<()> {
    for host in ["openrouter.ai", "eu.openrouter.ai", "us.openrouter.ai"] {
        let state = RecordingState::default();
        let mut endpoint = provider("OpenRouter");
        endpoint.base_url = format!("https://{host}/api/v1");
        let client = ResponsesClient::new(
            RecordingTransport::new(state.clone()),
            endpoint,
            Arc::new(NoAuth),
        );
        // Return to the default on the same client after an explicit opt-out.
        for zdr in [None, Some(true), Some(false), None] {
            for providers in [
                vec![],
                vec!["together".into()],
                vec!["cerebras/fp16".into(), "deepinfra".into()],
            ] {
                for with_tools in [false, true] {
                    let request = openrouter_zdr_request(with_tools);
                    let _stream = client
                        .stream_request(
                            request,
                            ResponsesOptions {
                                openrouter_zdr: zdr,
                                openrouter_providers: providers.clone(),
                                session_source: Some(SessionSource::SubAgent(
                                    SubAgentSource::Review,
                                )),
                                ..Default::default()
                            },
                        )
                        .await?;
                    let requests = state.take_stream_requests();
                    assert_path_ends_with(&requests, "/responses");
                    let body: serde_json::Value =
                        serde_json::from_slice(request_body_bytes(&requests[0]))?;
                    assert_eq!(body["provider"]["zdr"], zdr.unwrap_or(true), "{host}");
                    if providers.is_empty() {
                        assert_eq!(
                            body["provider"],
                            serde_json::json!({"zdr": zdr.unwrap_or(true)})
                        );
                    } else {
                        assert_eq!(
                            body["provider"],
                            serde_json::json!({"zdr": zdr.unwrap_or(true), "order": providers, "only": providers})
                        );
                    }
                    assert_eq!(body.get("tool_choice").is_some(), with_tools);
                    assert_eq!(body.get("parallel_tool_calls").is_some(), with_tools);
                    assert_eq!(body["store"], false);
                    assert!(body.get("previous_response_id").is_none());
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn openrouter_zdr_does_not_change_other_provider_payloads() -> Result<()> {
    for base_url in [
        "https://api.isoquant.ai/v1",
        "https://openrouter.ai.example.com/api/v1",
    ] {
        let state = RecordingState::default();
        let mut endpoint = provider("Custom");
        endpoint.base_url = base_url.into();
        endpoint
            .headers
            .insert("Isoquant-ZDR", HeaderValue::from_static("required"));
        let client = ResponsesClient::new(
            RecordingTransport::new(state.clone()),
            endpoint,
            Arc::new(NoAuth),
        );
        for zdr in [None, Some(true), Some(false)] {
            for with_tools in [false, true] {
                let request = openrouter_zdr_request(with_tools);
                let expected = serde_json::to_vec(&request)?;
                let _stream = client
                    .stream_request(
                        request,
                        ResponsesOptions {
                            openrouter_zdr: zdr,
                            ..Default::default()
                        },
                    )
                    .await?;
                let requests = state.take_stream_requests();
                assert_eq!(request_body_bytes(&requests[0]), expected);
                assert_eq!(
                    requests[0].headers.get("Isoquant-ZDR"),
                    Some(&HeaderValue::from_static("required"))
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn openrouter_zdr_and_allowlist_are_preserved_across_retries() -> Result<()> {
    for zdr in [None, Some(true), Some(false)] {
        let transport = FlakyTransport::new();
        let mut endpoint = provider("OpenRouter");
        endpoint.base_url = "https://openrouter.ai/api/v1".into();
        endpoint.retry.max_attempts = 2;
        let client = ResponsesClient::new(transport.clone(), endpoint, Arc::new(NoAuth));
        let _stream = client
            .stream_request(
                openrouter_zdr_request(true),
                ResponsesOptions {
                    openrouter_zdr: zdr,
                    openrouter_providers: vec!["together".into()],
                    ..Default::default()
                },
            )
            .await?;
        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        let RequestBody::EncodedJson(encoded) = &requests[0].0 else {
            panic!("expected JSON")
        };
        let body: serde_json::Value = serde_json::from_slice(encoded.as_bytes())?;
        assert_eq!(
            body["provider"],
            serde_json::json!({"zdr": zdr.unwrap_or(true), "order": ["together"], "only": ["together"]})
        );
    }
    Ok(())
}

#[tokio::test]
async fn openrouter_raw_requests_enforce_zdr_default_without_overriding_explicit_false()
-> Result<()> {
    let state = RecordingState::default();
    let mut endpoint = provider("OpenRouter");
    endpoint.base_url = "https://openrouter.ai/api/v1".into();
    let client = ResponsesClient::new(
        RecordingTransport::new(state.clone()),
        endpoint,
        Arc::new(NoAuth),
    );
    for routing in [
        None,
        Some(serde_json::json!({"only": ["together"]})),
        Some(serde_json::json!({"zdr": false})),
    ] {
        let mut request = serde_json::json!({"model": "uncatalogued", "input": "test"});
        if let Some(routing) = routing {
            request["provider"] = routing;
        }
        let expected_zdr = request["provider"]["zdr"].as_bool().unwrap_or(true);
        let _stream = client
            .stream(request.clone(), HeaderMap::new(), Compression::None, None)
            .await?;
        let requests = state.take_stream_requests();
        let body: serde_json::Value = serde_json::from_slice(request_body_bytes(&requests[0]))?;
        assert_eq!(body["provider"]["zdr"], expected_zdr);
        assert_eq!(body["provider"]["only"], request["provider"]["only"]);
    }
    for request in [
        serde_json::json!({"provider": null}),
        serde_json::json!({"provider":{"zdr":"false"}}),
    ] {
        assert!(matches!(
            client
                .stream(request, HeaderMap::new(), Compression::None, None)
                .await,
            Err(ApiError::InvalidRequest { .. })
        ));
        assert!(state.take_stream_requests().is_empty());
    }
    Ok(())
}
