//! Displays local daemon status and management guidance.

use super::*;

impl App {
    pub(super) fn open_daemon_menu(&mut self) {
        let message = self
            .chat_widget
            .remote_connection
            .as_ref()
            .map(|connection| format!("Background server: {}", connection.version))
            .unwrap_or_else(|| "This terminal uses an embedded local server.".to_string());
        self.chat_widget.add_info_message(
            message,
            Some("Manage the local server with codex app-server daemon.".to_string()),
        );
    }
}

#[cfg(test)]
#[path = "daemon_menu_tests.rs"]
mod tests;
