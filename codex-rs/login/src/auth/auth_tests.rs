use super::*;
use crate::auth::storage::AuthStorageBackend;
use crate::auth::storage::FileAuthStorage;
use crate::auth::storage::get_auth_file;
use crate::token_data::IdTokenInfo;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::auth::AuthMode;
use codex_protocol::auth::KnownPlan as InternalKnownPlan;
use codex_protocol::auth::PlanType as InternalPlanType;
use codex_protocol::protocol::SessionSource;

use base64::Engine;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_protocol::config_types::ModelProviderAuthInfo;
use codex_protocol::shell_environment::OPENAI_FEDERATION_RULE_ID_ENV_VAR;
use codex_protocol::shell_environment::OPENAI_IDENTITY_TOKEN_FILE_ENV_VAR;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::TempDir;
use tempfile::tempdir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const WORKSPACE_ID_ALLOWED: &str = "123e4567-e89b-42d3-a456-426614174000";
const WORKSPACE_ID_SECOND_ALLOWED: &str = "123e4567-e89b-42d3-a456-426614174001";
const WORKSPACE_ID_DISALLOWED: &str = "123e4567-e89b-42d3-a456-426614174002";

#[test]
fn header_auth_exposes_a_valid_chatgpt_account_id() {
    for (account_id_header, expected_account_id) in [
        (
            Some(WORKSPACE_ID_ALLOWED.as_bytes()),
            Some(WORKSPACE_ID_ALLOWED),
        ),
        (None, None),
        (Some(b""), None),
        (Some(b" account "), None),
        (Some(&[0xff]), None),
    ] {
        let mut headers = http::HeaderMap::new();
        if let Some(account_id_header) = account_id_header {
            headers.insert(
                "chatgpt-account-id",
                http::HeaderValue::from_bytes(account_id_header).expect("valid header bytes"),
            );
        }

        let auth = CodexAuth::Headers(AuthHeaders::new(headers));
        assert_eq!(auth.get_account_id().as_deref(), expected_account_id);
    }
}

