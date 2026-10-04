//! Thread-owned metadata for selected filesystem plugin roots.
use codex_extension_api::SelectedPluginContribution;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use std::sync::{Mutex, MutexGuard};

#[derive(Default)]
pub struct PluginsThreadState {
    state: Mutex<PluginContributorState>,
}
impl PluginsThreadState {
    pub(crate) fn contributor_state(&self) -> MutexGuard<'_, PluginContributorState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
#[derive(Default)]
pub(crate) struct PluginContributorState {
    pub(crate) executor_cache: Vec<CachedSelectedRoot>,
}
pub(crate) struct CachedSelectedRoot {
    pub(crate) root: SelectedCapabilityRoot,
    pub(crate) metadata: Option<SelectedPluginContribution>,
}
