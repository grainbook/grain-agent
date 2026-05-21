//! Outbound: project [`grain_agent_core::LlmContext`] into a [`genai::chat::ChatRequest`].
//!
//! Pure functions, no I/O. Thinking / reasoning content blocks
//! ([`grain_agent_core::AssistantContent::Thinking`]) are intentionally
//! dropped at this layer — PR 3b reintroduces them with provider-specific
//! replay logic driven by [`grain_llm_models::ThinkingProfile::reasoning_field_name`].

use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ContentPart, MessageContent, Tool, ToolCall,
    ToolResponse,
};
use grain_agent_core::{
    AssistantContent, AssistantMessage, LlmContext, Message, ToolDefinition,
    ToolResultMessage, UserContent, UserMessage,
};

/// Sensible defaults for any genai chat request driven by the agent loop:
/// capture content, usage, and tool calls so the terminal `StreamEnd` event
/// is enough to materialize a complete `AssistantMessage` in PR 3b.
pub fn baseline_chat_options() -> ChatOptions {
    ChatOptions::default()
        .with_capture_content(true)
        .with_capture_usage(true)
        .with_capture_tool_calls(true)
}

/// Translate an `LlmContext` snapshot into a `ChatRequest`.
///
/// - The system prompt is attached only when non-empty.
/// - Tools are forwarded only when the context lists any (genai treats an
///   empty `tools` vector and `None` differently for some providers).
/// - User / assistant / tool-result messages are each turned into a single
///   [`ChatMessage`] preserving in-message content order.
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
    // Single text → use the more compact text path; otherwise fall back to parts.
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
            // genai's Binary variant — providers handle data:URL / multipart per protocol.
            // Trailing `None` is the optional human-facing filename; not part of our model.
            ContentPart::from_binary_base64(img.mime_type.clone(), img.data.clone(), None)
        }
    }
}

fn assistant_to_chat_message(msg: &AssistantMessage) -> ChatMessage {
    // PR 3a scope: collect Text + ToolCall parts in source order; drop Thinking
    // (PR 3b will route through `ChatMessage::with_reasoning_content` or
    // `assistant_tool_calls_with_thoughts` depending on provider profile).
    let mut parts: Vec<ContentPart> = Vec::with_capacity(msg.content.len());
    for c in &msg.content {
        match c {
            AssistantContent::Text(t) => parts.push(ContentPart::Text(t.text.clone())),
            AssistantContent::ToolCall(tc) => parts.push(ContentPart::ToolCall(ToolCall {
                call_id: tc.id.clone(),
                fn_name: tc.name.clone(),
                fn_arguments: tc.arguments.clone(),
                thought_signatures: None,
            })),
            AssistantContent::Thinking(_) | AssistantContent::Image(_) => {
                // Thinking: PR 3b handles round-tripping with provider_metadata.
                // Image-from-assistant is rare; defer until a model actually needs it.
            }
        }
    }

    // Empty assistant turn: still emit a zero-length text so the wire format
    // is well-formed (some providers reject empty content arrays).
    if parts.is_empty() {
        return ChatMessage::assistant(MessageContent::from_text(String::new()));
    }

    if let [ContentPart::Text(text)] = parts.as_slice() {
        return ChatMessage::assistant(MessageContent::from_text(text.clone()));
    }

    ChatMessage::assistant(MessageContent::from_parts(parts))
}

fn tool_result_to_chat_message(msg: &ToolResultMessage) -> ChatMessage {
    // Flatten text parts; tool results in the grain transcript are typically
    // a single text segment. Image / binary tool responses aren't part of any
    // current provider's spec — drop them rather than guess.
    let text = msg
        .content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text(t) => Some(t.text.as_str()),
            UserContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    // genai 0.5.3's `ToolResponse` does not carry the function name; results are
    // correlated by `call_id`. `tool_name` on our `ToolResultMessage` is dropped
    // at this boundary (still preserved in the grain transcript for our records).
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
