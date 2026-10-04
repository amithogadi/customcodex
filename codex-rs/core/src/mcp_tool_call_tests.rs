use super::*;
use crate::config::ConfigBuilder;
use crate::config::ManagedFeatures;
use crate::environment_selection::EnvironmentConfigOrigin;
use crate::environment_selection::TurnEnvironmentState;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::tests::mcp_config_for_test;
use crate::session::tests::update_selected_settings_for_test;
use crate::session::tests::update_turn_settings_for_test;
use crate::session::turn_context::TurnEnvironment;
use crate::state::ActiveTurn;
use crate::test_support::models_manager_with_provider;
use crate::tools::hook_names::HookToolName;
use crate::turn_metadata::ExecutionMetadata;
use codex_app_server_protocol as app_server_protocol;
use codex_config::CONFIG_TOML_FILE;
use codex_config::config_toml::ConfigToml;
use codex_config::types::AppConfig;
use codex_config::types::AppToolConfig;
use codex_config::types::AppToolsConfig;
use codex_config::types::ApprovalsReviewer;
use codex_config::types::AppsConfigToml;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerToolConfig;
use codex_features::Features;
use codex_hooks::HooksConfig;
use codex_model_provider::create_model_provider;
use codex_protocol::ResponseItemId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GranularApprovalConfig;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::McpInvocation;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_path_uri::PathUri;
use core_test_support::hooks::trusted_config_layer_stack;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::tempdir;
use tracing::Instrument;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_test::internal::MockWriter;

fn annotations(
    read_only: Option<bool>,
    destructive: Option<bool>,
    open_world: Option<bool>,
) -> ToolAnnotations {
    ToolAnnotations::from_raw(
        /*title*/ None,
        read_only,
        destructive,
        /*idempotent_hint*/ None,
        open_world,
    )
}

fn approval_metadata(
    connector_id: Option<&str>,
    connector_name: Option<&str>,
    connector_description: Option<&str>,
    tool_title: Option<&str>,
    tool_description: Option<&str>,
) -> McpToolApprovalMetadata {
    McpToolApprovalMetadata {
        annotations: None,
        connector_id: connector_id.map(str::to_string),
        link_id: None,
        connector_name: connector_name.map(str::to_string),
        connector_description: connector_description.map(str::to_string),
        connected_account_email: None,
        plugin_id: None,
        tool_title: tool_title.map(str::to_string),
        tool_description: tool_description.map(str::to_string),
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
    }
}

fn approval_config(turn_context: &TurnContext) -> codex_mcp::McpConfig {
    (*mcp_config_for_test(&turn_context.config)).clone()
}

#[test_case::test_case(serde_json::json!({}); "no_account_metadata")]
#[test_case::test_case(serde_json::json!({ "_codex_apps": { "requires_explicit_link_id": true } }); "unrelated_apps_metadata")]
fn non_apps_tool_does_not_require_account_metadata(meta: JsonValue) {
    let tool_info = serde_json::from_value(serde_json::json!({
        "server_name": "custom_server",
        "tool_name": "events/create",
        "tool_namespace": "calendar",
        "tool": { "name": "calendar/events/create", "inputSchema": {}, "_meta": meta },
    }))
    .expect("tool info");

    assert_eq!(
        mcp_tool_metadata(&tool_info, /*plugin_id*/ None, /*arguments*/ None)
            .map(|metadata| metadata.link_id),
        Ok(None),
    );
}

#[test]
fn explicit_codex_apps_metadata_does_not_require_hosted_account_selection() {
    let tool_info = serde_json::from_value(serde_json::json!({
        "server_name": CODEX_APPS_MCP_SERVER_NAME,
        "tool_name": "events/create",
        "tool_namespace": "mcp__codex_apps",
        "tool": {
            "name": "events/create",
            "inputSchema": {},
            "_meta": {
                "link_id": "untrusted-account",
                "_codex_apps": { "requires_explicit_link_id": true }
            }
        }
    }))
    .expect("tool info");
    let metadata = mcp_tool_metadata(&tool_info, None, None).expect("ordinary MCP metadata");
    assert_eq!(metadata.link_id, None);
    assert_eq!(metadata.connected_account_email, None);
}

fn mcp_turn_metadata_context(turn_context: &TurnContext) -> ExecutionMetadata<'_> {
    ExecutionMetadata {
        model: turn_context.model_info().slug.as_str(),
        reasoning_effort: turn_context.effective_reasoning_effort(),
        node_repl_disabled: turn_context.model_info().node_repl_disabled,
        auto_review_enabled: crate::guardian::routes_approval_policy_to_guardian(
            turn_context.approval_policy(),
            turn_context.config.approvals_reviewer,
        ),
        node_repl_auto_review_required: turn_context.model_info().node_repl_auto_review_required,
    }
}

fn expected_mcp_turn_metadata(turn_context: &TurnContext) -> serde_json::Value {
    turn_context
        .turn_metadata_state
        .current_meta_value_for_mcp_request(mcp_turn_metadata_context(turn_context))
        .expect("turn metadata")
}

fn write_sample_plugin_mcp(codex_home: &std::path::Path) {
    let plugin_root = codex_home.join("plugins/cache/test/sample/local");
    std::fs::create_dir_all(plugin_root.join(".codex-plugin")).expect("create plugin manifest dir");
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "sample"
}"#,
    )
    .expect("write plugin manifest");
    std::fs::write(
        plugin_root.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "sample": {
      "type": "http",
      "url": "https://sample.example/mcp"
    }
  }
}"#,
    )
    .expect("write plugin mcp config");
}

fn prompt_options(
    allow_session_remember: bool,
    allow_persistent_approval: bool,
) -> McpToolApprovalPromptOptions {
    McpToolApprovalPromptOptions {
        allow_session_remember,
        allow_persistent_approval,
    }
}

fn install_mcp_permission_request_hook(
    session: &mut Session,
    turn_context: &TurnContext,
    matcher: &str,
    hook_output: &serde_json::Value,
) -> std::path::PathBuf {
    let script_path = turn_context
        .config
        .codex_home
        .join("mcp_permission_request_hook.py");
    let log_path = turn_context
        .config
        .codex_home
        .join("mcp_permission_request_hook_log.jsonl");
    let hook_output = hook_output.to_string();
    std::fs::create_dir_all(&turn_context.config.codex_home)
        .expect("create codex home for MCP permission hook");
    let script = format!(
        r#"import json
from pathlib import Path
import sys

payload = json.load(sys.stdin)
with Path(r"{log_path}").open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")

print({hook_output:?})
"#,
        log_path = log_path.display(),
        hook_output = hook_output,
    );

    std::fs::write(&script_path, script).expect("write MCP permission hook script");
    let python = if cfg!(windows) { "python" } else { "python3" };
    let script_path_arg = if cfg!(windows) {
        script_path.display().to_string()
    } else {
        format!(
            "'{}'",
            script_path.display().to_string().replace('\'', "'\\''")
        )
    };
    std::fs::write(
        turn_context.config.codex_home.join("hooks.json"),
        serde_json::json!({
            "hooks": {
                "PermissionRequest": [{
                    "matcher": matcher,
                    "hooks": [{
                        "type": "command",
                        "command": format!("{python} {script_path_arg}"),
                        "timeout_sec": 5,
                    }]
                }]
            }
        })
        .to_string(),
    )
    .expect("write hooks.json");
    let hook_list = codex_hooks::list_hooks(HooksConfig {
        feature_enabled: true,
        config_layer_stack: Some(turn_context.config.config_layer_stack.clone()),
        ..HooksConfig::default()
    });
    assert_eq!(hook_list.hooks.len(), 1);
    let trusted_config_layer_stack = trusted_config_layer_stack(
        &turn_context.config.config_layer_stack,
        &turn_context.config.codex_home,
        hook_list.hooks,
    );

    let hooks = session.hooks().reconfigured(HooksConfig {
        feature_enabled: true,
        config_layer_stack: Some(trusted_config_layer_stack),
        shell_program: (!cfg!(windows)).then_some("/bin/sh".to_string()),
        shell_args: if cfg!(windows) {
            Vec::new()
        } else {
            vec!["-c".to_string()]
        },
        ..HooksConfig::default()
    });
    session.services.hooks.store(Arc::new(hooks));

    log_path.to_path_buf()
}

#[test]
fn mcp_app_resource_uri_reads_known_tool_meta_keys() {
    let nested = serde_json::json!({
        "ui": {
            "resourceUri": "ui://widget/nested.html",
        },
    });
    assert_eq!(
        get_mcp_app_resource_uri(nested.as_object()),
        Some("ui://widget/nested.html".to_string())
    );

    let flat = serde_json::json!({
        "ui/resourceUri": "ui://widget/flat.html",
    });
    assert_eq!(
        get_mcp_app_resource_uri(flat.as_object()),
        Some("ui://widget/flat.html".to_string())
    );

    let output_template = serde_json::json!({
        "openai/outputTemplate": "ui://widget/output-template.html",
    });
    assert_eq!(
        get_mcp_app_resource_uri(output_template.as_object()),
        Some("ui://widget/output-template.html".to_string())
    );
}

#[test]
fn approval_required_when_read_only_false_and_destructive() {
    let annotations = annotations(Some(false), Some(true), /*open_world*/ None);
    assert_eq!(requires_mcp_tool_approval(Some(&annotations)), true);
}

#[test]
fn approval_required_when_read_only_false_and_open_world() {
    let annotations = annotations(Some(false), /*destructive*/ None, Some(true));
    assert_eq!(requires_mcp_tool_approval(Some(&annotations)), true);
}

#[test]
fn approval_required_when_destructive_even_if_read_only_true() {
    let annotations = annotations(Some(true), Some(true), Some(true));
    assert_eq!(requires_mcp_tool_approval(Some(&annotations)), true);
}

#[test]
fn approval_required_when_annotations_are_absent() {
    assert_eq!(requires_mcp_tool_approval(/*annotations*/ None), true);
}

#[test]
fn approval_not_required_when_read_only_and_other_hints_are_absent() {
    let annotations = annotations(
        Some(true),
        /*destructive*/ None,
        /*open_world*/ None,
    );
    assert_eq!(requires_mcp_tool_approval(Some(&annotations)), false);
}

#[test]
fn writes_mode_requires_approval_for_non_read_only_tools() {
    let annotations = annotations(Some(false), Some(false), Some(false));
    assert_eq!(
        requires_mcp_tool_approval_for_mode(Some(&annotations), AppToolApproval::Writes),
        true
    );
    assert_eq!(
        requires_mcp_tool_approval_for_mode(/*annotations*/ None, AppToolApproval::Writes),
        true
    );
}