#[test]
fn login_with_api_key_overwrites_existing_auth_json() {
    let dir = tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    let stale_auth = json!({
        "OPENAI_API_KEY": "sk-old",
        "tokens": {
            "id_token": "stale.header.payload",
            "access_token": "stale-access",
            "refresh_token": "stale-refresh",
            "account_id": "stale-acc"
        }
    });
    std::fs::write(
        &auth_path,
        serde_json::to_string_pretty(&stale_auth).unwrap(),
    )
    .unwrap();

    super::login_with_api_key(
        dir.path(),
        "sk-new",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .expect("login_with_api_key should succeed");

    let storage = FileAuthStorage::new(dir.path().to_path_buf());
    let auth = storage
        .try_read_auth_json(&auth_path)
        .expect("auth.json should parse");
    assert_eq!(auth.openai_api_key.as_deref(), Some("sk-new"));
    assert!(auth.tokens.is_none(), "tokens should be cleared");
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn missing_auth_json_returns_none() {
    let dir = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let auth = CodexAuth::from_auth_storage(
        dir.path(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
        AuthKeyringBackendKind::default(),
        &crate::test_support::transport_default_auth_route_config(),
    )
    .await
    .expect("call should succeed");
    assert_eq!(auth, None);
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn loads_api_key_from_auth_json() {
    let dir = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let auth_file = dir.path().join("auth.json");
    std::fs::write(
        auth_file,
        r#"{"OPENAI_API_KEY":"sk-test-key","tokens":null,"last_refresh":null}"#,
    )
    .unwrap();

    let auth = super::load_auth(
        dir.path(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*allowed_login_methods*/ None,
        /*forced_chatgpt_workspace_id*/ None,
        /*chatgpt_base_url*/ None,
        AuthKeyringBackendKind::Direct,
        /*agent_identity_authapi_base_url*/ None,
        &crate::test_support::transport_default_auth_route_config(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(auth.auth_mode(), AuthMode::ApiKey);
    assert_eq!(auth.api_key(), Some("sk-test-key"));

    assert!(auth.get_token_data().is_err());
}

#[test]
fn logout_removes_auth_file() -> Result<(), std::io::Error> {
    let dir = tempdir()?;
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("sk-test-key".to_string()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    };
    super::save_auth(
        dir.path(),
        &auth_dot_json,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    let auth_file = get_auth_file(dir.path());
    assert!(auth_file.exists());
    assert!(logout(
        dir.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?);
    assert!(!auth_file.exists());
    Ok(())
}

#[tokio::test]
async fn external_bearer_only_auth_manager_uses_cached_provider_token() {
    let script = ProviderAuthScript::new(&["provider-token", "next-token"]).unwrap();
    let manager = AuthManager::external_bearer_only(script.auth_config());

    let first = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));
    let second = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));

    assert_eq!(first.as_deref(), Some("provider-token"));
    assert_eq!(second.as_deref(), Some("provider-token"));
    assert_eq!(manager.auth_mode(), Some(AuthMode::ApiKey));
    assert_eq!(manager.get_api_auth_mode(), Some(AuthMode::ApiKey));
}

#[tokio::test]
async fn external_bearer_only_auth_manager_disables_auto_refresh_when_interval_is_zero() {
    let script = ProviderAuthScript::new(&["provider-token", "next-token"]).unwrap();
    let mut auth_config = script.auth_config();
    auth_config.refresh_interval_ms = 0;
    let manager = AuthManager::external_bearer_only(auth_config);

    let first = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));
    let second = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));

    assert_eq!(first.as_deref(), Some("provider-token"));
    assert_eq!(second.as_deref(), Some("provider-token"));
}

#[tokio::test]
async fn external_bearer_only_auth_manager_returns_none_when_command_fails() {
    let script = ProviderAuthScript::new_failing().unwrap();
    let manager = AuthManager::external_bearer_only(script.auth_config());

    assert_eq!(manager.auth().await, None);
}

#[tokio::test]
async fn unauthorized_recovery_retries_provider_command_after_initial_failure() {
    let script = ProviderAuthScript::new(&["provider-token"]).unwrap();
    std::fs::write(script.tempdir.path().join("fail-once"), "").unwrap();
    let manager = AuthManager::external_bearer_only(script.auth_config());
    let mut recovery = manager.unauthorized_recovery();

    assert_eq!(manager.auth().await, None);
    assert_eq!(manager.auth_cached(), None);
    assert!(recovery.has_next());
    assert_eq!(recovery.unavailable_reason(), "ready");

    let result = recovery
        .next()
        .await
        .expect("external refresh should succeed");

    assert_eq!(result.auth_state_changed(), Some(true));
    assert_eq!(
        manager.auth_cached(),
        Some(CodexAuth::from_api_key("provider-token"))
    );
    assert!(!recovery.has_next());
    assert_eq!(recovery.unavailable_reason(), "recovery_exhausted");
    recovery.next().await.expect_err("recovery is bounded");
}

#[test]
fn unauthorized_recovery_without_an_external_provider_still_requires_refreshable_auth() {
    for auth in [None, Some(CodexAuth::from_api_key("static-token"))] {
        let manager = AuthManager::from_optional_auth_for_testing(auth);
        let recovery = manager.unauthorized_recovery();

        assert!(!recovery.has_next());
        assert_eq!(recovery.unavailable_reason(), "not_chatgpt_auth");
    }
}

#[tokio::test]
async fn unauthorized_recovery_uses_external_refresh_for_bearer_manager() {
    let script = ProviderAuthScript::new(&["provider-token", "refreshed-provider-token"]).unwrap();
    let mut auth_config = script.auth_config();
    auth_config.refresh_interval_ms = 0;
    let manager = AuthManager::external_bearer_only(auth_config);
    let mut recovery = manager.unauthorized_recovery();
    let initial_token = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));

    assert!(recovery.has_next());
    assert_eq!(recovery.mode_name(), "external");
    assert_eq!(recovery.step_name(), "external_refresh");

    let result = recovery
        .next()
        .await
        .expect("external refresh should succeed");

    assert_eq!(result.auth_state_changed(), Some(true));
    let refreshed_token = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));
    assert_eq!(initial_token.as_deref(), Some("provider-token"));
    assert_eq!(refreshed_token.as_deref(), Some("refreshed-provider-token"));
}

#[derive(Clone)]
struct StaticExternalAuth(CodexAuth);

impl ExternalAuth for StaticExternalAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.0.clone()) })
    }

    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

struct RefreshingExternalAuth {
    initial: CodexAuth,
    refreshed: CodexAuth,
}

impl ExternalAuth for RefreshingExternalAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.initial.clone()) })
    }

    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.refreshed.clone()) })
    }
}

