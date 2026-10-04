#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use codex_config::LoaderOverrides;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::set_project_trust_level;
use codex_core_plugins::store::PluginStore;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_model_provider_info::AMAZON_BEDROCK_PROVIDER_ID;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_plugin::PluginId;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::config_types::TrustLevel;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
#[cfg(unix)]
use codex_protocol::protocol::GranularApprovalConfig;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use codex_skills_extension::HostSkillsLoadInput;
use codex_skills_extension::SkillsExtensionConfig;
use codex_skills_extension::install;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::apps_test_server::SEARCH_CALENDAR_CREATE_TOOL;
use core_test_support::apps_test_server::recorded_apps_tool_calls;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::ev_tool_search_call;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::namespace_child_tool;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_remote;
#[cfg(unix)]
use core_test_support::skip_if_sandbox;
use core_test_support::skip_if_target_windows;
use core_test_support::stdio_server_bin;
use core_test_support::submit_thread_settings;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use core_test_support::wait_for_mcp_server;
use core_test_support::zsh_fork::zsh_fork_runtime;
use core_test_support::zsh_fork::zsh_fork_test_builder;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use test_case::test_case;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const SAMPLE_PLUGIN_CONFIG_NAME: &str = "sample@test";
const SAMPLE_REMOTE_PLUGIN_CONFIG_NAME: &str = "sample@openai-curated-remote";
const SAMPLE_PLUGIN_DISPLAY_NAME: &str = "sample";
const SAMPLE_PLUGIN_DESCRIPTION: &str = "inspect sample data";
const SAMPLE_REMOTE_PLUGIN_ID: &str = "plugins~Plugin_sample";
const SAMPLE_PLUGIN_APP_NAMESPACE: &str = "mcp__codex_apps__google_calendar";
const SAMPLE_PLUGIN_MCP_NAMESPACE: &str = "mcp__sample";
const PLUGIN_APP_SEARCH_CALL_ID: &str = "plugin-app-search";
const PLUGIN_MCP_SEARCH_CALL_ID: &str = "plugin-mcp-search";
const REMOTE_PLUGIN_CONFIG_NAME: &str = "sample@openai-curated-remote";

fn skills_extensions() -> Arc<ExtensionRegistry<Config>> {
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install(&mut extensions, |config: &Config| SkillsExtensionConfig {
        include_instructions: config.include_skill_instructions,
        max_context_tokens: config.skill_max_context_tokens,
        bundled_skills_enabled: config.bundled_skills_enabled(),
        cloud_skill_enabled: config.cloud_skill_enabled,
    });
    Arc::new(extensions.build())
}

fn sample_plugin_root(home: &TempDir) -> std::path::PathBuf {
    home.path().join("plugins/cache/test/sample/local")
}

pub(super) fn write_sample_plugin_manifest_and_config(home: &TempDir) -> std::path::PathBuf {
    write_sample_plugin_manifest_and_config_at_root(
        home,
        sample_plugin_root(home),
        SAMPLE_PLUGIN_CONFIG_NAME,
    )
}

fn write_sample_plugin_manifest_and_config_at_root(
    home: &TempDir,
    plugin_root: std::path::PathBuf,
    plugin_config_name: &str,
) -> std::path::PathBuf {
    std::fs::create_dir_all(plugin_root.join(".codex-plugin")).expect("create plugin manifest dir");
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        format!(
            r#"{{"name":"{SAMPLE_PLUGIN_DISPLAY_NAME}","description":"{SAMPLE_PLUGIN_DESCRIPTION}"}}"#
        ),
    )
    .expect("write plugin manifest");
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[features]\nplugins = true\n\n[plugins.\"{plugin_config_name}\"]\nenabled = true\n"
        ),
    )
    .expect("write config");
    plugin_root
}

fn write_plugin_skill_plugin(home: &TempDir) -> std::path::PathBuf {
    write_sample_plugin_skill(write_sample_plugin_manifest_and_config(home))
}

