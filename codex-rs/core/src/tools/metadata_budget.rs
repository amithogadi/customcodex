//! Byte accounting for outgoing tool metadata budgets.

use codex_protocol::models::ResponseItem;
use codex_protocol::models::executed_tool_call_metadata_bytes;

pub(crate) fn metadata_bytes(items: &[ResponseItem]) -> usize {
    items.iter().fold(0_usize, |bytes, item| {
        bytes.saturating_add(executed_tool_call_metadata_bytes(item))
    })
}
