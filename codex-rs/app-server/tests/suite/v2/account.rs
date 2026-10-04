use anyhow::Result;
use anyhow::bail;
use app_test_support::TestAppServer;
use app_test_support::to_response;

use app_test_support::DEFAULT_CLIENT_NAME;
use codex_app_server_protocol::Account;
use codex_app_server_protocol::AccountLoginCompletedNotification;
use codex_app_server_protocol::AccountUpdatedNotification;
use codex_app_server_protocol::AuthMode;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::GetAccountParams;
use codex_app_server_protocol::GetAccountResponse;
use codex_app_server_protocol::GetAuthStatusParams;
use codex_app_server_protocol::GetAuthStatusResponse;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::LoginAccountResponse;
use codex_app_server_protocol::LogoutAccountResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::auth::BedrockAccessKeysAuth;
use codex_login::auth::BedrockApiKeyAuth;
use codex_login::load_auth_dot_json;
use codex_login::login_with_api_key;
use codex_login::login_with_bedrock_api_key;
use codex_protocol::auth::AuthMode as DomainAuthMode;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// Helper to create a minimal config.toml for the app server
#[derive(Default)]
struct CreateConfigTomlParams {
    forced_method: Option<String>,
    requires_openai_auth: Option<bool>,
    base_url: Option<String>,
    model_provider_id: Option<String>,
    extra_provider_config: Option<String>,
}

fn create_config_toml(codex_home: &Path, params: CreateConfigTomlParams) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    let base_url = params
        .base_url
        .unwrap_or_else(|| "http://127.0.0.1:0/v1".to_string());
    let forced_line = if let Some(method) = params.forced_method {
        format!("forced_login_method = \"{method}\"\n")
    } else {
        String::new()
    };
    let requires_line = match params.requires_openai_auth {
        Some(true) => "requires_openai_auth = true\n".to_string(),
        Some(false) => String::new(),
        None => String::new(),
    };
    let model_provider_id = params
        .model_provider_id
        .unwrap_or_else(|| "mock_provider".to_string());
    let provider_section = if model_provider_id == "mock_provider" {
        format!(
            r#"[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{base_url}"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
{requires_line}
"#
        )
    } else {
        params.extra_provider_config.unwrap_or_default()
    };
    let contents = format!(
        r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"
{forced_line}

model_provider = "{model_provider_id}"

[features]
shell_snapshot = false

{provider_section}
"#
    );
    std::fs::write(config_toml, contents)
}

fn read_config_toml(codex_home: &Path) -> Result<toml::Value> {
    Ok(toml::from_str(&std::fs::read_to_string(
        codex_home.join("config.toml"),
    )?)?)
}

fn load_file_auth(codex_home: &Path) -> Result<Option<AuthDotJson>> {
    Ok(load_auth_dot_json(
        codex_home,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?)
}

fn aws_managed_bedrock_config() -> CreateConfigTomlParams {
    CreateConfigTomlParams {
        model_provider_id: Some("amazon-bedrock".to_string()),
        extra_provider_config: Some(
            r#"[model_providers.amazon-bedrock.aws]
profile = "codex-bedrock"
region = "us-west-2"
"#
            .to_string(),
        ),
        ..Default::default()
    }
}

async fn read_account(mcp: &mut TestAppServer) -> Result<GetAccountResponse> {
    let request_id = mcp
        .send_get_account_request(GetAccountParams {
            refresh_token: false,
        })
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(request_id)).await?
}

async fn assert_account_updated(
    mcp: &mut TestAppServer,
    auth_mode: Option<AuthMode>,
) -> Result<()> {
    let payload: AccountUpdatedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_notification("account/updated"),
    )
    .await??;
    assert_eq!(
        payload,
        AccountUpdatedNotification {
            auth_mode,
            plan_type: None,
        }
    );
    Ok(())
}

