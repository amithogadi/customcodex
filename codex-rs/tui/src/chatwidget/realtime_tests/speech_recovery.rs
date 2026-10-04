//! Undelivered speech returns to visible history without leaks or clipping.

use super::*;
use pretty_assertions::assert_eq;

fn history_text(events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> String {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) if !cell.as_any().is::<FinalMessageSeparator>() => {
                Some(cell.display_lines(/*width*/ 80))
            }
            _ => None,
        })
        .flatten()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn delayed_voice_transcript_preserves_unspoken_text_fallback() {
    for completed_before_transcript in [false, true] {
        let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
        let thread_id = activate_voice(&mut chat);
        let turn_id = "voice-turn";
        start_item(
            &mut chat,
            thread_id,
            turn_id,
            user_item("<realtime_delegation><input>question</input></realtime_delegation>"),
        );
        let answer = agent_item("answer", "Current answer", Some(MessagePhase::FinalAnswer));
        start_item(&mut chat, thread_id, turn_id, answer.clone());
        if completed_before_transcript {
            complete_item(&mut chat, thread_id, turn_id, answer.clone());
        }
        chat.on_realtime_transcript_done("user".into(), "question".into());
        commit_realtime_history_events(&mut chat, &mut events);
        while events.try_recv().is_ok() {}
        if !completed_before_transcript {
            complete_item(&mut chat, thread_id, turn_id, answer.clone());
        }
        finish_turn(
            &mut chat,
            thread_id,
            turn_id,
            vec![answer],
            TurnStatus::Completed,
        );
        insta::allow_duplicates! {
            insta::assert_snapshot!(history_text(&mut events), @"• Current answer");
        }
        assert!(ops.try_recv().is_err());
    }
}

#[tokio::test]
async fn delegated_reasoning_never_enters_live_history() {
    let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    let turn_id = "private-reasoning-turn";
    start_item(
        &mut chat,
        thread_id,
        turn_id,
        user_item("<realtime_delegation><input>question</input></realtime_delegation>"),
    );
    chat.config.show_raw_agent_reasoning = true;
    for notification in [
        ServerNotification::ReasoningSummaryPartAdded(ReasoningSummaryPartAddedNotification {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.into(),
            item_id: "reasoning".into(),
            summary_index: 0,
        }),
        ServerNotification::ReasoningSummaryTextDelta(ReasoningSummaryTextDeltaNotification {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.into(),
            item_id: "reasoning".into(),
            delta: "private summary".into(),
            summary_index: 0,
        }),
        ServerNotification::ReasoningTextDelta(ReasoningTextDeltaNotification {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.into(),
            item_id: "reasoning".into(),
            delta: "private raw reasoning".into(),
            content_index: 0,
        }),
    ] {
        chat.handle_server_notification(notification, /*replay_kind*/ None);
    }
    complete_item(
        &mut chat,
        thread_id,
        turn_id,
        ThreadItem::Reasoning {
            id: "reasoning".into(),
            summary: vec!["private summary".into()],
            content: vec!["private raw reasoning".into()],
        },
    );
    chat.reset_realtime_conversation();
    start_item(
        &mut chat,
        thread_id,
        "late-start-turn",
        user_item("<realtime_delegation><input>delayed input</input></realtime_delegation>"),
    );
    assert!(chat.is_realtime_delegated_reasoning_turn("late-start-turn"));
    chat.handle_server_notification(
        ServerNotification::ReasoningSummaryTextDelta(ReasoningSummaryTextDeltaNotification {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.into(),
            item_id: "late-reasoning".into(),
            delta: "private late summary".into(),
            summary_index: 0,
        }),
        /*replay_kind*/ None,
    );
    complete_item(
        &mut chat,
        thread_id,
        turn_id,
        ThreadItem::Reasoning {
            id: "late-reasoning".into(),
            summary: vec!["private late summary".into()],
            content: Vec::new(),
        },
    );
    chat.handle_server_notification(
        ServerNotification::ReasoningSummaryTextDelta(ReasoningSummaryTextDeltaNotification {
            thread_id: thread_id.to_string(),
            turn_id: "late-start-turn".into(),
            item_id: "late-start-reasoning".into(),
            delta: "private delayed start".into(),
            summary_index: 0,
        }),
        /*replay_kind*/ None,
    );
    complete_item(
        &mut chat,
        thread_id,
        "late-start-turn",
        ThreadItem::Reasoning {
            id: "late-start-reasoning".into(),
            summary: vec!["private delayed start".into()],
            content: Vec::new(),
        },
    );
    chat.handle_server_notification(
        ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.into(),
            item_id: "late-commentary".into(),
            delta: "private commentary stream".into(),
        }),
        /*replay_kind*/ None,
    );
    complete_item(
        &mut chat,
        thread_id,
        turn_id,
        agent_item(
            "late-commentary",
            "private commentary item",
            Some(MessagePhase::Commentary),
        ),
    );
    finish_turn(
        &mut chat,
        thread_id,
        turn_id,
        vec![agent_item(
            "private-none",
            "[ANALYSIS] private fallback",
            /*phase*/ None,
        )],
        TurnStatus::Completed,
    );
    chat.flush_answer_stream_with_separator();
    while let Ok(event) = events.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            let rendered = cell
                .transcript_lines(/*width*/ 80)
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!rendered.contains("private"), "{rendered}");
        }
    }
    assert!(!chat.is_realtime_delegated_reasoning_turn(turn_id));
    let typed_reasoning = ThreadItem::Reasoning {
        id: "typed-reasoning".into(),
        summary: vec!["ordinary summary".into()],
        content: Vec::new(),
    };
    start_item(&mut chat, thread_id, "typed-turn", typed_reasoning.clone());
    chat.handle_server_notification(
        ServerNotification::ReasoningSummaryTextDelta(ReasoningSummaryTextDeltaNotification {
            thread_id: thread_id.to_string(),
            turn_id: "typed-turn".into(),
            item_id: "typed-reasoning".into(),
            delta: "ordinary summary".into(),
            summary_index: 0,
        }),
        /*replay_kind*/ None,
    );
    complete_item(&mut chat, thread_id, "typed-turn", typed_reasoning);
    let rendered = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(
                cell.transcript_lines(/*width*/ 80)
                    .into_iter()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<String>();
    assert!(rendered.contains("ordinary summary"), "{rendered}");
}

