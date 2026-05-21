//! Smoke tests for the LlmContext → ChatRequest mapping.
//!
//! No network, no thinking content — PR 3b extends this with thinking /
//! reasoning round-trip coverage.

use genai::chat::{ChatRequest, ChatRole, ContentPart, MessageContent};
use grain_agent_core::{
    AssistantContent, AssistantMessage, ImageContent, LlmContext, Message, StopReason,
    TextContent, ToolCall, ToolDefinition, ToolResultMessage, Usage, UserContent, UserMessage,
};
use grain_llm_genai::to_chat_request;

fn ctx(messages: Vec<Message>, tools: Vec<ToolDefinition>, system: &str) -> LlmContext {
    LlmContext {
        system_prompt: system.into(),
        messages,
        tools,
    }
}

fn user_text(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![UserContent::Text(TextContent { text: text.into() })],
        timestamp: 0,
    })
}

fn assistant_text(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::Text(TextContent { text: text.into() })],
        api: "anthropic".into(),
        provider: "anthropic".into(),
        model: "claude".into(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    })
}

fn collect_text_from_message_content(content: &MessageContent) -> Vec<String> {
    let json = serde_json::to_value(content).expect("MessageContent must serialize");
    let arr = json.as_array().expect("MessageContent serializes transparent as array");
    arr.iter()
        .filter_map(|part| {
            // Text is `{ "Text": "..." }` (or similar) under serde tag-less default.
            // Use the public ContentPart variant decode via re-deserialization.
            serde_json::from_value::<ContentPart>(part.clone()).ok()
        })
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t),
            _ => None,
        })
        .collect()
}

#[test]
fn empty_context_yields_empty_chat_request() {
    let chat: ChatRequest = to_chat_request(&ctx(vec![], vec![], ""));
    assert!(chat.messages.is_empty());
    assert!(chat.tools.is_none() || chat.tools.as_ref().is_none_or(|t| t.is_empty()));
    // No system component when system_prompt is empty.
    assert_eq!(chat.iter_systems().count(), 0);
}

#[test]
fn system_prompt_is_attached_when_nonempty() {
    let chat = to_chat_request(&ctx(vec![], vec![], "be precise"));
    let joined = chat.join_systems().expect("system attached");
    assert!(joined.contains("be precise"));
}

#[test]
fn user_text_message_maps_to_user_role() {
    let chat = to_chat_request(&ctx(vec![user_text("hello")], vec![], ""));
    assert_eq!(chat.messages.len(), 1);
    assert_eq!(chat.messages[0].role, ChatRole::User);
    let texts = collect_text_from_message_content(&chat.messages[0].content);
    assert_eq!(texts, vec!["hello".to_string()]);
}

#[test]
fn user_with_image_uses_parts() {
    let msg = Message::User(UserMessage {
        content: vec![
            UserContent::Text(TextContent { text: "describe this".into() }),
            UserContent::Image(ImageContent {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            }),
        ],
        timestamp: 0,
    });
    let chat = to_chat_request(&ctx(vec![msg], vec![], ""));
    let body = serde_json::to_value(&chat.messages[0].content).unwrap();
    let arr = body.as_array().expect("parts array");
    assert_eq!(arr.len(), 2, "two parts: text + binary");
}

#[test]
fn assistant_text_message_maps_to_assistant_role() {
    let chat = to_chat_request(&ctx(vec![assistant_text("done")], vec![], ""));
    assert_eq!(chat.messages[0].role, ChatRole::Assistant);
    let texts = collect_text_from_message_content(&chat.messages[0].content);
    assert_eq!(texts, vec!["done".to_string()]);
}

#[test]
fn assistant_with_tool_calls_emits_tool_call_part() {
    let msg = Message::Assistant(AssistantMessage {
        content: vec![
            AssistantContent::Text(TextContent { text: "calling tool".into() }),
            AssistantContent::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({ "value": "hi" }),
            }),
        ],
        api: "openai".into(),
        provider: "openai".into(),
        model: "gpt-4o".into(),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 0,
    });
    let chat = to_chat_request(&ctx(vec![msg], vec![], ""));
    let body = serde_json::to_value(&chat.messages[0].content).unwrap();
    let body_s = body.to_string();
    assert!(body_s.contains("call-1"), "tool call id preserved: {body_s}");
    assert!(body_s.contains("echo"), "fn_name preserved: {body_s}");
    assert!(body_s.contains("hi"), "arguments preserved: {body_s}");
    assert!(body_s.contains("calling tool"), "text part preserved: {body_s}");
}