#[tokio::test]
async fn logout_account_removes_auth_and_notifies() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;

    login_with_api_key(
        codex_home.path(),
        "sk-test-key",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    assert!(codex_home.path().join("auth.json").exists());

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let id = mcp.send_logout_account_request().await?;
    let _ok: LogoutAccountResponse = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(id)).await??;

    let note = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/updated"),
    )
    .await??;
    let parsed: ServerNotification = note.try_into()?;
    let ServerNotification::AccountUpdated(payload) = parsed else {
        bail!("unexpected notification: {parsed:?}");
    };
    assert!(
        payload.auth_mode.is_none(),
        "auth_method should be None after logout"
    );
    assert_eq!(payload.plan_type, None);

    assert!(
        !codex_home.path().join("auth.json").exists(),
        "auth.json should be deleted"
    );

    let get_id = mcp
        .send_get_account_request(GetAccountParams {
            refresh_token: false,
        })
        .await?;
    let account: GetAccountResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(get_id)).await??;
    assert_eq!(account.account, None);
    Ok(())
}

#[tokio::test]
async fn logout_account_succeeds_when_config_reload_fails() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;
    login_with_api_key(
        codex_home.path(),
        "sk-test-key",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    std::fs::write(codex_home.path().join("config.toml"), "invalid = [")?;

    let request_id = mcp.send_logout_account_request().await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        to_response::<LogoutAccountResponse>(response)?,
        LogoutAccountResponse {}
    );
    assert_eq!(load_file_auth(codex_home.path())?, None);
    assert_account_updated(&mut mcp, /*auth_mode*/ None).await?;

    Ok(())
}

#[test_case(None; "default_settings")]
#[test_case(Some("chatgpt"); "legacy_user_chatgpt_setting_is_ignored")]
#[tokio::test]
async fn login_account_api_key_succeeds_and_notifies(forced_method: Option<&str>) -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            forced_method: forced_method.map(str::to_string),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let req_id = mcp
        .send_login_account_api_key_request("sk-test-key")
        .await?;
    let login: LoginAccountResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(req_id)).await??;
    assert_eq!(login, LoginAccountResponse::ApiKey {});

    let note = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/login/completed"),
    )
    .await??;
    let parsed: ServerNotification = note.try_into()?;
    let ServerNotification::AccountLoginCompleted(payload) = parsed else {
        bail!("unexpected notification: {parsed:?}");
    };
    pretty_assertions::assert_eq!(payload.login_id, None);
    pretty_assertions::assert_eq!(payload.success, true);
    pretty_assertions::assert_eq!(payload.error, None);

    let note = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/updated"),
    )
    .await??;
    let parsed: ServerNotification = note.try_into()?;
    let ServerNotification::AccountUpdated(payload) = parsed else {
        bail!("unexpected notification: {parsed:?}");
    };
    pretty_assertions::assert_eq!(payload.auth_mode, Some(AuthMode::ApiKey));
    pretty_assertions::assert_eq!(payload.plan_type, None);

    login_with_api_key(
        codex_home.path(),
        "sk-test-key",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    assert!(codex_home.path().join("auth.json").exists());
    Ok(())
}

