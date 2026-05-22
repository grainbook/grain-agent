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

use std::collections::HashSet;

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
///
/// History entries whose tool-call arguments arrived as a malformed
/// string (truncated JSON from a streaming chunk that the inbound
/// normalizer couldn't unwrap) are dropped before the request goes
/// out — provider would reject the whole turn with
/// `tool_calls[].function.arguments must decode to a JSON object,
/// got str`. The corresponding tool_result entries are dropped
/// alongside them so we don't leave orphans.
pub fn to_chat_request(ctx: &LlmContext) -> ChatRequest {
    let mut chat = ChatRequest::new(Vec::with_capacity(ctx.messages.len()));

    if !ctx.system_prompt.is_empty() {
        chat = chat.with_system(ctx.system_prompt.clone());
    }

    // `corrupt_ids` are silently dropped — this scan runs on every
    // request and writing to stderr concurrently with a TUI alt
    // screen corrupts the display. The retry-on-overflow notifier
    // already surfaces a coarse "retrying" signal to the host; this
    // finer-grained "we dropped a malformed tool_call" stays in the
    // implementation log (visible via `RUST_LOG=debug` in headless
    // mode).
    let corrupt_ids = collect_corrupt_tool_call_ids(&ctx.messages);

    for msg in &ctx.messages {
        let cm = match msg {
            Message::User(u) => Some(user_to_chat_message(u)),
            Message::Assistant(a) => assistant_to_chat_message(a, &corrupt_ids),
            Message::ToolResult(t) if corrupt_ids.contains(&t.tool_call_id) => None,
            Message::ToolResult(t) => Some(tool_result_to_chat_message(t)),
        };
        if let Some(cm) = cm {
            chat = chat.append_message(cm);
        }
    }

    let tools: Vec<Tool> = ctx.tools.iter().map(tool_def_to_genai).collect();
    if !tools.is_empty() {
        chat = chat.with_tools(tools);
    }

    chat
}

/// Scan the transcript and return the IDs of tool_calls whose
/// `arguments` is a string that doesn't decode to a JSON object. These
/// entries can't be sent to the provider as-is.
fn collect_corrupt_tool_call_ids(messages: &[Message]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for msg in messages {
        if let Message::Assistant(a) = msg {
            for c in &a.content {
                if let AssistantContent::ToolCall(tc) = c
                    && is_tool_args_corrupt(&tc.arguments)
                {
                    ids.insert(tc.id.clone());
                }
            }
        }
    }
    ids
}

/// `arguments` is "corrupt" if it's a `Value::String` whose content
/// neither parses as JSON nor parses to an object. Non-string values
/// pass through here (object/array/scalar) — providers may still
/// reject array/scalar but that's a separate concern; we only guard
/// the specific "got str" failure mode here.
fn is_tool_args_corrupt(args: &serde_json::Value) -> bool {
    if let serde_json::Value::String(s) = args {
        !matches!(
            serde_json::from_str::<serde_json::Value>(s),
            Ok(v) if v.is_object()
        )
    } else {
        false
    }
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

fn assistant_to_chat_message(
    msg: &AssistantMessage,
    corrupt_tool_call_ids: &HashSet<String>,
) -> Option<ChatMessage> {
    let mut parts: Vec<ContentPart> = Vec::with_capacity(msg.content.len());
    let mut signatures: Vec<String> = Vec::new();

    for c in &msg.content {
        match c {
            AssistantContent::Text(t) => parts.push(ContentPart::Text(t.text.clone())),
            AssistantContent::ToolCall(tc) if corrupt_tool_call_ids.contains(&tc.id) => {
                // Drop — its tool_result is being dropped alongside in
                // `to_chat_request`, so no orphan.
            }
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

    // If every content part was a corrupt tool_call we'd drop the
    // whole message — an empty assistant turn confuses providers and
    // there's nothing useful to send anyway.
    if parts.is_empty() {
        return None;
    }
    let content = if let [ContentPart::Text(text)] = parts.as_slice() {
        MessageContent::from_text(text.clone())
    } else {
        MessageContent::from_parts(parts)
    };

    Some(ChatMessage::assistant(content))
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

    #[test]
    fn is_tool_args_corrupt_flags_unparseable_strings() {
        // Truncated JSON — what an interrupted streaming chunk leaves.
        assert!(is_tool_args_corrupt(&json!(r#"{"path": "/foo", "old": "abc"#)));
        // Valid JSON but not an object.
        assert!(is_tool_args_corrupt(&json!(r#""just a value""#)));
        // Plain object — fine.
        assert!(!is_tool_args_corrupt(&json!({"path": "/foo"})));
        // String encoding of an object — fine (normalize_outbound unwraps).
        assert!(!is_tool_args_corrupt(&json!(r#"{"path": "/foo"}"#)));
    }

    #[test]
    fn collect_corrupt_tool_call_ids_picks_up_only_broken_calls() {
        use grain_agent_core::{
            AssistantMessage, StopReason, ToolCall as GrainToolCall, Usage,
        };
        let good = Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::ToolCall(GrainToolCall {
                id: "call_good".into(),
                name: "read".into(),
                arguments: json!({"path": "/x"}),
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        });
        let bad = Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::ToolCall(GrainToolCall {
                id: "call_bad".into(),
                name: "edit".into(),
                // Truncated JSON, mimicking the kimi failure observed
                // in the wild on a 616-message resume.
                arguments: json!(r#"{"path": "/x", "old": "abc"#),
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        });
        let ids = collect_corrupt_tool_call_ids(&[good, bad]);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains("call_bad"));
    }
}
