use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::WarningNotification;
use codex_state::LogWriteFailureReporter;

use crate::outgoing_message::OutgoingMessageSender;

const LOG_WRITE_WARNING: &str = "Codex couldn't save diagnostic logs to its local database. Run `codex doctor` for diagnostics.";

pub(crate) struct LogWriteWarningReporter {
    outgoing: Weak<OutgoingMessageSender>,
    /// Permanently set by the first failure; limits warning delivery to one attempt
    /// for this reporter, even if later SQLite writes succeed.
    has_failure: AtomicBool,
    message: &'static str,
}

impl LogWriteWarningReporter {
    pub(crate) fn new(outgoing: &Arc<OutgoingMessageSender>) -> Arc<Self> {
        Arc::new(Self {
            outgoing: Arc::downgrade(outgoing),
            has_failure: AtomicBool::new(false),
            message: LOG_WRITE_WARNING,
        })
    }

    pub(crate) fn notify_failure(&self) {
        if !self.has_failure.swap(true, Ordering::Relaxed)
            && let Some(outgoing) = self.outgoing.upgrade()
        {
            outgoing.try_send_server_notification(ServerNotification::Warning(
                WarningNotification {
                    thread_id: None,
                    message: self.message.to_string(),
                },
            ));
        }
    }
}

impl LogWriteFailureReporter for LogWriteWarningReporter {
    fn report_failure(&self, _diagnostic: &str) {
        self.notify_failure();
    }
}

#[cfg(test)]
#[path = "log_write_warning_tests.rs"]
mod tests;
