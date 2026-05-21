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
                fn_arguments: tc.arguments.clone(),
                thought_signatures: None,
            })),
            AssistantContent::Thinking(t) => {
                if let Some(sig) = &t.signature {
                    signatures.push(sig.clone());
                }
                // `t.thinking` and `t.provider_metadata` are preserved in the
                // grain transcript but not echoed back; genai 0.5 has no
                // outbound slot for plain reasoning text.
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
    let text = msg
        .content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text(t) => Some(t.text.as_str()),
            UserContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

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