fn external_header_auth(account_id: Option<&'static str>) -> CodexAuth {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_static("Bearer external"),
    );
    if let Some(account_id) = account_id {
        headers.insert(
            "chatgpt-account-id",
            http::HeaderValue::from_static(account_id),
        );
    }
    CodexAuth::Headers(AuthHeaders::new(headers))
}

struct FailingExternalAuth {
    auth: CodexAuth,
    resolve_count: AtomicUsize,
}

impl ExternalAuth for FailingExternalAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        let resolve_count = self.resolve_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if resolve_count == 0 {
                Ok(self.auth.clone())
            } else {
                Err(std::io::Error::other("external auth failed"))
            }
        })
    }

    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Err(std::io::Error::other("external auth failed")) })
    }

    fn classify_error(&self, error: std::io::Error) -> RefreshTokenError {
        RefreshTokenError::Permanent(RefreshTokenFailedError::new(
            RefreshTokenFailedReason::Other,
            error.to_string(),
        ))
    }
}

#[tokio::test]
async fn external_auth_keeps_cached_credentials_after_permanent_reload_failure() {
    let manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("seed"));
    let auth = CodexAuth::from_api_key("configured-token");
    let external_auth = Arc::new(FailingExternalAuth {
        auth: auth.clone(),
        resolve_count: AtomicUsize::new(0),
    });
    manager
        .set_external_auth(external_auth.clone())
        .await
        .expect("external auth should install");

    assert_eq!(external_auth.resolve_count.load(Ordering::SeqCst), 1);

    assert_eq!(manager.auth().await, Some(auth.clone()));
    assert_eq!(external_auth.resolve_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        manager
            .refresh_failure_for_auth(&auth)
            .expect("permanent failure should be recorded")
            .to_string(),
        "external auth failed"
    );

    assert_eq!(manager.auth().await, Some(auth));
    assert_eq!(external_auth.resolve_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn replacing_external_auth_clears_permanent_failure() {
    let manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("seed"));
    let auth = CodexAuth::from_api_key("external-token");
    manager
        .set_external_auth(Arc::new(FailingExternalAuth {
            auth: auth.clone(),
            resolve_count: AtomicUsize::new(0),
        }))
        .await
        .expect("external auth should install");

    assert_eq!(manager.auth().await, Some(auth.clone()));
    assert!(manager.refresh_failure_for_auth(&auth).is_some());

    manager
        .set_external_auth(Arc::new(StaticExternalAuth(auth.clone())))
        .await
        .expect("replacement external auth should install");

    manager
        .refresh_token_from_authority()
        .await
        .expect("replacement external auth should refresh");
    assert_eq!(manager.auth_cached(), Some(auth));
}

#[tokio::test]
async fn runtime_external_auth_uses_provider_error_classification() {
    let manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("seed"));
    manager
        .set_external_auth(Arc::new(FailingExternalAuth {
            auth: CodexAuth::from_api_key("runtime-token"),
            resolve_count: AtomicUsize::new(0),
        }))
        .await
        .expect("runtime auth should install");

    assert!(matches!(
        manager.refresh_token_from_authority().await,
        Err(RefreshTokenError::Permanent(_))
    ));
}

#[tokio::test]
async fn external_auth_provider_can_install_headers() {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_static("Bearer external"),
    );
    headers.insert("x-external-auth", http::HeaderValue::from_static("enabled"));
    let auth = CodexAuth::Headers(AuthHeaders::new(headers));
    let codex_home = tempdir().expect("tempdir");
    let manager = AuthManager::new(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::Ephemeral,
        /*forced_chatgpt_workspace_id*/ None,
        /*chatgpt_base_url*/ None,
        AuthKeyringBackendKind::default(),
        crate::test_support::transport_default_auth_route_config(),
    )
    .await;

    manager
        .set_external_auth(Arc::new(StaticExternalAuth(auth.clone())))
        .await
        .expect("external auth should install");

    assert_eq!(manager.auth_cached(), Some(auth));
    assert!(
        manager
            .auth_cached()
            .is_some_and(|auth| auth.uses_codex_backend())
    );
    assert!(
        !manager
            .auth_cached()
            .is_some_and(|auth| auth.is_chatgpt_auth())
    );
}