#[test]
fn writes_mode_does_not_require_approval_for_read_only_tools() {
    let annotations = annotations(Some(true), Some(true), Some(true));
    assert_eq!(
        requires_mcp_tool_approval_for_mode(Some(&annotations), AppToolApproval::Writes),
        false
    );
}

#[test]
fn prompting_modes_do_not_allow_persistent_remember() {
    for approval_mode in [AppToolApproval::Prompt, AppToolApproval::Writes] {
        assert_eq!(
            normalize_approval_decision_for_mode(ReviewDecision::ApprovedForSession, approval_mode,),
            ReviewDecision::Approved
        );
        assert_eq!(
            normalize_approval_decision_for_mode(
                ReviewDecision::ApprovedMcpPolicyAmendment,
                approval_mode,
            ),
            ReviewDecision::Approved
        );
    }
}

#[test]
fn approval_elicitation_request_uses_message_override_and_preserves_tool_params_keys() {
    let question = build_mcp_tool_approval_question(
        "q".to_string(),
        CODEX_APPS_MCP_SERVER_NAME,
        "create_event",
        Some("Calendar"),
        prompt_options(
            /*allow_session_remember*/ true, /*allow_persistent_approval*/ true,
        ),
        Some("Allow Calendar to create an event?"),
    );

    let request = build_mcp_tool_approval_elicitation_request(McpToolApprovalElicitationRequest {
        server: CODEX_APPS_MCP_SERVER_NAME,
        metadata: Some(&approval_metadata(
            Some("calendar"),
            Some("Calendar"),
            Some("Manage events and schedules."),
            Some("Create Event"),
            Some("Create a calendar event."),
        )),
        tool_params: Some(&serde_json::json!({
            "calendar_id": "primary",
            "title": "Roadmap review",
        })),
        tool_params_display: Some(&[
            RenderedMcpToolApprovalParam {
                name: "calendar_id".to_string(),
                value: serde_json::json!("primary"),
                display_name: "Calendar".to_string(),
            },
            RenderedMcpToolApprovalParam {
                name: "title".to_string(),
                value: serde_json::json!("Roadmap review"),
                display_name: "Title".to_string(),
            },
        ]),
        question,
        message_override: Some("Allow Calendar to create an event?"),
        prompt_options: prompt_options(
            /*allow_session_remember*/ true, /*allow_persistent_approval*/ true,
        ),
    });

    assert_eq!(
        request,
        ElicitationRequest::Form {
            meta: Some(serde_json::json!({
                MCP_TOOL_APPROVAL_KIND_KEY: MCP_TOOL_APPROVAL_KIND_MCP_TOOL_CALL,
                MCP_TOOL_APPROVAL_PERSIST_KEY: [
                    MCP_TOOL_APPROVAL_PERSIST_SESSION,
                    MCP_TOOL_APPROVAL_PERSIST_ALWAYS,
                ],
                MCP_TOOL_APPROVAL_SOURCE_KEY: MCP_TOOL_APPROVAL_SOURCE_CONNECTOR,
                MCP_TOOL_APPROVAL_CONNECTOR_ID_KEY: "calendar",
                MCP_TOOL_APPROVAL_CONNECTOR_NAME_KEY: "Calendar",
                MCP_TOOL_APPROVAL_CONNECTOR_DESCRIPTION_KEY: "Manage events and schedules.",
                MCP_TOOL_APPROVAL_TOOL_TITLE_KEY: "Create Event",
                MCP_TOOL_APPROVAL_TOOL_DESCRIPTION_KEY: "Create a calendar event.",
                MCP_TOOL_APPROVAL_TOOL_PARAMS_KEY: {
                    "calendar_id": "primary",
                    "title": "Roadmap review",
                },
                MCP_TOOL_APPROVAL_TOOL_PARAMS_DISPLAY_KEY: [
                    {
                        "name": "calendar_id",
                        "value": "primary",
                        "display_name": "Calendar",
                    },
                    {
                        "name": "title",
                        "value": "Roadmap review",
                        "display_name": "Title",
                    },
                ],
            })),
            message: "Allow Calendar to create an event?".to_string(),
            requested_schema: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
        }
    );
}

#[test]
fn custom_mcp_tool_question_mentions_server_name() {
    let question = build_mcp_tool_approval_question(
        "q".to_string(),
        "custom_server",
        "run_action",
        /*connector_name*/ None,
        prompt_options(
            /*allow_session_remember*/ false, /*allow_persistent_approval*/ false,
        ),
        /*question_override*/ None,
    );

    assert_eq!(question.header, "Approve MCP tool call?");
    assert_eq!(
        question.question,
        "Allow the custom_server MCP server to run tool \"run_action\"?"
    );
    assert!(
        !question
            .options
            .expect("options")
            .into_iter()
            .map(|option| option.label)
            .any(|label| label == MCP_TOOL_APPROVAL_ACCEPT_AND_REMEMBER)
    );
}

#[test]
fn codex_apps_tool_question_uses_fallback_app_label() {
    let question = build_mcp_tool_approval_question(
        "q".to_string(),
        CODEX_APPS_MCP_SERVER_NAME,
        "run_action",
        /*connector_name*/ None,
        prompt_options(
            /*allow_session_remember*/ true, /*allow_persistent_approval*/ true,
        ),
        /*question_override*/ None,
    );

    assert_eq!(
        question.question,
        "Allow the codex_apps MCP server to run tool \"run_action\"?"
    );
}

#[test]
fn trusted_codex_apps_tool_question_offers_always_allow() {
    let question = build_mcp_tool_approval_question(
        "q".to_string(),
        CODEX_APPS_MCP_SERVER_NAME,
        "run_action",
        Some("Calendar"),
        prompt_options(
            /*allow_session_remember*/ true, /*allow_persistent_approval*/ true,
        ),
        /*question_override*/ None,
    );
    let options = question.options.expect("options");

    assert!(options.iter().any(|option| {
        option.label == MCP_TOOL_APPROVAL_ACCEPT_FOR_SESSION
            && option.description == "Run the tool and remember this choice for this session."
    }));
    assert!(options.iter().any(|option| {
        option.label == MCP_TOOL_APPROVAL_ACCEPT_AND_REMEMBER
            && option.description == "Run the tool and remember this choice for future tool calls."
    }));
    assert_eq!(
        options
            .into_iter()
            .map(|option| option.label)
            .collect::<Vec<_>>(),
        vec![
            MCP_TOOL_APPROVAL_ACCEPT.to_string(),
            MCP_TOOL_APPROVAL_ACCEPT_FOR_SESSION.to_string(),
            MCP_TOOL_APPROVAL_ACCEPT_AND_REMEMBER.to_string(),
            MCP_TOOL_APPROVAL_CANCEL.to_string(),
        ]
    );
}

#[test]
fn codex_apps_tool_question_without_elicitation_omits_always_allow() {
    let question = build_mcp_tool_approval_question(
        "q".to_string(),
        CODEX_APPS_MCP_SERVER_NAME,
        "run_action",
        Some("Calendar"),
        mcp_tool_approval_prompt_options(
            /*allow_session_remember*/ true, /*allow_persistent_approval*/ true,
            /*tool_call_mcp_elicitation_enabled*/ false,
        ),
        /*question_override*/ None,
    );

    assert_eq!(
        question
            .options
            .expect("options")
            .into_iter()
            .map(|option| option.label)
            .collect::<Vec<_>>(),
        vec![
            MCP_TOOL_APPROVAL_ACCEPT.to_string(),
            MCP_TOOL_APPROVAL_ACCEPT_FOR_SESSION.to_string(),
            MCP_TOOL_APPROVAL_CANCEL.to_string(),
        ]
    );
}

#[test]
fn custom_mcp_tool_question_offers_session_remember_and_always_allow() {
    let question = build_mcp_tool_approval_question(
        "q".to_string(),
        "custom_server",
        "run_action",
        /*connector_name*/ None,
        prompt_options(
            /*allow_session_remember*/ true, /*allow_persistent_approval*/ true,
        ),
        /*question_override*/ None,
    );

    assert_eq!(
        question
            .options
            .expect("options")
            .into_iter()
            .map(|option| option.label)
            .collect::<Vec<_>>(),
        vec![
            MCP_TOOL_APPROVAL_ACCEPT.to_string(),
            MCP_TOOL_APPROVAL_ACCEPT_FOR_SESSION.to_string(),
            MCP_TOOL_APPROVAL_ACCEPT_AND_REMEMBER.to_string(),
            MCP_TOOL_APPROVAL_CANCEL.to_string(),
        ]
    );
}

#[test]
fn custom_servers_support_session_and_persistent_approval() {
    let invocation = McpInvocation {
        server: "custom_server".to_string(),
        tool: "run_action".to_string(),
        arguments: None,
    };
    let expected = McpToolApprovalKey {
        server: "custom_server".to_string(),
        plugin_id: None,
        connector_id: None,
        link_id: None,
        tool_name: "run_action".to_string(),
    };

    assert_eq!(
        session_mcp_tool_approval_key(&invocation, /*metadata*/ None, AppToolApproval::Auto),
        Some(expected.clone())
    );
    assert_eq!(
        persistent_mcp_tool_approval_key(
            &invocation,
            /*metadata*/ None,
            AppToolApproval::Auto
        ),
        Some(expected.clone())
    );
    let mut metadata = approval_metadata(
        /*connector_id*/ None, /*connector_name*/ None,
        /*connector_description*/ None, /*tool_title*/ None,
        /*tool_description*/ None,
    );
    for plugin_id in ["alpha@test", "beta@test"] {
        metadata.plugin_id = Some(plugin_id.to_string());
        let plugin_key = McpToolApprovalKey {
            plugin_id: Some(plugin_id.to_string()),
            ..expected.clone()
        };
        assert_eq!(
            session_mcp_tool_approval_key(&invocation, Some(&metadata), AppToolApproval::Auto),
            Some(plugin_key)
        );
    }
}