#[test_case("amazonBedrock", None; "api_key")]
#[test_case("amazonBedrockAccessKeys", None; "access_keys")]
#[test_case("amazonBedrock", Some("chatgpt"); "legacy_user_chatgpt_setting_is_ignored")]
#[tokio::test]
async fn login_amazon_bedrock_replaces_primary_auth_and_persists_provider(
    credential_type: &str,
    forced_method: Option<&str>,
) -> Result<()> {
    let managed_access_keys = credential_type == "amazonBedrockAccessKeys";
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            forced_method: forced_method.map(str::to_string),
            ..Default::default()
        },
    )?;
    let config_path = codex_home.path().join("config.toml");
    let original_config = std::fs::read_to_string(&config_path)?;
    std::fs::write(
        &config_path,
        format!(
            "{original_config}\n[model_providers.amazon-bedrock]\n\
             http_headers = {{ X-Existing = \"preserved\" }}\n\
             [model_providers.amazon-bedrock.aws]\n\
             profile = \"stale-profile\"\n\
             region = \"us-east-1\"\n\
             auth_refresh = {{ command = \"aws\" }}\n"
        ),
    )?;
    login_with_api_key(
        codex_home.path(),
        "sk-test-key",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let mut expected_config = read_config_toml(codex_home.path())?;
    expected_config
        .as_table_mut()
        .expect("config should be a table")
        .insert(
            "model_provider".to_string(),
            toml::Value::String("amazon-bedrock".to_string()),
        );
    expected_config["model_providers"]["amazon-bedrock"]["aws"]
        .as_table_mut()
        .expect("AWS configuration should be a table")
        .remove("profile");
    if managed_access_keys {
        expected_config["model_providers"]["amazon-bedrock"]["aws"]["region"] =
            toml::Value::String("us-west-2".to_string());
    }
    let params = if managed_access_keys {
        json!({
            "type": credential_type,
            "accessKeyId": " test-id ",
            "secretAccessKey": " test-secret ",
            "sessionToken": " test-token ",
            "region": " us-west-2 ",
        })
    } else {
        json!({
            "type": credential_type,
            "apiKey": " managed-bedrock-api-key ",
            "region": " us-west-2 ",
        })
    };
    let request_id = mcp.send_login_account_request(params).await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        to_response::<LoginAccountResponse>(response)?,
        LoginAccountResponse::AmazonBedrock {}
    );

    assert_eq!(
        load_file_auth(codex_home.path())?,
        Some(AuthDotJson {
            auth_mode: Some(if managed_access_keys {
                DomainAuthMode::BedrockAccessKeys
            } else {
                DomainAuthMode::BedrockApiKey
            }),
            openai_api_key: None,
            tokens: None,
            last_refresh: None,
            agent_identity: None,
            personal_access_token: None,
            bedrock_api_key: (!managed_access_keys).then(|| BedrockApiKeyAuth {
                api_key: "managed-bedrock-api-key".to_string(),
                region: "us-west-2".to_string(),
            }),
            bedrock_access_keys: managed_access_keys.then(|| BedrockAccessKeysAuth {
                access_key_id: "test-id".to_string(),
                secret_access_key: "test-secret".to_string(),
                session_token: Some("test-token".to_string()),
            }),
        })
    );
    assert_eq!(read_config_toml(codex_home.path())?, expected_config);
    assert!(!codex_home.path().join(".env").exists());

    let notification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/login/completed"),
    )
    .await??;
    let ServerNotification::AccountLoginCompleted(payload) = notification.try_into()? else {
        bail!("unexpected notification")
    };
    assert_eq!(
        payload,
        AccountLoginCompletedNotification {
            login_id: None,
            success: true,
            error: None,
            onboarding_entrypoint: None,
        }
    );
    let auth_mode = if managed_access_keys {
        AuthMode::BedrockAccessKeys
    } else {
        AuthMode::BedrockApiKey
    };
    assert_account_updated(&mut mcp, Some(auth_mode)).await?;
    assert_eq!(
        read_account(&mut mcp).await?,
        GetAccountResponse {
            workspace_routing: None,
            account: Some(Account::AmazonBedrock {
                uses_codex_managed_credentials: true,
            }),
            requires_openai_auth: false,
        }
    );

    if managed_access_keys {
        let mut expected_logout_config = expected_config;
        let expected_logout_config_root = expected_logout_config
            .as_table_mut()
            .expect("config should be a table");
        expected_logout_config_root.remove("model_provider");
        expected_logout_config_root.remove("model");
        expected_logout_config["model_providers"]["amazon-bedrock"]
            .as_table_mut()
            .expect("Bedrock provider config should be a table")
            .remove("aws");

        let request_id = mcp.send_logout_account_request().await?;
        let response: LogoutAccountResponse =
            timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(request_id)).await??;
        assert_eq!(response, LogoutAccountResponse {});
        assert_eq!(load_file_auth(codex_home.path())?, None);
        assert_eq!(read_config_toml(codex_home.path())?, expected_logout_config);
        assert!(!codex_home.path().join(".env").exists());
        assert_account_updated(&mut mcp, /*auth_mode*/ None).await?;
        assert_eq!(
            read_account(&mut mcp).await?,
            GetAccountResponse {
                workspace_routing: None,
                account: None,
                requires_openai_auth: true,
            }
        );
    }

    Ok(())
}

