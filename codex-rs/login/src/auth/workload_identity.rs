//! Detect retired workload identity settings so startup can fail without exchanging tokens.
use codex_protocol::shell_environment::OPENAI_FEDERATION_RULE_ID_ENV_VAR;
use codex_protocol::shell_environment::OPENAI_IDENTITY_TOKEN_FILE_ENV_VAR;

pub fn is_workload_identity_selected() -> bool {
    std::env::var_os(OPENAI_FEDERATION_RULE_ID_ENV_VAR).is_some()
        || std::env::var_os(OPENAI_IDENTITY_TOKEN_FILE_ENV_VAR).is_some()
}