#[tokio::test]
async fn external_header_auth_obeys_workspace_policy() {
    for (account_id, should_succeed) in [
        (Some(WORKSPACE_ID_ALLOWED), true),
        (Some(WORKSPACE_ID_DISALLOWED), false),
        (None, false),
    ] {
        let auth = external_header_auth(account_id);
        let expected_auth = should_succeed.then_some(auth.clone());
        let manager = AuthManager::from_optional_auth_for_testing(/*auth*/ None);
        manager.set_forced_chatgpt_workspace_id(Some(vec![WORKSPACE_ID_ALLOWED.to_string()]));

        let result = manager
            .set_external_auth(Arc::new(StaticExternalAuth(auth)))
            .await;

        assert_eq!(result.is_ok(), should_succeed, "account ID: {account_id:?}");
        assert_eq!(manager.auth_cached(), expected_auth);
    }
}

#[tokio::test]
async fn external_header_auth_rejects_a_disallowed_workspace_on_refresh() {
    let allowed_auth = external_header_auth(Some(WORKSPACE_ID_ALLOWED));
    let disallowed_auth = external_header_auth(Some(WORKSPACE_ID_DISALLOWED));
    let manager = AuthManager::from_optional_auth_for_testing(/*auth*/ None);
    manager.set_forced_chatgpt_workspace_id(Some(vec![WORKSPACE_ID_ALLOWED.to_string()]));
    manager
        .set_external_auth(Arc::new(RefreshingExternalAuth {
            initial: allowed_auth.clone(),
            refreshed: disallowed_auth,
        }))
        .await
        .expect("initial external header auth should install");

    manager
        .refresh_token_from_authority()
        .await
        .expect_err("external header auth from a disallowed workspace must not replace the cache");

    assert_eq!(manager.auth_cached(), Some(allowed_auth));
}

struct ProviderAuthScript {
    tempdir: TempDir,
    command: String,
    args: Vec<String>,
}

impl ProviderAuthScript {
    fn new(tokens: &[&str]) -> std::io::Result<Self> {
        let tempdir = tempfile::tempdir()?;
        let token_file = tempdir.path().join("tokens.txt");
        // `cmd.exe`'s `set /p` treats LF-only input as one line, so use CRLF on Windows.
        let token_line_ending = if cfg!(windows) { "\r\n" } else { "\n" };
        let mut token_file_contents = String::new();
        for token in tokens {
            token_file_contents.push_str(token);
            token_file_contents.push_str(token_line_ending);
        }
        std::fs::write(&token_file, token_file_contents)?;

        #[cfg(unix)]
        let (command, args) = {
            let script_path = tempdir.path().join("print-token.sh");
            codex_utils_cargo_bin::write_executable(
                &script_path,
                r#"#!/bin/sh
if [ -f fail-once ]; then
    rm fail-once
    exit 1
fi
first_line=$(sed -n '1p' tokens.txt)
printf '%s\n' "$first_line"
tail -n +2 tokens.txt > tokens.next
mv tokens.next tokens.txt
"#,
            )?;
            ("./print-token.sh".to_string(), Vec::new())
        };

        #[cfg(windows)]
        let (command, args) = {
            let script_path = tempdir.path().join("print-token.cmd");
            std::fs::write(
                &script_path,
                r#"@echo off
setlocal EnableExtensions DisableDelayedExpansion
if exist fail-once (
    del fail-once
    exit /b 1
)
set "first_line="
<tokens.txt set /p "first_line="
if not defined first_line exit /b 1
setlocal EnableDelayedExpansion
echo(!first_line!
endlocal
more +1 tokens.txt > tokens.next
move /y tokens.next tokens.txt >nul
"#,
            )?;
            (
                "cmd.exe".to_string(),
                vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    ".\\print-token.cmd".to_string(),
                ],
            )
        };

        Ok(Self {
            tempdir,
            command,
            args,
        })
    }

    fn new_failing() -> std::io::Result<Self> {
        let tempdir = tempfile::tempdir()?;

        #[cfg(unix)]
        let (command, args) = {
            let script_path = tempdir.path().join("fail.sh");
            codex_utils_cargo_bin::write_executable(
                &script_path,
                r#"#!/bin/sh
exit 1
"#,
            )?;
            ("./fail.sh".to_string(), Vec::new())
        };

        #[cfg(windows)]
        let (command, args) = (
            "cmd.exe".to_string(),
            vec![
                "/d".to_string(),
                "/s".to_string(),
                "/c".to_string(),
                "exit /b 1".to_string(),
            ],
        );

        Ok(Self {
            tempdir,
            command,
            args,
        })
    }

