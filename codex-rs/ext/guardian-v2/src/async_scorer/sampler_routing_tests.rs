//! Keeps classifier evidence bound to the credentials that opened a request.

use super::connect_sampler;
use super::proxy_websocket_servers;
use super::sample_request;
use super::sampler_config;
use anyhow::Result;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::ExternalAuth;
use codex_login::ExternalAuthFuture;
use codex_login::ExternalAuthRefreshContext;
use codex_model_provider::create_model_provider;
use core_test_support::responses;
use core_test_support::responses::WebSocketConnectionConfig;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

struct SwitchingCredentials {
    initial: CodexAuth,
    updated: CodexAuth,
    resolved: AtomicBool,
}

impl ExternalAuth for SwitchingCredentials {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async {
            Ok(if self.resolved.load(Ordering::SeqCst) {
                self.updated.clone()
            } else {
                self.initial.clone()
            })
        })
    }

    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        ExternalAuth::resolve(self)
    }
}

#[tokio::test]
async fn credential_switch_cancels_request_opening_and_first_token_waits() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    for transport in ["http", "websocket"] {
        let http = responses::start_mock_server().await;
        let opening = responses::mount_response_once(
            &http,
            responses::sse_response(String::new()).set_delay(Duration::from_secs(/*secs*/ 60)),
        )
        .await;
        let stalled = WebSocketConnectionConfig {
            requests: vec![Vec::new()],
            response_headers: Vec::new(),
            accept_delay: None,
            close_after_requests: false,
        };
        let websocket = responses::start_websocket_server_with_headers(vec![stalled; 2]).await;
        let changes = Arc::new(SwitchingCredentials {
            initial: CodexAuth::from_api_key("provider-key-a"),
            updated: CodexAuth::from_api_key("provider-key-b"),
            resolved: AtomicBool::new(/*v*/ false),
        });
        let home = tempfile::tempdir()?;
        let auth = AuthManager::from_auth_for_testing_with_home(
            changes.initial.clone(),
            home.path().to_owned(),
        );
        auth.set_external_auth(changes.clone()).await?;
        let base_url = if transport == "http" {
            format!("{}/v1", http.uri())
        } else {
            proxy_websocket_servers(&[&websocket, &websocket]).await?
        };
        let mut config = sampler_config(base_url);
        config.provider = create_model_provider(config.provider.info().clone(), Some(auth.clone()));
        let sampler = if transport == "http" {
            super::LunaSampler::new(config)
        } else {
            connect_sampler(config).await?
        };
        let sample = sampler.sample(sample_request("turn-1"));
        tokio::pin!(sample);
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            loop {
                if !opening.requests().is_empty() || websocket.connections().iter().any(|requests| !requests.is_empty()) {
                    break Ok::<(), anyhow::Error>(());
                }
                tokio::select! {
                    result = &mut sample => anyhow::bail!("classification finished before account switch: {result:?}"),
                    () = tokio::task::yield_now() => {}
                }
            }
        }).await??;
        changes.resolved.store(/*val*/ true, Ordering::SeqCst);
        auth.set_external_auth(changes).await?;
        let error = tokio::time::timeout(Duration::from_secs(/*secs*/ 2), &mut sample)
            .await?
            .unwrap_err();
        assert!(
            error.to_string().contains("account changed"),
            "{transport}: {error}"
        );
        assert_eq!(opening.requests().len(), usize::from(transport == "http"));
        assert_eq!(
            websocket.connections().iter().map(Vec::len).sum::<usize>(),
            usize::from(transport == "websocket")
        );
    }
    Ok(())
}
