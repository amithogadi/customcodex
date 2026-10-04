//! Configured choices carry the provider identity; selecting one starts a fresh thread.
use super::*;

impl ChatWidget {
    pub(super) fn open_configured_model_popup(&mut self) -> bool {
        let mut providers = self
            .config
            .model_providers
            .iter()
            .filter(|(_, p)| !p.models.is_empty())
            .collect::<Vec<_>>();
        if providers.is_empty() {
            return false;
        }
        providers.sort_by_key(|(id, _)| *id);
        let mut items = Vec::new();
        for (provider_id, provider) in providers {
            for model in &provider.models {
                let is_current = self.config.model_provider_id == *provider_id
                    && self.current_model() == model.id;
                let provider_id = provider_id.clone();
                let model_id = model.id.clone();
                let source_thread = self.thread_id();
                let mut description = model
                    .reasoning_effort
                    .as_ref()
                    .map(|e| format!("{e} reasoning · "))
                    .unwrap_or_default();
                if !model.openrouter_providers.is_empty() {
                    description.push_str(&format!(
                        "only {} · ",
                        model.openrouter_providers.join(" → ")
                    ));
                }
                description.push_str(if is_current {
                    "Current model"
                } else {
                    "Starts a new chat"
                });
                items.push(SelectionItem {
                    name: format!(
                        "{} / {}",
                        provider.name,
                        model.name.as_deref().unwrap_or(&model.id)
                    ),
                    description: Some(description),
                    is_current,
                    actions: vec![Box::new(move |tx| {
                        if !is_current {
                            tx.send(AppEvent::SelectConfiguredModel {
                                source_thread,
                                provider: provider_id.clone(),
                                model: model_id.clone(),
                            });
                        }
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                });
            }
        }
        // Invalidate any outstanding request for the old provider's remote catalog.
        self.model_popup_request_id = None;
        let initial_selected_idx = items.iter().position(|item| item.is_current);
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Select Model".into()),
            subtitle: Some(
                "A different model starts a new chat; the current chat stays saved.".into(),
            ),
            items,
            initial_selected_idx,
            ..SelectionViewParams::picker()
        });
        true
    }
}