fn write_sample_plugin_skill(plugin_root: std::path::PathBuf) -> std::path::PathBuf {
    let skill_dir = plugin_root.join("skills/sample-search");
    std::fs::create_dir_all(skill_dir.as_path()).expect("create plugin skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: inspect sample data\n---\n\n# body\n",
    )
    .expect("write plugin skill");
    skill_dir.join("SKILL.md")
}

fn write_agent_plugin_skill_plugin(home: &TempDir) -> std::path::PathBuf {
    let plugin_root = home.path().join("plugins/cache/test/acme.tools/local");
    let direct_skill = plugin_root.join("skills/review");
    let nested_skill = plugin_root.join("skills/group/hidden");
    std::fs::create_dir_all(&direct_skill).expect("create direct skill");
    std::fs::create_dir_all(&nested_skill).expect("create nested skill");
    std::fs::write(
        plugin_root.join("plugin.json"),
        r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"acme.tools","extensions":{"com.openai":{"interface":{"displayName":"Acme Developer Tools"}}}}"#,
    )
    .expect("write Agent Plugin manifest");
    std::fs::write(
        direct_skill.join("SKILL.md"),
        format!(
            "---\nname: review\ndescription: Review code\n---\n\n{}\nAGENT_SKILL_TRUNCATED_TAIL\n",
            "x".repeat(9_000)
        ),
    )
    .expect("write direct skill");
    std::fs::write(
        nested_skill.join("SKILL.md"),
        "---\nname: hidden\ndescription: Hidden skill\n---\n\nHidden.\n",
    )
    .expect("write nested skill");
    std::fs::write(
        home.path().join("config.toml"),
        "[features]\nplugins = true\n\n[plugins.\"acme.tools@test\"]\nenabled = true\n",
    )
    .expect("write Agent Plugin config");
    direct_skill.join("SKILL.md")
}

fn write_plugin_mcp_plugin(home: &TempDir, command: &str) {
    let plugin_root = write_sample_plugin_manifest_and_config(home);
    std::fs::write(
        plugin_root.join(".mcp.json"),
        serde_json::to_vec(&serde_json::json!({
            "mcpServers": {
                "sample": {
                    "command": command,
                    "cwd": ".",
                    "startup_timeout_sec": 60.0,
                },
            },
        }))
        .expect("serialize plugin MCP configuration"),
    )
    .expect("write plugin mcp config");
}

fn block_plugin_mcp_startup(home: &TempDir, command: &str) -> std::path::PathBuf {
    let barrier = home.path().join("allow-plugin-initialize");
    std::fs::write(
        sample_plugin_root(home).join(".mcp.json"),
        serde_json::to_vec(&serde_json::json!({
            "mcpServers": {
                "sample": {
                    "command": command,
                    "cwd": ".",
                    "env": {
                        "MCP_TEST_INITIALIZE_BARRIER_FILE": barrier,
                    },
                    "startup_timeout_sec": 10,
                },
            },
        }))
        .expect("serialize blocked plugin MCP configuration"),
    )
    .expect("write blocked plugin MCP configuration");
    barrier
}

fn write_plugin_app_plugin(home: &TempDir) {
    write_plugin_app_plugin_with_name(home, "sample");
}

fn write_plugin_app_plugin_with_name(home: &TempDir, app_name: &str) {
    let plugin_root = write_sample_plugin_manifest_and_config(home);
    std::fs::write(
        plugin_root.join(".app.json"),
        format!(
            r#"{{
  "apps": {{
    "{app_name}": {{
      "id": "calendar"
    }}
  }}
}}"#
        ),
    )
    .expect("write plugin app config");
}

async fn mount_plugin_tool_search_turn(server: &MockServer) -> ResponseMock {
    mount_sse_sequence(
        server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_tool_search_call(
                    PLUGIN_APP_SEARCH_CALL_ID,
                    &serde_json::json!({"query": "create calendar event"}),
                ),
                ev_tool_search_call(
                    PLUGIN_MCP_SEARCH_CALL_ID,
                    &serde_json::json!({"query": "echo"}),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
        ],
    )
    .await
}

