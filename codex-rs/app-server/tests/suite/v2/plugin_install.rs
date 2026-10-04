use anyhow::Result;
use anyhow::bail;
use app_test_support::TestAppServer;
use axum::http::Uri;
use axum::routing::get;
use codex_app_server_protocol::AppSummary;
use codex_app_server_protocol::ListMcpServerStatusParams;
use codex_app_server_protocol::ListMcpServerStatusResponse;
use codex_app_server_protocol::McpServerOauthLoginCompletedNotification;
use codex_app_server_protocol::McpServerOauthLoginResponse;
use codex_app_server_protocol::McpServerToolCallParams;
use codex_app_server_protocol::McpServerToolCallResponse;
use codex_app_server_protocol::PluginAuthPolicy;
use codex_app_server_protocol::PluginInstallParams;
use codex_app_server_protocol::PluginInstallResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_http_client::HttpClientBuilder;
use codex_rmcp_client::McpOAuthCallbackMode;
use codex_rmcp_client::resolve_mcp_oauth_callback_url;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::stdio_server_bin;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::io::AsyncBufReadExt;
use tokio::net::TcpListener;
use tokio::time::timeout;
use url::Url;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[tokio::test]
async fn plugin_install_rejects_relative_marketplace_paths() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_raw_request(
            "plugin/install",
            Some(serde_json::json!({
                "marketplacePath": "relative-marketplace.json",
                "pluginName": "missing-plugin",
            })),
        )
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("Invalid request"));
    Ok(())
}

#[tokio::test]
async fn plugin_install_rejects_missing_install_source() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: None,
            remote_marketplace_name: None,
            install_attempt_id: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(
        err.error
            .message
            .contains("Online installation is unsupported")
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_rejects_multiple_install_sources() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(AbsolutePathBuf::try_from(
                codex_home.path().join("marketplace.json"),
            )?),
            remote_marketplace_name: Some("openai-curated-remote".to_string()),
            install_attempt_id: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(
        err.error
            .message
            .contains("Online installation is unsupported")
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_returns_invalid_request_for_missing_marketplace_file() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(AbsolutePathBuf::try_from(
                codex_home.path().join("missing-marketplace.json"),
            )?),
            remote_marketplace_name: None,
            install_attempt_id: None,
            plugin_name: "missing-plugin".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("marketplace file"));
    assert!(err.error.message.contains("does not exist"));
    Ok(())
}

#[tokio::test]
async fn plugin_install_returns_invalid_request_for_not_available_plugin() -> Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./sample-plugin",
        Some("NOT_AVAILABLE"),
        /*auth_policy*/ None,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin", &[])?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            install_attempt_id: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("not available for install"));
    Ok(())
}

#[tokio::test]
async fn plugin_install_returns_invalid_request_for_disallowed_product_plugin() -> Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    std::fs::create_dir_all(repo_root.path().join(".agents/plugins"))?;
    std::fs::write(
        repo_root.path().join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": {
        "source": "local",
        "path": "./sample-plugin"
      },
      "policy": {
        "products": ["CHATGPT"]
      }
    }
  ]
}"#,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin", &[])?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_args(&["--session-source", "atlas"])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            install_attempt_id: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("not available for install"));
    Ok(())
}

