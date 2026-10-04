//! OpenRouter accepts ordinary function tools, not Responses namespaces.
//! Translate only at the wire boundary; history keeps native tool identities.

use crate::common::{ResponseEvent, ResponseStream, ResponsesApiRequest};
use crate::error::ApiError;
use codex_protocol::models::{ContentItem, ResponseItem, plaintext_agent_message_content};
use futures::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) type ToolNames = BTreeMap<String, (String, Option<String>)>;

pub fn is_openrouter(base_url: &str) -> bool {
    url::Url::parse(base_url).is_ok_and(|url| {
        url.scheme() == "https"
            && matches!(
                url.host_str(),
                Some("openrouter.ai" | "eu.openrouter.ai" | "us.openrouter.ai")
            )
    })
}

fn invalid(message: impl ToString) -> ApiError {
    ApiError::InvalidRequest {
        message: format!("OpenRouter: {}", message.to_string()),
    }
}

fn alias(name: &str, namespace: Option<&str>) -> String {
    let candidate = namespace.map_or_else(|| name.to_owned(), |ns| format!("{ns}__{name}"));
    if candidate.len() <= 64
        && candidate
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        candidate
    } else {
        let identity = format!("{}\0{name}", namespace.unwrap_or_default());
        format!(
            "tool_{}",
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, identity.as_bytes()).simple()
        )
    }
}

fn flatten(
    tools: Vec<Value>,
    namespace: Option<&str>,
    names: &mut ToolNames,
    out: &mut Vec<Value>,
) -> Result<(), ApiError> {
    for mut tool in tools {
        if tool["type"] == "namespace" {
            let ns = tool["name"]
                .as_str()
                .ok_or_else(|| invalid("missing tool namespace"))?;
            let nested = tool["tools"]
                .as_array()
                .ok_or_else(|| invalid("missing namespace tools"))?;
            flatten(nested.clone(), Some(ns), names, out)?;
            continue;
        }
        if tool["type"] != "function" {
            return Err(invalid("configured models support function tools only"));
        }
        let name = tool["name"]
            .as_str()
            .ok_or_else(|| invalid("missing tool name"))?
            .to_owned();
        let wire_name = alias(&name, namespace);
        if names
            .insert(wire_name.clone(), (name, namespace.map(str::to_owned)))
            .is_some()
        {
            return Err(invalid("ambiguous tool names after namespace translation"));
        }
        tool["name"] = Value::String(wire_name);
        if let Some(fields) = tool.as_object_mut() {
            fields.remove("defer_loading");
        }
        out.push(tool);
    }
    Ok(())
}

pub(super) fn prepare(request: &mut ResponsesApiRequest) -> Result<ToolNames, ApiError> {
    let mut names = ToolNames::new();
    if let Some(tools) = &request.tools {
        let tools = serde_json::from_str(tools.as_raw_value().get()).map_err(invalid)?;
        let mut flat = Vec::new();
        flatten(tools, None, &mut names, &mut flat)?;
        request.tools = Some(crate::common::ResponsesApiTools::from(Arc::from(
            serde_json::value::to_raw_value(&flat).map_err(invalid)?,
        )));
    }
    for item in &mut request.input {
        match item {
            ResponseItem::FunctionCall {
                name, namespace, ..
            } => {
                *name = alias(name, namespace.as_deref());
                *namespace = None;
            }
            ResponseItem::AgentMessage {
                author,
                recipient,
                content,
                ..
            } => {
                let text = plaintext_agent_message_content(content)
                    .ok_or_else(|| invalid("cannot forward encrypted agent messages"))?;
                *item = ResponseItem::Message {
                    id: None,
                    role: "user".into(),
                    content: vec![ContentItem::InputText {
                        text: format!("[Agent message from {author} to {recipient}]\n{text}"),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                };
            }
            _ => {}
        }
    }
    Ok(names)
}

fn restore(event: &mut ResponseEvent, names: &ToolNames) -> Result<(), ApiError> {
    if let ResponseEvent::OutputItemAdded(item) | ResponseEvent::OutputItemDone(item) = event
        && let ResponseItem::FunctionCall {
            name,
            namespace,
            encrypted_function_args,
            ..
        } = item
    {
        // Providers may omit the name on the initial Added event.
        if name.is_empty() {
            return Ok(());
        }
        let (original, ns) = names
            .get(name)
            .ok_or_else(|| invalid("unknown function returned by provider"))?;
        *name = original.clone();
        *namespace = ns.clone();
        *encrypted_function_args = Some(Vec::new());
    }
    Ok(())
}

pub(super) fn restore_stream(mut upstream: ResponseStream, names: ToolNames) -> ResponseStream {
    let upstream_request_id = upstream.upstream_request_id.clone();
    let interrupt = upstream.interrupt.take();
    let (tx, rx_event) = mpsc::channel(128);
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                biased;
                _ = tx.closed() => return,
                next = upstream.next() => next,
            };
            let Some(event) = next else {
                return;
            };
            let event = event.and_then(|mut event| {
                restore(&mut event, &names)?;
                Ok(event)
            });
            let failed = event.is_err();
            if tx.send(event).await.is_err() || failed {
                return;
            }
        }
    });
    ResponseStream {
        rx_event,
        upstream_request_id,
        interrupt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn namespace_tools_round_trip_and_collaboration_is_plaintext() {
        let mut names = ToolNames::new();
        let mut tools = Vec::new();
        flatten(vec![json!({"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","parameters":{"type":"object"}}]})], None, &mut names, &mut tools).unwrap();
        assert_eq!(tools[0]["name"], "collaboration__spawn_agent");
        let item: ResponseItem = serde_json::from_value(json!({"type":"function_call","call_id":"a","name":"collaboration__spawn_agent","arguments":"{}"})).unwrap();
        let mut event = ResponseEvent::OutputItemDone(item);
        restore(&mut event, &names).unwrap();
        assert!(
            matches!(event, ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {name, namespace:Some(ns), encrypted_function_args:Some(fields),..}) if name=="spawn_agent" && ns=="collaboration" && fields.is_empty())
        );
    }

    #[test]
    fn ambiguous_tool_names_fail_before_sending_and_long_names_are_stable() {
        let mut names = ToolNames::new();
        let mut tools = Vec::new();
        let input = vec![
            json!({"type":"function","name":"mcp__read"}),
            json!({"type":"namespace","name":"mcp","tools":[{"type":"function","name":"read"}]}),
        ];
        assert!(flatten(input, None, &mut names, &mut tools).is_err());
        let name = "x".repeat(100);
        assert_eq!(alias(&name, Some("mcp")), alias(&name, Some("mcp")));
        assert!(alias(&name, Some("mcp")).len() <= 64);
        assert!(!is_openrouter("https://openrouter.ai.example.com/api/v1"));
        assert!(!is_openrouter("https://api.isoquant.ai/v1"));
    }

    #[tokio::test]
    async fn dropping_translated_stream_cancels_upstream() {
        let (tx, rx_event) = mpsc::channel(1);
        let stream = restore_stream(
            ResponseStream {
                rx_event,
                upstream_request_id: Some("request-id".into()),
                interrupt: None,
            },
            ToolNames::new(),
        );
        assert_eq!(stream.upstream_request_id.as_deref(), Some("request-id"));
        drop(stream);
        tokio::time::timeout(std::time::Duration::from_secs(1), tx.closed())
            .await
            .unwrap();
    }
}