fn assert_plugin_provenance(tool: &serde_json::Value) {
    let description = tool
        .get("description")
        .and_then(serde_json::Value::as_str)
        .expect("plugin tool description should be present");
    assert!(
        description.contains("This tool is part of plugin `sample`."),
        "expected plugin provenance in tool description: {description:?}"
    );
}

fn searched_plugin_tools(
    request: &ResponsesRequest,
) -> (Option<serde_json::Value>, Option<serde_json::Value>) {
    let app_output = request.tool_search_output(PLUGIN_APP_SEARCH_CALL_ID);
    let mcp_output = request.tool_search_output(PLUGIN_MCP_SEARCH_CALL_ID);
    (
        namespace_child_tool(
            &app_output,
            SAMPLE_PLUGIN_APP_NAMESPACE,
            SEARCH_CALENDAR_CREATE_TOOL,
        )
        .cloned(),
        namespace_child_tool(&mcp_output, SAMPLE_PLUGIN_MCP_NAMESPACE, "echo").cloned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_disabled_plugins_filter_skills_and_tools_without_changing_shared_plugins()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let home = Arc::new(TempDir::new()?);
    let skill_path = dunce::canonicalize(write_plugin_skill_plugin(home.as_ref()))?;
    std::fs::write(
        &skill_path,
        "---\ndescription: inspect sample data\n---\nTHREAD_PLUGIN_SKILL_BODY\n",
    )?;
    write_plugin_mcp_plugin(home.as_ref(), &stdio_server_bin()?);
    write_plugin_app_plugin_with_name(home.as_ref(), "sample_app");
    let mut builder = test_codex()
        .with_home(home)
        .with_extensions(skills_extensions());
    let test = builder.build_with_remote_and_local_env(&server).await?;
    let manager = test.thread_manager.plugins_manager();
    let plugins_input = test.config.plugins_config_input();
    let shared_plugins = manager.plugins_for_config(&plugins_input).await;
    let tool_calls = [(
        "sample",
        "echo",
        Some(serde_json::json!({"message": "plugin check"})),
    )];

    for (phase, enabled, injection_count) in [(0, true, 1), (1, false, 1), (2, true, 2)] {
        if !enabled {
            submit_thread_settings(
                &test.codex,
                ThreadSettingsOverrides {
                    disabled_plugin_ids: Some(vec![SAMPLE_PLUGIN_CONFIG_NAME.to_string()]),
                    ..Default::default()
                },
            )
            .await?;
            // Saving pending settings must not change the admitted runtime.
            for (server_name, tool, arguments) in &tool_calls {
                test.codex
                    .call_mcp_tool(server_name, tool, arguments.clone(), /*meta*/ None)
                    .await?;
            }
        }
        let mcp_call_id = format!("plugin-mcp-{phase}");
        let mock = mount_sse_sequence(
            &server,
            vec![
                sse(vec![
                    ev_response_created("search"),
                    ev_tool_search_call(&mcp_call_id, &serde_json::json!({"query": "echo"})),
                    ev_completed("search"),
                ]),
                sse(vec![ev_response_created("done"), ev_completed("done")]),
            ],
        )
        .await;
        test.codex
            .start_or_steer_turn(
                TurnInputRequest::user_input(vec![
                    UserInput::Skill {
                        name: "sample:sample-search".into(),
                        path: skill_path.clone(),
                    },
                    UserInput::Mention {
                        name: "sample".into(),
                        path: format!("plugin://{SAMPLE_PLUGIN_CONFIG_NAME}"),
                    },
                ])
                .with_thread_settings(ThreadSettingsOverrides {
                    disabled_plugin_ids: enabled.then(Vec::new),
                    ..Default::default()
                }),
            )
            .await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let requests = mock.requests();
        // Previously injected instructions remain in history; disabled turns must add none.
        for (role, marker) in [
            ("user", "THREAD_PLUGIN_SKILL_BODY"),
            ("developer", "Skills from this plugin"),
        ] {
            assert_eq!(
                requests[0]
                    .message_input_texts(role)
                    .iter()
                    .filter(|text| text.contains(marker))
                    .count(),
                injection_count
            );
        }
        for (call_id, namespace, tool) in [(&mcp_call_id, SAMPLE_PLUGIN_MCP_NAMESPACE, "echo")] {
            assert_eq!(
                namespace_child_tool(&requests[1].tool_search_output(call_id), namespace, tool)
                    .is_some(),
                enabled
            );
        }
        // Direct MCP calls observe the admitted plugin configuration.
        for (server_name, tool, arguments) in &tool_calls {
            let result = test
                .codex
                .call_mcp_tool(server_name, tool, arguments.clone(), /*meta*/ None)
                .await;
            assert_eq!(
                result.is_ok(),
                enabled,
                "unexpected {server_name}/{tool} result: {result:?}"
            );
        }
        assert_eq!(
            manager.plugins_for_config(&plugins_input).await,
            shared_plugins
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_plugin_skills_use_shared_catalog_and_direct_child_discovery() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let resp_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp1"), ev_completed("resp1")]),
    )
    .await;
    let codex_home = Arc::new(TempDir::new()?);
    let skill_path = dunce::canonicalize(write_agent_plugin_skill_plugin(codex_home.as_ref()))?;
    let mut builder = test_codex()
        .with_home(Arc::clone(&codex_home))
        .with_extensions(skills_extensions());
    let test_codex = builder.build_with_auto_env(&server).await?;

    test_codex
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Skill {
            name: "acme.tools:review".into(),
            path: skill_path,
        }]))
        .await?;
    let warning = wait_for_event(&test_codex.codex, |ev| {
        matches!(
            ev,
            EventMsg::Warning(warning)
                if warning.message.contains("main prompt context limit")
        )
    })
    .await;
    wait_for_event(&test_codex.codex, |ev| {
        matches!(ev, EventMsg::TurnComplete(_))
    })
    .await;

    let developer_text = resp_mock
        .single_request()
        .message_input_texts("developer")
        .join("\n");
    assert!(developer_text.contains("acme.tools:review: Review code"));
    assert!(!developer_text.contains("acme.tools:hidden"));
    let user_text = resp_mock
        .single_request()
        .message_input_texts("user")
        .join("\n");
    assert!(user_text.contains("acme.tools:review"));
    assert!(!user_text.contains("AGENT_SKILL_TRUNCATED_TAIL"));
    let EventMsg::Warning(warning) = warning else {
        unreachable!("wait_for_event matched an Agent skill truncation warning")
    };
    assert!(warning.message.contains("acme.tools:review"));
    Ok(())
}

