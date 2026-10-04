//! Verifies explicit MCP prepared-call authority across sessions.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ElicitationAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::PathExt;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::apps_test_server::SEARCH_CALENDAR_CREATE_TOOL;
use core_test_support::apps_test_server::SEARCH_CALENDAR_NAMESPACE;
use core_test_support::apps_test_server::apps_enabled_builder;
use core_test_support::apps_test_server::recorded_apps_tool_call_by_call_id;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_mcp_server;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apps_call_survives_catalog_restoration_while_awaiting_approval() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let tools_available = Arc::new(AtomicBool::new(true));
    let apps =
        AppsTestServer::mount_with_tools_available_when(&server, Arc::clone(&tools_available))
            .await?;
    let fixture = apps_enabled_builder(apps.chatgpt_base_url)
        .with_model_info_override("gpt-5.5", |model| model.supports_search_tool = false)
        .with_config(|config| {
            config.approvals_reviewer = ApprovalsReviewer::User;
            let mut servers = config.mcp_servers.get().clone();
            servers
                .get_mut(CODEX_APPS_MCP_SERVER_NAME)
                .expect("explicit MCP server")
                .default_tools_approval_mode = Some(codex_config::types::AppToolApproval::Prompt);
            config
                .mcp_servers
                .set(servers)
                .expect("explicit MCP approval config");
            config
                .features
                .enable(Feature::ToolCallMcpElicitation)
                .expect("test config should allow MCP approvals");
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, CODEX_APPS_MCP_SERVER_NAME).await?;
    let call = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("calendar-call"),
            responses::ev_function_call_with_namespace(
                "calendar-call",
                SEARCH_CALENDAR_NAMESPACE,
                SEARCH_CALENDAR_CREATE_TOOL,
                &json!({"title": "Lunch", "starts_at": "2026-03-10T12:00:00Z"}).to_string(),
            ),
            responses::ev_completed("calendar-call"),
        ]),
    )
    .await;
    let completion = responses::mount_sse_once(&server, responses::sse_completed("done")).await;
    fixture
        .codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Use [$calendar](app://calendar) to create a calendar event.".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::OnRequest),
                permission_profile: Some(PermissionProfile::Disabled),
                ..Default::default()
            }),
        )
        .await?;
    let EventMsg::ElicitationRequest(approval) = wait_for_event(&fixture.codex, |event| {
        matches!(
            event,
            EventMsg::ElicitationRequest(_) | EventMsg::TurnComplete(_)
        )
    })
    .await
    else {
        anyhow::bail!("the calendar call should wait for approval");
    };
    assert!(
        responses::namespace_child_tool(
            &call.single_request().body_json(),
            SEARCH_CALENDAR_NAMESPACE,
            SEARCH_CALENDAR_CREATE_TOOL,
        )
        .is_some()
    );

    // A peer removes the tools, then restores them without refreshing the waiting client.
    tools_available.store(false, Ordering::SeqCst);
    let peer = fixture
        .thread_manager
        .start_thread(StartThreadOptions::new(fixture.config.clone()))
        .await?
        .thread;
    wait_for_mcp_server(&peer, CODEX_APPS_MCP_SERVER_NAME).await?;
    tools_available.store(true, Ordering::SeqCst);

    fixture
        .codex
        .submit(Op::ResolveElicitation {
            server_name: approval.server_name,
            request_id: approval.id,
            decision: ElicitationAction::Accept,
            content: None,
            meta: None,
        })
        .await?;
    wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let executed = recorded_apps_tool_call_by_call_id(&server, "calendar-call").await;
    assert_eq!(executed["params"]["name"], "calendar_create_event");
    assert!(
        completion
            .single_request()
            .function_call_output_text("calendar-call")
            .is_some()
    );
    peer.shutdown_and_wait().await?;
    Ok(())
}
