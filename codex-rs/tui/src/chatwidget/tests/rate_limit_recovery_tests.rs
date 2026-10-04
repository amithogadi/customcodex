use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn rate_limit_recovery_preserves_settings_hold_and_clears_on_account_change() {
    let (mut chat, _events, mut ops) = make_chatwidget_manual(Some("test-model-a")).await;
    set_chatgpt_auth(&mut chat);
    chat.thread_id = Some(ThreadId::new());
    chat.set_queue_autosend_suppressed(/*suppressed*/ true);
    chat.on_rate_limit_error(RateLimitErrorKind::UsageLimit, "Usage exhausted".into());
    chat.queue_user_message(UserMessage::from("queued follow-up"));
    chat.finish_rate_limit_recovery();
    assert_no_submit_op(&mut ops);
    assert!(chat.input_queue.suppress_queue_autosend);
    assert!(!chat.input_queue.rate_limit_recovery_pending);

    chat.on_rate_limit_error(RateLimitErrorKind::UsageLimit, "Usage exhausted".into());
    chat.update_account_state(
        /*status_account_display*/ None, /*plan_type*/ None,
        /*has_chatgpt_account*/ false,
    );
    assert!(!chat.input_queue.rate_limit_recovery_pending);
    assert_eq!(chat.queued_user_message_texts(), vec!["queued follow-up"]);
    assert_no_submit_op(&mut ops);
}