#[tokio::test]
async fn login_amazon_bedrock_rejects_non_bedrock_provider_override_without_changes() -> Result<()>
{
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;
    login_with_api_key(
        codex_home.path(),
        "sk-test-key",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    let expected_auth = load_file_auth(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .with_args(&["-c", "model_provider=\"mock_provider\""])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let expected_config = read_config_toml(codex_home.path())?;

    let request_id = mcp
        .send_login_account_amazon_bedrock_request("managed-bedrock-api-key", "us-west-2")
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        error.error.message,
        "Amazon Bedrock login cannot select `amazon-bedrock` because session-flags sets `model_provider` to \"mock_provider\""
    );
    assert_eq!(load_file_auth(codex_home.path())?, expected_auth);
    assert_eq!(read_config_toml(codex_home.path())?, expected_config);

    let maybe_completed = timeout(
        Duration::from_millis(500),
        mcp.read_stream_until_notification_message("account/login/completed"),
    )
    .await;
    assert!(
        maybe_completed.is_err(),
        "account/login/completed should not be emitted when the provider is overridden"
    );
    let maybe_updated = timeout(
        Duration::from_millis(500),
        mcp.read_stream_until_notification_message("account/updated"),
    )
    .await;
    assert!(
        maybe_updated.is_err(),
        "account/updated should not be emitted when the provider is overridden"
    );

    Ok(())
}

#[tokio::test]
async fn login_amazon_bedrock_access_keys_rejects_overridden_aws_configuration() -> Result<()> {
    for config_override in [
        r#"model_providers.amazon-bedrock.aws.profile="other-account""#,
        r#"model_providers.amazon-bedrock.aws.region="eu-west-1""#,
    ] {
        let codex_home = TempDir::new()?;
        create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;
        login_with_api_key(
            codex_home.path(),
            "sk-test-key",
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )?;
        let expected_auth = load_file_auth(codex_home.path())?;

        let mut mcp = TestAppServer::builder()
            .with_codex_home(codex_home.path())
            .without_auto_env()
            .with_env_overrides(&[("OPENAI_API_KEY", None)])
            .with_args(&["-c", config_override])
            .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
            .await?;

        let request_id = mcp
            .send_login_account_request(json!({
                "type": "amazonBedrockAccessKeys",
                "accessKeyId": "managed-access-key-id",
                "secretAccessKey": "managed-secret-access-key",
                "region": "us-west-2",
            }))
            .await?;
        let error = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
        )
        .await??;

        assert_eq!(
            error.error.message,
            "Amazon Bedrock configuration cannot take effect: Overridden by session flags"
        );
        assert_eq!(load_file_auth(codex_home.path())?, expected_auth);
    }

    Ok(())
}

#[tokio::test]
async fn login_amazon_bedrock_allows_bedrock_provider_override() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;
    let mut expected_config = read_config_toml(codex_home.path())?;
    expected_config
        .as_table_mut()
        .expect("config should be a table")
        .insert(
            "model_provider".to_string(),
            toml::Value::String("amazon-bedrock".to_string()),
        );

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .with_args(&["-c", "model_provider=\"amazon-bedrock\""])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_login_account_amazon_bedrock_request("managed-bedrock-api-key", "us-west-2")
        .await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        to_response::<LoginAccountResponse>(response)?,
        LoginAccountResponse::AmazonBedrock {}
    );
    assert_eq!(
        load_file_auth(codex_home.path())?,
        Some(AuthDotJson {
            auth_mode: Some(DomainAuthMode::BedrockApiKey),
            openai_api_key: None,
            tokens: None,
            last_refresh: None,
            agent_identity: None,
            personal_access_token: None,
            bedrock_api_key: Some(BedrockApiKeyAuth {
                api_key: "managed-bedrock-api-key".to_string(),
                region: "us-west-2".to_string(),
            }),
            bedrock_access_keys: None,
        })
    );
    assert_eq!(read_config_toml(codex_home.path())?, expected_config);
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/login/completed"),
    )
    .await??;
    assert_account_updated(&mut mcp, Some(AuthMode::BedrockApiKey)).await?;

    Ok(())
}