#[test_case("CHATGPT", false, None; "product restricted skill is unavailable")]
#[test_case("CODEX", true, Some("native review skill"); "native skill wins over migrated command")]
#[test_case("CHATGPT", true, Some("migrated review command"); "migrated command replaces filtered native skill")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_skill_product_policy_and_migrated_command_precedence_reach_agent_turns(
    native_skill_product: &str,
    include_migrated_command: bool,
    expected_skill_description: Option<&str>,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp1"), ev_completed("resp1")]),
    )
    .await;
    let codex_home = Arc::new(TempDir::new()?);
    let plugin_root = write_sample_plugin_manifest_and_config(codex_home.as_ref());
    let native_skill_dir = plugin_root.join("skills/review");
    std::fs::create_dir_all(native_skill_dir.join("agents"))?;
    std::fs::write(
        native_skill_dir.join("SKILL.md"),
        "---\nname: source-command-review\ndescription: native review skill\n---\n",
    )?;
    std::fs::write(
        native_skill_dir.join("agents/openai.yaml"),
        format!("policy:\n  products: [{native_skill_product}]\n"),
    )?;
    if include_migrated_command {
        let migrated_skill_dir =
            plugin_root.join(".codex-plugin/migrated-command-skills/source-command-review");
        std::fs::create_dir_all(&migrated_skill_dir)?;
        std::fs::write(
            migrated_skill_dir.join("SKILL.md"),
            "---\nname: source-command-review\ndescription: migrated review command\n---\n",
        )?;
    }

    let mut builder = test_codex()
        .with_home(Arc::clone(&codex_home))
        .with_extensions(skills_extensions());
    let test = builder.build_with_auto_env(&server).await?;
    let plugin_outcome = test
        .thread_manager
        .plugins_manager()
        .plugins_for_config(&test.config.plugins_config_input())
        .await;
    assert_eq!(
        plugin_outcome
            .plugins()
            .iter()
            .map(|plugin| (plugin.config_name.as_str(), plugin.has_enabled_skills))
            .collect::<Vec<_>>(),
        vec![(
            SAMPLE_PLUGIN_CONFIG_NAME,
            expected_skill_description.is_some()
        )]
    );
    assert_eq!(
        plugin_outcome
            .capability_summaries()
            .iter()
            .map(|plugin| (plugin.config_name.as_str(), plugin.has_skills))
            .collect::<Vec<_>>(),
        expected_skill_description
            .map(|_| (SAMPLE_PLUGIN_CONFIG_NAME, true))
            .into_iter()
            .collect::<Vec<_>>()
    );

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Inspect the available plugin skills.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = response.requests();
    let developer_text = requests[0].message_input_texts("developer").join("\n");
    assert_eq!(
        (
            developer_text.contains("sample:source-command-review: native review skill"),
            developer_text.contains("sample:source-command-review: migrated review command"),
        ),
        (
            expected_skill_description == Some("native review skill"),
            expected_skill_description == Some("migrated review command"),
        )
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_plugin_skill_prompt_remains_complete() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let resp_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp1"), ev_completed("resp1")]),
    )
    .await;
    let codex_home = Arc::new(TempDir::new()?);
    let skill_path = write_plugin_skill_plugin(codex_home.as_ref());
    let skill_contents = format!(
        "---\nname: sample-search\ndescription: inspect sample data\n---\n\n{}\nLEGACY_SKILL_FULL_TAIL\n",
        "x".repeat(9_000)
    );
    std::fs::write(&skill_path, &skill_contents)?;
    let skill_path = dunce::canonicalize(skill_path)?;
    let mut builder = test_codex()
        .with_home(codex_home)
        .with_extensions(skills_extensions());
    let test_codex = builder.build_with_auto_env(&server).await?;

    test_codex
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Skill {
            name: "sample:sample-search".into(),
            path: skill_path,
        }]))
        .await?;
    wait_for_event(&test_codex.codex, |ev| {
        matches!(ev, EventMsg::TurnComplete(_))
    })
    .await;

    let user_text = resp_mock
        .single_request()
        .message_input_texts("user")
        .join("\n");
    assert!(user_text.contains(&skill_contents));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_plugin_root_mcp_stdio_tool_round_trip_expands_reserved_paths_and_codex_env_overlay()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let search_call_id = "search-agent-echo";
    let tool_call_id = "call-agent-echo";
    let overlay_call_id = "call-agent-overlay-env";
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_tool_search_call(search_call_id, &serde_json::json!({"query": "echo"})),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call_with_namespace(
                    tool_call_id,
                    "mcp__agent",
                    "echo",
                    r#"{"message":"ping"}"#,
                ),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_function_call_with_namespace(
                    overlay_call_id,
                    "mcp__agent",
                    "echo",
                    r#"{"message":"ping","env_var":"INSTA_WORKSPACE_ROOT"}"#,
                ),
                ev_completed("resp-3"),
            ]),
            sse(vec![
                ev_response_created("resp-4"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-4"),
            ]),
        ],
    )
    .await;
    let codex_home = Arc::new(TempDir::new()?);
    write_agent_plugin_skill_plugin(codex_home.as_ref());
    let plugin_root = codex_home
        .path()
        .join("plugins/cache/test/acme.tools/local");
    let stdio_server = match stdio_server_bin() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("test_stdio_server binary not available, skipping test: {err}");
            return Ok(());
        }
    };
    let stdio_server_name = format!("test_stdio_server{}", std::env::consts::EXE_SUFFIX);
    codex_utils_cargo_bin::copy_executable(
        std::path::Path::new(&stdio_server),
        &plugin_root.join(&stdio_server_name),
    )?;
    let mcp_config = serde_json::json!({
        "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
        "mcpServers": {
            "agent": {
                "type": "stdio",
                "command": format!("./{stdio_server_name}"),
                "env": {"MCP_TEST_VALUE": "${PLUGIN_ROOT}|${PLUGIN_DATA}"}
            }
        }
    });
    std::fs::write(
        plugin_root.join("mcp.json"),
        serde_json::to_vec_pretty(&mcp_config)?,
    )?;
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"acme.tools","mcpServers":{"agent":{"command":"ignored","env_vars":["INSTA_WORKSPACE_ROOT"]}}}"#,
    )?;
    let mut builder = test_codex().with_home(Arc::clone(&codex_home));
    let test_codex = builder.build_with_remote_and_local_env(&server).await?;
    wait_for_mcp_server(&test_codex.codex, "agent").await?;
    let data_root = dunce::canonicalize(
        std::fs::read_dir(codex_home.path().join("plugins/data/agent-plugins"))?
            .next()
            .expect("Agent Plugin data root")?
            .path(),
    )?;
    let expected_env = format!(
        "{}|{}",
        dunce::canonicalize(&plugin_root)?.display(),
        data_root.display()
    );

    test_codex
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "call the Agent Plugin echo tool".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let end = wait_for_event(&test_codex.codex, |event| {
        matches!(event, EventMsg::McpToolCallEnd(_))
    })
    .await;
    let overlay_end = wait_for_event(&test_codex.codex, |event| {
        matches!(event, EventMsg::McpToolCallEnd(_))
    })
    .await;
    wait_for_event(&test_codex.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let EventMsg::McpToolCallEnd(end) = end else {
        unreachable!("wait_for_event matched an MCP tool end")
    };
    let result = end.result.as_ref().expect("Agent Plugin MCP tool result");
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .and_then(|content| content.get("env"))
            .and_then(serde_json::Value::as_str),
        Some(expected_env.as_str())
    );
    let EventMsg::McpToolCallEnd(overlay_end) = overlay_end else {
        unreachable!("wait_for_event matched an MCP tool end")
    };
    let overlay_result = overlay_end
        .result
        .as_ref()
        .expect("Agent Plugin overlay MCP tool result");
    assert_eq!(
        overlay_result
            .structured_content
            .as_ref()
            .and_then(|content| content.get("env"))
            .and_then(serde_json::Value::as_str),
        Some(std::env::var("INSTA_WORKSPACE_ROOT")?.as_str())
    );
    let requests = mock.requests();
    let search_output = requests[1].tool_search_output(search_call_id);
    assert!(namespace_child_tool(&search_output, "mcp__agent", "echo").is_some());
    assert!(requests[2].function_call_output(tool_call_id).is_object());
    assert!(
        requests[3]
            .function_call_output(overlay_call_id)
            .is_object()
    );
    Ok(())
}

