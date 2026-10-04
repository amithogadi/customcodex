//! Translate the small user catalog into the agent's existing model descriptors.

use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::openai_models::{
    InputModality, ModelVisibility, ModelsResponse, ReasoningEffortPreset, ToolMode,
};

pub(crate) fn catalog(provider: &ModelProviderInfo) -> Option<ModelsResponse> {
    if provider.models.is_empty() {
        return None;
    }
    let function_tools_only = provider
        .base_url
        .as_deref()
        .is_some_and(codex_api::is_openrouter_responses_provider);
    Some(ModelsResponse {
        models: provider
            .models
            .iter()
            .enumerate()
            .map(|(index, configured)| {
                let mut model =
                    codex_models_manager::model_info::model_info_with_defaults(&configured.id);
                model.display_name = configured
                    .name
                    .clone()
                    .unwrap_or_else(|| configured.id.clone());
                model.visibility = ModelVisibility::List;
                model.priority = index as i32;
                model.default_reasoning_level = configured.reasoning_effort.clone();
                model.supported_reasoning_levels = configured
                    .reasoning_effort
                    .iter()
                    .map(|effort| ReasoningEffortPreset {
                        effort: effort.clone(),
                        description: format!("{effort} reasoning"),
                    })
                    .collect();
                model.context_window = Some(configured.context_window);
                model.max_context_window = Some(configured.context_window);
                model.input_modalities = vec![InputModality::Text];
                model.supports_reasoning_summary_parameter = false;
                model.default_reasoning_summary = ReasoningSummary::None;
                if function_tools_only {
                    // Model metadata takes precedence over code_mode feature flags.
                    // OpenRouter's adapter cannot send the freeform exec tool.
                    model.tool_mode = Some(ToolMode::Direct);
                }
                model.used_fallback_model_metadata = false;
                model
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_model_provider_info::ConfiguredModel;
    use codex_protocol::openai_models::ReasoningEffort;
    use pretty_assertions::assert_eq;

    #[test]
    fn openrouter_catalog_forces_direct_tools_only_for_adapter_endpoints() {
        for (base_url, expected) in [
            ("https://openrouter.ai/api/v1", Some(ToolMode::Direct)),
            ("https://eu.openrouter.ai/api/v1/", Some(ToolMode::Direct)),
            ("https://us.openrouter.ai/api/v1", Some(ToolMode::Direct)),
            ("https://api.isoquant.ai/v1", None),
            ("https://openrouter.ai.example.com/api/v1", None),
            ("http://127.0.0.1:1/v1", None),
        ] {
            let info = ModelProviderInfo {
                base_url: Some(base_url.into()),
                models: vec![ConfiguredModel {
                    id: "test-model".into(),
                    name: None,
                    context_window: 65536,
                    reasoning_effort: None,
                    openrouter_providers: Vec::new(),
                    openrouter_zdr: None,
                }],
                ..Default::default()
            };
            let metadata = super::catalog(&info).unwrap().models.remove(0);
            assert_eq!(metadata.tool_mode, expected, "{base_url}");
        }
    }

    #[tokio::test]
    async fn explicit_models_replace_bundled_catalog_without_discovery() {
        let info = ModelProviderInfo {
            name: "Test".into(),
            base_url: Some("http://127.0.0.1:1/v1".into()),
            models: vec![ConfiguredModel {
                id: "qwen/qwen3.8-27b".into(),
                name: Some("Qwen".into()),
                context_window: 65536,
                reasoning_effort: Some(ReasoningEffort::High),
                openrouter_providers: Vec::new(),
                openrouter_zdr: None,
            }],
            ..Default::default()
        };
        let provider = crate::create_model_provider(info, None);
        let catalog = provider
            .models_manager_without_cache(None)
            .list_models(
                codex_models_manager::manager::RefreshStrategy::Online,
                codex_http_client::HttpClientFactory::new(
                    codex_http_client::OutboundProxyPolicy::ReqwestDefault,
                ),
            )
            .await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].model, "qwen/qwen3.8-27b");
        assert_eq!(catalog[0].default_reasoning_effort, ReasoningEffort::High);
        let metadata = super::catalog(provider.info()).unwrap().models.remove(0);
        assert_eq!(metadata.context_window, Some(65536));
        assert!(!metadata.supports_reasoning_summary_parameter);
        assert!(!provider.capabilities().web_search);
        assert!(!provider.capabilities().image_generation);
    }
}