#[test]
fn codex_apps_connectors_support_persistent_approval() {
    let invocation = McpInvocation {
        server: CODEX_APPS_MCP_SERVER_NAME.to_string(),
        tool: "calendar/list_events".to_string(),
        arguments: None,
    };
    let mut metadata = approval_metadata(
        Some("calendar"),
        Some("Calendar"),
        /*connector_description*/ None,
        /*tool_title*/ None,
        /*tool_description*/ None,
    );
    metadata.link_id = Some("link_a".to_string());
    let expected = McpToolApprovalKey {
        server: CODEX_APPS_MCP_SERVER_NAME.to_string(),
        plugin_id: None,
        connector_id: Some("calendar".to_string()),
        link_id: Some("link_a".to_string()),
        tool_name: "calendar/list_events".to_string(),
    };

    assert_eq!(
        session_mcp_tool_approval_key(&invocation, Some(&metadata), AppToolApproval::Auto),
        Some(expected.clone())
    );
    assert_eq!(
        persistent_mcp_tool_approval_key(&invocation, Some(&metadata), AppToolApproval::Auto),
        Some(expected)
    );
}

#[test]
fn sanitize_mcp_tool_result_for_model_rewrites_unsupported_media_content() {
    let result = Ok(CallToolResult {
        content: vec![
            serde_json::json!({
                "type": "image",
                "data": "Zm9v",
                "mimeType": "image/png",
            }),
            serde_json::json!({
                "type": "audio",
                "data": "YmFy",
                "mimeType": "audio/wav",
            }),
            serde_json::json!({
                "type": "text",
                "text": "hello",
            }),
        ],
        structured_content: None,
        is_error: Some(false),
        meta: None,
    });

    let got = sanitize_mcp_tool_result_for_model(&[InputModality::Text], result)
        .expect("sanitized result");

    assert_eq!(
        got,
        CallToolResult {
            content: vec![
                serde_json::json!({
                    "type": "text",
                    "text": "<image content omitted because you do not support image input>",
                }),
                serde_json::json!({
                    "type": "text",
                    "text": "<audio content omitted because you do not support audio input>",
                }),
                serde_json::json!({
                    "type": "text",
                    "text": "hello",
                }),
            ],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        }
    );
}

#[test]
fn sanitize_mcp_tool_result_for_model_preserves_supported_media() {
    let original = CallToolResult {
        content: vec![
            serde_json::json!({
                "type": "image",
                "data": "Zm9v",
                "mimeType": "image/png",
            }),
            serde_json::json!({
                "type": "audio",
                "data": "YmFy",
                "mimeType": "audio/wav",
            }),
        ],
        structured_content: Some(serde_json::json!({"x": 1})),
        is_error: Some(false),
        meta: Some(serde_json::json!({"k": "v"})),
    };

    let got = sanitize_mcp_tool_result_for_model(
        &[
            InputModality::Text,
            InputModality::Image,
            InputModality::Audio,
        ],
        Ok(original.clone()),
    )
    .expect("unsanitized result");

    assert_eq!(got, original);
}

#[test]
fn truncate_mcp_tool_result_for_event_bounds_large_error() {
    let got = truncate_mcp_tool_result_for_event(&Err("error-message-".repeat(200_000)))
        .expect_err("large error should remain an error");

    // `truncate_text` includes its own marker, so allow a small amount of
    // overhead beyond the requested byte budget.
    assert!(got.len() < MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES + 1024);
    assert!(got.contains("truncated"));
}