#[test_case("amazon-bedrock", "mock-model"; "mantle_clears_generic_model")]
#[test_case("amazon-bedrock-runtime", "global.openai.gpt-5.6-terra"; "runtime_clears_bedrock_model")]
#[tokio::test]
async fn logout_managed_bedrock_restores_default_account(
    model_provider_id: &str,
    model: &str,
) -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let request_id = mcp
        .send_login_account_amazon_bedrock_request("managed-bedrock-api-key", "us-west-2")
        .await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        to_response::<LoginAccountResponse>(response)?,
        LoginAccountResponse::AmazonBedrock {}
    );
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/login/completed"),
    )
    .await??;
    assert_account_updated(&mut mcp, Some(AuthMode::BedrockApiKey)).await?;
    assert_eq!(
        read_account(&mut mcp).await?,
        GetAccountResponse {
            workspace_routing: None,
            account: Some(Account::AmazonBedrock {
                uses_codex_managed_credentials: true,
            }),
            requires_openai_auth: false,
        }
    );

    let config_path = codex_home.path().join("config.toml");
    let config = std::fs::read_to_string(&config_path)?
        .replace(
            "model_provider = \"amazon-bedrock\"",
            &format!("model_provider = \"{model_provider_id}\""),
        )
        .replace(
            "model = \"mock-model\"",
            &format!("model = \"{model}\"\nmodel_reasoning_effort = \"high\""),
        );
    std::fs::write(
        config_path,
        format!(
            "{config}\n[model_providers.{model_provider_id}]\nbase_url = \"https://bedrock.example.com/v1\"\n[model_providers.{model_provider_id}.aws]\nprofile = \"managed-profile\"\nregion = \"us-west-2\"\nauth_refresh = {{ command = \"aws\" }}\n"
        ),
    )?;
    let mut expected_config = read_config_toml(codex_home.path())?;
    let expected_config_root = expected_config
        .as_table_mut()
        .expect("config should be a table");
    expected_config_root.remove("model_provider");
    expected_config_root.remove("model");
    expected_config["model_providers"][model_provider_id]
        .as_table_mut()
        .expect("Bedrock provider config should be a table")
        .remove("aws");

    let request_id = mcp.send_logout_account_request().await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        to_response::<LogoutAccountResponse>(response)?,
        LogoutAccountResponse {}
    );
    assert_eq!(load_file_auth(codex_home.path())?, None);
    assert_eq!(read_config_toml(codex_home.path())?, expected_config);
    assert_account_updated(&mut mcp, /*auth_mode*/ None).await?;
    assert_eq!(
        read_account(&mut mcp).await?,
        GetAccountResponse {
            workspace_routing: None,
            account: None,
            requires_openai_auth: true,
        }
    );
    Ok(())
}

