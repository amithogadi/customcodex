//! Apply session model choices separately from saved model defaults.
//!
//! A successful config write can still be overridden. Report that distinction without
//! replacing the active task's explicit selection with launch-time configuration.

use super::App;
use crate::app_server_session::AppServerSession;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ConfigEdit;
use codex_app_server_protocol::WriteStatus;
use codex_protocol::config_types::ModeKind;
use codex_protocol::openai_models::ReasoningEffort;
use color_eyre::eyre::Result;

/// Validate and stage a complete provider choice without mutating the active session.
pub(super) fn apply_configured_model(
    config: &mut crate::legacy_core::config::Config,
    provider_id: &str,
    model_id: &str,
) -> Result<()> {
    if config
        .config_layer_stack
        .required_model_provider()
        .is_some_and(|required| required != provider_id)
    {
        color_eyre::eyre::bail!("The selected provider is disallowed by managed configuration");
    }
    let provider = config
        .model_providers
        .get(provider_id)
        .ok_or_else(|| color_eyre::eyre::eyre!("Unknown provider: {provider_id}"))?
        .clone();
    provider
        .validate()
        .map_err(|err| color_eyre::eyre::eyre!(err))?;
    // Resolve env-key errors before opening the replacement thread; never print the value.
    provider.api_key()?;
    let model = provider
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "Model is not configured for provider {provider_id}: {model_id}"
            )
        })?;
    config.model_reasoning_effort = model.reasoning_effort.clone();
    config.plan_mode_reasoning_effort = model.reasoning_effort.clone();
    config.model_reasoning_summary = Some(codex_protocol::config_types::ReasoningSummary::None);
    config.model_context_window = Some(model.context_window);
    config.service_tier = None;
    config.model = Some(model.id.clone());
    config.model_provider_id = provider_id.to_owned();
    config.model_provider = provider;
    Ok(())
}

impl App {
    pub(super) async fn select_session_model(
        &mut self,
        app_server: &mut AppServerSession,
        model: String,
        effort: Option<ReasoningEffort>,
    ) {
        let model_changed = self.chat_widget.current_model() != model
            || self.chat_widget.current_collaboration_mode().model() != model;
        if model_changed
            && self
                .active_thread_model_setting_update_params(model.clone())
                .is_some_and(|params| params.permissions.is_some())
            && self.reject_pending_permission_change()
        {
            return;
        }
        let in_plan_mode = self.chat_widget.effective_collaboration_mode().mode == ModeKind::Plan;
        let ultra = effort == Some(ReasoningEffort::Ultra);
        let clear_default_ultra = self
            .chat_widget
            .current_collaboration_mode()
            .reasoning_effort()
            == Some(ReasoningEffort::Ultra)
            && self.config.model_reasoning_effort != Some(ReasoningEffort::Ultra);
        let clear_plan_ultra = self.chat_widget.config_ref().plan_mode_reasoning_effort
            == Some(ReasoningEffort::Ultra)
            && self.config.plan_mode_reasoning_effort != Some(ReasoningEffort::Ultra);
        self.chat_widget.set_model(&model);
        if !in_plan_mode || ultra || clear_default_ultra {
            self.chat_widget.set_reasoning_effort(effort.clone());
        }
        if in_plan_mode || ultra || clear_plan_ultra {
            self.chat_widget
                .set_plan_mode_reasoning_effort(effort.clone());
        }
        if model_changed {
            self.sync_active_thread_model_setting(app_server, model.clone(), effort.clone())
                .await;
        } else if let Some(mut params) =
            self.active_thread_reasoning_setting_update_params(effort.clone())
        {
            params.collaboration_mode = Some(self.chat_widget.effective_collaboration_mode());
            self.send_thread_settings_update(app_server, params).await;
        }
        self.sync_active_thread_service_tier_to_cached_session()
            .await;
        let mut message = format!("Model changed to {model}");
        if let Some(label) = Self::reasoning_label_for(&model, effort.as_ref()) {
            message.push(' ');
            message.push_str(&label);
        }
        message.push_str(" for this session only");
        self.chat_widget.add_info_message(message, /*hint*/ None);
    }

    pub(super) async fn persist_model_defaults(
        &mut self,
        request_handle: AppServerRequestHandle,
        edits: Vec<ConfigEdit>,
        setting: &str,
    ) -> Result<()> {
        let response = crate::config_update::write_config_batch(request_handle, edits).await?;
        if response.status == WriteStatus::OkOverridden {
            self.chat_widget.add_warning_message(format!(
                "Saved {setting}, but a higher-priority configuration layer overrides the saved value."
            ));
        }
        Ok(())
    }
}
