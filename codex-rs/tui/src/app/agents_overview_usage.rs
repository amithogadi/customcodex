//! Token counts received from local agent notifications.

use crate::status::format_tokens_compact;
use codex_app_server_protocol::TokenUsageBreakdown;
use ratatui::style::Stylize;
use ratatui::text::Line;

#[derive(Default)]
pub(super) struct AgentsOverviewUsage {
    pub(super) tokens: Option<TokenUsageBreakdown>,
}

pub(super) fn usage_lines(usage: &AgentsOverviewUsage) -> Vec<Line<'static>> {
    let Some(tokens) = &usage.tokens else {
        return Vec::new();
    };
    vec![
        vec![
            "Tokens: ".dim(),
            format!(
                "{} in · {} out",
                format_tokens_compact(tokens.input_tokens),
                format_tokens_compact(tokens.output_tokens),
            )
            .into(),
        ]
        .into(),
    ]
}