#[tokio::test]
async fn mcp_tool_call_request_meta_includes_turn_metadata_for_custom_server() {
    let (_, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    turn_context
        .turn_metadata_state
        .set_responsesapi_client_metadata(HashMap::from([
            (
                "node_repl_auto_review_required".to_string(),
                "true".to_string(),
            ),
            ("node_repl_disabled".to_string(), "true".to_string()),
        ]));
    let expected_turn_metadata = expected_mcp_turn_metadata(&turn_context);

    let meta = build_mcp_tool_call_request_meta(
        &step_context,
        "custom_server",
        "call-custom",
        /*metadata*/ None,
    )
    .expect("custom servers should receive turn metadata");
    let turn_metadata = meta
        .get(crate::X_CODEX_TURN_METADATA_HEADER)
        .expect("turn metadata should be present");

    assert_eq!(
        turn_metadata
            .get("model")
            .and_then(serde_json::Value::as_str),
        Some(step_context.settings.model_info.slug.as_str())
    );
    assert_eq!(
        turn_metadata["node_repl_auto_review_required"],
        serde_json::Value::Bool(
            step_context
                .settings
                .model_info
                .node_repl_auto_review_required
        ),
    );
    assert_eq!(
        turn_metadata["node_repl_disabled"],
        serde_json::Value::Bool(step_context.settings.model_info.node_repl_disabled),
    );
    assert_eq!(
        turn_metadata
            .get("reasoning_effort")
            .and_then(serde_json::Value::as_str),
        step_context
            .settings
            .effective_reasoning_effort()
            .map(|effort| effort.to_string())
            .as_deref()
    );

    assert_eq!(
        meta,
        serde_json::json!({
            "callId": "call-custom",
            crate::X_CODEX_TURN_METADATA_HEADER: expected_turn_metadata,
        })
    );
}

#[test_case::test_case(Some(ReasoningEffortConfig::Low), "low"; "configured effort")]
#[test_case::test_case(None, "high"; "pinned model default")]
#[tokio::test]
async fn mcp_tool_call_request_meta_uses_the_issuing_step(
    effort: Option<ReasoningEffortConfig>,
    expected_effort: &str,
) {
    let (_, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let step_a = StepContext::for_test(Arc::clone(&turn_context));
    let original_meta =
        build_mcp_tool_call_request_meta(&step_a, "node_repl", "call-a", /*metadata*/ None);
    let mut step_b = StepContext::for_test(Arc::clone(&turn_context));
    let settings = Arc::make_mut(&mut Arc::get_mut(&mut step_b).expect("unique step").settings);
    update_selected_settings_for_test(settings, |selected| {
        selected.collaboration_mode = selected.collaboration_mode.with_updates(
            Some("model-b".to_string()),
            Some(effort),
            /*developer_instructions*/ None,
        );
    });
    let model = Arc::make_mut(&mut settings.model_info);
    model.slug = "model-b".to_string();
    model.default_reasoning_level = Some(ReasoningEffortConfig::High);
    model.node_repl_disabled = true;
    // Node review requirements are captured with the issuing step.
    model.node_repl_auto_review_required =
        !turn_context.model_info().node_repl_auto_review_required;

    let mut expected = expected_mcp_turn_metadata(&turn_context);
    expected["model"] = serde_json::json!("model-b");
    expected["reasoning_effort"] = serde_json::json!(expected_effort);
    expected["node_repl_disabled"] = serde_json::json!(true);
    expected["node_repl_auto_review_required"] =
        serde_json::json!(step_b.settings.model_info.node_repl_auto_review_required);
    assert_eq!(
        build_mcp_tool_call_request_meta(&step_b, "node_repl", "call-b", /*metadata*/ None),
        Some(serde_json::json!({
            "callId": "call-b",
            crate::X_CODEX_TURN_METADATA_HEADER: expected,
            CONFIRMATION_POLICIES_META_KEY: {},
        })),
    );
    assert_eq!(
        build_mcp_tool_call_request_meta(&step_a, "node_repl", "call-a", /*metadata*/ None),
        original_meta,
    );
}

#[tokio::test]
async fn guardian_mcp_tool_call_request_meta_excludes_actor_confirmation_policy() {
    for session_source in [
        SessionSource::Internal(InternalSessionSource::Guardian),
        SessionSource::SubAgent(SubAgentSource::Other(
            crate::guardian::GUARDIAN_REVIEWER_NAME.to_string(),
        )),
    ] {
        let (_, mut turn_context) = make_session_and_context().await;
        turn_context.session_source = session_source;
        update_turn_settings_for_test(&mut turn_context, |settings| {
            Arc::make_mut(&mut settings.model_info).model_messages = Some(
                serde_json::from_value(serde_json::json!({
                    "confirmation_policies": {
                        "browser_use": "actor-only raw Markdown",
                        "computer_use": "actor-only native Markdown",
                    },
                }))
                .expect("confirmation policy fixture should deserialize"),
            );
        });
        let expected = Some(serde_json::json!({
            "callId": "call-guardian",
            crate::X_CODEX_TURN_METADATA_HEADER: expected_mcp_turn_metadata(&turn_context),
        }));
        let step_context = StepContext::for_test(Arc::new(turn_context));

        for server in ["node_repl", "cua_repl"] {
            assert_eq!(
                build_mcp_tool_call_request_meta(
                    &step_context,
                    server,
                    "call-guardian",
                    /*metadata*/ None,
                ),
                expected,
                "{server}: {:?}",
                step_context.turn.session_source,
            );
        }
    }
}

#[tokio::test]
async fn mcp_tool_call_request_meta_includes_turn_started_at_unix_ms() {
    let (_, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    turn_context
        .turn_metadata_state
        .set_turn_started_at_unix_ms(/*turn_started_at_unix_ms*/ 1_700_000_000_123);

    let meta = build_mcp_tool_call_request_meta(
        &step_context,
        "custom_server",
        "call-custom",
        /*metadata*/ None,
    )
    .expect("custom servers should receive turn metadata");
    let turn_metadata = meta
        .get(crate::X_CODEX_TURN_METADATA_HEADER)
        .expect("turn metadata should be present");

    assert_eq!(
        turn_metadata
            .get("turn_started_at_unix_ms")
            .and_then(serde_json::Value::as_i64),
        Some(1_700_000_000_123)
    );
}

#[tokio::test]
async fn mcp_sandbox_cwd_uses_matching_server_environment_uri() -> anyhow::Result<()> {
    let (_, mut turn_context) = make_session_and_context().await;
    let secondary_cwd = PathUri::parse("file:///C:/remote/project")?;
    let environment = turn_context
        .initial_environments
        .primary()
        .expect("primary environment")
        .environment
        .clone();
    turn_context
        .initial_environments
        .environments
        .push(TurnEnvironmentState::Ready(TurnEnvironment::new(
            TurnEnvironmentSelection {
                environment_id: "remote".to_string(),
                cwd: secondary_cwd.clone(),
                workspace_roots: Vec::new(),
                config: EnvironmentConfigState::Ready(EnvironmentConfig {
                    allow_login_shell: true,
                    workspace_roots: Vec::new(),
                    windows_sandbox_level: turn_context.windows_sandbox_level,
                    windows_sandbox_type: turn_context.config.permissions.windows_sandbox_type,
                    use_legacy_landlock: turn_context.config.features.use_legacy_landlock(),
                    permission_profile: turn_context
                        .config
                        .permissions
                        .permission_profile_state()
                        .snapshot(),
                    shell_environment_policy: Default::default(),
                    exec_policy: None,
                    mcp_policy: None,
                    network_policy: None,
                    selected_capability_roots: Vec::new(),
                }),
            },
            EnvironmentConfigOrigin::Thread,
            environment,
            /*shell*/ None,
        )));

    let step_context = StepContext::for_test(Arc::new(turn_context));
    let sandbox_cwd = sandbox_cwd_for_mcp_server(&step_context, "remote");

    assert_eq!(sandbox_cwd, Some(secondary_cwd));
    Ok(())
}

#[tokio::test]
async fn mcp_sandbox_cwd_is_none_for_unselected_server_environment() -> anyhow::Result<()> {
    let (_, turn_context) = make_session_and_context().await;

    let step_context = StepContext::for_test(Arc::new(turn_context));
    let sandbox_cwd = sandbox_cwd_for_mcp_server(&step_context, "remote");

    assert_eq!(sandbox_cwd, None);
    Ok(())
}

#[tokio::test]
async fn plugin_mcp_tool_call_request_meta_includes_plugin_id() {
    let (_, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let expected_turn_metadata = expected_mcp_turn_metadata(&turn_context);
    let mut metadata = approval_metadata(
        /*connector_id*/ None, /*connector_name*/ None,
        /*connector_description*/ None, /*tool_title*/ None,
        /*tool_description*/ None,
    );
    metadata.plugin_id = Some("sample@test".to_string());

    assert_eq!(
        build_mcp_tool_call_request_meta(&step_context, "sample", "call-plugin", Some(&metadata),),
        Some(serde_json::json!({
            "callId": "call-plugin",
            crate::X_CODEX_TURN_METADATA_HEADER: expected_turn_metadata,
            MCP_TOOL_PLUGIN_ID_META_KEY: "sample@test",
        }))
    );
}

#[test]
fn mcp_tool_call_item_metadata_only_trusts_codex_apps_identity() {
    let mut metadata = approval_metadata(
        Some("asdk_app_0123456789abcdef0123456789abcdef"),
        Some("Calendar"),
        /*connector_description*/ None,
        Some("Create a calendar event"),
        /*tool_description*/ None,
    );
    metadata.link_id = Some("link_fedcba9876543210fedcba9876543210".to_string());
    metadata.annotations = Some(annotations(
        Some(false),
        /*destructive*/ None,
        /*open_world*/ None,
    ));
    metadata.codex_apps_meta = Some(
        serde_json::json!({
            "resource_uri": "/asdk_app_0123456789abcdef0123456789abcdef/link_fedcba9876543210fedcba9876543210/create_event",
        })
        .as_object()
        .cloned()
        .expect("_codex_apps metadata should be an object"),
    );

    assert_eq!(
        McpToolCallItemMetadata::from_tool_metadata(CODEX_APPS_MCP_SERVER_NAME, Some(&metadata),),
        McpToolCallItemMetadata {
            connector_id: Some("asdk_app_0123456789abcdef0123456789abcdef".to_string()),
            link_id: Some("link_fedcba9876543210fedcba9876543210".to_string()),
            mcp_app_resource_uri: None,
            mcp_app_ui: None,
            app_name: Some("Calendar".to_string()),
            action_name: Some("create_event".to_string()),
            plugin_id: None,
            read_only_hint: Some(false),
        }
    );
    assert_eq!(
        McpToolCallItemMetadata::from_tool_metadata("custom_server", Some(&metadata)),
        McpToolCallItemMetadata {
            connector_id: None,
            link_id: None,
            mcp_app_resource_uri: None,
            mcp_app_ui: None,
            app_name: None,
            action_name: None,
            plugin_id: None,
            read_only_hint: Some(false),
        }
    );
}

#[tokio::test]
async fn mcp_tool_call_item_includes_app_identity() {
    let (session, turn_context, rx_event) = make_session_and_context_with_rx().await;

    notify_mcp_tool_call_started(
        &session,
        &turn_context,
        "call-plugin",
        McpInvocation {
            server: CODEX_APPS_MCP_SERVER_NAME.to_string(),
            tool: "echo".to_string(),
            arguments: None,
        },
        McpToolCallItemMetadata {
            connector_id: Some("asdk_app_0123456789abcdef0123456789abcdef".to_string()),
            link_id: Some("link_fedcba9876543210fedcba9876543210".to_string()),
            mcp_app_resource_uri: None,
            mcp_app_ui: None,
            app_name: Some("Calendar".to_string()),
            action_name: Some("create_event".to_string()),
            plugin_id: Some("sample@test".to_string()),
            read_only_hint: Some(false),
        },
    )
    .await;

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx_event.recv())
        .await
        .expect("tool call item timed out")
        .expect("tool call item event");
    let EventMsg::ItemStarted(item_started) = event.msg else {
        panic!("expected ItemStarted event");
    };
    let TurnItem::McpToolCall(item) = item_started.item else {
        panic!("expected MCP tool call item");
    };

    assert_eq!(
        item.connector_id.as_deref(),
        Some("asdk_app_0123456789abcdef0123456789abcdef")
    );
    assert_eq!(
        item.link_id.as_deref(),
        Some("link_fedcba9876543210fedcba9876543210")
    );
    assert_eq!(item.plugin_id.as_deref(), Some("sample@test"));
    assert_eq!(item.app_name.as_deref(), Some("Calendar"));
    assert_eq!(item.action_name.as_deref(), Some("create_event"));
    assert_eq!(item.read_only_hint, Some(false));
}

#[tokio::test]
async fn codex_apps_tool_call_request_meta_includes_turn_metadata_and_codex_apps_meta() {
    let (_, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let expected_turn_metadata = expected_mcp_turn_metadata(&turn_context);
    let metadata = McpToolApprovalMetadata {
        annotations: None,
        connector_id: Some("calendar".to_string()),
        link_id: None,
        connector_name: Some("Calendar".to_string()),
        connector_description: Some("Manage events".to_string()),
        connected_account_email: None,
        plugin_id: None,
        tool_title: Some("Create Event".to_string()),
        tool_description: Some("Create a calendar event.".to_string()),
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: Some(
            serde_json::json!({
                "resource_uri": "connector://calendar/tools/calendar_create_event",
                "contains_mcp_source": true,
                "connector_id": "calendar",
            })
            .as_object()
            .cloned()
            .expect("_codex_apps metadata should be an object"),
        ),
    };

    assert_eq!(
        build_mcp_tool_call_request_meta(
            &step_context,
            CODEX_APPS_MCP_SERVER_NAME,
            "call_abc123xyz789",
            Some(&metadata),
        ),
        Some(serde_json::json!({
            "callId": "call_abc123xyz789",
            crate::X_CODEX_TURN_METADATA_HEADER: expected_turn_metadata,
            MCP_TOOL_CODEX_APPS_META_KEY: {
                "call_id": "call_abc123xyz789",
                "resource_uri": "connector://calendar/tools/calendar_create_event",
                "contains_mcp_source": true,
                "connector_id": "calendar",
            },
        }))
    );
}

#[tokio::test]
async fn codex_apps_tool_call_request_meta_includes_call_id_without_existing_codex_apps_meta() {
    let (_, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let expected_turn_metadata = expected_mcp_turn_metadata(&turn_context);

    assert_eq!(
        build_mcp_tool_call_request_meta(
            &step_context,
            CODEX_APPS_MCP_SERVER_NAME,
            "call_abc123xyz789",
            /*metadata*/ None,
        ),
        Some(serde_json::json!({
            "callId": "call_abc123xyz789",
            crate::X_CODEX_TURN_METADATA_HEADER: expected_turn_metadata,
            MCP_TOOL_CODEX_APPS_META_KEY: {
                "call_id": "call_abc123xyz789",
            },
        }))
    );
}

#[test]
fn mcp_tool_call_ids_are_added_to_request_meta() {
    let origin = crate::tools::context::ToolCallOrigin {
        item_id: Some(ResponseItemId::from_server("fc-live".to_string())),
        window_id: "thread-live:2".to_string(),
    };

    assert_eq!(
        with_mcp_tool_call_ids_meta(
            Some(serde_json::json!({
                "source": "test-client",
                "threadId": "stale-thread",
                "sessionId": "stale-session",
                "windowId": "stale-window",
                "itemId": "stale-item",
            })),
            "thread-live",
            "session-live",
            Some(&origin),
        ),
        Some(serde_json::json!({
            "source": "test-client",
            "threadId": "thread-live",
            "sessionId": "session-live",
            "itemId": "fc-live",
            "windowId": "thread-live:2",
        }))
    );

    assert_eq!(
        with_mcp_tool_call_ids_meta(
            /*meta*/ None,
            "thread-live",
            "session-live",
            /*originating_call*/ None,
        ),
        Some(serde_json::json!({
            "threadId": "thread-live",
            "sessionId": "session-live",
        }))
    );

    assert_eq!(
        with_mcp_tool_call_ids_meta(
            Some(serde_json::json!("invalid-meta")),
            "thread-live",
            "session-live",
            /*originating_call*/ None,
        ),
        Some(serde_json::json!("invalid-meta"))
    );
}

#[test]
fn accepted_elicitation_content_converts_to_request_user_input_response() {
    let response = request_user_input_response_from_elicitation_content(Some(serde_json::json!(
        {
            "approval": MCP_TOOL_APPROVAL_ACCEPT_AND_REMEMBER,
        }
    )));

    assert_eq!(
        response,
        Some(RequestUserInputResponse {
            answers: std::collections::HashMap::from([(
                "approval".to_string(),
                RequestUserInputAnswer {
                    answers: vec![MCP_TOOL_APPROVAL_ACCEPT_AND_REMEMBER.to_string()],
                },
            )]),
        })
    );
}

#[test]
fn approval_elicitation_meta_marks_tool_approvals() {
    assert_eq!(
        build_mcp_tool_approval_elicitation_meta(
            "custom_server",
            /*metadata*/ None,
            /*tool_params*/ None,
            /*tool_params_display*/ None,
            prompt_options(
                /*allow_session_remember*/ false, /*allow_persistent_approval*/ false
            ),
        ),
        Some(serde_json::json!({
            MCP_TOOL_APPROVAL_KIND_KEY: MCP_TOOL_APPROVAL_KIND_MCP_TOOL_CALL,
        }))
    );
}

#[test]
fn approval_elicitation_meta_merges_session_and_always_persist_for_custom_servers() {
    assert_eq!(
        build_mcp_tool_approval_elicitation_meta(
            "custom_server",
            Some(&approval_metadata(
                /*connector_id*/ None,
                /*connector_name*/ None,
                /*connector_description*/ None,
                Some("Run Action"),
                Some("Runs the selected action."),
            )),
            Some(&serde_json::json!({"id": 1})),
            /*tool_params_display*/ None,
            prompt_options(
                /*allow_session_remember*/ true, /*allow_persistent_approval*/ true
            ),
        ),
        Some(serde_json::json!({
            MCP_TOOL_APPROVAL_KIND_KEY: MCP_TOOL_APPROVAL_KIND_MCP_TOOL_CALL,
            MCP_TOOL_APPROVAL_PERSIST_KEY: [
                MCP_TOOL_APPROVAL_PERSIST_SESSION,
                MCP_TOOL_APPROVAL_PERSIST_ALWAYS,
            ],
            MCP_TOOL_APPROVAL_TOOL_TITLE_KEY: "Run Action",
            MCP_TOOL_APPROVAL_TOOL_DESCRIPTION_KEY: "Runs the selected action.",
            MCP_TOOL_APPROVAL_TOOL_PARAMS_KEY: {
                "id": 1,
            },
        }))
    );
}

#[test]
fn guardian_mcp_review_request_includes_invocation_metadata() {
    let invocation = McpInvocation {
        server: CODEX_APPS_MCP_SERVER_NAME.to_string(),
        tool: "browser_navigate".to_string(),
        arguments: Some(serde_json::json!({
            "url": "https://example.com",
        })),
    };

    let mut metadata = approval_metadata(
        Some("playwright"),
        Some("Playwright"),
        Some("Browser automation"),
        Some("Navigate"),
        Some("Open a page"),
    );
    metadata.connected_account_email = Some("owner@example.com".to_string());
    let request = build_guardian_mcp_tool_review_request("call-1", &invocation, Some(&metadata));

    assert_eq!(
        request,
        GuardianApprovalRequest::McpToolCall {
            id: "call-1".to_string(),
            server: CODEX_APPS_MCP_SERVER_NAME.to_string(),
            tool_name: "browser_navigate".to_string(),
            arguments: Some(serde_json::json!({
                "url": "https://example.com",
            })),
            connector_id: Some("playwright".to_string()),
            connector_name: Some("Playwright".to_string()),
            connector_description: Some("Browser automation".to_string()),
            connected_account_email: Some("owner@example.com".to_string()),
            tool_title: Some("Navigate".to_string()),
            tool_description: Some("Open a page".to_string()),
            annotations: None,
        }
    );
}

#[test]
fn guardian_mcp_review_request_includes_annotations_when_present() {
    let invocation = McpInvocation {
        server: "custom_server".to_string(),
        tool: "dangerous_tool".to_string(),
        arguments: None,
    };
    let metadata = McpToolApprovalMetadata {
        annotations: Some(annotations(Some(false), Some(true), Some(true))),
        connector_id: None,
        link_id: None,
        connector_name: None,
        connector_description: None,
        connected_account_email: None,
        plugin_id: None,
        tool_title: None,
        tool_description: None,
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
    };

    let request = build_guardian_mcp_tool_review_request("call-1", &invocation, Some(&metadata));

    assert_eq!(
        request,
        GuardianApprovalRequest::McpToolCall {
            id: "call-1".to_string(),
            server: "custom_server".to_string(),
            tool_name: "dangerous_tool".to_string(),
            arguments: None,
            connector_id: None,
            connector_name: None,
            connector_description: None,
            connected_account_email: None,
            tool_title: None,
            tool_description: None,
            annotations: Some(GuardianMcpAnnotations {
                destructive_hint: Some(true),
                open_world_hint: Some(true),
                read_only_hint: Some(false),
            }),
        }
    );
}

#[test]
fn guardian_mcp_review_request_ignores_untrusted_connected_account_email() {
    let invocation = McpInvocation {
        server: "custom_server".to_string(),
        tool: "dangerous_tool".to_string(),
        arguments: None,
    };
    let mut metadata = approval_metadata(
        /*connector_id*/ None, /*connector_name*/ None,
        /*connector_description*/ None, /*tool_title*/ None,
        /*tool_description*/ None,
    );
    metadata.connected_account_email = Some("spoofed@example.com".to_string());

    let request = build_guardian_mcp_tool_review_request("call-1", &invocation, Some(&metadata));

    assert_eq!(
        request,
        GuardianApprovalRequest::McpToolCall {
            id: "call-1".to_string(),
            server: "custom_server".to_string(),
            tool_name: "dangerous_tool".to_string(),
            arguments: None,
            connector_id: None,
            connector_name: None,
            connector_description: None,
            connected_account_email: None,
            tool_title: None,
            tool_description: None,
            annotations: None,
        }
    );
}

#[test]
fn approval_elicitation_meta_includes_connector_source_for_codex_apps() {
    assert_eq!(
        build_mcp_tool_approval_elicitation_meta(
            CODEX_APPS_MCP_SERVER_NAME,
            Some(&approval_metadata(
                Some("calendar"),
                Some("Calendar"),
                Some("Manage events and schedules."),
                Some("Run Action"),
                Some("Runs the selected action."),
            )),
            Some(&serde_json::json!({
                "calendar_id": "primary",
            })),
            /*tool_params_display*/ None,
            prompt_options(
                /*allow_session_remember*/ false, /*allow_persistent_approval*/ false
            ),
        ),
        Some(serde_json::json!({
            MCP_TOOL_APPROVAL_KIND_KEY: MCP_TOOL_APPROVAL_KIND_MCP_TOOL_CALL,
            MCP_TOOL_APPROVAL_SOURCE_KEY: MCP_TOOL_APPROVAL_SOURCE_CONNECTOR,
            MCP_TOOL_APPROVAL_CONNECTOR_ID_KEY: "calendar",
            MCP_TOOL_APPROVAL_CONNECTOR_NAME_KEY: "Calendar",
            MCP_TOOL_APPROVAL_CONNECTOR_DESCRIPTION_KEY: "Manage events and schedules.",
            MCP_TOOL_APPROVAL_TOOL_TITLE_KEY: "Run Action",
            MCP_TOOL_APPROVAL_TOOL_DESCRIPTION_KEY: "Runs the selected action.",
            MCP_TOOL_APPROVAL_TOOL_PARAMS_KEY: {
                "calendar_id": "primary",
            },
        }))
    );
}

#[test]
fn approval_elicitation_meta_merges_session_and_always_persist_with_connector_source() {
    assert_eq!(
        build_mcp_tool_approval_elicitation_meta(
            CODEX_APPS_MCP_SERVER_NAME,
            Some(&approval_metadata(
                Some("calendar"),
                Some("Calendar"),
                Some("Manage events and schedules."),
                Some("Run Action"),
                Some("Runs the selected action."),
            )),
            Some(&serde_json::json!({
                "calendar_id": "primary",
            })),
            /*tool_params_display*/ None,
            prompt_options(
                /*allow_session_remember*/ true, /*allow_persistent_approval*/ true
            ),
        ),
        Some(serde_json::json!({
            MCP_TOOL_APPROVAL_KIND_KEY: MCP_TOOL_APPROVAL_KIND_MCP_TOOL_CALL,
            MCP_TOOL_APPROVAL_PERSIST_KEY: [
                MCP_TOOL_APPROVAL_PERSIST_SESSION,
                MCP_TOOL_APPROVAL_PERSIST_ALWAYS,
            ],
            MCP_TOOL_APPROVAL_SOURCE_KEY: MCP_TOOL_APPROVAL_SOURCE_CONNECTOR,
            MCP_TOOL_APPROVAL_CONNECTOR_ID_KEY: "calendar",
            MCP_TOOL_APPROVAL_CONNECTOR_NAME_KEY: "Calendar",
            MCP_TOOL_APPROVAL_CONNECTOR_DESCRIPTION_KEY: "Manage events and schedules.",
            MCP_TOOL_APPROVAL_TOOL_TITLE_KEY: "Run Action",
            MCP_TOOL_APPROVAL_TOOL_DESCRIPTION_KEY: "Runs the selected action.",
            MCP_TOOL_APPROVAL_TOOL_PARAMS_KEY: {
                "calendar_id": "primary",
            },
        }))
    );
}

#[test]
fn declined_elicitation_response_stays_decline() {
    let response = parse_mcp_tool_approval_elicitation_response(
        Some(ElicitationResponse {
            action: ElicitationAction::Decline,
            content: Some(serde_json::json!({
                "approval": MCP_TOOL_APPROVAL_ACCEPT,
            })),
            meta: None,
        }),
        "approval",
    );

    assert_eq!(
        response,
        ReviewDecision::denied("user rejected MCP tool call")
    );
}

#[test]
fn accepted_elicitation_response_uses_always_persist_meta() {
    let response = parse_mcp_tool_approval_elicitation_response(
        Some(ElicitationResponse {
            action: ElicitationAction::Accept,
            content: None,
            meta: Some(serde_json::json!({
                MCP_TOOL_APPROVAL_PERSIST_KEY: MCP_TOOL_APPROVAL_PERSIST_ALWAYS,
            })),
        }),
        "approval",
    );

    assert_eq!(response, ReviewDecision::ApprovedMcpPolicyAmendment);
}

#[test]
fn accepted_elicitation_response_uses_session_persist_meta() {
    let response = parse_mcp_tool_approval_elicitation_response(
        Some(ElicitationResponse {
            action: ElicitationAction::Accept,
            content: None,
            meta: Some(serde_json::json!({
                MCP_TOOL_APPROVAL_PERSIST_KEY: MCP_TOOL_APPROVAL_PERSIST_SESSION,
            })),
        }),
        "approval",
    );

    assert_eq!(response, ReviewDecision::ApprovedForSession);
}

#[test]
fn accepted_elicitation_without_content_defaults_to_accept() {
    let response = parse_mcp_tool_approval_elicitation_response(
        Some(ElicitationResponse {
            action: ElicitationAction::Accept,
            content: None,
            meta: None,
        }),
        "approval",
    );

    assert_eq!(response, ReviewDecision::Approved);
}

#[tokio::test]
async fn persist_custom_mcp_tool_approval_writes_tool_override() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        "[mcp_servers.docs]\ncommand = \"docs-server\"\n",
    )
    .expect("seed config");
    let config = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .build()
        .await
        .expect("load config");

    persist_custom_mcp_tool_approval(&config, "docs", "search")
        .await
        .expect("persist approval");

    let contents = std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("read config");
    let parsed: ConfigToml = toml::from_str(&contents).expect("parse config");
    let tool = parsed
        .mcp_servers
        .get("docs")
        .and_then(|server| server.tools.get("search"))
        .expect("docs/search tool config exists");

    assert_eq!(
        tool,
        &McpServerToolConfig {
            approval_mode: Some(AppToolApproval::Approve),
            ..Default::default()
        }
    );
    assert!(contents.contains("[mcp_servers.docs.tools.search]"));
}

#[tokio::test]
async fn custom_mcp_tool_approval_mode_uses_server_default_with_tool_override() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        r#"
[mcp_servers.docs]
command = "docs-server"
default_tools_approval_mode = "approve"

[mcp_servers.docs.tools.search]
approval_mode = "prompt"
"#,
    )
    .expect("seed config");
    let config = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .build()
        .await
        .expect("load config");
    let (session, mut turn_context) = make_session_and_context().await;
    turn_context.config = Arc::new(config);

    assert_eq!(
        custom_mcp_tool_approval_mode(&session, &turn_context, "docs", "read").await,
        AppToolApproval::Approve
    );
    assert_eq!(
        custom_mcp_tool_approval_mode(&session, &turn_context, "docs", "search").await,
        AppToolApproval::Prompt
    );
    assert_eq!(
        custom_mcp_tool_approval_mode(&session, &turn_context, "unknown", "search").await,
        AppToolApproval::Auto
    );
}

#[tokio::test]
async fn custom_mcp_tool_approval_mode_uses_plugin_mcp_policy() {
    let (session, mut turn_context) = make_session_and_context().await;
    let codex_home = session.codex_home().await;
    write_sample_plugin_mcp(codex_home.as_path());
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"
[features]
plugins = true

[plugins."sample@test"]
enabled = true

[plugins."sample@test".mcp_servers.sample]
default_tools_approval_mode = "prompt"

[plugins."sample@test".mcp_servers.sample.tools.search]
approval_mode = "approve"
"#,
    )
    .expect("seed config");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .build()
        .await
        .expect("load config");
    turn_context.config = Arc::new(config);
    session.services.plugins_manager.clear_cache();

    assert_eq!(
        custom_mcp_tool_approval_mode(&session, &turn_context, "sample", "read").await,
        AppToolApproval::Prompt
    );
    assert_eq!(
        custom_mcp_tool_approval_mode(&session, &turn_context, "sample", "search").await,
        AppToolApproval::Approve
    );
}