#[tokio::test]
async fn logout_aws_managed_bedrock_clears_provider_and_restores_default_account() -> Result<()> {
    for managed_bedrock_auth in [false, true] {
        let codex_home = TempDir::new()?;
        create_config_toml(codex_home.path(), aws_managed_bedrock_config())?;
        let config_path = codex_home.path().join("config.toml");
        let config = std::fs::read_to_string(&config_path)?
            .replace(
                "model = \"mock-model\"",
                "model = \"openai.gpt-5.6-sol\"\nmodel_reasoning_effort = \"high\"",
            )
            .replace(
                "[model_providers.amazon-bedrock.aws]",
                "[model_providers.amazon-bedrock]\nbase_url = \"https://bedrock.example.com/v1\"\n[model_providers.amazon-bedrock.aws]\nauth_refresh = { command = \"aws\" }",
            );
        std::fs::write(config_path, config)?;
        let dotenv_path = codex_home.path().join(".env");
        let aws_credentials_path = codex_home.path().join("aws-credentials");
        let dotenv = "AWS_ACCESS_KEY_ID=environment-id\nAWS_SECRET_ACCESS_KEY=environment-secret\n";
        let aws_credentials = "[codex-bedrock]\naws_access_key_id = profile-id\naws_secret_access_key = profile-secret\n";
        std::fs::write(&dotenv_path, dotenv)?;
        std::fs::write(&aws_credentials_path, aws_credentials)?;
        if managed_bedrock_auth {
            login_with_bedrock_api_key(
                codex_home.path(),
                "managed-bedrock-api-key",
                "us-east-1",
                AuthCredentialsStoreMode::File,
                AuthKeyringBackendKind::default(),
            )?;
        } else {
            login_with_api_key(
                codex_home.path(),
                "sk-test-key",
                AuthCredentialsStoreMode::File,
                AuthKeyringBackendKind::default(),
            )?;
        }

        let aws_credentials_env_path = aws_credentials_path.to_string_lossy();
        let mut mcp = TestAppServer::builder()
            .with_codex_home(codex_home.path())
            .without_auto_env()
            .with_env_overrides(&[
                ("OPENAI_API_KEY", None),
                ("AWS_ACCESS_KEY_ID", Some("environment-id")),
                ("AWS_SECRET_ACCESS_KEY", Some("environment-secret")),
                (
                    "AWS_SHARED_CREDENTIALS_FILE",
                    Some(aws_credentials_env_path.as_ref()),
                ),
            ])
            .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
            .await?;
        assert_eq!(
            read_account(&mut mcp).await?,
            GetAccountResponse {
                workspace_routing: None,
                account: Some(Account::AmazonBedrock {
                    uses_codex_managed_credentials: false,
                }),
                requires_openai_auth: false,
            }
        );
        let mut expected_config = read_config_toml(codex_home.path())?;
        let expected_config_root = expected_config
            .as_table_mut()
            .expect("config should be a table");
        expected_config_root.remove("model_provider");
        expected_config_root.remove("model");
        expected_config["model_providers"]["amazon-bedrock"]
            .as_table_mut()
            .expect("Bedrock provider config should be a table")
            .remove("aws");

        let request_id = mcp.send_logout_account_request().await?;
        let response = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
        )
        .await??;
        assert_eq!(
            to_response::<LogoutAccountResponse>(response)?,
            LogoutAccountResponse {}
        );
        assert_eq!(load_file_auth(codex_home.path())?, None);
        assert_eq!(read_config_toml(codex_home.path())?, expected_config);
        assert_eq!(std::fs::read_to_string(dotenv_path)?, dotenv);
        assert_eq!(
            std::fs::read_to_string(aws_credentials_path)?,
            aws_credentials
        );
        assert_account_updated(&mut mcp, /*auth_mode*/ None).await?;
        assert_eq!(
            read_account(&mut mcp).await?,
            GetAccountResponse {
                workspace_routing: None,
                account: None,
                requires_openai_auth: true,
            }
        );
    }
    Ok(())
}

#[tokio::test]
async fn logout_managed_bedrock_preserves_changed_provider_without_experimental_api() -> Result<()>
{
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), aws_managed_bedrock_config())?;
    login_with_bedrock_api_key(
        codex_home.path(),
        "managed-bedrock-api-key",
        "us-west-2",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    let initialized = mcp
        .initialize_with_capabilities(
            ClientInfo {
                name: DEFAULT_CLIENT_NAME.to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: false,
                ..Default::default()
            }),
        )
        .await?;
    assert!(matches!(initialized, JSONRPCMessage::Response(_)));

    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;
    let config_path = codex_home.path().join("config.toml");
    let config = std::fs::read_to_string(&config_path)?;
    std::fs::write(
        config_path,
        format!(
            "{config}\n[model_providers.amazon-bedrock.aws]\nprofile = \"preserved\"\nregion = \"us-west-2\"\n"
        ),
    )?;
    let expected_config = read_config_toml(codex_home.path())?;

    let request_id = mcp.send_logout_account_request().await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        to_response::<LogoutAccountResponse>(response)?,
        LogoutAccountResponse {}
    );
    assert_eq!(load_file_auth(codex_home.path())?, None);
    assert_eq!(read_config_toml(codex_home.path())?, expected_config);
    assert_account_updated(&mut mcp, /*auth_mode*/ None).await?;
    assert_eq!(
        read_account(&mut mcp).await?,
        GetAccountResponse {
            workspace_routing: None,
            account: None,
            requires_openai_auth: false,
        }
    );
    Ok(())
}

