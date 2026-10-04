//! Verifies that new local user input invalidates root and worker cached approvals.

use super::*;
use codex_core::TurnInputRequest;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use pretty_assertions::assert_eq;

fn request_body(request: &wiremock::Request) -> Option<serde_json::Value> {
    let compressed = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("zstd"));
    let bytes = if compressed {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok()?
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&bytes).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_user_input_invalidates_root_and_worker_cached_approvals() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let (_, test, registry, thread_server) =
        sample_configured_conversation_history_with_thread_server(
            Vec::new(),
            r#"{"path":"README.md"}"#,
            Some(TEST_GUARDIAN_POLICY),
            "[features]\nmulti_agent = true\nmulti_agent_v2 = true",
            /*model_defaults*/ None,
            ToolCallSource::Direct,
        )
        .await?;
    let root_store = test.codex.thread_extension_data();
    let root_progress = root_store.get::<GuardianV2ScoreProgress>().unwrap();
    tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
        while cached_score(root_store).is_none() || root_progress.inspect(/*call_id*/ None).lag > 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("initial Guardian score did not settle"))?;
    let root_id = test.session_configured.thread_id;
    let mut created = test.thread_manager.subscribe_thread_created();
    let root_request = move |marker: &'static str| {
        move |request: &wiremock::Request| {
            request_body(request).is_some_and(|body| {
                body["client_metadata"]["thread_id"] == json!(root_id)
                    && body.to_string().contains(marker)
            })
        }
    };
    mount_sse_once_match(
        &thread_server,
        root_request("Start a worker"),
        sse(vec![
            ev_function_call_with_namespace(
                "spawn-worker",
                "collaboration",
                "spawn_agent",
                &json!({"message": "Inspect the deployment.", "task_name": "worker"}).to_string(),
            ),
            ev_completed("spawn-worker"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &thread_server,
        root_request("spawn-worker"),
        sse(vec![ev_completed("root-complete")]),
    )
    .await;
    mount_sse_once_match(
        &thread_server,
        move |request: &wiremock::Request| {
            request_body(request).is_some_and(|body| {
                body["client_metadata"]["x-codex-parent-thread-id"] == json!(root_id)
                    && body["client_metadata"]["x-openai-subagent"] != "guardian"
            })
        },
        sse(vec![ev_completed("worker-complete")]),
    )
    .await;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Start a worker to inspect the deployment.".to_owned(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let worker = test
        .thread_manager
        .get_thread(tokio::time::timeout(ASYNC_TEST_TIMEOUT, created.recv()).await??)
        .await?;
    ThreadIdle::wait(&test.codex).await;
    ThreadIdle::wait(&worker).await;
    let mut config = test.config.clone();
    config.features.enable(Feature::GuardianV2)?;
    let session_store = ExtensionData::new("worker-cache-session");
    registry.thread_lifecycle_contributors()[0]
        .on_thread_start(ThreadStartInput {
            config: &config,
            session_source: &SessionSource::Exec,
            persistent_thread_state_available: false,
            environments: &[],
            mcp_resource_client: None,
            session_store: &session_store,
            thread_store: worker.thread_extension_data(),
        })
        .await;
    let root_before = ScoreAuthorization::current(&test.codex, &Default::default()).await;
    let worker_before = ScoreAuthorization::current(&worker, &Default::default()).await;
    assert_eq!(
        worker_before.root_review_context_revision,
        Some(root_before.review_context_revision)
    );
    let mut score = cached_score(root_store).expect("initial Guardian score");
    score.scores.insert("action_risk".to_owned(), 0.25);
    set_cached_score(root_store, score);
    // Hold tool lag at zero so only new user input can invalidate either approval.
    for (thread, before) in [(&test.codex, &root_before), (&worker, &worker_before)] {
        let store = thread.thread_extension_data();
        let progress = store.get::<GuardianV2ScoreProgress>().unwrap();
        seed_cached_score(&progress, store, /*index*/ 1_000, before.clone());
        assert_eq!(
            cached_approval(&registry, store, "review action",).await,
            Some(ReviewDecision::Approved)
        );
    }

    test.codex
        .inject_response_items(vec![user_instruction("Do not deploy without my approval.")])
        .await?;
    let (root_after, worker_after) = tokio::time::timeout(ASYNC_TEST_TIMEOUT, async {
        loop {
            let root = ScoreAuthorization::current(&test.codex, &Default::default()).await;
            let worker = ScoreAuthorization::current(&worker, &Default::default()).await;
            if root.local != root_before.local && worker.root != worker_before.root {
                break (root, worker);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("user input did not update root and worker authorization"))?;
    assert_eq!(
        root_after,
        ScoreAuthorization {
            local: codex_core::GuardianAuthorizationVersion {
                user_message_revision: root_before.local.user_message_revision + 1,
                ..root_before.local
            },
            ..root_before
        }
    );
    assert_eq!(
        worker_after,
        ScoreAuthorization {
            root: Some(root_after.local),
            ..worker_before
        }
    );
    for thread in [&test.codex, &worker] {
        assert_eq!(
            cached_approval(&registry, thread.thread_extension_data(), "review action",).await,
            None
        );
    }
    // Missing retained root instructions must not permanently veto a matching LOW score.
    test.codex
        .inject_response_items(
            (0..20)
                .map(|index| user_instruction(&format!("Root instruction {index}.")))
                .collect(),
        )
        .await?;
    let snapshot = worker
        .guardian_root_snapshot()
        .await
        .expect("root snapshot");
    assert!(!snapshot.authorization_version.retained_context_complete);
    assert!(
        snapshot
            .messages
            .contains(&codex_core::GuardianRootMessage::IncompleteRootInstructions)
    );
    let store = worker.thread_extension_data();
    let progress = store.get::<GuardianV2ScoreProgress>().unwrap();
    let authorization = ScoreAuthorization::current(&worker, &Default::default()).await;
    assert!(authorization.local.retained_context_complete);
    seed_cached_score(&progress, store, /*index*/ 1_000, authorization);
    assert_eq!(
        cached_approval(&registry, store, "review action",).await,
        Some(ReviewDecision::Approved)
    );
    let shutdown = test
        .thread_manager
        .shutdown_all_threads_bounded(ASYNC_TEST_TIMEOUT)
        .await;
    assert!(shutdown.timed_out.is_empty());
    Ok(())
}