#[tokio::test]
async fn custom_mcp_tool_approval_mode_uses_updated_plugin_mcp_policy_after_cache_warm() {
    let (session, mut turn_context) = make_session_and_context().await;
    let codex_home = session.codex_home().await;
    write_sample_plugin_mcp(codex_home.as_path());
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"
[features]
plugins = true

[plugins."sample@test"]
enabled = true
"#,
    )
    .expect("seed config");
    let initial_config = ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .build()
        .await
        .expect("load initial config");
    session
        .services
        .plugins_manager
        .plugins_for_config(&initial_config.plugins_config_input())
        .await;
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"
[features]
plugins = true

[plugins."sample@test"]
enabled = true

[plugins."sample@test".mcp_servers.sample.tools.search]
approval_mode = "approve"
"#,
    )
    .expect("update config");
    let updated_config = ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .build()
        .await
        .expect("load updated config");
    turn_context.config = Arc::new(updated_config);

    assert_eq!(
        custom_mcp_tool_approval_mode(&session, &turn_context, "sample", "search").await,
        AppToolApproval::Approve
    );
}

#[tokio::test]
#[test_case::test_case("docs"; "ordinary_server")]
#[test_case::test_case("codex_apps"; "formerly_reserved_name")]
async fn maybe_persist_mcp_tool_approval_reloads_session_config_for_custom_server(
    server_name: &str,
) {
    let (session, mut turn_context) = make_session_and_context().await;
    let codex_home = session.codex_home().await;
    std::fs::create_dir_all(&codex_home).expect("create codex home");
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        format!("[mcp_servers.{server_name}]\ncommand = \"docs-server\"\n"),
    )
    .expect("seed config");
    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.clone().to_path_buf())
        .build()
        .await
        .expect("load config");
    turn_context.config = Arc::new(config);
    let key = McpToolApprovalKey {
        server: server_name.to_string(),
        plugin_id: None,
        connector_id: None,
        link_id: None,
        tool_name: "search".to_string(),
    };

    maybe_persist_mcp_tool_approval(&session, &turn_context, key.clone()).await;

    let config = session.get_config().await;
    let mcp_servers_toml = config
        .config_layer_stack
        .effective_config()
        .as_table()
        .and_then(|table| table.get("mcp_servers"))
        .cloned()
        .expect("mcp_servers table");
    let mcp_servers = HashMap::<String, McpServerConfig>::deserialize(mcp_servers_toml)
        .expect("deserialize MCP servers");
    let tool = mcp_servers
        .get(server_name)
        .and_then(|server| server.tools.get("search"))
        .expect("docs/search tool config exists");

    assert_eq!(
        tool,
        &McpServerToolConfig {
            approval_mode: Some(AppToolApproval::Approve),
            ..Default::default()
        }
    );
    assert_eq!(mcp_tool_approval_is_remembered(&session, &key).await, true);
}