#[tokio::test]
async fn managed_bedrock_login_requires_experimental_api() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    let initialized = mcp
        .initialize_with_capabilities(
            ClientInfo {
                name: DEFAULT_CLIENT_NAME.to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: false,
                ..Default::default()
            }),
        )
        .await?;
    assert!(matches!(initialized, JSONRPCMessage::Response(_)));

    let request_id = mcp
        .send_login_account_amazon_bedrock_request("managed-bedrock-api-key", "us-west-2")
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        error.error.message,
        "account/login/start.amazonBedrock requires experimentalApi capability"
    );
    assert_eq!(load_file_auth(codex_home.path())?, None);
    Ok(())
}

#[tokio::test]
async fn login_managed_bedrock_updates_active_bedrock_account() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let request_id = mcp
        .send_login_account_amazon_bedrock_request("managed-bedrock-api-key", "us-west-2")
        .await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        to_response::<LoginAccountResponse>(response)?,
        LoginAccountResponse::AmazonBedrock {}
    );
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/login/completed"),
    )
    .await??;
    assert_account_updated(&mut mcp, Some(AuthMode::BedrockApiKey)).await?;
    assert_eq!(
        read_account(&mut mcp).await?,
        GetAccountResponse {
            workspace_routing: None,
            account: Some(Account::AmazonBedrock {
                uses_codex_managed_credentials: true,
            }),
            requires_openai_auth: false,
        }
    );

    login_with_api_key(
        codex_home.path(),
        "sk-test-key",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    assert!(codex_home.path().join("auth.json").exists());
    Ok(())
}

#[tokio::test]
async fn login_account_amazon_bedrock_rejects_invalid_credentials_without_changes() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let expected_config = read_config_toml(codex_home.path())?;

    let request_id = mcp
        .send_login_account_amazon_bedrock_request("  ", "us-west-2")
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        error.error.message,
        "Amazon Bedrock API key must not be empty."
    );

    let request_id = mcp
        .send_login_account_amazon_bedrock_request("managed-bedrock-api-key", "us-west-1")
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        error.error.message,
        "Amazon Bedrock does not support region `us-west-1`"
    );

    let request_id = mcp
        .send_login_account_request(json!({
            "type": "amazonBedrockAccessKeys",
            "accessKeyId": " ",
            "secretAccessKey": "test-secret",
            "region": "us-west-2",
        }))
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        error.error.message,
        "AWS access key ID and secret access key must not be empty."
    );
    assert_eq!(load_file_auth(codex_home.path())?, None);
    assert_eq!(read_config_toml(codex_home.path())?, expected_config);

    Ok(())
}

#[tokio::test]
async fn get_account_no_auth() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let params = GetAccountParams {
        refresh_token: false,
    };
    let request_id = mcp.send_get_account_request(params).await?;

    let account: GetAccountResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(request_id)).await??;

    assert_eq!(account.account, None, "expected no account");
    assert_eq!(account.requires_openai_auth, true);
    Ok(())
}

