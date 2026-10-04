use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCRequest;
use codex_exec_server_protocol::RequestId;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::time::timeout;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::prelude::*;

use super::ConcurrentRequestLimit;
use super::RequestDispatchMode;
use super::RequestDispatcher;
use super::RequestTaskResult;
use crate::ExecServerRuntimeOptions;
use crate::connection::JsonRpcConnectionEvent;
use crate::rpc::RpcNotificationSender;
use crate::rpc::RpcRouter;
use crate::rpc::RpcServerOutboundMessage;
use crate::rpc::invalid_request;
use crate::server::ExecServerHandler;
use crate::server::session_registry::SessionRegistry;

/// Public limits reject values that cannot safely enable semaphore-backed concurrency.
#[test]
fn concurrent_request_limit_rejects_invalid_values() {
    assert_eq!(
        ConcurrentRequestLimit::new(/*max_concurrent_requests*/ 0),
        None
    );
    assert_eq!(
        ConcurrentRequestLimit::new(/*max_concurrent_requests*/ 1),
        None
    );
    assert_eq!(
        ConcurrentRequestLimit::new(Semaphore::MAX_PERMITS.saturating_add(1)),
        None
    );
    assert_eq!(
        ConcurrentRequestLimit::new(/*max_concurrent_requests*/ 2).map(ConcurrentRequestLimit::get),
        Some(2)
    );
}

/// CLI parsing keeps one request inline and bounds larger positive concurrency limits.
#[test]
fn request_dispatch_mode_parses_bounded_concurrency() {
    assert!(matches!("1".parse(), Ok(RequestDispatchMode::Inline)));
    assert!("0".parse::<RequestDispatchMode>().is_err());

    let oversized_limit = Semaphore::MAX_PERMITS.saturating_add(1).to_string();
    let mode = oversized_limit
        .parse::<RequestDispatchMode>()
        .expect("parse oversized concurrent request limit");
    let RequestDispatchMode::Concurrent {
        max_concurrent_requests,
    } = mode
    else {
        panic!("expected concurrent request dispatch");
    };
    assert_eq!(max_concurrent_requests.get(), Semaphore::MAX_PERMITS);
}

/// Requests wait for admission before executing.
#[tokio::test]
async fn request_queue_waits_for_dispatcher_admission() {
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(/*buffer*/ 1);
    let notifications = RpcNotificationSender::new(outgoing_tx.clone());
    let requests = notifications.request_sender();
    let handler = Arc::new(ExecServerHandler::new(
        SessionRegistry::new(),
        notifications,
        ExecServerRuntimeOptions::new(
            std::env::current_exe().expect("current executable"),
            /*codex_linux_sandbox_exe*/ None,
        )
        .expect("runtime paths"),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    ));
    let mut router = RpcRouter::new();
    let execution_started = Arc::new(Notify::new());
    let release_execution = Arc::new(Notify::new());
    let notify_execution_started = Arc::clone(&execution_started);
    let wait_for_execution_release = Arc::clone(&release_execution);
    let route_setup_duration = Duration::from_millis(200);
    router.request(
        "test/queued",
        move |_handler: Arc<ExecServerHandler>, _params: ()| {
            let execution_started = Arc::clone(&notify_execution_started);
            let release_execution = Arc::clone(&wait_for_execution_release);
            std::thread::sleep(route_setup_duration);
            async move {
                execution_started.notify_one();
                release_execution.notified().await;
                Ok::<_, codex_exec_server_protocol::JSONRPCErrorError>(())
            }
        },
    );
    let (_disconnected_tx, disconnected_rx) = watch::channel(/*init*/ false);
    let mut dispatcher = RequestDispatcher::new(
        Arc::new(router),
        handler,
        outgoing_tx,
        disconnected_rx,
        requests,
        RequestDispatchMode::Concurrent {
            max_concurrent_requests: ConcurrentRequestLimit::new(
                /*max_concurrent_requests*/ 2,
            )
            .expect("valid request limit"),
        },
    );
    dispatcher.initialized = true;
    let admission = Arc::clone(
        &dispatcher
            .lanes
            .as_ref()
            .expect("concurrent request lanes")
            .ordinary,
    );
    let occupied_permits = admission
        .acquire_many_owned(/*n*/ 2)
        .await
        .expect("occupy the request admission lane");
    let JsonRpcConnectionEvent::QueuedRequest {
        request,
        request_span,
        queued_at,
    } = JsonRpcConnectionEvent::message(JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(1),
        method: "test/queued".to_string(),
        params: None,
        trace: None,
    }))
    else {
        panic!("requests should start a server span before dispatch");
    };

    assert!(matches!(
        dispatcher
            .dispatch_request(request, request_span, queued_at)
            .await,
        RequestTaskResult::Completed
    ));
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(25)).await;

    assert!(
        timeout(Duration::from_millis(25), execution_started.notified())
            .await
            .is_err(),
        "queued request must not execute before admission"
    );
    drop(occupied_permits);
    timeout(Duration::from_secs(1), execution_started.notified())
        .await
        .expect("queued request should execute after admission");
    release_execution.notify_one();
    let response = timeout(Duration::from_secs(1), outgoing_rx.recv())
        .await
        .expect("queued request should send its response")
        .expect("queued request response");
    assert!(matches!(
        response,
        RpcServerOutboundMessage::Response {
            request_id: RequestId::Integer(1),
            ..
        }
    ));
    assert!(matches!(
        dispatcher.join_next().await,
        RequestTaskResult::Completed
    ));
}

