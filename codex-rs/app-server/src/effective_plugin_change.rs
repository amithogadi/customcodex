use crate::config_manager::ConfigManager;
use crate::request_processors::ConfigRequestProcessor;
use crate::request_serialization::RequestSerializationQueues;
use codex_core::ThreadManager;
use codex_core_plugins::EffectivePluginsChange;
use codex_login::AuthManager;
use std::sync::Arc;
/// Invalidate local plugin consumers after a configuration change.
pub(crate) fn effective_plugins_changed_callback(
    _auth_manager: Arc<AuthManager>,
    thread_manager: Arc<ThreadManager>,
    _config_manager: ConfigManager,
    _config_processor: ConfigRequestProcessor,
    _request_serialization_queues: RequestSerializationQueues,
) -> Arc<dyn Fn(EffectivePluginsChange) + Send + Sync> {
    Arc::new(move |_change| {
        thread_manager.plugins_manager().clear_cache();
        thread_manager.skills_service().clear_cache();

        let refresh_thread_manager = Arc::clone(&thread_manager);
        tokio::spawn(async move {
            refresh_thread_manager.invalidate_mcp_runtimes().await;
            refresh_thread_manager.refresh_hook_runtimes().await;
        });
    })
}
