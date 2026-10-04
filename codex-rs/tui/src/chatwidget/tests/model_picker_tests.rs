//! Model-family presentation at constrained sizes and with configured list bindings.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn configured_picker_uses_provider_identity_and_ignores_bundled_refresh() {
    let (mut chat, mut events, _ops) = make_chatwidget_manual(Some("shared-model")).await;
    for id in ["openrouter", "isoquant"] {
        chat.config.model_providers.insert(
            id.into(),
            codex_model_provider_info::ModelProviderInfo {
                name: id.into(),
                models: vec![codex_model_provider_info::ConfiguredModel {
                    id: "shared-model".into(),
                    name: None,
                    context_window: 65536,
                    reasoning_effort: Some(ReasoningEffortConfig::High),
                    openrouter_providers: if id == "openrouter" {
                        vec!["cerebras".into()]
                    } else {
                        Vec::new()
                    },
                }],
                ..Default::default()
            },
        );
    }
    chat.config.model_provider_id = "isoquant".into();
    chat.open_all_models_popup();
    let popup = render_bottom_popup(&chat, 80);
    assert!(popup.contains("isoquant / shared-model"));
    assert!(popup.contains("openrouter / shared-model"));
    assert!(popup.contains("only cerebras"));
    assert!(!popup.contains("gpt-"));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert!(
        events.try_recv().is_err(),
        "selecting the current model is a no-op"
    );
    chat.open_model_popup_with_presets(vec![]);
    assert!(render_bottom_popup(&chat, 80).contains("isoquant / shared-model"));
    chat.handle_key_event(KeyEvent::from(KeyCode::Down));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert_matches!(events.try_recv(), Ok(AppEvent::SelectConfiguredModel {provider,model,..}) if provider == "openrouter" && model == "shared-model");
    assert!(
        events.try_recv().is_err(),
        "no legacy model update or catalog fetch"
    );
}

#[tokio::test]
async fn model_picker_compact_hint_and_actions_follow_configured_bindings() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(Some("gpt-5.5")).await;
    let mut keymap = crate::keymap::RuntimeKeymap::defaults();
    keymap.list.accept = vec![key_hint::plain(KeyCode::F(3))];
    keymap.list.cancel = vec![key_hint::plain(KeyCode::F(2))];
    chat.bottom_pane.set_keymap_bindings(&keymap);
    chat.open_all_models_popup();
    let popup = render_bottom_popup(&chat, /*width*/ 40);
    assert_eq!(popup.lines().last().unwrap().trim(), "f3 select · f2 back");
    chat.handle_key_event(KeyEvent::from(KeyCode::F(3)));
    assert_matches!(rx.try_recv(), Ok(AppEvent::OpenReasoningPopup { .. }));
    chat.handle_key_event(KeyEvent::from(KeyCode::F(2)));
    assert!(chat.no_modal_or_popup_active());
}
