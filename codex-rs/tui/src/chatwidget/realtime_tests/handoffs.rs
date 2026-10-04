//! Voice delegation routes completed answers without exposing private protocol text.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn closing_voice_before_turn_completion_restores_the_delegated_answer_once() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    let turn_id = "voice-turn-closing";
    start_item(
        &mut chat,
        thread_id,
        turn_id,
        user_item("<realtime_delegation><input>the answer?</input></realtime_delegation>"),
    );
    let answer = agent_item(
        "final-answer",
        "The answer is 42.",
        Some(MessagePhase::FinalAnswer),
    );
    start_item(&mut chat, thread_id, turn_id, answer.clone());
    complete_item(&mut chat, thread_id, turn_id, answer.clone());
    assert!(events.try_recv().is_err());

    chat.on_realtime_conversation_closed(/*reason*/ None);
    finish_turn(
        &mut chat,
        thread_id,
        turn_id,
        vec![answer],
        TurnStatus::Completed,
    );

    let mut visible_answers = 0;
    commit_realtime_history_events(&mut chat, &mut events);
    while let Ok(event) = events.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            visible_answers += cell
                .display_lines(/*width*/ 80)
                .iter()
                .any(|line| line.to_string().contains("The answer is 42."))
                as usize;
        }
    }
    assert_eq!(visible_answers, 1);
    assert!(ops.try_recv().is_err());
}

#[tokio::test]
async fn delegated_async_question_stays_local_and_expires_when_its_turn_ends() {
    let (mut chat, _sender, _events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    let turn_id = "question-turn";
    start_item(
        &mut chat,
        thread_id,
        turn_id,
        user_item("<realtime_delegation><input>choose</input></realtime_delegation>"),
    );
    let answer = ThreadItem::AgentMessage {
        id: "question-answer".into(),
        text: "Which option?".into(),
        phase: Some(MessagePhase::FinalAnswer),
        questions: Some(vec![AsyncUserInputQuestion {
            title: "Which option?".into(),
            options: None,
        }]),
        memory_citation: None,
        delivery: None,
    };
    start_item(&mut chat, thread_id, turn_id, answer.clone());
    complete_item(&mut chat, thread_id, turn_id, answer.clone());
    assert_eq!(
        chat.bottom_pane
            .questions
            .as_ref()
            .map(|editor| editor.unanswered_count()),
        Some(1)
    );
    finish_turn(
        &mut chat,
        thread_id,
        turn_id,
        vec![answer],
        TurnStatus::Completed,
    );

    assert_eq!(
        chat.bottom_pane
            .questions
            .as_ref()
            .map(|editor| editor.unanswered_count()),
        Some(0)
    );
    assert!(
        ops.try_recv().is_err(),
        "question-bearing answer stays in the TUI"
    );
}

#[tokio::test]
async fn newer_spoken_input_prevents_an_older_voice_turn_from_speaking() {
    let (mut chat, _sender, _events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.on_realtime_transcript_delta("user".to_string(), "first question".to_string());
    chat.on_realtime_transcript_done("user".to_string(), "first question".to_string());

    let old_turn_id = "old-voice-turn";
    start_item(
        &mut chat,
        thread_id,
        old_turn_id,
        user_item("<realtime_delegation><input>first question</input></realtime_delegation>"),
    );
    let old_answer = agent_item(
        "old-answer",
        "answer to the earlier question",
        Some(MessagePhase::FinalAnswer),
    );
    start_item(&mut chat, thread_id, old_turn_id, old_answer.clone());
    complete_item(&mut chat, thread_id, old_turn_id, old_answer.clone());

    chat.on_realtime_transcript_delta("user".to_string(), "newer question".to_string());
    chat.on_realtime_transcript_done("user".to_string(), "newer question".to_string());
    finish_turn(
        &mut chat,
        thread_id,
        old_turn_id,
        vec![old_answer],
        TurnStatus::Completed,
    );

    assert!(ops.try_recv().is_err());
}
