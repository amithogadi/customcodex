//! Terminal history and clear-screen UI helpers for the TUI app.
//!
//! This module owns rendering the fresh session header, clearing inline or alternate-screen UI
//! state, and resetting transcript-related app state after `/clear` or Ctrl-L. Owned-screen sessions
//! keep committed cells as the render source and never enqueue terminal-scrollback rows here.

use super::*;
use crate::terminal_hyperlinks::HyperlinkLine;
use std::sync::Weak;

pub(super) struct RenderedHistoryTail {
    pub(super) cell: Weak<dyn HistoryCell>,
    pub(super) lines: Vec<HyperlinkLine>,
}

impl App {
    pub(super) fn insert_history_cell(&mut self, tui: &mut tui::Tui, cell: Box<dyn HistoryCell>) {
        // Global deprecations can be delivered again by hidden threads. Scope deduplication to
        // retained history so clearing or rebuilding a transcript can show the notice again.
        if let Some(notice) = cell
            .as_any()
            .downcast_ref::<history_cell::DeprecationNoticeCell>()
            && self.transcript_cells.iter().any(|existing| {
                existing
                    .as_any()
                    .downcast_ref::<history_cell::DeprecationNoticeCell>()
                    == Some(notice)
            })
        {
            return;
        }
        if !crate::empty_state_animation::is_startup_cell(cell.as_ref()) {
            self.chat_widget
                .empty_state_animation
                .borrow_mut()
                .dismiss();
        }
        if let Some(warnings) = cell
            .as_any()
            .downcast_ref::<history_cell::StartupWarningsCell>()
        {
            self.merge_startup_warnings(tui, warnings);
            return;
        }
        let is_session_header = cell.as_any().is::<history_cell::SessionInfoCell>();
        let cell: Arc<dyn HistoryCell> = cell.into();
        if let Some(Overlay::Transcript(t)) = &mut self.overlay {
            t.insert_cell(cell.clone());
            tui.frame_requester().schedule_frame();
        }
        self.transcript_cells.push(cell.clone());
        let deferred = tui.is_owned_screen() || self.native_history.insert(&cell);
        self.render_inserted_history_cell(tui, &cell, deferred);
        if is_session_header {
            self.merge_startup_warnings(tui, &history_cell::StartupWarningsCell::default());
        }
    }

    /// Track mutable status cards before choosing retained or terminal-owned rendering.
    pub(super) fn render_inserted_history_cell(
        &mut self,
        tui: &mut tui::Tui,
        cell: &Arc<dyn HistoryCell>,
        deferred: bool,
    ) {
        let width = self
            .chat_widget
            .history_wrap_width(tui.terminal.last_known_screen_size.width);
        // Owned replay must not eagerly format every historical entry.
        let lines = if !deferred {
            cell.display_hyperlink_lines_for_mode(width, self.chat_widget.history_render_mode())
        } else {
            Vec::new()
        };
        // Invisible diagnostics still update the badge without replacing the rendered tail.
        if deferred || lines.is_empty() {
            tui.frame_requester().schedule_frame();
            return;
        }
        if self.initial_history_replay_buffer.as_ref().is_some() {
            self.insert_history_cell_lines_with_initial_replay_buffer(tui, cell.as_ref(), width);
            self.last_rendered_history_tail = None;
        } else {
            self.insert_history_cell_lines(tui, cell.as_ref(), width);
            self.last_rendered_history_tail = if self.overlay.is_none() {
                Some(RenderedHistoryTail {
                    cell: Arc::downgrade(cell),
                    lines,
                })
            } else {
                None
            };
        }
    }

    pub(super) fn open_url_in_browser(&mut self, url: String) {
        if let Err(err) = webbrowser::open(&url) {
            self.chat_widget
                .add_error_message(format!("Failed to open browser for {url}: {err}"));
        }
    }