    fn auth_config(&self) -> ModelProviderAuthInfo {
        serde_json::from_value(json!({
            "command": self.command,
            "args": self.args,
            // Process startup can be slow on loaded Windows CI workers, so leave enough slack to
            // avoid turning these auth-cache assertions into a process-launch timing test.
            "timeout_ms": 10_000,
            "refresh_interval_ms": 60000,
            "cwd": self.tempdir.path(),
        }))
        .expect("provider auth config should deserialize")
    }
}

struct AuthFileParams {
    openai_api_key: Option<String>,
    chatgpt_plan_type: Option<String>,
    chatgpt_account_id: Option<String>,
}

fn write_auth_file(params: AuthFileParams, codex_home: &Path) -> std::io::Result<String> {
    let fake_jwt = fake_jwt_for_auth_file_params(&params)?;
    let auth_file = get_auth_file(codex_home);
    let auth_json_data = json!({
        "OPENAI_API_KEY": params.openai_api_key,
        "tokens": {
            "id_token": fake_jwt,
            "access_token": "test-access-token",
            "refresh_token": "test-refresh-token"
        },
        "last_refresh": Utc::now(),
    });
    let auth_json = serde_json::to_string_pretty(&auth_json_data)?;
    std::fs::write(auth_file, auth_json)?;
    Ok(fake_jwt)
}

fn fake_jwt_for_auth_file_params(params: &AuthFileParams) -> std::io::Result<String> {
    #[derive(Serialize)]
    struct Header {
        alg: &'static str,
        typ: &'static str,
    }

    let header = Header {
        alg: "none",
        typ: "JWT",
    };
    let mut auth_payload = serde_json::json!({
        "chatgpt_user_id": "user-12345",
        "user_id": "user-12345",
    });

    if let Some(chatgpt_plan_type) = params.chatgpt_plan_type.as_ref() {
        auth_payload["chatgpt_plan_type"] = serde_json::Value::String(chatgpt_plan_type.clone());
    }

    if let Some(chatgpt_account_id) = params.chatgpt_account_id.as_ref() {
        auth_payload["chatgpt_account_id"] = serde_json::Value::String(chatgpt_account_id.clone());
    }

    let payload = serde_json::json!({
        "email": "user@example.com",
        "email_verified": true,
        "https://api.openai.com/auth": auth_payload,
    });
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let header_b64 = b64(&serde_json::to_vec(&header)?);
    let payload_b64 = b64(&serde_json::to_vec(&payload)?);
    let signature_b64 = b64(b"sig");
    Ok(format!("{header_b64}.{payload_b64}.{signature_b64}"))
}

async fn build_config(
    codex_home: &Path,
    forced_login_method: Option<ForcedLoginMethod>,
    forced_chatgpt_workspace_id: Option<Vec<String>>,
) -> AuthConfig {
    AuthConfig {
        codex_home: codex_home.to_path_buf(),
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        keyring_backend_kind: AuthKeyringBackendKind::Direct,
        forced_login_method,
        forced_chatgpt_workspace_id,
        managed_auth_policy: ManagedAuthPolicy::default(),
        chatgpt_base_url: None,
        auth_route_config: crate::test_support::transport_default_auth_route_config(),
    }
}

/// Use sparingly.
/// TODO (gpeal): replace this with an injectable env var provider.
#[cfg(test)]
struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let original = env::var_os(key);
        unsafe {
            env::set_var(key, value);
        }
        Self { key, original }
    }

    fn remove(key: &'static str) -> Self {
        let original = env::var_os(key);
        unsafe {
            env::remove_var(key);
        }
        Self { key, original }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }
}

fn remove_access_token_env_var() -> EnvVarGuard {
    EnvVarGuard::remove(CODEX_ACCESS_TOKEN_ENV_VAR)
}

struct TestAuthManagerConfig(AuthConfig);

impl AuthManagerConfig for TestAuthManagerConfig {
    fn codex_home(&self) -> PathBuf {
        self.0.codex_home.clone()
    }

    fn cli_auth_credentials_store_mode(&self) -> AuthCredentialsStoreMode {
        self.0.auth_credentials_store_mode
    }

    fn auth_keyring_backend_kind(&self) -> AuthKeyringBackendKind {
        self.0.keyring_backend_kind
    }