#[tokio::test]
async fn plugin_install_skips_mcp_oauth_disabled_by_plugin_requirements() -> Result<()> {
    let oauth_server = MockServer::start().await;
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        "[features]\nplugins = true\n",
    )?;
    std::fs::write(
        codex_home.path().join("requirements.toml"),
        r#"[plugins."sample-plugin@debug".mcp_servers.allowed.identity]
url = "https://example.com/allowed-mcp"
"#,
    )?;

    let repo_root = TempDir::new()?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./sample-plugin",
        /*install_policy*/ None,
        /*auth_policy*/ None,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin", &[])?;
    write_plugin_mcp_config(repo_root.path(), "sample-plugin", &oauth_server.uri())?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            install_attempt_id: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;
    let _: PluginInstallResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;

    assert!(
        oauth_server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_skips_mcp_oauth_for_disabled_server() -> Result<()> {
    let oauth_server = MockServer::start().await;
    let mcp_settings = "enabled = false";
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            r#"[features]
plugins = true

[plugins."sample-plugin@debug".mcp_servers.sample-mcp]
{mcp_settings}
"#
        ),
    )?;

    let repo_root = TempDir::new()?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./sample-plugin",
        /*install_policy*/ None,
        /*auth_policy*/ None,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin", &[])?;
    write_plugin_mcp_config(repo_root.path(), "sample-plugin", &oauth_server.uri())?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            install_attempt_id: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;
    let _: PluginInstallResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;

    assert!(
        oauth_server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
    let persisted_config = std::fs::read_to_string(codex_home.path().join("config.toml"))?;
    let persisted_config = toml::from_str::<toml::Value>(&persisted_config)?;
    assert_eq!(
        persisted_config
            .get("plugins")
            .and_then(|plugins| plugins.get("sample-plugin@debug"))
            .and_then(|plugin| plugin.get("mcp_servers"))
            .and_then(|servers| servers.get("sample-mcp")),
        Some(&toml::from_str::<toml::Value>(mcp_settings)?)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_install_skips_mcp_oauth_for_unowned_environment() -> Result<()> {
    const UNOWNED_ENVIRONMENT_ID: &str = "plugin-unowned-executor";

    let oauth_server = MockServer::start().await;
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        "[features]\nplugins = true\n",
    )?;
    let mut executor =
        tokio::process::Command::new(codex_utils_cargo_bin::cargo_bin("exec-server")?)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
    let executor_stdout = executor
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("exec-server fixture stdout was not captured"))?;
    let mut executor_stdout_lines = tokio::io::BufReader::new(executor_stdout).lines();
    let executor_url = timeout(DEFAULT_TIMEOUT, executor_stdout_lines.next_line())
        .await??
        .ok_or_else(|| anyhow::anyhow!("exec-server fixture did not emit its WebSocket URL"))?;
    let executor_url = toml::Value::String(executor_url);
    std::fs::write(
        codex_home.path().join("environments.toml"),
        format!(
            r#"include_local = true

[[environments]]
id = "{UNOWNED_ENVIRONMENT_ID}"
url = {executor_url}
"#
        ),
    )?;

    let repo_root = TempDir::new()?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./sample-plugin",
        /*install_policy*/ None,
        /*auth_policy*/ None,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin", &[])?;
    std::fs::write(
        repo_root.path().join("sample-plugin/.mcp.json"),
        serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "sample-mcp": {
                    "type": "http",
                    "url": format!("{}/mcp", oauth_server.uri()),
                    "environment_id": UNOWNED_ENVIRONMENT_ID,
                },
            },
        }))?,
    )?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            install_attempt_id: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;
    let _: PluginInstallResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;

    assert!(
        oauth_server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_starts_mcp_oauth_through_configured_http_proxy() -> Result<()> {
    let proxy = MockServer::start().await;
    let resource_url = "http://plugin-mcp.invalid";
    let authorization_url = "http://plugin-oauth.invalid";
    let resource_metadata_url = format!("{resource_url}/oauth-resource");
    let challenge = format!("Bearer resource_metadata=\"{resource_metadata_url}\"");
    Mock::given(method("GET"))
        .and(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(401).insert_header("WWW-Authenticate", challenge.as_str()),
        )
        .mount(&proxy)
        .await;
    Mock::given(method("GET"))
        .and(path("/oauth-resource"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resource": resource_url,
            "authorization_servers": [authorization_url],
        })))
        .mount(&proxy)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_endpoint": format!("{authorization_url}/oauth/authorize"),
            "token_endpoint": format!("{authorization_url}/oauth/token"),
            "registration_endpoint": format!("{authorization_url}/oauth/register"),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
        })))
        .mount(&proxy)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/register"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&proxy)
        .await;

    let plugin_callback_listener = TcpListener::bind("127.0.0.1:0").await?;
    let plugin_callback_port = plugin_callback_listener.local_addr()?.port();
    let global_callback_listener = TcpListener::bind("127.0.0.1:0").await?;
    let global_callback_port = global_callback_listener.local_addr()?.port();
    drop(plugin_callback_listener);

    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!("mcp_oauth_callback_port = {global_callback_port}\n\n[features]\nplugins = true\n"),
    )?;
    let repo_root = TempDir::new()?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./sample-plugin",
        /*install_policy*/ None,
        /*auth_policy*/ None,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin", &[])?;
    std::fs::write(
        repo_root.path().join("sample-plugin/.mcp.json"),
        serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "sample-mcp": {
                    "type": "http",
                    "url": format!("{resource_url}/mcp"),
                    "oauth": {
                        "callbackPort": plugin_callback_port,
                        "callbackUrl": "http://127.0.0.1/plugin/callback",
                    },
                }
            }
        }))?,
    )?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let proxy_uri = proxy.uri();
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[
            ("HTTP_PROXY", Some(proxy_uri.as_str())),
            ("http_proxy", Some(proxy_uri.as_str())),
            ("HTTPS_PROXY", None),
            ("https_proxy", None),
            ("ALL_PROXY", None),
            ("all_proxy", None),
            ("NO_PROXY", None),
            ("no_proxy", None),
        ])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            install_attempt_id: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;
    let _: PluginInstallResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;
    wait_for_oauth_request_count(&proxy, "POST", "/oauth/register", /*expected_count*/ 1).await?;

    let requests = proxy.received_requests().await.unwrap_or_default();
    let resource_metadata_requested = requests
        .iter()
        .any(|request| request.url.path() == "/oauth-resource");
    assert!(resource_metadata_requested);

    let registration_request = requests
        .iter()
        .find(|request| request.url.path() == "/oauth/register")
        .expect("OAuth client registration request");
    let registration: serde_json::Value = serde_json::from_slice(&registration_request.body)?;
    let redirect_uri: Uri = registration["redirect_uris"][0]
        .as_str()
        .expect("OAuth client registration redirect URI")
        .parse()?;
    let expected_redirect_uri: Uri =
        format!("http://127.0.0.1:{plugin_callback_port}/plugin/callback/Jb0pRxZ4-luq").parse()?;
    assert_eq!(redirect_uri, expected_redirect_uri);
    Ok(())
}

