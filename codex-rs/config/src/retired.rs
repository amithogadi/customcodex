//! Compatibility diagnostics for settings belonging to removed online services.

pub(crate) fn is_retired_config_path(path: &[String]) -> bool {
    let path = match path {
        [profiles, _, rest @ ..] if profiles == "profiles" => rest,
        path => path,
    };
    match path {
        [key, ..]
            if matches!(
                key.as_str(),
                "analytics"
                    | "otel"
                    | "feedback"
                    | "audio"
                    | "realtime"
                    | "apps"
                    | "apps_mcp_product_sku"
                    | "forced_login_method"
                    | "forced_chatgpt_workspace_id"
                    | "browser_use"
                    | "in_app_browser"
                    | "computer_use"
                    | "check_for_update_on_startup"
            ) =>
        {
            true
        }
        [key, ..] if key.starts_with("experimental_realtime_") => true,
        [section, feature, ..] if section == "features" => is_retired_feature(feature),
        _ => false,
    }
}

fn is_retired_feature(feature: &str) -> bool {
    matches!(
        feature,
        "apps"
            | "remote_plugin"
            | "remote_control"
            | "realtime_conversation"
            | "browser_use"
            | "in_app_browser"
            | "computer_use"
            | "voice_transcription"
            | "apps_mcp_gateway"
            | "codex_apps_mcp_20260728"
            | "analytics_plan_history"
            | "runtime_metrics"
            | "guardianv2_decisions_comparison"
            | "use_agent_identity"
            | "in_app_voice"
    )
}

pub(crate) fn required_retired_capability(value: &toml::Value) -> Option<String> {
    if value
        .get("forced_login_method")
        .and_then(toml::Value::as_str)
        == Some("chatgpt")
    {
        return Some("forced_login_method".to_string());
    }
    if value
        .get("forced_chatgpt_workspace_id")
        .is_some_and(|workspaces| match workspaces {
            toml::Value::String(workspace) => !workspace.trim().is_empty(),
            toml::Value::Array(workspaces) => !workspaces.is_empty(),
            _ => true,
        })
    {
        return Some("forced_chatgpt_workspace_id".to_string());
    }
    for section in ["features", "feature_requirements"] {
        if let Some(features) = value.get(section).and_then(toml::Value::as_table) {
            for (feature, enabled) in features {
                if is_retired_feature(feature) && enabled.as_bool() == Some(true) {
                    return Some(format!("{section}.{feature}"));
                }
            }
        }
    }
    for section in ["analytics", "feedback"] {
        if value
            .get(section)
            .and_then(|v| v.get("enabled"))
            .and_then(toml::Value::as_bool)
            == Some(true)
        {
            return Some(format!("{section}.enabled"));
        }
    }
    if value
        .get("otel")
        .is_some_and(|v| v.as_table().is_some_and(|table| !table.is_empty()))
    {
        return Some("otel".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_requirements_fail_closed_only_when_requested() {
        let required: toml::Value = toml::from_str("[features]\nremote_plugin = true").unwrap();
        assert_eq!(
            required_retired_capability(&required).as_deref(),
            Some("features.remote_plugin")
        );
        let disabled: toml::Value =
            toml::from_str("[features]\nremote_plugin = false\n[feedback]\nenabled = false")
                .unwrap();
        assert_eq!(required_retired_capability(&disabled), None);
        for (contents, expected) in [
            ("forced_login_method = 'chatgpt'", "forced_login_method"),
            (
                "forced_chatgpt_workspace_id = 'workspace'",
                "forced_chatgpt_workspace_id",
            ),
        ] {
            let required = toml::from_str(contents).unwrap();
            assert_eq!(
                required_retired_capability(&required).as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn custom_provider_settings_are_never_retired() {
        assert!(!is_retired_config_path(&[
            "model_providers".into(),
            "remote_plugin".into(),
            "base_url".into()
        ]));
        assert!(!is_retired_config_path(&[
            "mcp_servers".into(),
            "apps".into()
        ]));
        assert!(is_retired_config_path(&[
            "profiles".into(),
            "work".into(),
            "otel".into()
        ]));
    }
}
