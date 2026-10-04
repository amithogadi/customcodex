use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::app_server_json_shutdown_event;
use app_test_support::create_exec_command_sse_response;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::LoginAccountResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use codex_state::LogQuery;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use tempfile::TempDir;
use test_case::test_case;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn credentials_stay_out_of_persisted_logs() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let bearer = "synthetic-provider-bearer";
    let header = "synthetic-provider-header";
    let api_key = "synthetic-api-key";
    let server = MockServer::start().await;
    let success = responses::sse_response(create_final_assistant_message_sse_response("done")?);
    let responses =
        responses::mount_response_sequence(&server, vec![success.clone(), success]).await;
    let codex_home = TempDir::new()?;
    let server_uri = server.uri();
    MockResponsesConfig::new(&server_uri)
        .with_provider_config("requires_openai_auth = true\nsupports_websockets = false")
        .with_extra_config(&format!(
            r#"
[model_providers.bearer_provider]
name = "Bearer provider"
base_url = "{server_uri}/v1"
experimental_bearer_token = "{bearer}"
http_headers = {{ X-Credential = "{header}" }}
supports_websockets = false
"#
        ))
        .write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized()
        .await?;
    let login_id = app_server
        .send_login_account_api_key_request(api_key)
        .await?;
    let _: LoginAccountResponse = app_server.read_response(login_id).await?;

    for provider in ["bearer_provider", "mock_provider"] {
        let thread = app_server
            .start_thread(ThreadStartParams {
                model_provider: Some(provider.into()),
                ..Default::default()
            })
            .await?
            .thread;
        app_server
            .send_turn_start_request(TurnStartParams {
                thread_id: thread.id,
                input: vec![UserInput::Text {
                    text: "hello".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })
            .await?;
        let completed = timeout(
            Duration::from_secs(/*secs*/ 60),
            app_server.read_stream_until_notification_message("turn/completed"),
        )
        .await??;
        assert_eq!(
            completed.params.context("missing turn/completed params")?["turn"]["status"],
            "completed"
        );
    }
    let requests = responses.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.header("authorization"))
            .collect::<Vec<_>>(),
        vec![
            Some(format!("Bearer {bearer}")),
            Some(format!("Bearer {api_key}"))
        ]
    );
    assert_eq!(requests[0].header("x-credential").as_deref(), Some(header));

    // A later event proves queued request logs were persisted before checking for leaks.
    let barrier = "credential-log-barrier";
    app_server
        .send_response(RequestId::String(barrier.into()), json!({}))
        .await?;
    let state = StateRuntime::init(
        SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".into(),
    )
    .await?;
    let logs = timeout(Duration::from_secs(/*secs*/ 60), async {
        loop {
            let logs = format!("{:?}", state.query_logs(&LogQuery::default()).await?);
            if logs.contains(barrier) {
                break Ok::<_, anyhow::Error>(logs);
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
        }
    })
    .await??;
    state.close().await;
    for secret in [bearer, header, api_key] {
        anyhow::ensure!(
            !logs.contains(secret),
            "credential leaked into SQLite logs: {secret}"
        );
    }
    Ok(())
}

#[test]
fn standalone_app_server_emits_json_info_events() -> Result<()> {
    let codex_home = TempDir::new()?;
    let event = app_server_json_shutdown_event("codex-app-server", &[], codex_home.path())?;

    assert_eq!(
        event,
        json!({
            "level": "INFO",
            "fields": {
                "message": "processor task exited",
                "exit_reason": "stdio_connection_closed",
                "remaining_connection_count": 0,
                "shutdown_forced": false,
            },
            "target": "codex_app_server",
        })
    );

    Ok(())
}

#[tokio::test]
async fn app_server_emits_structured_tool_call_timing_event() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = create_mock_responses_server_sequence(vec![
        create_exec_command_sse_response("exec-call-1")?,
        create_final_assistant_message_sse_response("done")?,
    ])
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::UnifiedExec)
        .with_root_config("compact_prompt = \"compact\"\nmodel_auto_compact_token_limit = 100000")
        .with_provider_config("supports_websockets = false")
        .write(codex_home.path())?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_json_logging("warn,codex_core::tools::parallel=info")
        .build_initialized()
        .await?;

    let thread = app_server
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?
        .thread;

    let TurnStartResponse { turn } = app_server
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "run a command".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;

    timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let mut tool_call = app_server
        .wait_for_json_log_event("codex.tool_call")
        .await?;
    let tool_call_object = tool_call
        .as_object_mut()
        .context("tool call log event must be an object")?;
    // JsonLogCapture already validates the timestamp as RFC 3339.
    tool_call_object
        .remove("timestamp")
        .context("tool call log event must include a timestamp")?;
    let fields = tool_call_object
        .get_mut("fields")
        .and_then(Value::as_object_mut)
        .context("tool call log event fields must be an object")?;
    let dispatch_duration_ms = fields
        .remove("dispatch_duration_ms")
        .and_then(|duration| duration.as_u64())
        .context("dispatch_duration_ms must be a nonnegative integer")?;
    let handler_duration_ms = fields
        .remove("handler_duration_ms")
        .and_then(|duration| duration.as_u64())
        .context("handler_duration_ms must be a nonnegative integer")?;
    let total_duration_ms = fields
        .remove("total_duration_ms")
        .and_then(|duration| duration.as_u64())
        .context("total_duration_ms must be a nonnegative integer")?;
    let accounted_duration_ms = dispatch_duration_ms
        .checked_add(handler_duration_ms)
        .context("dispatch and handler durations must not overflow")?;
    anyhow::ensure!(
        total_duration_ms >= accounted_duration_ms
            && total_duration_ms - accounted_duration_ms <= 1,
        "dispatch and handler durations must account for total duration within integer truncation"
    );

    assert_eq!(
        tool_call,
        json!({
            "level": "INFO",
            "fields": {
                "message": "tool call completed",
                "event.name": "codex.tool_call",
                "conversation.id": thread.id,
                "turn_id": turn.id,
                "tool_name": "exec_command",
                "call_id": "exec-call-1",
                "tool_source": "direct",
                "execution_started": true,
            },
            "target": "codex_core::tools::parallel",
        })
    );

    Ok(())
}