#[test_case(TrustLevel::Trusted, true, true, false, &[]; "trusted project disables the plugin")]
#[test_case(TrustLevel::Untrusted, true, true, false, &["echo_tool"]; "untrusted project cannot disable the plugin")]
#[test_case(TrustLevel::Trusted, false, true, true, &["echo"]; "trusted project enables system-disabled server and overrides user tool policy")]
#[test_case(TrustLevel::Untrusted, false, true, true, &[]; "untrusted project cannot enable system-disabled server")]
#[test_case(TrustLevel::Trusted, true, false, true, &[]; "trusted project disables system-enabled server")]
#[test_case(TrustLevel::Untrusted, true, false, true, &["echo_tool"]; "untrusted project preserves system startup and user tool policy")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_marketplace_plugin_honors_layered_activation_and_mcp_policy(
    trust_level: TrustLevel,
    system_enabled: bool,
    project_enabled: bool,
    plugin_enabled: bool,
    expected_tools: &[&str],
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let mock = mount_plugin_tool_search_turn(&server).await;
    let codex_home = Arc::new(TempDir::new()?);
    let project = TempDir::new()?;
    write_plugin_mcp_plugin(codex_home.as_ref(), &stdio_server_bin()?);
    let user_config_path = codex_home.path().join("config.toml");
    let user_config = std::fs::read_to_string(&user_config_path)?;
    std::fs::write(
        &user_config_path,
        format!(
            "{user_config}\n[plugins.\"{SAMPLE_PLUGIN_CONFIG_NAME}\".mcp_servers.sample]\ndisabled_tools = [\"echo\"]\n"
        ),
    )?;
    let system_config_path = codex_home.path().join("system.toml");
    let marketplace = TempDir::new()?;
    std::fs::create_dir_all(marketplace.path().join(".agents/plugins"))?;
    std::fs::write(
        marketplace.path().join(".agents/plugins/marketplace.json"),
        r#"{"name":"test","plugins":[{"name":"sample","source":{"source":"local","path":"./sample"}}]}"#,
    )?;
    let marketplace_source = toml::Value::String(marketplace.path().to_string_lossy().into_owned());
    std::fs::write(
        &system_config_path,
        format!(
            "[marketplaces.test]\nsource_type = \"local\"\nsource = {marketplace_source}\n[plugins.\"{SAMPLE_PLUGIN_CONFIG_NAME}\".mcp_servers.sample]\nenabled = {system_enabled}\nenabled_tools = [\"echo\", \"echo-tool\"]\n"
        ),
    )?;
    // The cached plugin may activate only through this system-defined marketplace.
    // Without the definition, source restrictions exclude it from the real turn.
    let requirements_path = codex_home.path().join("requirements.toml");
    std::fs::write(
        &requirements_path,
        format!(
            "[marketplaces]\nrestrict_to_allowed_sources = true\n[marketplaces.allowed_sources.test]\nsource = \"local\"\npath = {marketplace_source}\n"
        ),
    )?;
    std::fs::create_dir_all(project.path().join(".git"))?;
    std::fs::create_dir_all(project.path().join(".customcodex"))?;
    std::fs::write(
        project.path().join(".customcodex/config.toml"),
        format!(
            "[plugins.\"{SAMPLE_PLUGIN_CONFIG_NAME}\"]\nenabled = {plugin_enabled}\n[plugins.\"{SAMPLE_PLUGIN_CONFIG_NAME}\".mcp_servers.sample]\nenabled = {project_enabled}\ndisabled_tools = [\"echo-tool\"]\n"
        ),
    )?;
    set_project_trust_level(codex_home.path(), project.path(), trust_level)?;
    // Exercise the real layer loader and trust checks while keeping the test harness's
    // mock model provider and automatically selected executor environment.
    let layered_config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(project.path().to_path_buf()))
        .loader_overrides(LoaderOverrides {
            system_config_path: Some(system_config_path),
            system_requirements_path: Some(requirements_path),
            ..LoaderOverrides::without_managed_config_for_tests()
        })
        .build()
        .await?;
    let mut builder = test_codex()
        .with_home(codex_home)
        .with_config(move |config| config.config_layer_stack = layered_config.config_layer_stack);
    let test = builder.build_with_remote_and_local_env(&server).await?;
    let startup = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::McpStartupComplete(summary) => Some(summary.clone()),
        _ => None,
    })
    .await;
    let expected_ready = if expected_tools.is_empty() {
        vec![]
    } else {
        vec!["sample"]
    };
    assert_eq!(
        serde_json::to_value(startup)?,
        serde_json::json!({"ready": expected_ready, "failed": [], "cancelled": []}),
    );
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Mention {
            name: "sample".into(),
            path: format!("plugin://{SAMPLE_PLUGIN_CONFIG_NAME}"),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = mock.requests();
    let output = requests[1].tool_search_output(PLUGIN_MCP_SEARCH_CALL_ID);
    let mut visible_tools = output["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|namespace| namespace["name"] == SAMPLE_PLUGIN_MCP_NAMESPACE)
        .flat_map(|namespace| namespace["tools"].as_array().into_iter().flatten())
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    visible_tools.sort_unstable();
    assert_eq!(visible_tools, expected_tools);
    Ok(())
}

#[derive(Clone, Copy)]
enum ExplicitMcpRequest {
    Plugin,
    PluginSkill,
    ServerMention,
    LinkedServerMention,
}

#[test_case(ExplicitMcpRequest::Plugin; "plugin mention")]
#[test_case(ExplicitMcpRequest::PluginSkill; "plugin skill")]
#[test_case(ExplicitMcpRequest::ServerMention; "MCP server mention")]
#[test_case(ExplicitMcpRequest::LinkedServerMention; "linked MCP server mention")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicitly_requested_mcp_waits_for_startup(request: ExplicitMcpRequest) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let mock = mount_plugin_tool_search_turn(&server).await;

    let codex_home = Arc::new(TempDir::new()?);
    let rmcp_test_server_bin = match stdio_server_bin() {
        Ok(bin) => bin,
        Err(err) => {
            eprintln!("test_stdio_server binary not available, skipping test: {err}");
            return Ok(());
        }
    };
    let skill_path = dunce::canonicalize(write_plugin_skill_plugin(codex_home.as_ref()))?;
    write_plugin_mcp_plugin(codex_home.as_ref(), &rmcp_test_server_bin);
    write_plugin_app_plugin(codex_home.as_ref());
    let initialize_barrier = block_plugin_mcp_startup(codex_home.as_ref(), &rmcp_test_server_bin);

    let mut builder = test_codex()
        .with_home(codex_home)
        .with_auth(CodexAuth::from_api_key("Test API Key"));
    let test_codex = builder.build_with_remote_and_local_env(&server).await?;
    let codex = Arc::clone(&test_codex.codex);

    let input = match request {
        ExplicitMcpRequest::Plugin => UserInput::Mention {
            name: "sample".into(),
            path: format!("plugin://{SAMPLE_PLUGIN_CONFIG_NAME}"),
        },
        ExplicitMcpRequest::PluginSkill => UserInput::Skill {
            name: "sample:sample-search".into(),
            path: skill_path,
        },
        ExplicitMcpRequest::ServerMention => UserInput::Mention {
            name: "sample".into(),
            path: "mcp://sample".into(),
        },
        ExplicitMcpRequest::LinkedServerMention => UserInput::Text {
            text: "use [$sample](mcp://sample)".to_string(),
            text_elements: Vec::new(),
        },
    };
    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![input]))
        .await?;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        mock.requests().is_empty(),
        "an explicitly requested MCP should finish starting before inference"
    );
    std::fs::write(initialize_barrier, "ready")?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let requests = mock.requests();
    let model_request = &requests[0];
    let developer_messages = model_request.message_input_texts("developer");
    if matches!(request, ExplicitMcpRequest::Plugin) {
        assert!(
            developer_messages
                .iter()
                .any(|text| text.contains("Skills from this plugin")),
            "expected plugin skills guidance: {developer_messages:?}"
        );
        assert!(
            developer_messages
                .iter()
                .any(|text| text.contains("MCP servers from this plugin")),
            "expected visible plugin MCP guidance: {developer_messages:?}"
        );
    }
    if matches!(request, ExplicitMcpRequest::PluginSkill) {
        let user_messages = model_request.message_input_texts("user");
        assert!(
            user_messages
                .iter()
                .any(|message| message.contains("sample:sample-search")),
            "expected explicitly requested skill instructions: {user_messages:?}"
        );
    }
    assert!(
        !developer_messages
            .iter()
            .any(|text| text.contains("Apps from this plugin")),
        "expected plugin app guidance to be suppressed for API-key auth: {developer_messages:?}"
    );
    assert!(
        model_request
            .tool_by_name(SAMPLE_PLUGIN_APP_NAMESPACE, SEARCH_CALENDAR_CREATE_TOOL)
            .is_none(),
        "plugin app tool should not leak into the request for API-key auth"
    );
    let (calendar_tool, echo_tool) = searched_plugin_tools(&requests[1]);
    assert!(
        calendar_tool.is_none(),
        "plugin app tool should be hidden for API-key auth"
    );
    let echo_tool = echo_tool.expect("plugin MCP tool should be searchable");
    assert_plugin_provenance(&echo_tool);

    Ok(())
}

#[derive(Clone, Copy)]
enum ImplicitPluginSkillInvocation {
    SkillDocumentRead,
    SkillScriptRun,
}