#[tokio::test]
async fn answer_exceeding_speech_budget_is_shown_in_full_instead() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    let turn_id = "text-only-final-turn";
    start_item(
        &mut chat,
        thread_id,
        turn_id,
        user_item("<realtime_delegation><input>question</input></realtime_delegation>"),
    );
    while events.try_recv().is_ok() {}
    let text = format!("Start. {} End.", "word ".repeat(/*n*/ 800));
    let answer = agent_item("text-only-answer", &text, Some(MessagePhase::FinalAnswer));
    start_item(&mut chat, thread_id, turn_id, answer.clone());
    complete_item(&mut chat, thread_id, turn_id, answer.clone());
    finish_turn(
        &mut chat,
        thread_id,
        turn_id,
        vec![answer],
        TurnStatus::Completed,
    );

    assert!(
        ops.try_recv().is_err(),
        "an over-budget answer must not be clipped for speech"
    );
    assert!(chat.realtime_conversation.pending_speech.is_empty());
    let rendered = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) if !cell.as_any().is::<FinalMessageSeparator>() => {
                Some(
                    cell.transcript_lines(/*width*/ 80)
                        .into_iter()
                        .map(|line| line.to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        rendered.split_whitespace().collect::<Vec<_>>().join(" "),
        format!("• {text}")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    );
}

#[tokio::test]
async fn oversized_delegated_answer_remains_in_normal_history() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    let turn_id = "large-final-turn";
    start_item(
        &mut chat,
        thread_id,
        turn_id,
        user_item("<realtime_delegation><input>question</input></realtime_delegation>"),
    );
    let answer = agent_item(
        "large-answer",
        &"word ".repeat(super::super::MAX_PENDING_SPEECH_ITEM_BYTES / 5),
        Some(MessagePhase::FinalAnswer),
    );
    start_item(&mut chat, thread_id, turn_id, answer.clone());
    complete_item(&mut chat, thread_id, turn_id, answer.clone());
    finish_turn(
        &mut chat,
        thread_id,
        turn_id,
        vec![answer],
        TurnStatus::Completed,
    );

    assert!(chat.realtime_conversation.pending_speech.is_empty());
    assert!(ops.try_recv().is_err());
    assert!(events.try_recv().is_ok());
    assert_eq!(
        chat.transcript.last_completed_agent_message,
        Some((turn_id.to_string(), "large-answer".to_string()))
    );
}

#[tokio::test]
async fn interrupted_voice_delegation_never_speaks_a_completed_item() {
    let (mut chat, _sender, _events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    let turn_id = "interrupted-turn";
    start_item(
        &mut chat,
        thread_id,
        turn_id,
        user_item("<realtime_delegation><input>stop me</input></realtime_delegation>"),
    );
    let answer = agent_item("answer", "Never say this", Some(MessagePhase::FinalAnswer));
    start_item(&mut chat, thread_id, turn_id, answer.clone());
    complete_item(&mut chat, thread_id, turn_id, answer.clone());
    finish_turn(
        &mut chat,
        thread_id,
        turn_id,
        vec![answer],
        TurnStatus::Interrupted,
    );

    assert!(ops.try_recv().is_err());
}

#[tokio::test]
async fn replay_projects_the_delegated_request_without_internal_xml() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    chat.replay_thread_item(
        user_item(
            "<realtime_delegation><input>show &lt;code&gt; &amp; tests</input></realtime_delegation>",
        ),
        "old-turn".to_string(),
        ReplayKind::ResumeInitialMessages,
    );

    let Ok(AppEvent::InsertHistoryCell(cell)) = events.try_recv() else {
        panic!("a resumed voice delegation should preserve the original user request");
    };
    let rendered = cell
        .display_lines(/*width*/ 80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("show <code> & tests"));
    assert!(!rendered.contains("realtime_delegation"));
    assert!(ops.try_recv().is_err());
}

#[tokio::test]
async fn locally_typed_delegation_shaped_text_keeps_its_normal_answer() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    let typed_text = "<realtime_delegation><input>explain this xml</input></realtime_delegation>";
    chat.note_realtime_typed_input(typed_text);
    let turn_id = "typed-xml-turn";
    let prompt = user_item(typed_text);
    start_item(&mut chat, thread_id, turn_id, prompt.clone());
    complete_item(&mut chat, thread_id, turn_id, prompt);
    let answer = agent_item("xml-answer", "It is an XML element.", /*phase*/ None);
    start_item(&mut chat, thread_id, turn_id, answer.clone());
    complete_item(&mut chat, thread_id, turn_id, answer.clone());
    finish_turn(
        &mut chat,
        thread_id,
        turn_id,
        vec![answer],
        TurnStatus::Completed,
    );

    let mut saw_answer = false;
    while let Ok(event) = events.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            saw_answer |= cell
                .display_lines(/*width*/ 80)
                .iter()
                .any(|line| line.to_string().contains("It is an XML element."));
        }
    }
    assert!(saw_answer);
    assert!(ops.try_recv().is_err());
}