#[tokio::test]
async fn maybe_persist_mcp_tool_approval_writes_plugin_mcp_policy() {
    let (session, turn_context) = make_session_and_context().await;
    let codex_home = session.codex_home().await;
    std::fs::create_dir_all(&codex_home).expect("create codex home");
    let key = McpToolApprovalKey {
        server: "sample".to_string(),
        plugin_id: Some("sample@test".to_string()),
        connector_id: None,
        link_id: None,
        tool_name: "search".to_string(),
    };

    maybe_persist_mcp_tool_approval(&session, &turn_context, key.clone()).await;

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let parsed: ConfigToml = toml::from_str(&contents).expect("parse config");
    let tool = parsed
        .plugins
        .get("sample@test")
        .and_then(|plugin| plugin.mcp_servers.get("sample"))
        .and_then(|server| server.tools.get("search"))
        .expect("sample/search tool config exists");

    assert_eq!(
        tool,
        &McpServerToolConfig {
            approval_mode: Some(AppToolApproval::Approve),
            ..Default::default()
        }
    );
    assert!(contents.contains(r#"[plugins."sample@test".mcp_servers.sample.tools.search]"#));
    assert_eq!(mcp_tool_approval_is_remembered(&session, &key).await, true);
}

#[tokio::test]
async fn maybe_persist_mcp_tool_approval_writes_project_config_for_project_server() {
    let (session, mut turn_context) = make_session_and_context().await;
    let codex_home = session.codex_home().await;
    let project_dir = tempdir().expect("tempdir");
    std::fs::write(project_dir.path().join(".git"), "gitdir: nowhere").expect("seed git marker");
    let project_codex_dir = project_dir.path().join(".codex");
    std::fs::create_dir_all(&project_codex_dir).expect("create project .codex dir");
    std::fs::write(
        project_codex_dir.join(CONFIG_TOML_FILE),
        "[mcp_servers.docs]\ncommand = \"docs-server\"\n",
    )
    .expect("seed project config");
    ConfigEditsBuilder::new(&codex_home)
        .set_project_trust_level(
            project_dir.path(),
            codex_protocol::config_types::TrustLevel::Trusted,
        )
        .apply()
        .await
        .expect("trust project");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .fallback_cwd(Some(project_dir.path().to_path_buf()))
        .build()
        .await
        .expect("load project config");
    turn_context.config = Arc::new(config);
    let key = McpToolApprovalKey {
        server: "docs".to_string(),
        plugin_id: None,
        connector_id: None,
        link_id: None,
        tool_name: "search".to_string(),
    };

    maybe_persist_mcp_tool_approval(&session, &turn_context, key.clone()).await;

    let contents = std::fs::read_to_string(project_codex_dir.join(CONFIG_TOML_FILE))
        .expect("read project config");
    let parsed: ConfigToml = toml::from_str(&contents).expect("parse project config");
    let tool = parsed
        .mcp_servers
        .get("docs")
        .and_then(|server| server.tools.get("search"))
        .expect("docs/search tool config exists");

    assert_eq!(
        tool,
        &McpServerToolConfig {
            approval_mode: Some(AppToolApproval::Approve),
            ..Default::default()
        }
    );
    assert!(contents.contains("[mcp_servers.docs.tools.search]"));
    assert_eq!(mcp_tool_approval_is_remembered(&session, &key).await, true);
}

#[tokio::test]
async fn approve_mode_skips_when_annotations_do_not_require_approval() {
    let (session, turn_context) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let invocation = McpInvocation {
        server: "custom_server".to_string(),
        tool: "read_only_tool".to_string(),
        arguments: None,
    };
    let metadata = McpToolApprovalMetadata {
        annotations: Some(annotations(
            Some(true),
            /*destructive*/ None,
            /*open_world*/ None,
        )),
        connector_id: None,
        link_id: None,
        connector_name: None,
        connector_description: None,
        connected_account_email: None,
        plugin_id: None,
        tool_title: Some("Read Only Tool".to_string()),
        tool_description: None,
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
    };

    let decision = maybe_request_mcp_tool_approval(
        &session,
        &StepContext::for_test(Arc::clone(&turn_context)),
        &CancellationToken::new(),
        "call-1",
        &invocation,
        &ToolName::namespaced(&invocation.server, &invocation.tool),
        &HookToolName::new("mcp__test__tool"),
        &metadata,
        &approval_config(&turn_context),
        turn_context.config.permissions.permission_profile(),
        McpToolApprovalPolicy::for_server(AppToolApproval::Approve),
    )
    .await;

    assert_eq!(decision, None);
}

#[tokio::test]
async fn guardian_mode_skips_auto_when_annotations_do_not_require_approval() {
    use wiremock::Mock;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    let server = start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let (mut session, mut turn_context) = make_session_and_context().await;
    Arc::make_mut(&mut turn_context.config)
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("test setup should allow updating approval policy");
    let mut config = (*turn_context.config).clone();
    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    config.approvals_reviewer = ApprovalsReviewer::AutoReview;
    let config = Arc::new(config);
    let models_manager = models_manager_with_provider(
        config.codex_home.to_path_buf(),
        Arc::clone(&session.services.auth_manager),
        config.model_provider.clone(),
    );
    session.services.models_manager = models_manager;
    turn_context.config = Arc::clone(&config);
    turn_context.provider = create_model_provider(
        config.model_provider.clone(),
        turn_context.auth_manager.clone(),
    );

    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let invocation = McpInvocation {
        server: "custom_server".to_string(),
        tool: "read_only_tool".to_string(),
        arguments: None,
    };
    let metadata = McpToolApprovalMetadata {
        annotations: Some(annotations(
            Some(true),
            /*destructive*/ None,
            /*open_world*/ None,
        )),
        connector_id: None,
        link_id: None,
        connector_name: None,
        connector_description: None,
        connected_account_email: None,
        plugin_id: None,
        tool_title: Some("Read Only Tool".to_string()),
        tool_description: None,
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
    };

    let decision = maybe_request_mcp_tool_approval(
        &session,
        &StepContext::for_test(Arc::clone(&turn_context)),
        &CancellationToken::new(),
        "call-guardian",
        &invocation,
        &ToolName::namespaced(&invocation.server, &invocation.tool),
        &HookToolName::new("mcp__test__tool"),
        &metadata,
        &approval_config(&turn_context),
        turn_context.config.permissions.permission_profile(),
        McpToolApprovalPolicy::for_server(AppToolApproval::Auto),
    )
    .await;

    assert_eq!(decision, None);
}

#[tokio::test]
async fn permission_request_hook_allows_mcp_tool_call() {
    let (mut session, turn_context) = make_session_and_context().await;
    let log_path = install_mcp_permission_request_hook(
        &mut session,
        &turn_context,
        "mcp__memory__.*",
        &serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "allow" }
            }
        }),
    );
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let invocation = McpInvocation {
        server: "memory".to_string(),
        tool: "create_entities".to_string(),
        arguments: Some(serde_json::json!({
            "entities": [{
                "name": "Ada",
                "entityType": "person"
            }]
        })),
    };
    let metadata = McpToolApprovalMetadata {
        annotations: Some(annotations(
            Some(false),
            Some(true),
            /*open_world*/ None,
        )),
        connector_id: None,
        link_id: None,
        connector_name: None,
        connector_description: None,
        connected_account_email: None,
        plugin_id: None,
        tool_title: Some("Create entities".to_string()),
        tool_description: None,
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
    };

    let decision = maybe_request_mcp_tool_approval(
        &session,
        &StepContext::for_test(Arc::clone(&turn_context)),
        &CancellationToken::new(),
        "call-mcp-hook",
        &invocation,
        &ToolName::namespaced(&invocation.server, &invocation.tool),
        &HookToolName::new("mcp__memory__create_entities"),
        &metadata,
        &approval_config(&turn_context),
        turn_context.config.permissions.permission_profile(),
        McpToolApprovalPolicy::for_server(AppToolApproval::Auto),
    )
    .await;

    assert_eq!(decision, Some(ReviewDecision::Approved));
    let log = std::fs::read_to_string(log_path).expect("read MCP permission hook log");
    let inputs = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse hook input"))
        .collect::<Vec<_>>();
    #[allow(deprecated)]
    let turn_cwd = turn_context.cwd.clone();
    assert_eq!(
        inputs,
        vec![serde_json::json!({
            "session_id": session.session_id(),
            "turn_id": "turn_id",
            "cwd": turn_cwd,
            "transcript_path": null,
            "model": turn_context.model_info().slug,
            "permission_mode": "default",
            "tool_name": "mcp__memory__create_entities",
            "hook_event_name": "PermissionRequest",
            "tool_input": {
                "entities": [{
                    "name": "Ada",
                    "entityType": "person"
                }]
            }
        })]
    );
}

