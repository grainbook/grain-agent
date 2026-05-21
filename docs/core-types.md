# `grain_agent_core::types`

Data model used across the agent runtime: messages, tools, events, context, state snapshots. Wire format matches the TS reference (`packages/agent/src/types.ts`) — serde defaults to `camelCase`.

中文版：[zh/core-types.md](./zh/core-types.md).

## Content blocks

```rust
pub struct TextContent     { pub text: String }
pub struct ImageContent    { pub data: String, pub mime_type: String }
pub struct ThinkingContent {
    pub thinking: String,
    pub signature: Option<String>,
    pub provider_metadata: Option<serde_json::Value>,
}
pub struct ToolCall        { pub id: String, pub name: String, pub arguments: serde_json::Value }
```

Two role-bound enums:

- `AssistantContent` — what an assistant can emit: `Text` / `Image` / `Thinking` / `ToolCall`.
- `UserContent` — what user or tool-result messages carry: `Text` / `Image`. Helper: `UserContent::text("...")`.

## Messages

```rust
pub enum Message {            // LLM-facing
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

pub enum AgentMessage {        // transcript-facing
    Standard(Message),
    Custom(serde_json::Value),
}
```

Key invariant: `AgentMessage` is what the transcript stores; `Message` is what the LLM sees. `Custom` holds any app-level JSON and must include a `role` discriminator (used by `AgentMessage::role()`). `Custom` doesn't reach the LLM unless your `ConvertToLlmFn` maps it (see [harness-messages.md](./harness-messages.md)).

Constructors:

```rust
AgentMessage::user(user_msg);
AgentMessage::assistant(asst_msg);
AgentMessage::tool_result(trm);

if let Some(asst) = agent_msg.as_assistant() { ... }
```

## Usage / cost / model

```rust
pub struct Cost  { input, output, cache_read, cache_write, total: f64 }
pub struct Usage { input, output, cache_read, cache_write, total_tokens: u64, cost: Cost }

pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    pub context_window: u64,
    pub max_tokens: u64,
    pub cost: Cost,
}
Model::unknown()
```

## Enums

```rust
pub enum StopReason       { Stop, ToolUse, Length, Error, Aborted, Refused }
pub enum ThinkingLevel    { Off, Minimal, Low, Medium, High, XHigh }     // Default = Off
pub enum ToolExecutionMode { Sequential, Parallel }                       // Default = Parallel
pub enum QueueMode        { All, OneAtATime }                             // Default = OneAtATime
```

## Tools

```rust
use async_trait::async_trait;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use tokio_util::sync::CancellationToken;

struct EchoTool { def: ToolDefinition }

#[async_trait]
impl AgentTool for EchoTool {
    fn definition(&self) -> &ToolDefinition { &self.def }

    async fn execute(
        &self,
        _id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let v = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
        Ok(AgentToolResult {
            content: vec![UserContent::text(v)],
            details: serde_json::json!({}),
            terminate: None,
        })
    }
}
```

`ToolDefinition.parameters` is JSON Schema — forwarded verbatim to the LLM. `execution_mode = None` follows the global setting; `Some(Sequential)` forces the whole batch to serial.

Hooks (optional): `prepare_arguments` (pre-validation transform), `validate_arguments` (schema check; default accepts anything).

`AgentToolResult`:

```rust
AgentToolResult::text("...")
AgentToolResult::error("...")               // same shape; is_error decided by caller
AgentToolResult {
    content: vec![UserContent::text("hi")],
    details: serde_json::json!({}),
    terminate: Some(true),                  // whole batch terminate=true → loop ends
}
```

`AgentToolError`: `Message(String)` / `Aborted` / `Validation(String)`. Helper: `AgentToolError::msg("…")`.

`ToolUpdateCallback` is `Arc<dyn Fn(AgentToolResult) + Send + Sync>` — call it from inside `execute` to emit `AgentEvent::ToolExecutionUpdate`s while running.

## Streaming events

`AssistantMessageEvent` (snake_case-tagged):

| Variant | When |
|---------|------|
| `Start { partial }` | New assistant message |
| `TextStart` / `TextDelta` / `TextEnd` | Text block |
| `ThinkingStart` / `ThinkingDelta` / `ThinkingEnd` | Thinking block |
| `ToolcallStart` / `ToolcallDelta` / `ToolcallEnd` | Tool-call block |
| `Done { result }` | Terminal success |
| `Error { error, result }` | Terminal failure |

Helpers: `event.partial()`, `event.is_terminal()`.

## Agent-level events

```rust
pub enum AgentEvent {
    AgentStart,
    AgentEnd { messages: Vec<AgentMessage> },
    TurnStart,
    TurnEnd { message: AssistantMessage, tool_results: Vec<ToolResultMessage> },
    MessageStart { message: AgentMessage },
    MessageUpdate { message: AssistantMessage, assistant_message_event: AssistantMessageEvent },
    MessageEnd { message: AgentMessage },
    ToolExecutionStart  { tool_call_id, tool_name, args },
    ToolExecutionUpdate { tool_call_id, tool_name, args, partial_result },
    ToolExecutionEnd    { tool_call_id, tool_name, result, is_error },
}
```

Events are emitted by [`agent_loop`](./core-agent-loop.md) through an `EventSink` in order; [`Agent`](./core-agent.md) further broadcasts to `subscribe`d listeners.

## Context / state

```rust
pub struct LlmContext {                // sent to the LLM
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

pub struct AgentContext {              // sent into agent_loop
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

pub struct AgentState {                // returned by Agent::state()
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
```

`AgentState` and `AgentContext` both implement `Debug` with tools rendered as a `name` list.