#[test_case(false, false, false; "legacy provider falls back to default callback")]
#[test_case(false, true, false; "legacy provider falls back to global callback")]
#[test_case(true, true, false; "issuer bound provider preserves registered callback")]
#[test_case(false, true, true; "legacy provider preserves server specific registered callback")]
#[tokio::test]
async fn plugin_oauth_login_preserves_registered_callbacks_or_uses_legacy_fallback(
    issuer_supported: bool,
    use_global_callback: bool,
    callback_already_server_specific: bool,
) -> Result<()> {
    let oauth = MockServer::start().await;
    let authorization_server = oauth.uri();
    let server_url = format!("{authorization_server}/mcp");
    let resource_metadata_url = format!("{authorization_server}/oauth-resource");
    let challenge = format!("Bearer resource_metadata=\"{resource_metadata_url}\"");

    let global_callback = use_global_callback.then_some("http://127.0.0.1/global/callback");
    let legacy_callback = resolve_mcp_oauth_callback_url(
        &server_url,
        global_callback,
        McpOAuthCallbackMode::CallbackSpecific,
    )?;
    let callback_id = legacy_callback
        .rsplit('/')
        .next()
        .expect("legacy callback should contain the server-specific callback ID");
    let registered_callback = if callback_already_server_specific {
        format!("http://127.0.0.1/callback/registered/{callback_id}")
    } else {
        "http://127.0.0.1/callback/registered".to_string()
    };

    let codex_home = TempDir::new()?;
    let global_callback_config = global_callback
        .map(|callback| format!("mcp_oauth_callback_url = \"{callback}\"\n"))
        .unwrap_or_default();
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            "mcp_oauth_credentials_store = \"file\"\n{global_callback_config}\n[features]\nplugins = true\n"
        ),
    )?;

    let repo_root = TempDir::new()?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./sample-plugin",
        /*install_policy*/ None,
        /*auth_policy*/ None,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin", &[])?;
    std::fs::write(
        repo_root.path().join("sample-plugin/.mcp.json"),
        serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "sample-mcp": {
                    "type": "http",
                    "url": server_url,
                    "oauth": {
                        "clientId": "registered-client",
                        "callbackUrl": registered_callback,
                    },
                }
            }
        }))?,
    )?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let install_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            install_attempt_id: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;
    let _: PluginInstallResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(install_id)).await??;

    // Install before exposing OAuth metadata so automatic plugin login cannot
    // launch a platform browser; the explicit public login remains end-to-end.
    Mock::given(method("GET"))
        .and(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(401).insert_header("WWW-Authenticate", challenge.as_str()),
        )
        .mount(&oauth)
        .await;
    Mock::given(method("GET"))
        .and(path("/oauth-resource"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resource": authorization_server,
            "authorization_servers": [authorization_server],
        })))
        .mount(&oauth)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": authorization_server,
            "authorization_endpoint": format!("{authorization_server}/oauth/authorize"),
            "token_endpoint": format!("{authorization_server}/oauth/token"),
            "registration_endpoint": format!("{authorization_server}/oauth/register"),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
            "authorization_response_iss_parameter_supported": issuer_supported,
        })))
        .mount(&oauth)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/register"))
        .respond_with(ResponseTemplate::new(400))
        .expect(0)
        .mount(&oauth)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "registered-plugin-token",
            "token_type": "Bearer",
        })))
        .expect(1)
        .mount(&oauth)
        .await;

    let login_id = mcp
        .send_raw_request(
            "mcpServer/oauth/login",
            Some(json!({
                "name": "sample-mcp",
                "scopes": ["read"],
                "timeoutSecs": 10,
            })),
        )
        .await?;
    let response: McpServerOauthLoginResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(login_id)).await??;
    let authorization_url = Url::parse(&response.authorization_url)?;
    let query: BTreeMap<_, _> = authorization_url.query_pairs().into_owned().collect();
    assert_eq!(
        query.get("client_id").map(String::as_str),
        Some("registered-client")
    );

    let mut callback_url = Url::parse(&query["redirect_uri"])?;
    let expected_callback = if issuer_supported || callback_already_server_specific {
        registered_callback.as_str()
    } else {
        legacy_callback.as_str()
    };
    assert_eq!(callback_url.path(), Url::parse(expected_callback)?.path());
    callback_url
        .query_pairs_mut()
        .append_pair("code", "registered-plugin-code")
        .append_pair("state", &query["state"]);
    if issuer_supported {
        callback_url
            .query_pairs_mut()
            .append_pair("iss", &authorization_server);
    }
    HttpClientBuilder::new()
        .build_direct()?
        .get(callback_url)
        .send()
        .await?
        .error_for_status()?;

    let completed: McpServerOauthLoginCompletedNotification = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_notification("mcpServer/oauthLogin/completed"),
    )
    .await??;
    assert_eq!(
        completed.login_id.as_deref(),
        Some(
            response
                .login_id
                .as_deref()
                .expect("login response should contain an ID")
        )
    );
    assert_eq!(
        completed,
        McpServerOauthLoginCompletedNotification {
            name: "sample-mcp".to_string(),
            thread_id: None,
            login_id: response.login_id,
            success: true,
            error: None,
        }
    );
    oauth.verify().await;
    Ok(())
}

