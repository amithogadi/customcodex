//! Logs process lifecycle events locally and bounds process identifiers recorded by tracing.
//! Lifecycle events retain the identity captured at launch across reconnects and later operations.

use std::sync::Arc;

use codex_sandboxing::SandboxType;

use crate::connection_metadata::ExecutorRegistration;

/// Log fields captured at launch, never refreshed from a resumed session.
#[derive(Clone, Default)]
pub(crate) struct ProcessLogContext {
    pub(crate) thread_id: Option<String>,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) executor_registration: Option<Arc<ExecutorRegistration>>,
}

/// Lifecycle events with outcome fields only when a process has exited.
pub(crate) enum ProcessLogEvent {
    Start,
    SpawnFailed,
    SandboxDenied,
    Exit {
        exit_code: i32,
        termination_requested: bool,
    },
}

impl ProcessLogContext {
    pub(crate) fn log(&self, event: ProcessLogEvent, sandbox: SandboxType) {
        let (event_name, exit_code, termination_requested, reason) = match event {
            ProcessLogEvent::Start => ("codex.exec_server.process_start", None, None, None),
            ProcessLogEvent::SpawnFailed => {
                ("codex.exec_server.process_spawn_failed", None, None, None)
            }
            ProcessLogEvent::SandboxDenied => (
                "codex.exec_server.sandbox_denied",
                None,
                None,
                Some("inferred_denial"),
            ),
            ProcessLogEvent::Exit {
                exit_code,
                termination_requested,
            } => (
                "codex.exec_server.process_exit",
                Some(exit_code),
                Some(termination_requested),
                None,
            ),
        };
        tracing::event!(
            target: "codex_exec_server::process",
            tracing::Level::INFO,
            event.name = event_name,
            conversation.id = self.thread_id.as_deref(),
            tool.call_id = self.tool_call_id.as_deref(),
            executor.environment_id = self.executor_registration.as_ref().map(|registration| registration.environment_id.as_str()),
            executor.registration_id = self.executor_registration.as_ref().map(|registration| registration.executor_registration_id.as_str()),
            sandbox.type = ?sandbox,
            process.exit_code = exit_code,
            process.termination_requested = termination_requested,
            reason,
        );
    }
}

pub(crate) fn trace_process_id(process_id: &str) -> Option<&str> {
    (!process_id.is_empty() && process_id.len() <= 64).then_some(process_id)
}