#[tokio::test]
async fn permission_request_hook_uses_hook_tool_name_without_metadata() {
    let (mut session, turn_context) = make_session_and_context().await;
    let log_path = install_mcp_permission_request_hook(
        &mut session,
        &turn_context,
        "mcp__memory__.*",
        &serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "allow" }
            }
        }),
    );
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let invocation = McpInvocation {
        server: "memory".to_string(),
        tool: "create_entities".to_string(),
        arguments: Some(serde_json::json!({ "entities": [] })),
    };
    let metadata = approval_metadata(
        /*connector_id*/ None, /*connector_name*/ None,
        /*connector_description*/ None, /*tool_title*/ None,
        /*tool_description*/ None,
    );

    let decision = maybe_request_mcp_tool_approval(
        &session,
        &StepContext::for_test(Arc::clone(&turn_context)),
        &CancellationToken::new(),
        "call-mcp-hook-no-metadata",
        &invocation,
        &ToolName::namespaced(&invocation.server, &invocation.tool),
        &HookToolName::new("mcp__memory__create_entities"),
        &metadata,
        &approval_config(&turn_context),
        turn_context.config.permissions.permission_profile(),
        McpToolApprovalPolicy::for_server(AppToolApproval::Auto),
    )
    .await;

    assert_eq!(decision, Some(ReviewDecision::Approved));
    let log = std::fs::read_to_string(log_path).expect("read MCP permission hook log");
    let inputs = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse hook input"))
        .collect::<Vec<_>>();
    #[allow(deprecated)]
    let turn_cwd = turn_context.cwd.clone();
    assert_eq!(
        inputs,
        vec![serde_json::json!({
            "session_id": session.session_id(),
            "turn_id": "turn_id",
            "cwd": turn_cwd,
            "transcript_path": null,
            "model": turn_context.model_info().slug,
            "permission_mode": "default",
            "tool_name": "mcp__memory__create_entities",
            "hook_event_name": "PermissionRequest",
            "tool_input": { "entities": [] }
        })]
    );
}

#[tokio::test]
async fn permission_request_hook_runs_after_remembered_mcp_approval() {
    let (mut session, turn_context) = make_session_and_context().await;
    let log_path = install_mcp_permission_request_hook(
        &mut session,
        &turn_context,
        "mcp__memory__.*",
        &serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {
                    "behavior": "deny",
                    "message": "should be skipped"
                }
            }
        }),
    );
    let invocation = McpInvocation {
        server: "memory".to_string(),
        tool: "create_entities".to_string(),
        arguments: Some(serde_json::json!({ "entities": [] })),
    };
    let metadata = McpToolApprovalMetadata {
        annotations: Some(annotations(
            Some(false),
            Some(true),
            /*open_world*/ None,
        )),
        connector_id: None,
        link_id: None,
        connector_name: None,
        connector_description: None,
        connected_account_email: None,
        plugin_id: None,
        tool_title: Some("Create entities".to_string()),
        tool_description: None,
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
    };
    let remembered_key =
        session_mcp_tool_approval_key(&invocation, Some(&metadata), AppToolApproval::Auto)
            .expect("memory MCP tool should support session approval");
    remember_mcp_tool_approval(&session, remembered_key).await;

    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let decision = maybe_request_mcp_tool_approval(
        &session,
        &StepContext::for_test(Arc::clone(&turn_context)),
        &CancellationToken::new(),
        "call-mcp-remembered",
        &invocation,
        &ToolName::namespaced(&invocation.server, &invocation.tool),
        &HookToolName::new("mcp__memory__create_entities"),
        &metadata,
        &approval_config(&turn_context),
        turn_context.config.permissions.permission_profile(),
        McpToolApprovalPolicy::for_server(AppToolApproval::Auto),
    )
    .await;

    assert_eq!(decision, Some(ReviewDecision::Approved));
    assert!(
        !log_path.exists(),
        "remembered approval should skip PermissionRequest hooks"
    );
}

#[tokio::test]
async fn strict_auto_review_forces_guardian_for_mcp_policy_skip() {
    let server = start_mock_server().await;
    let guardian_request_log = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-guardian"),
            ev_assistant_message(
                "msg-guardian",
                &serde_json::json!({
                    "risk_level": "high",
                    "user_authorization": "low",
                    "outcome": "deny",
                    "rationale": "The tool call would expose private calendar data without clear user authorization.",
                })
                .to_string(),
            ),
            ev_completed("resp-guardian"),
        ]),
    )
    .await;

    let (mut session, mut turn_context) = make_session_and_context().await;
    Arc::make_mut(&mut turn_context.config)
        .permissions
        .approval_policy
        .set(AskForApproval::UnlessTrusted)
        .expect("test setup should allow updating approval policy");
    let mut config = (*turn_context.config).clone();
    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    config.approvals_reviewer = ApprovalsReviewer::User;
    let config = Arc::new(config);
    let models_manager = models_manager_with_provider(
        config.codex_home.to_path_buf(),
        Arc::clone(&session.services.auth_manager),
        config.model_provider.clone(),
    );
    session.services.models_manager = models_manager;
    turn_context.config = Arc::clone(&config);
    turn_context.provider = create_model_provider(
        config.model_provider.clone(),
        turn_context.auth_manager.clone(),
    );

    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let invocation = McpInvocation {
        server: "custom_server".to_string(),
        tool: "dangerous_tool".to_string(),
        arguments: Some(serde_json::json!({ "calendar_id": "primary" })),
    };
    let metadata = McpToolApprovalMetadata {
        annotations: Some(annotations(Some(false), Some(true), Some(true))),
        connector_id: None,
        link_id: None,
        connector_name: None,
        connector_description: None,
        connected_account_email: None,
        plugin_id: None,
        tool_title: Some("Dangerous Tool".to_string()),
        tool_description: Some("Reads calendar data.".to_string()),
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
    };
    let mut captured_mcp_config = approval_config(&turn_context);
    captured_mcp_config
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("captured MCP policy should allow updating approval policy");

    turn_context.record_granted_permissions(
        codex_exec_server::LOCAL_ENVIRONMENT_ID,
        Default::default(),
        /*strict_auto_review*/ true,
    );
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let decision = maybe_request_mcp_tool_approval(
        &session,
        &step_context,
        &CancellationToken::new(),
        "call-guardian-deny",
        &invocation,
        &ToolName::namespaced(&invocation.server, &invocation.tool),
        &HookToolName::new("mcp__test__tool"),
        &metadata,
        &captured_mcp_config,
        &captured_mcp_config.permission_profile,
        McpToolApprovalPolicy::for_server(AppToolApproval::Approve),
    )
    .await;

    let Some(ReviewDecision::Denied { rejection: message }) = decision else {
        panic!("guardian-denied MCP approval should carry a rejection message");
    };
    assert!(message.contains("Reason: The tool call would expose private calendar data"));
    assert!(message.contains("policy circumvention"));
    assert_eq!(
        guardian_request_log.single_request().path(),
        "/v1/responses"
    );
}

#[tokio::test]
async fn session_approval_preserves_mcp_session_persistence_choice() {
    assert_mcp_user_approval_persistence(
        MCP_TOOL_APPROVAL_PERSIST_SESSION,
        ReviewDecision::ApprovedForSession,
    )
    .await;
}

