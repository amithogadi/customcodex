use super::*;
use crate::app::agents_overview_usage::usage_lines;
use codex_app_server_protocol::ThreadTokenUsage;
use codex_app_server_protocol::ThreadTokenUsageUpdatedNotification;
use codex_app_server_protocol::TokenUsageBreakdown;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn local_agent_token_usage_is_shown_and_cleared_on_revert() {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    app.agents_overview.threads.insert(
        thread_id,
        Some(overview_thread(
            thread_id,
            None,
            "Review parser",
            ThreadStatus::Idle,
        )),
    );
    let tokens = TokenUsageBreakdown {
        total_tokens: 17_000,
        input_tokens: 13_000,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 4_000,
        reasoning_output_tokens: 0,
    };
    app.track_agents_overview_notification(&ServerNotification::ThreadTokenUsageUpdated(
        ThreadTokenUsageUpdatedNotification {
            thread_id: thread_id.to_string(),
            turn_id: "turn".into(),
            token_usage: ThreadTokenUsage {
                total: tokens.clone(),
                last: tokens.clone(),
                model_context_window: None,
            },
        },
    ));
    assert_eq!(app.agents_overview.usage[&thread_id].tokens, Some(tokens));
    let text = usage_lines(&app.agents_overview.usage[&thread_id])
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(text, "Tokens: 13K in · 4K out");

    app.track_agents_overview_notification(&ServerNotification::ThreadReverted(
        codex_app_server_protocol::ThreadRevertedNotification {
            thread_id: thread_id.to_string(),
        },
    ));
    assert_eq!(app.agents_overview.usage[&thread_id].tokens, None);
    assert!(usage_lines(&app.agents_overview.usage[&thread_id]).is_empty());
}