    fn forced_login_method(&self) -> Option<ForcedLoginMethod> {
        self.0.forced_login_method
    }

    fn forced_chatgpt_workspace_id(&self) -> Option<Vec<String>> {
        self.0.forced_chatgpt_workspace_id.clone()
    }

    fn managed_auth_policy(&self) -> ManagedAuthPolicy {
        self.0.managed_auth_policy.clone()
    }

    fn chatgpt_base_url(&self) -> String {
        self.0
            .chatgpt_base_url
            .clone()
            .expect("test config should include a ChatGPT base URL")
    }

    fn auth_route_config(&self) -> AuthRouteConfig {
        self.0.auth_route_config.clone()
    }
}

fn test_auth_manager_config(codex_home: &Path) -> TestAuthManagerConfig {
    TestAuthManagerConfig(AuthConfig {
        codex_home: codex_home.to_path_buf(),
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        keyring_backend_kind: AuthKeyringBackendKind::Direct,
        forced_login_method: Some(ForcedLoginMethod::Chatgpt),
        chatgpt_base_url: Some("https://chatgpt-staging.com/backend-api".to_string()),
        forced_chatgpt_workspace_id: Some(vec!["forced-workspace".to_string()]),
        managed_auth_policy: ManagedAuthPolicy {
            allowed_login_methods: Some(vec![ForcedLoginMethod::Chatgpt]),
            allowed_chatgpt_workspaces: Some(vec![
                "forced-workspace".to_string(),
                "managed-workspace".to_string(),
            ]),
        },
        auth_route_config: AuthRouteConfig::from_http_client_factory(HttpClientFactory::new(
            OutboundProxyPolicy::RespectSystemProxy,
        )),
    })
}

