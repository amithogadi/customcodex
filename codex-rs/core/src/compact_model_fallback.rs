use crate::compaction_state::CompactionImplementation;
use crate::compaction_state::CompactionReason;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use tracing::warn;

/// Returns whether a failed compaction attempt should use the current model.
pub(crate) fn should_retry_with_current_model(error: &CodexErr) -> bool {
    !matches!(
        error.details(),
        CodexErrorDetails::TurnAborted
            | CodexErrorDetails::Interrupted
            | CodexErrorDetails::SessionBudgetExceeded
    )
}

pub(crate) fn record_model_fallback(
    previous_model: &str,
    current_model: &str,
    reason: CompactionReason,
    implementation: CompactionImplementation,
    fallback_error: Option<&CodexErr>,
) {
    let outcome = if fallback_error.is_none() {
        "succeeded"
    } else {
        "failed"
    };
    warn!(
        previous_model,
        current_model,
        ?reason,
        ?implementation,
        outcome,
        ?fallback_error,
        "previous-model compaction failed; retried with current model"
    );
}
