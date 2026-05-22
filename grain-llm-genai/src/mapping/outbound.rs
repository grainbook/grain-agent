//! Outbound: project [`grain_agent_core::LlmContext`] into a [`genai::chat::ChatRequest`].
//!
//! Pure functions, no I/O. Thinking / reasoning replay (PR 3b):
//! - `Thinking.signature` values are attached to the **first** outgoing
//!   [`genai::chat::ToolCall::thought_signatures`] — matches the convention
//!   in `genai 0.5`'s own stream-end finalizer for Anthropic-style signed
//!   thinking blocks. This is the path that preserves multi-turn correctness
//!   on Anthropic (provider re-validates via signature).
//! - **Reasoning text is intentionally not echoed back.** `genai 0.5` does
//!   not expose an outbound slot for `reasoning_content`; providers
//!   regenerate their own reasoning each turn (OpenAI o-series, DeepSeek-R1)
//!   so re-sending it isn't required for correctness. The text is still
//!   preserved in the grain transcript via [`AssistantContent::Thinking`].

use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ContentPart, MessageContent, Tool, ToolCall,
    ToolResponse,
};
use grain_agent_core::{
    AssistantContent, AssistantMessage, LlmContext, Message, ToolDefinition,
    ToolResultMessage, UserContent, UserMessage,
};

/// Sensible defaults for any genai chat request driven by the agent loop.
pub fn baseline_chat_options() -> ChatOptions {
    ChatOptions::default()
        .with_capture_content(true)
        .with_capture_usage(true)
        .with_capture_tool_calls(true)
        .with_capture_reasoning_content(true)
}

/// Translate an `LlmContext` snapshot into a `ChatRequest`.
pub fn to_chat_request(ctx: &LlmContext) -> ChatRequest {
    let mut chat = ChatRequest::new(Vec::with_capacity(ctx.messages.len()));

    if !ctx.system_prompt.is_empty() {
        chat = chat.with_system(ctx.system_prompt.clone());
    }

    for msg in &ctx.messages {
        let cm = match msg {
            Message::User(u) => user_to_chat_message(u),
            Message::Assistant(a) => assistant_to_chat_message(a),
            Message::ToolResult(t) => tool_result_to_chat_message(t),
        };
        chat = chat.append_message(cm);
    }

    let tools: Vec<Tool> = ctx.tools.iter().map(tool_def_to_genai).collect();
    if !tools.is_empty() {
        chat = chat.with_tools(tools);
    }

    chat
}

// ---------------------------------------------------------------------------
// Per-role conversion
// ---------------------------------------------------------------------------

fn user_to_chat_message(msg: &UserMessage) -> ChatMessage {
    ChatMessage::user(user_content_to_message_content(&msg.content))
}

fn user_content_to_message_content(content: &[UserContent]) -> MessageContent {
    if let [UserContent::Text(t)] = content {
        return MessageContent::from_text(t.text.clone());
    }
    let parts: Vec<ContentPart> = content.iter().map(user_content_to_part).collect();
    MessageContent::from_parts(parts)
}

fn user_content_to_part(c: &UserContent) -> ContentPart {
    match c {
        UserContent::Text(t) => ContentPart::Text(t.text.clone()),
        UserContent::Image(img) => {
            ContentPart::from_binary_base64(img.mime_type.clone(), img.data.clone(), None)
        }
    }
}