    pub(super) fn clear_ui_header_lines_with_version(
        &self,
        width: u16,
        version: &'static str,
    ) -> Vec<Line<'static>> {
        self.clear_ui_header_cell(version)
            .display_lines_for_mode(width, self.chat_widget.history_render_mode())
    }

    /// Share the source-backed header between retained drawing and legacy terminal insertion.
    fn clear_ui_header_cell(
        &self,
        version: &'static str,
    ) -> history_cell::SessionHeaderHistoryCell {
        history_cell::SessionHeaderHistoryCell::new(
            self.chat_widget.model_display_name().to_string(),
            self.chat_widget.current_reasoning_effort(),
            self.config.cwd.to_path_buf(),
            version,
        )
        .with_yolo_mode(history_cell::is_yolo_mode(&self.config))
    }

    pub(super) fn clear_ui_header_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.clear_ui_header_lines_with_version(width, CODEX_CLI_VERSION)
    }

    pub(super) fn queue_clear_ui_header(&mut self, tui: &mut tui::Tui) {
        if tui.is_owned_screen() {
            if !self.transcript_cells.iter().any(|cell| {
                cell.as_any().is::<history_cell::SessionInfoCell>()
                    || cell.as_any().is::<history_cell::SessionHeaderHistoryCell>()
            }) {
                let header: Arc<dyn HistoryCell> =
                    Arc::new(self.clear_ui_header_cell(CODEX_CLI_VERSION));
                self.transcript_cells.insert(/*index*/ 0, header);
            }
            tui.frame_requester().schedule_frame();
            return;
        }
        let width = self
            .chat_widget
            .history_wrap_width(tui.terminal.last_known_screen_size.width);
        let header_lines = self.clear_ui_header_lines(width);
        if !header_lines.is_empty() {
            tui.insert_history_lines_with_wrap_policy(
                header_lines,
                self.history_line_wrap_policy(),
            );
            self.has_emitted_history_lines = true;
        }
    }

    pub(super) fn clear_terminal_ui(
        &mut self,
        tui: &mut tui::Tui,
        redraw_header: bool,
    ) -> Result<()> {
        let is_alt_screen_active = tui.is_alt_screen_active();

        // Drop queued history insertions so stale transcript lines cannot be flushed after /clear.
        tui.clear_pending_history_lines();

        if tui.is_owned_screen() || is_alt_screen_active {
            tui.terminal.clear_visible_screen()?;
        } else {
            // Some terminals (Terminal.app, Warp) do not reliably drop scrollback when purge and
            // clear are emitted as separate backend commands. Prefer a single ANSI sequence.
            tui.terminal.clear_scrollback_and_visible_screen_ansi()?;
        }

        let mut area = tui.terminal.viewport_area;
        if area.y > 0 {
            // After a full clear, anchor the inline viewport at the top and redraw a fresh header
            // box. `insert_history_lines()` will shift the viewport down by the rendered height.
            area.y = 0;
            tui.terminal.set_viewport_area(area);
        }
        self.has_emitted_history_lines = false;

        if redraw_header {
            self.queue_clear_ui_header(tui);
        }
        Ok(())
    }

    pub(super) fn reset_app_ui_state_after_clear(&mut self) {
        self.reset_transcript_state_after_clear();
    }

    pub(super) fn reset_transcript_state_after_clear(&mut self) {
        self.overlay = None;
        self.transcript_cells.clear();
        self.turn_tips.dismiss();
        self.chat_widget.warning_display_state.dismissed.clear();
        self.chat_widget.warning_display_state.transcript = Arc::default();
        self.chat_widget.warning_display_state.synced_cells = None;
        self.native_history = Default::default();
        self.cancel_pending_key_chord();
        self.transcript_view = Default::default();
        self.last_rendered_history_tail = None;
        self.deferred_history_lines.clear();
        self.has_emitted_history_lines = false;
        self.transcript_reflow.clear();
        self.initial_history_replay_buffer = None;
        self.scrollback_has_older_history = false;
        self.backtrack = BacktrackState::default();
        self.backtrack_render_pending = false;
        self.skill_load_warnings.clear();
    }
}

#[cfg(test)]
#[path = "history_ui_tests.rs"]
mod tests;