#[test]
fn thinking_text_is_not_echoed_back_to_provider() {
    use grain_agent_core::ThinkingContent;
    // genai 0.5 has no outbound reasoning_content slot; reasoning text stays
    // in the grain transcript but does not appear on the wire.
    let msg = Message::Assistant(AssistantMessage {
        content: vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "internal reasoning".into(),
                signature: None,
                provider_metadata: None,
            }),
            AssistantContent::Text(TextContent { text: "final answer".into() }),
        ],
        api: "openai".into(),
        provider: "openai".into(),
        model: "o3-mini".into(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    });
    let chat = to_chat_request(&ctx(vec![msg], vec![], ""));
    let body = serde_json::to_value(&chat.messages[0].content).unwrap();
    let body_s = body.to_string();
    assert!(!body_s.contains("internal reasoning"), "no outbound slot for reasoning text");
    assert!(body_s.contains("final answer"));
}

#[test]
fn thinking_signature_attaches_to_first_tool_call() {
    use grain_agent_core::ThinkingContent;
    let msg = Message::Assistant(AssistantMessage {
        content: vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "anthropic chain".into(),
                signature: Some("sig-abc".into()),
                provider_metadata: None,
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({ "value": "hi" }),
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "call-2".into(),
                name: "echo".into(),
                arguments: serde_json::json!({ "value": "ho" }),
            }),
        ],
        api: "anthropic".into(),
        provider: "anthropic".into(),
        model: "claude".into(),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 0,
    });
    let chat = to_chat_request(&ctx(vec![msg], vec![], ""));
    let body = serde_json::to_value(&chat.messages[0].content).unwrap();
    let arr = body.as_array().expect("parts");
    // First tool call carries the signature; second does not.
    let to_calls: Vec<&serde_json::Value> = arr
        .iter()
        .filter_map(|v| v.get("ToolCall"))
        .collect();
    assert_eq!(to_calls.len(), 2);
    assert_eq!(
        to_calls[0].get("thought_signatures"),
        Some(&serde_json::json!(["sig-abc"]))
    );
    assert!(
        to_calls[1].get("thought_signatures").is_none()
            || to_calls[1].get("thought_signatures") == Some(&serde_json::Value::Null),
        "second tool call has no signatures",
    );
}

#[test]
fn tool_result_message_maps_to_tool_role() {
    let msg = Message::ToolResult(ToolResultMessage {
        tool_call_id: "call-1".into(),
        tool_name: "echo".into(),
        content: vec![UserContent::Text(TextContent { text: "echo: hi".into() })],
        details: serde_json::Value::Null,
        is_error: false,
        timestamp: 0,
    });
    let chat = to_chat_request(&ctx(vec![msg], vec![], ""));
    assert_eq!(chat.messages[0].role, ChatRole::Tool);
    let body = serde_json::to_value(&chat.messages[0].content).unwrap();
    let body_s = body.to_string();
    assert!(body_s.contains("call-1"));
    assert!(body_s.contains("echo: hi"));
}

#[test]
fn tools_translate_to_genai_tool_definitions() {
    let tool = ToolDefinition {
        name: "echo".into(),
        label: "Echo".into(),
        description: "Echo back the value".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"]
        }),
        execution_mode: None,
    };
    let chat = to_chat_request(&ctx(vec![], vec![tool], ""));
    let tools = chat.tools.expect("tools attached");
    assert_eq!(tools.len(), 1);
    let t = &tools[0];
    assert_eq!(t.name, "echo");
    assert_eq!(t.description.as_deref(), Some("Echo back the value"));
    let schema = t.schema.as_ref().expect("schema attached");
    assert_eq!(
        schema.get("required").and_then(|r| r.as_array()).map(|a| a.len()),
        Some(1)
    );
}

#[test]
fn empty_tool_set_does_not_attach_tools() {
    let chat = to_chat_request(&ctx(vec![user_text("hi")], vec![], ""));
    assert!(
        chat.tools.is_none() || chat.tools.as_ref().is_none_or(|t| t.is_empty()),
        "no tools attached when none configured"
    );
}