#[tokio::test]
async fn plugin_install_starts_mcp_oauth_for_api_key_dual_surface_plugin() -> Result<()> {
    let oauth_server = MockServer::start().await;
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
mcp_oauth_credentials_store = "file"

[features]
plugins = true
"#,
    )?;

    let repo_root = TempDir::new()?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./sample-plugin",
        /*install_policy*/ None,
        /*auth_policy*/ None,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin", &["sample-mcp"])?;
    write_plugin_mcp_config(repo_root.path(), "sample-plugin", &oauth_server.uri())?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", Some("test-api-key"))])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            install_attempt_id: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;
    let response: PluginInstallResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;

    assert_eq!(response.auth_policy, PluginAuthPolicy::OnInstall);
    assert!(oauth_discovery_request_count(&oauth_server).await > 0);
    Ok(())
}

#[tokio::test]
async fn plugin_install_makes_bundled_mcp_servers_available_to_followup_requests() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        "[features]\nplugins = true\n",
    )?;
    let repo_root = TempDir::new()?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./sample-plugin",
        /*install_policy*/ None,
        /*auth_policy*/ None,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin", &[])?;
    std::fs::write(
        repo_root.path().join("sample-plugin/.mcp.json"),
        serde_json::to_vec(&json!({
            "mcpServers": {
                "sample-mcp": {
                    "command": stdio_server_bin()?,
                }
            }
        }))?,
    )?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        // The bundled stdio MCP fixture is a host-local executable.
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_thread_start_request(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            install_attempt_id: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;
    let response: PluginInstallResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;
    assert_eq!(response.apps_needing_auth, Vec::<AppSummary>::new());
    let config = std::fs::read_to_string(codex_home.path().join("config.toml"))?;
    assert!(!config.contains("[mcp_servers.sample-mcp]"));

    let request_id = mcp
        .send_mcp_server_tool_call_request(McpServerToolCallParams {
            thread_id: thread.id,
            server: "sample-mcp".to_string(),
            tool: "echo".to_string(),
            arguments: Some(json!({ "message": "installed in the same thread" })),
            meta: None,
        })
        .await?;
    let response: McpServerToolCallResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;
    assert_eq!(
        response.structured_content,
        Some(json!({ "echo": "ECHOING: installed in the same thread", "env": null })),
    );

    let request_id = mcp
        .send_list_mcp_server_status_request(ListMcpServerStatusParams {
            server_name: None,
            cursor: None,
            limit: None,
            detail: None,
            thread_id: None,
        })
        .await?;
    let response: ListMcpServerStatusResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;
    let [server] = response.data.as_slice() else {
        bail!("expected exactly one bundled MCP server");
    };

    assert_eq!(
        (server.name.as_str(), server.plugin_id.as_deref()),
        ("sample-mcp", Some("sample-plugin@debug")),
    );
    assert!(
        server.server_info.is_some(),
        "bundled MCP server did not initialize"
    );
    assert!(
        server.tools.contains_key("echo"),
        "bundled MCP server did not expose its tools"
    );

    let request_id = mcp
        .send_raw_request(
            "mcpServer/oauth/login",
            Some(json!({
                "name": "sample-mcp",
            })),
        )
        .await?;
    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert_eq!(
        err.error.message,
        "OAuth login is only supported for streamable HTTP servers."
    );
    Ok(())
}

