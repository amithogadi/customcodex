//! Protect connection credentials in diagnostics without changing signaling data.

use super::AppCommand;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::json;
