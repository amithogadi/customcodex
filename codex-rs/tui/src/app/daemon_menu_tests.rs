use super::*;
use crate::app::test_support::make_test_app;
use crate::chatwidget::tests::helpers::render_bottom_popup;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn daemon_version_notice_preserves_comparison_and_local_guidance() {
    let mut app = make_test_app().await;
    app.app_server_target = AppServerTarget::LocalDaemon {
        allow_embedded_fallback: true,
        endpoint: crate::RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::relative_to_current_dir("codex.sock").unwrap(),
        },
    };
    let view = app.agents_overview_view(Vec::new(), /*selected_thread_id*/ None);
    app.chat_widget.show_bottom_pane_view(Box::new(view));
    let mut notices = Vec::new();
    for (client, server, comparison) in [
        ("0.155.0-alpha.23", "0.155.0-alpha.22", "older than"),
        ("0.155.0-alpha.23", "0.156.0", "different from"),
        ("0.0.0", "0.156.0", "different from"),
    ] {
        app.local_settings.tui.show_server_version_notice = true;
        assert_eq!(
            app.initialize_server_version_notice(client, Some(server)),
            Some(format!(
                "A background Codex service is running v{server}, {comparison} your Codex CLI v{client}."
            ))
        );
        let overview = render_bottom_popup(&app.chat_widget, /*width*/ 100);
        notices.push(
            overview
                .lines()
                .find(|line| line.contains("Service v"))
                .unwrap()
                .trim()
                .to_string(),
        );

        app.local_settings.tui.show_server_version_notice = false;
        assert_eq!(
            app.initialize_server_version_notice(client, Some(server)),
            None
        );
        assert_eq!(
            app.agents_overview
                .view_state
                .lock()
                .unwrap()
                .server_version_notice,
            None
        );
    }
    insta::assert_snapshot!(notices.join("\n"), @"
    Service v0.155.0-alpha.22 < Codex CLI v0.155.0-alpha.23 · /daemon
    Service v0.156.0 ≠ Codex CLI v0.155.0-alpha.23 · /daemon
    Service v0.156.0 ≠ Codex CLI v0.0.0 · /daemon
    ");
}
