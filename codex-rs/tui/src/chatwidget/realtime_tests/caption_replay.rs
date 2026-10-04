//! Replay and captions reconcile once across thread and session changes.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn replay_preserves_typed_updates_before_voice_steers_the_turn() {
    let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(ThreadId::new());
    chat.replay_thread_turns(
        vec![Turn {
            id: "typed-then-voice".into(),
            items: vec![
                user_item("Typed question"),
                agent_item(
                    "typed-update",
                    "Checking the typed request",
                    Some(MessagePhase::Commentary),
                ),
                ThreadItem::Reasoning {
                    id: "typed-reasoning".into(),
                    summary: vec!["Typed reasoning summary".into()],
                    content: Vec::new(),
                },
                user_item(
                    "<realtime_delegation><input>spoken correction</input></realtime_delegation>",
                ),
                agent_item(
                    "private-update",
                    "Private voice commentary",
                    Some(MessagePhase::Commentary),
                ),
                ThreadItem::Reasoning {
                    id: "private-reasoning".into(),
                    summary: vec!["Private voice reasoning".into()],
                    content: Vec::new(),
                },
            ],
            items_view: TurnItemsView::Full,
            status: TurnStatus::Completed,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }],
        ReplayKind::ThreadSnapshot,
    );
    chat.flush_answer_stream_with_separator();
    commit_realtime_history_events(&mut chat, &mut events);
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
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("Checking the typed request"),
        "{rendered}"
    );
    assert!(rendered.contains("Typed reasoning summary"), "{rendered}");
    assert!(!rendered.contains("Private voice"), "{rendered}");
    insta::assert_snapshot!("typed_turn_replay_before_voice_steering", rendered);
}

#[tokio::test]
async fn in_progress_voice_replay_restores_the_late_reasoning_guard() {
    let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    let voice_request =
        user_item("<realtime_delegation><input>question</input></realtime_delegation>");
    chat.replay_thread_turns(
        vec![Turn {
            id: "saved-voice-turn".into(),
            items: vec![voice_request.clone()],
            items_view: TurnItemsView::Full,
            status: TurnStatus::InProgress,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }],
        ReplayKind::ThreadSnapshot,
    );
    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: thread_id.to_string(),
            turn_id: "buffered-voice-turn".into(),
            item: voice_request,
            started_at_ms: 0,
        }),
        Some(ReplayKind::ThreadSnapshot),
    );
    while events.try_recv().is_ok() {}
    for turn_id in ["saved-voice-turn", "buffered-voice-turn"] {
        chat.handle_server_notification(
            ServerNotification::ReasoningSummaryTextDelta(ReasoningSummaryTextDeltaNotification {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.into(),
                item_id: "late-reasoning".into(),
                delta: "private after switch".into(),
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
                summary: vec!["private after switch".into()],
                content: Vec::new(),
            },
        );
        assert!(chat.is_realtime_delegated_reasoning_turn(turn_id));
    }
    commit_realtime_history_events(&mut chat, &mut events);
    while let Ok(event) = events.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            let rendered = cell
                .transcript_lines(/*width*/ 80)
                .into_iter()
                .map(|line| line.to_string())
                .collect::<String>();
            assert!(!rendered.contains("private"), "{rendered}");
        }
    }
}

#[tokio::test]
async fn restored_partial_caption_accepts_late_completion_without_duplicate_history() {
    let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    chat.local_settings.tui.animations = false;
    chat.restore_realtime_transcript_cells(VecDeque::from([
        super::super::RealtimeTranscriptRecord {
            role: "user".into(),
            text: "last ".into(),
            complete: false,
            before_turn_id: None,
        },
    ]));
    assert!(chat.realtime_conversation.live_transcript_cell.is_none());
    chat.on_realtime_transcript_delta("user".into(), "words".into());
    let retained = chat.take_realtime_transcript_cells_for_replay();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].text, "last words");
    chat.restore_realtime_transcript_cells(retained);
    chat.on_realtime_transcript_done("user".into(), "last words".into());
    assert!(chat.realtime_conversation.live_transcript_cell.is_none());
    assert_eq!(chat.realtime_conversation.accepted_transcripts.len(), 1);
    assert_eq!(
        chat.realtime_conversation.accepted_transcripts[0].text,
        "last words"
    );
    assert!(chat.realtime_conversation.accepted_transcripts[0].complete);
    commit_realtime_history_events(&mut chat, &mut events);
    let rendered = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(
                cell.display_lines(/*width*/ 80)
                    .into_iter()
                    .map(|line| line.to_string())
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(rendered.matches("last words").count(), 1);
}

#[tokio::test]
async fn empty_late_completion_discards_the_restored_partial() {
    let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    chat.restore_realtime_transcript_cells(VecDeque::from([
        super::super::RealtimeTranscriptRecord {
            role: "user".into(),
            text: "unfinished".into(),
            complete: false,
            before_turn_id: None,
        },
    ]));
    chat.on_realtime_transcript_done("user".into(), String::new());

    assert!(chat.realtime_conversation.live_transcript_cell.is_none());
    assert!(chat.realtime_conversation.accepted_transcripts.is_empty());
    assert!(events.try_recv().is_err());
}