fn assistant_to_chat_message(msg: &AssistantMessage) -> ChatMessage {
    let mut parts: Vec<ContentPart> = Vec::with_capacity(msg.content.len());
    let mut signatures: Vec<String> = Vec::new();

    for c in &msg.content {
        match c {
            AssistantContent::Text(t) => parts.push(ContentPart::Text(t.text.clone())),
            AssistantContent::ToolCall(tc) => parts.push(ContentPart::ToolCall(ToolCall {
                call_id: tc.id.clone(),
                fn_name: tc.name.clone(),
                fn_arguments: normalize_outbound_tool_args(&tc.arguments),
                thought_signatures: None,
            })),
            AssistantContent::Thinking(t) => {
                if let Some(sig) = &t.signature {
                    signatures.push(sig.clone());
                }
                // Plain reasoning text travels via genai 0.6's
                // `ContentPart::ReasoningContent`. The adapter rehydrates
                // it into the provider-native wire field
                // (`reasoning_content` for DeepSeek / OpenAI-compat
                // thinking models). Required for DeepSeek-v4-pro
                // thinking mode — the API rejects requests that
                // include an assistant turn whose `reasoning_content`
                // is missing.
                //
                // For Anthropic the `signatures` path above is what
                // round-trips signed thinking blocks; the
                // ReasoningContent variant is informational on that
                // adapter and won't conflict.
                if !t.thinking.is_empty() {
                    parts.push(ContentPart::ReasoningContent(t.thinking.clone()));
                }
                // `t.provider_metadata` stays in the transcript only —
                // none of our supported providers ask for it back.
            }
            AssistantContent::Image(_) => {}
        }
    }

    // Anthropic-style: thought signatures travel on the first tool call.
    // Mirrors `genai 0.5`'s own captured_thought_signatures placement on
    // stream-end finalization. Critical for multi-turn signed-thinking flows.
    if !signatures.is_empty() {
        for part in parts.iter_mut() {
            if let ContentPart::ToolCall(tc) = part {
                tc.thought_signatures = Some(signatures.clone());
                break;
            }
        }
    }

    let content = if parts.is_empty() {
        MessageContent::from_text(String::new())
    } else if let [ContentPart::Text(text)] = parts.as_slice() {
        MessageContent::from_text(text.clone())
    } else {
        MessageContent::from_parts(parts)
    };

    ChatMessage::assistant(content)
}

fn tool_result_to_chat_message(msg: &ToolResultMessage) -> ChatMessage {
    let mut dropped_images = 0usize;
    let text = msg
        .content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text(t) => Some(t.text.as_str()),
            UserContent::Image(_) => {
                dropped_images += 1;
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if dropped_images > 0 {
        // genai 0.5's `ToolResponse` carries plain `content: String` only —
        // there's no slot for image attachments. Surface this so operators
        // know the data was lost instead of silently truncating.
        eprintln!(
            "[warn] grain-llm-genai: dropped {dropped_images} image content block(s) from tool \
             result for call_id={} (genai 0.5 ToolResponse is text-only)",
            msg.tool_call_id
        );
    }

    // `tool_name` is dropped at the genai boundary — results are correlated
    // by `call_id` (genai 0.5's `ToolResponse` has no fn_name slot).
    let response = ToolResponse::new(msg.tool_call_id.clone(), text);
    ChatMessage::from(response)
}

// ---------------------------------------------------------------------------
// Tool definition
// ---------------------------------------------------------------------------

fn tool_def_to_genai(def: &ToolDefinition) -> Tool {
    let mut tool = Tool::new(def.name.clone());
    if !def.description.is_empty() {
        tool = tool.with_description(def.description.clone());
    }
    if !def.parameters.is_null() {
        tool = tool.with_schema(def.parameters.clone());
    }
    tool
}

/// Coerce a tool-call `arguments` value into the JSON object shape the
/// provider expects.
///
/// History entries sometimes land in the session with `arguments` stored
/// as a JSON-encoded string (a streaming chunk that finalized before
/// `inbound::normalize_tool_args` could unwrap it, or a /resume of an
/// older malformed entry). Sending such a string back to the provider
/// triggers `"function.arguments must decode to a JSON object, got
/// str"` 400s. Defensive symmetry with the inbound normalizer.
fn normalize_outbound_tool_args(raw: &serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::String(s) = raw
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s)
        && parsed.is_object()
    {
        return parsed;
    }
    raw.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outbound_tool_args_unwraps_json_encoded_object_string() {
        let raw = json!(r#"{"path": "/foo", "content": "hi"}"#);
        let normalized = normalize_outbound_tool_args(&raw);
        assert_eq!(normalized, json!({"path": "/foo", "content": "hi"}));
    }

    #[test]
    fn outbound_tool_args_passes_through_object() {
        let raw = json!({"path": "/foo"});
        assert_eq!(normalize_outbound_tool_args(&raw), raw);
    }

    #[test]
    fn outbound_tool_args_leaves_non_object_string_alone() {
        // A bare string literal isn't a JSON object — keep as-is so the
        // shape mismatch surfaces (rather than being silently masked).
        let raw = json!("just a string");
        assert_eq!(normalize_outbound_tool_args(&raw), raw);
    }
}