#[tokio::test]
async fn get_account_with_api_key() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let req_id = mcp
        .send_login_account_api_key_request("sk-test-key")
        .await?;
    let _login_ok: LoginAccountResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(req_id)).await??;

    let params = GetAccountParams {
        refresh_token: false,
    };
    let request_id = mcp.send_get_account_request(params).await?;

    let received: GetAccountResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(request_id)).await??;

    let expected = GetAccountResponse {
        workspace_routing: None,
        account: Some(Account::ApiKey {}),
        requires_openai_auth: true,
    };
    assert_eq!(received, expected);
    Ok(())
}

#[tokio::test]
async fn get_account_when_auth_not_required() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(false),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let params = GetAccountParams {
        refresh_token: false,
    };
    let request_id = mcp.send_get_account_request(params).await?;

    let received: GetAccountResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(request_id)).await??;

    let expected = GetAccountResponse {
        workspace_routing: None,
        account: None,
        requires_openai_auth: false,
    };
    assert_eq!(received, expected);
    Ok(())
}

#[tokio::test]
async fn get_account_with_aws_provider() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            model_provider_id: Some("amazon-bedrock".to_string()),
            extra_provider_config: Some(
                r#"[model_providers.amazon-bedrock.aws]
profile = "codex-bedrock"
region = "us-west-2"
"#
                .to_string(),
            ),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let params = GetAccountParams {
        refresh_token: false,
    };
    let request_id = mcp.send_get_account_request(params).await?;

    let received: GetAccountResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(request_id)).await??;

    let expected = GetAccountResponse {
        workspace_routing: None,
        account: Some(Account::AmazonBedrock {
            uses_codex_managed_credentials: false,
        }),
        requires_openai_auth: false,
    };
    assert_eq!(received, expected);
    Ok(())
}

#[tokio::test]
async fn get_account_with_user_managed_bedrock_provider() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            model_provider_id: Some("amazon-bedrock".to_string()),
            extra_provider_config: Some(
                r#"[model_providers.amazon-bedrock]
base_url = "https://bedrock.example.com/v1"

[model_providers.amazon-bedrock.auth]
command = "print-token"
"#
                .to_string(),
            ),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    assert_eq!(
        read_account(&mut mcp).await?,
        GetAccountResponse {
            workspace_routing: None,
            account: Some(Account::AmazonBedrock {
                uses_codex_managed_credentials: false,
            }),
            requires_openai_auth: false,
        }
    );
    Ok(())
}

#[tokio::test]
async fn account_reads_use_startup_config_when_config_reload_fails() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            model_provider_id: Some("amazon-bedrock".to_string()),
            extra_provider_config: Some(
                r#"[model_providers.amazon-bedrock.aws]
profile = "codex-bedrock"
region = "us-west-2"
"#
                .to_string(),
            ),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    std::fs::write(codex_home.path().join("config.toml"), "invalid = [")?;

    assert_eq!(
        read_account(&mut mcp).await?,
        GetAccountResponse {
            workspace_routing: None,
            account: Some(Account::AmazonBedrock {
                uses_codex_managed_credentials: false,
            }),
            requires_openai_auth: false,
        }
    );

    let request_id = mcp
        .send_get_auth_status_request(GetAuthStatusParams {
            include_token: Some(false),
            refresh_token: Some(false),
        })
        .await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        to_response::<GetAuthStatusResponse>(response)?,
        GetAuthStatusResponse {
            auth_method: None,
            auth_token: None,
            requires_openai_auth: Some(false),
        }
    );

    Ok(())
}

#[tokio::test]
async fn get_account_with_managed_bedrock_provider() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            model_provider_id: Some("amazon-bedrock".to_string()),
            ..Default::default()
        },
    )?;
    login_with_bedrock_api_key(
        codex_home.path(),
        "managed-bedrock-api-key",
        "us-west-2",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_get_account_request(GetAccountParams {
            refresh_token: false,
        })
        .await?;
    let received: GetAccountResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(request_id)).await??;

    assert_eq!(
        received,
        GetAccountResponse {
            workspace_routing: None,
            account: Some(Account::AmazonBedrock {
                uses_codex_managed_credentials: true,
            }),
            requires_openai_auth: false,
        }
    );
    Ok(())
}