struct RequestFixture {
    dispatcher: RequestDispatcher,
    outgoing_rx: mpsc::Receiver<RpcServerOutboundMessage>,
    disconnected_tx: watch::Sender<bool>,
}

fn request_fixture(router: RpcRouter<ExecServerHandler>) -> RequestFixture {
    let (outgoing_tx, outgoing_rx) = mpsc::channel(/*buffer*/ 1);
    let notifications = RpcNotificationSender::new(outgoing_tx.clone());
    let requests = notifications.request_sender();
    let handler = Arc::new(ExecServerHandler::new(
        SessionRegistry::new(),
        notifications,
        ExecServerRuntimeOptions::new(
            std::env::current_exe().expect("current executable"),
            /*codex_linux_sandbox_exe*/ None,
        )
        .expect("runtime paths"),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    ));
    let (disconnected_tx, disconnected_rx) = watch::channel(/*init*/ false);
    let dispatcher = RequestDispatcher::new(
        Arc::new(router),
        handler,
        outgoing_tx,
        disconnected_rx,
        requests,
        RequestDispatchMode::Inline,
    );
    RequestFixture {
        dispatcher,
        outgoing_rx,
        disconnected_tx,
    }
}

/// Dispatch preserves success, failure, and disconnected response paths.
#[tokio::test]
async fn dispatch_preserves_completion_and_disconnection_results() {
    for (method, _expected_method, _expected_result, close_response) in [
        ("test/success", "test/success", "success", false),
        ("test/error", "test/error", "error", false),
        ("test/unknown", "unknown", "error", false),
        ("test/success", "test/success", "disconnected", true),
        ("test/unknown", "unknown", "disconnected", true),
    ] {
        let mut router = RpcRouter::new();
        router.request(
            "test/success",
            |_handler: Arc<ExecServerHandler>, _params: ()| async {
                Ok::<_, codex_exec_server_protocol::JSONRPCErrorError>(())
            },
        );
        router.request(
            "test/error",
            |_handler: Arc<ExecServerHandler>, _params: ()| async {
                Err::<(), _>(invalid_request("synthetic route error".to_string()))
            },
        );
        let mut fixture = request_fixture(router);
        if close_response {
            fixture.outgoing_rx.close();
        }
        let pre_dispatch_wait = Duration::from_secs(5);
        let received_at = Instant::now() - pre_dispatch_wait;
        let result = fixture
            .dispatcher
            .dispatch_request(
                JSONRPCRequest {
                    id: RequestId::Integer(1),
                    method: method.to_string(),
                    params: None,
                    trace: None,
                },
                tracing::Span::none(),
                received_at,
            )
            .await;
        assert_eq!(
            matches!(result, RequestTaskResult::ConnectionClosed),
            close_response
        );
    }
}

/// Disconnecting a running request cancels its work.
#[tokio::test]
async fn disconnection_cancels_running_request() {
    let execution_started = Arc::new(Notify::new());
    let notify_execution_started = Arc::clone(&execution_started);
    let mut router = RpcRouter::new();
    router.request(
        "test/pending",
        move |_handler: Arc<ExecServerHandler>, _params: ()| {
            let execution_started = Arc::clone(&notify_execution_started);
            async move {
                execution_started.notify_one();
                std::future::pending::<Result<(), codex_exec_server_protocol::JSONRPCErrorError>>()
                    .await
            }
        },
    );
    let mut fixture = request_fixture(router);
    let pre_dispatch_wait = Duration::from_secs(5);
    let received_at = Instant::now() - pre_dispatch_wait;
    let dispatch = fixture.dispatcher.dispatch_request(
        JSONRPCRequest {
            id: RequestId::Integer(1),
            method: "test/pending".to_string(),
            params: None,
            trace: None,
        },
        tracing::Span::none(),
        received_at,
    );
    let disconnect = async {
        execution_started.notified().await;
        fixture
            .disconnected_tx
            .send(/*value*/ true)
            .expect("disconnect request");
    };
    let (result, ()) = timeout(Duration::from_secs(1), async {
        tokio::join!(dispatch, disconnect)
    })
    .await
    .expect("disconnected request should finish");
    assert!(matches!(result, RequestTaskResult::ConnectionClosed));
}
