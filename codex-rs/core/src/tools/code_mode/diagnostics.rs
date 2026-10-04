//! Local code-mode diagnostics and extension timing callbacks.

use codex_protocol::ThreadId;
use std::sync::Arc;
use std::time::Duration;
use tracing::Span;

pub(super) struct CodeModeToolCallGuard {
    thread_id: String,
    turn_id: String,
    call_id: String,
    pub(super) cell_id: Option<String>,
    tool_name: &'static str,
    handler_span: Span,
    extensions: Arc<codex_extension_api::ExtensionRegistry<crate::config::Config>>,
}

impl CodeModeToolCallGuard {
    pub(super) fn new(
        session: &crate::session::session::Session,
        turn_id: String,
        call_id: String,
        tool_name: &'static str,
        handler_span: Span,
    ) -> Self {
        Self {
            thread_id: session.thread_id.to_string(),
            turn_id,
            call_id,
            cell_id: None,
            tool_name,
            handler_span,
            extensions: Arc::clone(&session.services.extensions),
        }
    }

    pub(super) fn finish(&mut self, success: bool) {
        self.handler_span
            .record("outcome", if success { "completed" } else { "failed" });
    }

    pub(super) fn record_code_mode_host_duration(&self, duration: Duration) {
        for observer in self.extensions.tool_lifecycle_contributors() {
            observer.on_tool_timing(codex_extension_api::ToolTimingInput {
                thread_id: &self.thread_id,
                turn_id: &self.turn_id,
                call_id: &self.call_id,
                boundary: codex_extension_api::ToolTimingBoundary::HostOperation,
                duration,
            });
        }
        let Ok(code_mode_host_duration_ns) = u64::try_from(duration.as_nanos()) else {
            return;
        };
        // Bridge joins this record to the outer tool-completion event. Emit it
        // before the handler returns so consumers never need another timeout.
        tracing::info!(
            target: "codex_code_mode::timing",
            {
                event.name = "codex.code_mode.host_timing",
                conversation_id = %self.thread_id,
                turn_id = %self.turn_id,
                call_id = %self.call_id,
                cell_id = self.cell_id.as_deref(),
                tool_name = self.tool_name,
                code_mode_host_duration_ns,
            },
            "code-mode host operation completed"
        );
    }
}

pub(super) struct NestedToolDispatchTrace {
    thread_id: ThreadId,
    pub(super) call_id: String,
    pub(super) interruption: Option<DispatchInterruption>,
    pub(super) span: Span,
}

impl NestedToolDispatchTrace {
    pub(super) fn new(thread_id: ThreadId, call_id: String) -> Self {
        Self {
            thread_id,
            call_id,
            interruption: Some(DispatchInterruption::Abandoned),
            span: Span::current(),
        }
    }
}

impl Drop for NestedToolDispatchTrace {
    fn drop(&mut self) {
        let Some(outcome) = self.interruption.take() else {
            return;
        };
        let outcome = match outcome {
            DispatchInterruption::Cancelled => "cancelled",
            DispatchInterruption::CellClosed => "cell_closed",
            DispatchInterruption::Abandoned => "abandoned",
        };
        tracing::event!(
            name: "codex.code_mode.nested_tool_dispatch_interrupted",
            target: "codex.trace_safe",
            parent: &self.span,
            tracing::Level::INFO,
            event.name = "codex.code_mode.nested_tool_dispatch_interrupted",
            conversation.id = %self.thread_id,
            call_id = self.call_id.as_str(),
            outcome,
        );
    }
}

pub(super) enum DispatchInterruption {
    Cancelled,
    CellClosed,
    Abandoned,
}

pub(super) fn trace_id(id: &str) -> Option<&str> {
    (!id.is_empty() && id.len() <= 256).then_some(id)
}
