//! Inbound: turn `genai::chat::ChatStreamEvent` into
//! `grain_agent_core::AssistantMessageEvent`.
//!
//! Modeled as a pure state machine: each genai event mutates the partial
//! [`AssistantMessage`] and returns zero or more grain events. No I/O.
//! Tested in isolation by feeding a hand-crafted sequence of events
//! (see `tests/inbound.rs`).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use genai::chat::{ChatStreamEvent, StreamEnd, ToolCall as GenaiToolCall};
use grain_agent_core::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Model, StopReason, TextContent,
    ThinkingContent, ToolCall as GrainToolCall, Usage,
};

use crate::mapping::usage::map_usage;

/// Streaming state for one assistant turn.
///
/// The state machine emits well-formed grain events (matching the contract in
/// `grain-agent-core::stream`): exactly one [`AssistantMessageEvent::Start`]
/// followed by Text/Thinking/Toolcall block events, terminated by exactly one
/// [`AssistantMessageEvent::Done`] or [`AssistantMessageEvent::Error`].
///
/// **Design note (M-3)**: `OpenBlock` only tracks `Text` and `Thinking`
/// blocks. Tool calls arrive from genai 0.5 as complete `ToolCallChunk`s
/// (one chunk per fully assembled call), so the state machine emits
/// paired `ToolcallStart` + `ToolcallEnd` immediately without holding the
/// block open. If a future genai release exposes streaming tool-call
/// argument deltas, we'd add an `OpenBlock::ToolCall { index }` variant
/// and route `ToolCallDelta` through it.
pub struct InboundState {
    base: AssistantMessage,
    blocks: Vec<AssistantContent>,
    open: Option<OpenBlock>,
    started: bool,
    /// Map from provider's `tool_call_id` to the index in `blocks` that
    /// holds the in-flight ToolCall block. genai 0.6 streams tool-call
    /// arguments as **cumulative** chunks — each subsequent chunk carries
    /// the latest accumulated JSON for the same call_id. Without this
    /// map, the old "push a new block every chunk" behavior produced N
    /// duplicate ToolCall blocks for one call, which the agent loop
    /// would then execute N times and which provider validation
    /// (DeepSeek's 400, etc.) rejects as duplicate tool_call_ids.
    tool_call_indices: HashMap<String, usize>,
}

#[derive(Debug)]
enum OpenBlock {
    Text { index: usize },
    Thinking { index: usize },
}

impl InboundState {
    /// Initialize using `model` to populate `api` / `provider` / `model`
    /// fields on the partial [`AssistantMessage`].
    pub fn new(model: &Model) -> Self {
        InboundState {
            base: empty_assistant(model),
            blocks: Vec::new(),
            open: None,
            started: false,
            tool_call_indices: HashMap::new(),
        }
    }

    fn partial(&self) -> AssistantMessage {
        let mut m = self.base.clone();
        m.content.clone_from(&self.blocks);
        m
    }

    /// Dispatch a single genai event. May produce 0, 1, or 2+ grain events
    /// in emission order (e.g. a text → tool-call transition closes the open
    /// text block then opens the tool-call block).
    pub fn on_event(&mut self, event: ChatStreamEvent) -> Vec<AssistantMessageEvent> {
        match event {
            ChatStreamEvent::Start => self.on_start(),
            ChatStreamEvent::Chunk(c) => self.on_text_chunk(c.content),
            ChatStreamEvent::ReasoningChunk(c) => self.on_reasoning_chunk(c.content),
            ChatStreamEvent::ThoughtSignatureChunk(c) => self.on_thought_signature(c.content),
            ChatStreamEvent::ToolCallChunk(t) => self.on_tool_call(t.tool_call),
            ChatStreamEvent::End(e) => self.on_end(e),
        }
    }

