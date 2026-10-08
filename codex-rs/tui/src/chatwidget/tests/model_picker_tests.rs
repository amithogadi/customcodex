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
                    openrouter_zdr: None,
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
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::High));
    chat.thread_id = Some(ThreadId::new());
    let mut alternative = chat.config.model_providers["isoquant"].models[0].clone();
    alternative.id = "alternative".into();
    chat.config
        .model_providers
        .get_mut("isoquant")
        .unwrap()
        .models
        .push(alternative);
    chat.open_all_models_popup();
    let popup = render_bottom_popup(&chat, 120);
    assert!(popup.contains("isoquant / shared-model"));
    assert!(popup.contains("openrouter / shared-model"));
    assert!(popup.contains("only cerebras"));
    assert!(popup.contains("Switches in this chat"));
    assert!(popup.contains("Starts a new chat"));
    assert!(!popup.contains("gpt-"));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert!(
        events.try_recv().is_err(),
        "selecting the current model is a no-op"
    );
    chat.open_model_popup_with_presets(vec![]);
    assert!(render_bottom_popup(&chat, 80).contains("isoquant / shared-model"));
    chat.handle_key_event(KeyEvent::from(KeyCode::Down));
    chat.handle_key_event(KeyEvent::from(KeyCode::Down));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert_matches!(events.try_recv(), Ok(AppEvent::SelectConfiguredModel {provider,model,..}) if provider == "openrouter" && model == "shared-model");
    assert!(
        events.try_recv().is_err(),
        "no legacy model update or catalog fetch"
    );
}

#[tokio::test]
async fn configured_alias_picker_distinguishes_reasoning_for_the_same_model() {
    for (provider_id, effort, direction, target) in [
        (
            "openrouter",
            ReasoningEffortConfig::Max,
            KeyCode::Down,
            "openrouter-high",
        ),
        (
            "openrouter-high",
            ReasoningEffortConfig::High,
            KeyCode::Up,
            "openrouter",
        ),
    ] {
        let (mut chat, mut events, _ops) = make_chatwidget_manual(Some("flash")).await;
        let primary: codex_model_provider_info::ModelProviderInfo = toml::from_str(
            r#"
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1"
[[models]]
id = "flash"
context_window = 1048576
reasoning_effort = "max"
"#,
        )
        .unwrap();
        let mut high = primary.clone();
        high.models[0].reasoning_effort = Some(ReasoningEffortConfig::High);
        chat.config.model_providers = std::collections::HashMap::from([
            ("openrouter".into(), primary),
            ("openrouter-high".into(), high),
        ]);
        chat.config.model_provider_id = provider_id.into();
        chat.thread_id = Some(ThreadId::new());
        chat.set_reasoning_effort(Some(effort));
        chat.open_all_models_popup();
        let popup = render_bottom_popup(&chat, 120);
        assert_eq!(popup.matches("Current model").count(), 1, "{popup}");
        assert!(popup.contains("Switches in this chat"), "{popup}");
        assert!(!popup.contains("Starts a new chat"), "{popup}");
        chat.handle_key_event(KeyEvent::from(direction));
        chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
        assert_matches!(events.try_recv(), Ok(AppEvent::SelectConfiguredModel {provider,model,..}) if provider == target && model == "flash");
    }
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
