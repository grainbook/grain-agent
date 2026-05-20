//! Core message, tool, event, and state types.
//!
//! Ports `packages/agent/src/types.ts` from the reference TypeScript implementation,
//! plus the message/model primitives that live in `@earendil-works/pi-ai`.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TextContent {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThinkingContent {
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Content blocks legal in an assistant message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantContent {
    Text(TextContent),
    Image(ImageContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

/// Content blocks legal in user or tool-result messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserContent {
    Text(TextContent),
    Image(ImageContent),
}

impl UserContent {
    pub fn text(s: impl Into<String>) -> Self {
        UserContent::Text(TextContent { text: s.into() })
    }
}

// ---------------------------------------------------------------------------
// Usage / cost / model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Cost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default, rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(default, rename = "cacheWrite")]
    pub cache_write: f64,
    #[serde(default)]
    pub total: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cost: Cost,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub max_tokens: u64,
    #[serde(default)]
    pub cost: Cost,
}

impl Model {
    pub fn unknown() -> Self {
        Model {
            id: "unknown".into(),
            name: "unknown".into(),
            api: "unknown".into(),
            provider: "unknown".into(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Stop reasons, thinking levels, queue / execution modes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    ToolUse,
    Length,
    Error,
    Aborted,
    Refused,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: Vec<UserContent>,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<AssistantContent>,
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<UserContent>,
    #[serde(default)]
    pub details: serde_json::Value,
    pub is_error: bool,
    pub timestamp: i64,
}

/// Standard LLM message accepted by `convertToLlm`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn role(&self) -> &'static str {
        match self {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        }
    }
}

/// Agent transcript message: a standard LLM message or an opaque custom payload.
///
/// Custom variants mirror the TypeScript `CustomAgentMessages` extension point:
/// applications stash app-specific records here and filter / convert them in
/// `convertToLlm` before reaching the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AgentMessage {
    Standard(Message),
    Custom(serde_json::Value),
}

impl AgentMessage {
    pub fn user(message: UserMessage) -> Self {
        AgentMessage::Standard(Message::User(message))
    }
    pub fn assistant(message: AssistantMessage) -> Self {
        AgentMessage::Standard(Message::Assistant(message))
    }
    pub fn tool_result(message: ToolResultMessage) -> Self {
        AgentMessage::Standard(Message::ToolResult(message))
    }

    pub fn role(&self) -> &str {
        match self {
            AgentMessage::Standard(m) => m.role(),
            AgentMessage::Custom(v) => v.get("role").and_then(|r| r.as_str()).unwrap_or("custom"),
        }
    }

    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            AgentMessage::Standard(Message::Assistant(m)) => Some(m),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tool result + tools
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolResult {
    pub content: Vec<UserContent>,
    #[serde(default)]
    pub details: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

impl AgentToolResult {
    pub fn text(message: impl Into<String>) -> Self {
        AgentToolResult {
            content: vec![UserContent::text(message)],
            details: serde_json::Value::Object(Default::default()),
            terminate: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        AgentToolResult::text(message)
    }
}

pub type ToolUpdateCallback = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

/// Serializable description of a tool, suitable for forwarding to an LLM provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub label: String,
    pub description: String,
    /// JSON Schema (typebox-equivalent) describing the parameters.
    #[serde(default)]
    pub parameters: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_mode: Option<ToolExecutionMode>,
}

/// Trait implemented by agent tools.
#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    fn definition(&self) -> &ToolDefinition;

    /// Pre-processes raw tool-call arguments before schema validation.
    /// Defaults to identity.
    fn prepare_arguments(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, AgentToolError> {
        Ok(args)
    }

    /// Validates arguments against the tool schema. Default impl accepts anything.
    /// Implementations can use the JSON value in `definition().parameters`.
    fn validate_arguments(&self, args: &serde_json::Value) -> Result<(), AgentToolError> {
        let _ = args;
        Ok(())
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError>;
}

impl dyn AgentTool {
    pub fn name(&self) -> &str {
        &self.definition().name
    }

    pub fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.definition().execution_mode
    }
}

impl fmt::Debug for dyn AgentTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentTool")
            .field("definition", self.definition())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentToolError {
    #[error("{0}")]
    Message(String),
    #[error("operation aborted")]
    Aborted,
    #[error("validation failed: {0}")]
    Validation(String),
}

impl AgentToolError {
    pub fn msg(s: impl Into<String>) -> Self {
        AgentToolError::Message(s.into())
    }
}

// ---------------------------------------------------------------------------
// Streaming events
// ---------------------------------------------------------------------------

/// Streaming events emitted while an assistant message is being produced.
///
/// Mirrors the event stream returned by `streamSimple` in `@earendil-works/pi-ai`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantMessageEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        partial: AssistantMessage,
        content_index: usize,
    },
    TextDelta {
        partial: AssistantMessage,
        content_index: usize,
        delta: String,
    },
    TextEnd {
        partial: AssistantMessage,
        content_index: usize,
    },
    ThinkingStart {
        partial: AssistantMessage,
        content_index: usize,
    },
    ThinkingDelta {
        partial: AssistantMessage,
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        partial: AssistantMessage,
        content_index: usize,
    },
    ToolcallStart {
        partial: AssistantMessage,
        content_index: usize,
    },
    ToolcallDelta {
        partial: AssistantMessage,
        content_index: usize,
        delta: String,
    },
    ToolcallEnd {
        partial: AssistantMessage,
        content_index: usize,
    },
    Done {
        result: AssistantMessage,
    },
    Error {
        error: String,
        result: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    pub fn partial(&self) -> Option<&AssistantMessage> {
        match self {
            AssistantMessageEvent::Start { partial }
            | AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolcallStart { partial, .. }
            | AssistantMessageEvent::ToolcallDelta { partial, .. }
            | AssistantMessageEvent::ToolcallEnd { partial, .. } => Some(partial),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        )
    }
}

// ---------------------------------------------------------------------------
// Agent-level events
// ---------------------------------------------------------------------------

/// Top-level events emitted by the agent loop.
///
/// `MessageUpdate` carries the largest payload (a full [`AssistantMessageEvent`]),
/// but events are emitted at low rate and frequently cloned by listeners, so the
/// variant size is acceptable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnStart,
    TurnEnd {
        message: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: AssistantMessage,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
        partial_result: AgentToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
}

// ---------------------------------------------------------------------------
// Context snapshots + state
// ---------------------------------------------------------------------------

/// LLM-facing context: filtered transcript ready to send to the model.
#[derive(Debug, Default, Clone)]
pub struct LlmContext {
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

/// Agent-facing context snapshot passed into the low-level loop.
#[derive(Default, Clone)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

impl fmt::Debug for AgentContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentContext")
            .field("system_prompt", &self.system_prompt)
            .field("messages", &self.messages)
            .field(
                "tools",
                &self
                    .tools
                    .iter()
                    .map(|t| t.definition().name.clone())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Public snapshot of agent state.
#[derive(Clone)]
pub struct AgentState {
    pub system_prompt: String,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<AgentMessage>,
    pub is_streaming: bool,
    pub streaming_message: Option<AgentMessage>,
    pub pending_tool_calls: HashSet<String>,
    pub error_message: Option<String>,
}

impl fmt::Debug for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentState")
            .field("system_prompt", &self.system_prompt)
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field("tools", &self.tools.len())
            .field("messages", &self.messages.len())
            .field("is_streaming", &self.is_streaming)
            .field("streaming_message", &self.streaming_message.is_some())
            .field("pending_tool_calls", &self.pending_tool_calls)
            .field("error_message", &self.error_message)
            .finish()
    }
}