#[test]
fn auth_config_from_preserves_all_fields() {
    let codex_home = tempdir().expect("tempdir");
    let config = test_auth_manager_config(codex_home.path());

    assert_eq!(auth_config_from(&config), config.0);
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn api_only_policy_rejects_access_tokens_before_hydration() {
    let codex_home = tempdir().unwrap();
    let server = MockServer::start().await;
    let _authapi_guard = EnvVarGuard::set("CODEX_AUTHAPI_BASE_URL", &server.uri());
    let _access_token_guard = EnvVarGuard::set(CODEX_ACCESS_TOKEN_ENV_VAR, "at-rejected");
    let mut config = build_config(
        codex_home.path(),
        /*forced_login_method*/ None,
        /*forced_chatgpt_workspace_id*/ None,
    )
    .await;
    config.managed_auth_policy.allowed_login_methods = Some(vec![ForcedLoginMethod::Api]);
    let manager =
        AuthManager::shared_from_auth_config(config, /*enable_codex_api_key_env*/ false)
            .await
            .expect("auth manager");

    assert_eq!(manager.auth().await, None);
    assert!(
        server
            .received_requests()
            .await
            .expect("inspect auth requests")
            .is_empty(),
        "rejected access tokens must not call whoami or register Agent Identity"
    );
}

const TEST_AGENT_IDENTITY_RSA_PRIVATE_KEY_PEM: &[u8] = br#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDWpAXYypOsYAwO
bvBduMk/mxaoYDze0AZSzaSzLuIlcsl2EKDgC3AabhIWXh/qTGEJLOU3VB1e5mO9
FPbBlmIZSL3FQTbyt/hYutPFKfCou5PLmScw/TzILS3/RhT8UY9kxxZvXiEbTki9
mvxRuZFpVqDFJHwfitIjKZGhXDCYVKurPTrxetYZJg0h8sQBLKjkZ0BqqaTUkAsg
0eBgZAlXEzG3By8PGhUqYLt6W1Q3KYw0FmGy/gTyzH1g0ukGgSJvOd8SkNT8MbOs
zl5kKxDNqpuEE6UZ3jbuJ+5382d31w+rOAJRzbf7QVdI9+luCSwJcDACYPQ4WNBa
uCpV0ovpAgMBAAECggEAVu84LwZdqYN9XpswX8VoPYrjMm9IODapWQBRpQFoNyK2
1ksF3bjEPvA2Azk8U/l7k+vLKw22l6lY3EyRZPcz5GnB8xLm3ogE3mtNOp4yCyVu
RxhQ91aaN7mU17/a4BdorLi2LYVCg3zBmYociD1Q2AluNGsCmwPu+K7tfR2J0Sg8
NjqiTbDG1XDpR/icwgC9t6vh8lZpCHDhF4tbQfLLVLeA/OdcuzXDyMCXbmdVIdBQ
rm4aIFmr2e1/2ctTbCg85S6AGFTH+pSLjrwTzyvf+F6NW5uNjLQAQLFj+EznBDxj
Xdx90cySrjsKK6PVWQF4RiTvkSW8eWL7R6B2FZbGwQKBgQDuVQRj72hWloR7mbEL
aUEEv3pIXTMXWEsoMBNczos/1L1RnAN1AI44TurznasPZAWvQj+kVbLDR+TAeZrL
iA8HIWswQUI18hFmgKzSkwIXGtubcKVrgsKeS4lMDKCM/Ef6WAYdeq6ronoY5lCN
YrJFmGp81W5zcV7lyiycgbSiGwKBgQDmjWYf6pZjrK7Z+OJ3X1AZfi2vss15SCvL
3fPgzIDbViztpGyQhc3DQZIsBNIu0xZp/veGce9TEeTds2ro9NfdJFeou8+fC7Pq
sOsM3amGFFi+ZW/9BWyjZEM88bgWWAjqLHbpfHDxjAf5CSxddqxgHlbP0Ytyb1Vg
gmPDn9YKSwKBgQDbTi3hC35WFuDHn0/zcSHcDZmnFuOZeqyFyV83yfMGhGrEuqvP
sPgtRikajJ3IZsB4WZyYSidZXEFY/0z6NjOl2xF38MTNQPbT/FmK1q1Yt2UWrlv5
BvSwlk87RG9D7C0LZo4R+D7cPoDdgqjiwMvMEIkEX5zn641oI1ZTmWKuuwKBgQCD
KF+3unnRvHRAVoFnTZbA2fJdqMeRvogD04GhGlYX8V9f1hFY6nXTJaNlXVzA/J8c
r8ra9kgjJuPfZ+ljG58OFFW2DRohLcQtuHYPfK6rMzoFHqnl9EcIcMp7ijuionR3
29HOJFgQYgxLFXfit9d6WugiE+BTupiEbckZif13HwKBgE/lAlkVHP6YahOO2Ljc
J1bwkqKZTB5dHolX9A58e/xXnfZ5P8f3Z83+Izap3FwqQulk7b1WO1MQcHuVg2NN
5da0D4h2rYOXnbYIg0BVu4spQbaM6ewsp66b8+MzLOBvj8SzWdt1Oyw0q/MRyQAR
8U4M2TSWCKUY/A6sT4W8+mT9
-----END PRIVATE KEY-----"#;

#[tokio::test]
async fn saved_chatgpt_credentials_are_ignored_without_modifying_the_file() {
    let home = tempdir().unwrap();
    let id_token = fake_jwt_for_auth_file_params(&AuthFileParams {
        openai_api_key: None,
        chatgpt_plan_type: None,
        chatgpt_account_id: None,
    })
    .unwrap();
    let original = serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {"id_token": id_token, "access_token": "old-access", "refresh_token": "old-refresh"},
        "last_refresh": "2000-01-01T00:00:00Z"
    }).to_string();
    std::fs::write(home.path().join("auth.json"), &original).unwrap();
    let auth = CodexAuth::from_auth_storage(
        home.path(),
        AuthCredentialsStoreMode::File,
        None,
        AuthKeyringBackendKind::default(),
        &crate::test_support::transport_default_auth_route_config(),
    )
    .await
    .unwrap();
    assert!(auth.is_none());
    assert_eq!(
        std::fs::read_to_string(home.path().join("auth.json")).unwrap(),
        original
    );
}

#[tokio::test]
async fn removed_mandatory_login_policy_fails_without_deleting_credentials() {
    let home = tempdir().unwrap();
    login_with_api_key(
        home.path(),
        "provider-key",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .unwrap();
    let before = std::fs::read(home.path().join("auth.json")).unwrap();
    let config = build_config(home.path(), Some(ForcedLoginMethod::Chatgpt), None).await;
    assert!(
        enforce_login_restrictions(&config)
            .await
            .unwrap_err()
            .to_string()
            .contains("removed ChatGPT")
    );
    assert_eq!(
        std::fs::read(home.path().join("auth.json")).unwrap(),
        before
    );
}