async fn oauth_discovery_request_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path().contains("oauth-authorization-server"))
        .count()
}

fn write_plugin_marketplace(
    repo_root: &std::path::Path,
    marketplace_name: &str,
    plugin_name: &str,
    source_path: &str,
    install_policy: Option<&str>,
    auth_policy: Option<&str>,
) -> std::io::Result<()> {
    let policy = if install_policy.is_some() || auth_policy.is_some() {
        let installation = install_policy
            .map(|installation| format!("\n        \"installation\": \"{installation}\""))
            .unwrap_or_default();
        let separator = if install_policy.is_some() && auth_policy.is_some() {
            ","
        } else {
            ""
        };
        let authentication = auth_policy
            .map(|authentication| {
                format!("{separator}\n        \"authentication\": \"{authentication}\"")
            })
            .unwrap_or_default();
        format!(",\n      \"policy\": {{{installation}{authentication}\n      }}")
    } else {
        String::new()
    };
    std::fs::create_dir_all(repo_root.join(".git"))?;
    std::fs::create_dir_all(repo_root.join(".agents/plugins"))?;
    std::fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        format!(
            r#"{{
  "name": "{marketplace_name}",
  "plugins": [
    {{
      "name": "{plugin_name}",
      "source": {{
        "source": "local",
        "path": "{source_path}"
      }}{policy}
    }}
  ]
}}"#
        ),
    )
}

fn write_plugin_source(
    repo_root: &std::path::Path,
    plugin_name: &str,
    app_ids: &[&str],
) -> Result<()> {
    let plugin_root = repo_root.join(plugin_name);
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        format!(r#"{{"name":"{plugin_name}"}}"#),
    )?;

    let apps = app_ids
        .iter()
        .map(|app_id| ((*app_id).to_string(), json!({ "id": app_id })))
        .collect::<serde_json::Map<_, _>>();
    std::fs::write(
        plugin_root.join(".app.json"),
        serde_json::to_vec_pretty(&json!({ "apps": apps }))?,
    )?;
    Ok(())
}

fn write_plugin_mcp_config(
    repo_root: &std::path::Path,
    plugin_name: &str,
    mcp_base_url: &str,
) -> Result<()> {
    std::fs::write(
        repo_root.join(plugin_name).join(".mcp.json"),
        format!(
            r#"{{
  "mcpServers": {{
    "sample-mcp": {{
      "type": "http",
      "url": "{mcp_base_url}/mcp"
    }}
  }}
}}"#
        ),
    )?;
    Ok(())
}

async fn wait_for_oauth_request_count(
    server: &MockServer,
    method: &str,
    path: &str,
    expected: usize,
) -> Result<()> {
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            let requests = server.received_requests().await.unwrap_or_default();
            let count = requests
                .iter()
                .filter(|request| request.method.as_str() == method && request.url.path() == path)
                .count();
            if count >= expected {
                assert_eq!(count, expected);
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}
