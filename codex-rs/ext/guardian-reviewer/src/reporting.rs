//! Owns review reporting and the per-thread denial window. The host publishes
//! events and enforces interruption only on the still-active owning turn.

use std::sync::Arc;

use codex_extension_api::ExtensionData;
use codex_protocol::approvals::GuardianAssessmentAction;
use codex_protocol::approvals::GuardianReviewReason;
use codex_protocol::guardian_review::GuardianApprovalRequestSource;
use codex_protocol::guardian_review::GuardianReviewDetails;
use codex_protocol::guardian_review::GuardianReviewedAction;
use codex_protocol::items::ModelInvocationContext;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::GuardianAssessmentDecisionSource;
use codex_protocol::protocol::GuardianAssessmentEvent;
use codex_protocol::protocol::GuardianAssessmentStatus;
use tokio::sync::Mutex;

use crate::GuardianRejectionCircuitBreaker;
use crate::GuardianRejectionCircuitBreakerAction;
use crate::GuardianRejectionCircuitBreakerPolicy;
use crate::GuardianReviewOutcome;
use crate::ReviewCompletion;

/// Captured action attribution used for both the started and terminal report.
pub struct ReviewMetadata {
    pub thread_id: String,
    pub turn_id: String,
    pub review_id: String,
    pub target_item_id: Option<String>,
    pub plugin_id: Option<String>,
    pub script_path: Option<String>,
    pub approval_request_source: GuardianApprovalRequestSource,
    pub reviewed_action: GuardianReviewedAction,
    pub action: GuardianAssessmentAction,
    pub review_reason: GuardianReviewReason,
    pub model_context: ModelInvocationContext,
}

pub struct ReviewReport {
    started: GuardianAssessmentEvent,
    approval_request_source: GuardianApprovalRequestSource,
    reviewed_action: GuardianReviewedAction,
}

impl ReviewReport {
    pub fn new(metadata: ReviewMetadata) -> Self {
        let started = GuardianAssessmentEvent {
            review_reason: Some(metadata.review_reason),
            model_context: Some(metadata.model_context),
            id: metadata.review_id,
            target_item_id: metadata.target_item_id,
            plugin_id: metadata.plugin_id,
            script_path: metadata.script_path,
            turn_id: metadata.turn_id,
            started_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(i64::MAX),
            completed_at_ms: None,
            status: GuardianAssessmentStatus::InProgress,
            risk_level: None,
            user_authorization: None,
            rationale: None,
            decision_source: None,
            action: metadata.action,
        };
        Self {
            started,
            approval_request_source: metadata.approval_request_source,
            reviewed_action: metadata.reviewed_action,
        }
    }

    pub fn turn_id(&self) -> &str {
        &self.started.turn_id
    }

    pub fn started_event(&self) -> GuardianAssessmentEvent {
        self.started.clone()
    }

    pub fn complete(
        &self,
        outcome: GuardianReviewOutcome,
        model: &ModelInfo,
        require_guardian: bool,
        analytics: GuardianReviewDetails,
        completed_at_ms: i64,
    ) -> ReviewCompletion {
        let mut event = self.started.clone();
        event.completed_at_ms = Some(completed_at_ms);
        event.decision_source = Some(GuardianAssessmentDecisionSource::Agent);
        crate::complete_review(outcome, model, require_guardian, event, analytics)
    }
}

/// Thread-scoped accounting shared by synchronous and cached approvals.
#[derive(Default)]
pub struct ReviewDenials(Mutex<GuardianRejectionCircuitBreaker>);

impl ReviewDenials {
    pub fn for_thread(store: &ExtensionData) -> Arc<Self> {
        store.get_or_init(Self::default)
    }

    pub async fn clear_turn(store: &ExtensionData, turn_id: &str) {
        if let Some(state) = store.get::<Self>() {
            state.0.lock().await.clear_turn(turn_id);
        }
    }

    pub async fn record_non_denial(&self, turn_id: &str) {
        self.0.lock().await.record_non_denial(turn_id);
    }

    /// Returns a warning exactly once when this turn reaches its denial limit.
    pub async fn record_denial(&self, turn_id: &str, model: &ModelInfo) -> Option<String> {
        let action = self
            .0
            .lock()
            .await
            .record_denial(turn_id, GuardianRejectionCircuitBreakerPolicy::from(model));
        match action {
            GuardianRejectionCircuitBreakerAction::Continue => None,
            GuardianRejectionCircuitBreakerAction::InterruptTurn {
                consecutive_denials,
                recent_denials,
            } => {
                let window = crate::AUTO_REVIEW_DENIAL_WINDOW_SIZE;
                Some(format!(
                    "Automatic approval review rejected too many approval requests for this turn ({consecutive_denials} consecutive, {recent_denials} in the last {window} reviews); interrupting the turn."
                ))
            }
        }
    }
}
