//! Exercise Reserve picker focus, its status label, and the composer palette across entry and exit.

use super::helpers::normalize_snapshot_paths;
use super::*;
use crate::terminal_palette::with_test_default_colors;
use crate::terminal_probe::DefaultColors;
use pretty_assertions::assert_eq;

fn reserve_snapshot(primary_used: i32, weekly_used: i32) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some("base_model_inference".into()),
        limit_name: Some("gpt-reserve".into()),
        normal_model_slug: None,
        primary: Some(RateLimitWindow {
            used_percent: primary_used,
            window_duration_mins: Some(300),
            resets_at: None,
        }),
        secondary: Some(RateLimitWindow {
            used_percent: weekly_used,
            window_duration_mins: Some(10080),
            resets_at: None,
        }),
        ..snapshot(/*percent*/ 0.0)
    }
}

#[tokio::test]
async fn luna_reserve_status_tracks_the_active_model() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(Some("gpt-5.6-sol")).await;
    chat.has_chatgpt_account = true;
    chat.on_rate_limit_snapshot(Some(reserve_snapshot(
        /*primary_used*/ 25, /*weekly_used*/ 60,
    )));
    assert!(
        !normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 80))
            .contains("Luna Reserve")
    );

    chat.set_model("gpt-reserve");
    let rendered = normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 80));
    assert!(rendered.contains("Luna Reserve default"));
    insta::assert_snapshot!("luna_reserve_usage_wide", rendered);
    insta::assert_snapshot!(
        "luna_reserve_usage_narrow",
        normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 34))
    );

    chat.set_model("gpt-5.6-sol");
    assert!(
        !normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 80))
            .contains("Luna Reserve")
    );
    chat.set_model("gpt-reserve");
    assert!(
        normalize_snapshot_paths(render_bottom_popup(&chat, /*width*/ 80)).contains("Luna Reserve")
    );
}

#[tokio::test]
async fn luna_reserve_prompt_preserves_the_composer_palette_on_exit() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(Some("gpt-reserve")).await;
    chat.has_chatgpt_account = true;
    chat.on_rate_limit_snapshot(Some(reserve_snapshot(
        /*primary_used*/ 25, /*weekly_used*/ 60,
    )));
    for colors in [
        DefaultColors {
            fg: (230, 230, 230),
            bg: (20, 20, 20),
        },
        DefaultColors {
            fg: (25, 25, 25),
            bg: (255, 255, 255),
        },
    ] {
        with_test_default_colors(colors, || {
            chat.set_model("gpt-reserve");
            let area = Rect::new(0, 0, 80, chat.bottom_pane.desired_height(/*width*/ 80));
            let mut active = Buffer::empty(area);
            chat.bottom_pane.render(area, &mut active);
            let cursor = chat.bottom_pane.cursor_pos(area).expect("composer cursor");
            let ordinary_style = crate::style::user_message_style();
            assert_eq!(active[(0, cursor.1)].bg, ordinary_style.bg.unwrap());
            assert_eq!(active[(0, cursor.1)].bg, active[(0, cursor.1 - 1)].bg);
            chat.set_model("gpt-5.6-sol");
            let area = Rect::new(0, 0, 80, chat.bottom_pane.desired_height(/*width*/ 80));
            let mut inactive = Buffer::empty(area);
            chat.bottom_pane.render(area, &mut inactive);
            let restored_cursor = chat.bottom_pane.cursor_pos(area).expect("composer cursor");
            assert_eq!(
                inactive[(0, restored_cursor.1)].bg,
                ordinary_style.bg.unwrap()
            );
            assert_eq!(restored_cursor.1, cursor.1);
        });
    }
}