#[tokio::test]
async fn session_approval_preserves_mcp_always_persistence_choice() {
    assert_mcp_user_approval_persistence(
        MCP_TOOL_APPROVAL_PERSIST_ALWAYS,
        ReviewDecision::ApprovedMcpPolicyAmendment,
    )
    .await;
}

async fn assert_mcp_user_approval_persistence(
    persistence: &'static str,
    expected_decision: ReviewDecision,
) {
    let (session, turn_context, rx_event) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let invocation = McpInvocation {
        server: "memory".to_string(),
        tool: "create_entities".to_string(),
        arguments: Some(serde_json::json!({ "entities": [] })),
    };
    let metadata = McpToolApprovalMetadata {
        annotations: Some(annotations(
            Some(false),
            Some(true),
            /*open_world*/ None,
        )),
        connector_id: None,
        link_id: None,
        connector_name: None,
        connector_description: None,
        connected_account_email: None,
        plugin_id: None,
        tool_title: Some("Create entities".to_string()),
        tool_description: None,
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
    };

    let approval_task = tokio::spawn({
        let session = Arc::clone(&session);
        let turn_context = Arc::clone(&turn_context);
        async move {
            maybe_request_mcp_tool_approval(
                &session,
                &StepContext::for_test(Arc::clone(&turn_context)),
                &CancellationToken::new(),
                "call-mcp-persist",
                &invocation,
                &ToolName::namespaced(&invocation.server, &invocation.tool),
                &HookToolName::new("mcp__memory__create_entities"),
                &metadata,
                &approval_config(&turn_context),
                turn_context.config.permissions.permission_profile(),
                McpToolApprovalPolicy::for_server(AppToolApproval::Auto),
            )
            .await
        }
    });

    let request = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx_event.recv())
            .await
            .expect("MCP approval elicitation timed out")
            .expect("expected MCP approval elicitation event");
        if let EventMsg::ElicitationRequest(request) = event.msg {
            break request;
        }
    };
    assert_eq!(request.server_name, "memory");
    session
        .resolve_elicitation(
            "memory".to_string(),
            rmcp::model::RequestId::String("mcp_tool_call_approval_call-mcp-persist".into()),
            ElicitationResponse {
                action: ElicitationAction::Accept,
                content: None,
                meta: Some(serde_json::json!({
                    MCP_TOOL_APPROVAL_PERSIST_KEY: persistence,
                })),
            },
        )
        .await
        .expect("MCP approval elicitation should resolve");

    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), approval_task)
            .await
            .expect("MCP approval task timed out")
            .expect("MCP approval task failed"),
        Some(expected_decision),
    );
}

#[tokio::test]
async fn prompt_mode_waits_for_approval_when_annotations_do_not_require_approval() {
    let (session, turn_context, _rx_event) = make_session_and_context_with_rx().await;
    {
        let mut active_turn = session.active_turn.lock().await;
        *active_turn = Some(ActiveTurn::default());
    }
    let invocation = McpInvocation {
        server: "custom_server".to_string(),
        tool: "read_only_tool".to_string(),
        arguments: None,
    };
    let metadata = McpToolApprovalMetadata {
        annotations: Some(annotations(
            Some(true),
            /*destructive*/ None,
            /*open_world*/ None,
        )),
        connector_id: None,
        link_id: None,
        connector_name: None,
        connector_description: None,
        connected_account_email: None,
        plugin_id: None,
        tool_title: Some("Read Only Tool".to_string()),
        tool_description: None,
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
    };

    let mut approval_task = {
        let session = Arc::clone(&session);
        let turn_context = Arc::clone(&turn_context);
        tokio::spawn(async move {
            maybe_request_mcp_tool_approval(
                &session,
                &StepContext::for_test(Arc::clone(&turn_context)),
                &CancellationToken::new(),
                "call-prompt",
                &invocation,
                &ToolName::namespaced(&invocation.server, &invocation.tool),
                &HookToolName::new("mcp__test__tool"),
                &metadata,
                &approval_config(&turn_context),
                turn_context.config.permissions.permission_profile(),
                McpToolApprovalPolicy::for_server(AppToolApproval::Prompt),
            )
            .await
        })
    };

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), &mut approval_task)
            .await
            .is_err(),
        "prompt mode should wait for approval instead of auto-allowing"
    );
    approval_task.abort();
}

#[tokio::test]
async fn full_access_mode_skips_mcp_tool_approval_for_all_approval_modes() {
    let (session, mut turn_context) = make_session_and_context().await;
    Arc::make_mut(&mut turn_context.config)
        .permissions
        .approval_policy
        .set(AskForApproval::Never)
        .expect("test setup should allow updating approval policy");
    Arc::make_mut(&mut turn_context.config)
        .permissions
        .set_permission_profile(PermissionProfile::Disabled)
        .expect("test setup should allow updating permission profile");

    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let invocation = McpInvocation {
        server: CODEX_APPS_MCP_SERVER_NAME.to_string(),
        tool: "dangerous_tool".to_string(),
        arguments: Some(serde_json::json!({ "id": 1 })),
    };
    let metadata = McpToolApprovalMetadata {
        annotations: Some(annotations(Some(false), Some(true), Some(true))),
        connector_id: Some("calendar".to_string()),
        link_id: None,
        connector_name: Some("Calendar".to_string()),
        connector_description: Some("Manage events".to_string()),
        connected_account_email: None,
        plugin_id: None,
        tool_title: Some("Dangerous Tool".to_string()),
        tool_description: Some("Performs a risky action.".to_string()),
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
    };

    for approval_mode in [
        AppToolApproval::Auto,
        AppToolApproval::Prompt,
        AppToolApproval::Approve,
    ] {
        let decision = maybe_request_mcp_tool_approval(
            &session,
            &StepContext::for_test(Arc::clone(&turn_context)),
            &CancellationToken::new(),
            "call-2",
            &invocation,
            &ToolName::namespaced(&invocation.server, &invocation.tool),
            &HookToolName::new("mcp__test__tool"),
            &metadata,
            &approval_config(&turn_context),
            turn_context.config.permissions.permission_profile(),
            McpToolApprovalPolicy::for_server(approval_mode),
        )
        .await;

        assert_eq!(decision, None);
    }
}

#[tokio::test]
async fn approve_mode_skips_guardian_in_every_permission_mode() {
    use wiremock::Mock;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    let server = start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let invocation = McpInvocation {
        server: CODEX_APPS_MCP_SERVER_NAME.to_string(),
        tool: "dangerous_tool".to_string(),
        arguments: Some(serde_json::json!({ "id": 1 })),
    };
    let metadata = McpToolApprovalMetadata {
        annotations: Some(annotations(Some(false), Some(true), Some(true))),
        connector_id: Some("calendar".to_string()),
        link_id: None,
        connector_name: Some("Calendar".to_string()),
        connector_description: Some("Manage events".to_string()),
        connected_account_email: None,
        plugin_id: None,
        tool_title: Some("Dangerous Tool".to_string()),
        tool_description: Some("Performs a risky action.".to_string()),
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
    };

    for approval_policy in [
        AskForApproval::UnlessTrusted,
        AskForApproval::OnRequest,
        AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: true,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: true,
        }),
        AskForApproval::Never,
    ] {
        let (mut session, mut turn_context) = make_session_and_context().await;
        turn_context.auth_manager = Some(crate::test_support::auth_manager_from_auth(
            codex_login::CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        ));
        Arc::make_mut(&mut turn_context.config)
            .permissions
            .approval_policy
            .set(approval_policy)
            .expect("test setup should allow updating approval policy");
        let mut config = (*turn_context.config).clone();
        config.chatgpt_base_url = server.uri();
        config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
        config.approvals_reviewer = ApprovalsReviewer::User;
        let config = Arc::new(config);
        let models_manager = models_manager_with_provider(
            config.codex_home.to_path_buf(),
            Arc::clone(&session.services.auth_manager),
            config.model_provider.clone(),
        );
        session.services.models_manager = models_manager;
        turn_context.config = Arc::clone(&config);
        turn_context.provider = create_model_provider(
            config.model_provider.clone(),
            turn_context.auth_manager.clone(),
        );

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let decision = maybe_request_mcp_tool_approval(
            &session,
            &StepContext::for_test(Arc::clone(&turn_context)),
            &CancellationToken::new(),
            "call-3",
            &invocation,
            &ToolName::namespaced(&invocation.server, &invocation.tool),
            &HookToolName::new("mcp__test__tool"),
            &metadata,
            &approval_config(&turn_context),
            turn_context.config.permissions.permission_profile(),
            McpToolApprovalPolicy::for_server(AppToolApproval::Approve),
        )
        .await;

        assert_eq!(decision, None);
    }
}

#[tokio::test]
async fn approval_metadata_is_released_when_the_invocation_future_is_dropped() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let invocation = McpInvocation {
        server: CODEX_APPS_MCP_SERVER_NAME.to_string(),
        tool: "write_record".to_string(),
        arguments: Some(serde_json::json!({"value": 42})),
    };
    let metadata = approval_metadata(
        Some("connector"),
        /*connector_name*/ None,
        /*connector_description*/ None,
        /*tool_title*/ None,
        /*tool_description*/ None,
    );
    let mut call = Box::pin(async {
        let _approval_metadata =
            session.register_mcp_tool_approval_metadata("call", &invocation, metadata.clone());
        std::future::pending::<()>().await;
    });
    assert!(futures::poll!(call.as_mut()).is_pending());
    let _other_metadata =
        session.register_mcp_tool_approval_metadata("other-call", &invocation, metadata.clone());
    assert_eq!(
        session
            .mcp_tool_approval_metadata(CODEX_APPS_MCP_SERVER_NAME, "call")
            .map(|(invocation, metadata)| (invocation, metadata.connector_id)),
        Some((Some(invocation.clone()), Some("connector".to_string()))),
    );
    assert!(
        session
            .mcp_tool_approval_metadata("another-server", "call")
            .is_none()
    );
    drop(call);
    assert!(
        session
            .mcp_tool_approval_metadata(CODEX_APPS_MCP_SERVER_NAME, "call")
            .is_none()
    );
    let _next_metadata =
        session.register_mcp_tool_approval_metadata("next-call", &invocation, metadata);
    let registry = session.mcp_tool_approval_metadata.lock().unwrap();
    let mut keys = registry.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            (
                CODEX_APPS_MCP_SERVER_NAME.to_string(),
                "next-call".to_string()
            ),
            (
                CODEX_APPS_MCP_SERVER_NAME.to_string(),
                "other-call".to_string()
            ),
        ],
    );
}