    fn on_start(&mut self) -> Vec<AssistantMessageEvent> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![AssistantMessageEvent::Start {
            partial: self.partial(),
        }]
    }

    fn ensure_started(&mut self, out: &mut Vec<AssistantMessageEvent>) {
        if !self.started {
            self.started = true;
            out.push(AssistantMessageEvent::Start {
                partial: self.partial(),
            });
        }
    }

    fn on_text_chunk(&mut self, content: String) -> Vec<AssistantMessageEvent> {
        let mut out = Vec::new();
        self.ensure_started(&mut out);
        // Close mismatched open block.
        if matches!(self.open, Some(OpenBlock::Thinking { .. })) {
            self.close_open(&mut out);
        }
        if self.open.is_none() {
            self.blocks
                .push(AssistantContent::Text(TextContent::default()));
            let idx = self.blocks.len() - 1;
            self.open = Some(OpenBlock::Text { index: idx });
            out.push(AssistantMessageEvent::TextStart {
                partial: self.partial(),
                content_index: idx,
            });
        }
        if let Some(OpenBlock::Text { index }) = self.open {
            if let AssistantContent::Text(t) = &mut self.blocks[index] {
                t.text.push_str(&content);
            }
            out.push(AssistantMessageEvent::TextDelta {
                partial: self.partial(),
                content_index: index,
                delta: content,
            });
        }
        out
    }

    fn on_reasoning_chunk(&mut self, content: String) -> Vec<AssistantMessageEvent> {
        let mut out = Vec::new();
        self.ensure_started(&mut out);
        if matches!(self.open, Some(OpenBlock::Text { .. })) {
            self.close_open(&mut out);
        }
        if self.open.is_none() {
            self.blocks
                .push(AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    signature: None,
                    provider_metadata: None,
                }));
            let idx = self.blocks.len() - 1;
            self.open = Some(OpenBlock::Thinking { index: idx });
            out.push(AssistantMessageEvent::ThinkingStart {
                partial: self.partial(),
                content_index: idx,
            });
        }
        if let Some(OpenBlock::Thinking { index }) = self.open {
            if let AssistantContent::Thinking(t) = &mut self.blocks[index] {
                t.thinking.push_str(&content);
            }
            out.push(AssistantMessageEvent::ThinkingDelta {
                partial: self.partial(),
                content_index: index,
                delta: content,
            });
        }
        out
    }

    fn on_thought_signature(&mut self, content: String) -> Vec<AssistantMessageEvent> {
        // Anthropic-style signed thinking: silently update the open thinking
        // block's `signature`. No separate grain event — subscribers see the
        // updated signature on the next partial.
        if let Some(OpenBlock::Thinking { index }) = self.open
            && let AssistantContent::Thinking(t) = &mut self.blocks[index]
        {
            match &mut t.signature {
                Some(existing) => existing.push_str(&content),
                None => t.signature = Some(content),
            }
        }
        Vec::new()
    }

    fn on_tool_call(&mut self, tc: GenaiToolCall) -> Vec<AssistantMessageEvent> {
        let mut out = Vec::new();
        self.ensure_started(&mut out);

        // genai 0.6 streams cumulative arguments: each ToolCallChunk for
        // the same call_id carries the latest accumulated JSON. If we've
        // already seen this id this turn, overwrite the existing block's
        // arguments instead of appending a duplicate. The cumulative
        // semantics mean we always end up with the final, complete JSON.
        let arguments = normalize_tool_args(tc.fn_arguments);
        if let Some(&idx) = self.tool_call_indices.get(&tc.call_id) {
            if let AssistantContent::ToolCall(existing) = &mut self.blocks[idx] {
                existing.arguments = arguments;
                // No event — subscribers already saw a Toolcall{Start,End}
                // for this id; they can refresh from the partial when
                // they care about the latest args.
            }
            return out;
        }

        if self.open.is_some() {
            self.close_open(&mut out);
        }
        let grain_tc = GrainToolCall {
            id: tc.call_id.clone(),
            name: tc.fn_name,
            arguments,
        };
        self.blocks.push(AssistantContent::ToolCall(grain_tc));
        let idx = self.blocks.len() - 1;
        self.tool_call_indices.insert(tc.call_id, idx);
        out.push(AssistantMessageEvent::ToolcallStart {
            partial: self.partial(),
            content_index: idx,
        });
        // Emit a paired End immediately for the *first* chunk of this id so
        // subscribers that only watch Start/End boundaries see a
        // well-formed sequence; subsequent argument refinements update
        // the block silently. The agent loop only executes tool calls
        // from the final AssistantMessage on `Done`, so it always picks
        // up the fully-accumulated args.
        out.push(AssistantMessageEvent::ToolcallEnd {
            partial: self.partial(),
            content_index: idx,
        });
        out
    }

    fn on_end(&mut self, end: StreamEnd) -> Vec<AssistantMessageEvent> {
        let mut out = Vec::new();
        self.ensure_started(&mut out);
        if self.open.is_some() {
            self.close_open(&mut out);
        }

        let mut result = self.base.clone();
        result.content = std::mem::take(&mut self.blocks);
        if let Some(u) = end.captured_usage {
            result.usage = map_usage(u);
        }
        result.stop_reason = infer_stop_reason(&result.content);
        result.timestamp = now_ms();

        out.push(AssistantMessageEvent::Done { result });
        out
    }

    /// Consume self and emit a terminal aborted error event.
    pub fn into_aborted(mut self) -> AssistantMessageEvent {
        let mut result = self.base.clone();
        result.content = std::mem::take(&mut self.blocks);
        result.stop_reason = StopReason::Aborted;
        result.error_message = Some("aborted".into());
        result.timestamp = now_ms();
        AssistantMessageEvent::Error {
            error: "aborted".into(),
            result,
        }
    }

    /// Consume self and emit a terminal error event with the given message.
    pub fn into_error_msg(mut self, msg: impl Into<String>) -> AssistantMessageEvent {
        let msg = msg.into();
        let mut result = self.base.clone();
        result.content = std::mem::take(&mut self.blocks);
        result.stop_reason = StopReason::Error;
        result.error_message = Some(msg.clone());
        result.timestamp = now_ms();
        AssistantMessageEvent::Error { error: msg, result }
    }

    fn close_open(&mut self, out: &mut Vec<AssistantMessageEvent>) {
        let Some(open) = self.open.take() else { return };
        match open {
            OpenBlock::Text { index } => out.push(AssistantMessageEvent::TextEnd {
                partial: self.partial(),
                content_index: index,
            }),
            OpenBlock::Thinking { index } => out.push(AssistantMessageEvent::ThinkingEnd {
                partial: self.partial(),
                content_index: index,
            }),
        }
    }
}

fn infer_stop_reason(content: &[AssistantContent]) -> StopReason {
    if content
        .iter()
        .any(|c| matches!(c, AssistantContent::ToolCall(_)))
    {
        StopReason::ToolUse
    } else {
        StopReason::Stop
    }
}

fn empty_assistant(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Normalize a `tool_call.fn_arguments` value to a JSON object.
///
/// genai 0.6 sometimes delivers `fn_arguments` as a `Value::String` whose
/// content is a JSON-encoded object (cumulative streaming sends partial
/// JSON as a string until the call is complete). Our tools then run
/// `serde_json::from_value::<ToolArgs>(args)` and fail with
/// `invalid type: string "...", expected struct ToolArgs`. Re-parse the
/// inner string when we see it; pass through everything else unchanged.
fn normalize_tool_args(raw: serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::String(s) = &raw
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s)
    {
        return parsed;
    }
    raw
}
