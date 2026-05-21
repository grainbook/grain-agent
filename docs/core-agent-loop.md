# `grain_agent_core::agent_loop`

Low-level agent loop. All event routing, tool scheduling, steering / follow-up queue draining, and prepare-next-turn / should-stop-after-turn hooks live here. Most apps should use the more ergonomic [`Agent`](./core-agent.md) wrapper; this layer is for callers that need full lifecycle control.

中文版：[zh/core-agent-loop.md](./zh/core-agent-loop.md).

## Entry points

```rust
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: EventSink,
    cancel: CancellationToken,
    stream_fn: StreamFn,
) -> Result<Vec<AgentMessage>, AgentLoopError>;

pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: EventSink,
    cancel: CancellationToken,
    stream_fn: StreamFn,
) -> Result<Vec<AgentMessage>, AgentLoopError>;
```

- `run_agent_loop` starts a fresh loop with a batch of prompts. Emits `AgentStart` + `TurnStart` plus `MessageStart` / `MessageEnd` for each prompt up front.
- `run_agent_loop_continue` continues an existing transcript. **Preconditions**: `context.messages` non-empty, and the last message must not be `assistant` (otherwise the provider rejects it). Returns `AgentLoopError::Other` if either is violated.

Both return *new* messages produced during this loop call (not the full transcript).

## `AgentLoopConfig`

```rust
pub struct AgentLoopConfig {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
    pub tool_execution: ToolExecutionMode,

    pub convert_to_llm: ConvertToLlmFn,                  // required
    pub transform_context: Option<TransformContextFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub get_steering_messages: Option<MessagesProviderFn>,
    pub get_follow_up_messages: Option<MessagesProviderFn>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
}
```

Convenience constructor:

```rust
let mut config = AgentLoopConfig::new(model, convert_to_llm);
config.thinking_level = ThinkingLevel::Medium;
config.tool_execution = ToolExecutionMode::Parallel;
```

## Hook signatures

```rust
pub type ConvertToLlmFn = Arc<
    dyn Fn(Vec<AgentMessage>) -> BoxFuture<'static, Vec<Message>> + Send + Sync,
>;

pub type TransformContextFn = Arc<
    dyn Fn(Vec<AgentMessage>, CancellationToken)
            -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync,
>;

pub type GetApiKeyFn =
    Arc<dyn Fn(String /* provider */) -> BoxFuture<'static, Option<String>> + Send + Sync>;

pub type MessagesProviderFn =
    Arc<dyn Fn() -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync>;

pub type BeforeToolCallFn = Arc<
    dyn Fn(BeforeToolCallContext, CancellationToken)
            -> BoxFuture<'static, Option<BeforeToolCallResult>> + Send + Sync,
>;

pub type AfterToolCallFn = Arc<
    dyn Fn(AfterToolCallContext, CancellationToken)
            -> BoxFuture<'static, Option<AfterToolCallResult>> + Send + Sync,
>;

pub type PrepareNextTurnFn = Arc<
    dyn Fn(PrepareNextTurnContext) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>>
        + Send + Sync,
>;

pub type ShouldStopAfterTurnFn = Arc<
    dyn Fn(ShouldStopAfterTurnContext) -> BoxFuture<'static, bool> + Send + Sync,
>;
```

Call order per turn (see "Lifecycle"):

1. `get_steering_messages()` injects user / tool-result messages into the transcript.
2. `transform_context` → `convert_to_llm` → `get_api_key` → `stream_fn.stream`.
3. Stream events flow into `emit`; on terminal event we have an `AssistantMessage`.
4. For each `ToolCall`: `tool.prepare_arguments` → `tool.validate_arguments` → `before_tool_call` → `tool.execute` → `after_tool_call`.
5. `TurnEnd` emitted.
6. `prepare_next_turn` can swap `context` / `model` / `thinking_level` for the next turn.
7. `should_stop_after_turn` → `true` ends the loop with `AgentEnd`.
8. Pull steering messages again; when empty and no more tool calls, pull `get_follow_up_messages` once; if both empty → `AgentEnd`.

### Before / After hooks

```rust
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub args: serde_json::Value,
    pub context: Arc<AgentContext>,
}

pub struct BeforeToolCallResult { pub block: bool, pub reason: Option<String> }

pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub args: serde_json::Value,
    pub result: AgentToolResult,
    pub is_error: bool,
    pub context: Arc<AgentContext>,
}

pub struct AfterToolCallResult {
    pub content: Option<Vec<UserContent>>,
    pub details: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}
```

- `before_tool_call` returning `Some({ block: true, .. })` converts the call into an `is_error = true` tool result (default reason `"Tool execution was blocked"`); `tool.execute` is skipped.
- `after_tool_call` can rewrite the result (mask, compress, override `terminate`, etc).

### Terminating the loop from a tool

Make every tool in a batch return `AgentToolResult.terminate = Some(true)`; the loop won't fire another assistant turn.

### `PrepareNextTurnFn` / `ShouldStopAfterTurnFn`

```rust
pub struct ShouldStopAfterTurnContext {  // PrepareNextTurnContext = alias
    pub message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub context: Arc<AgentContext>,
    pub new_messages: Vec<AgentMessage>,
}

pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}
```

`should_stop_after_turn = true` exits cleanly, emits `AgentEnd`, no extra `TurnEnd`.

## Tool execution modes

- Default `ToolExecutionMode::Parallel`: concurrent execution, **source order preserved** in the resulting `ToolResultMessage[]` (via `FuturesOrdered`).
- If any tool's `definition().execution_mode == Some(Sequential)`, the **whole batch** degrades to serial. Same if `config.tool_execution = Sequential`.
- In serial mode, an active `cancel.is_cancelled()` skips remaining tools.

## Errors and terminals

- `stream_fn.stream(...).Err(_)`: loop synthesizes a placeholder `StopReason::Error` assistant message, emits `MessageStart` + `MessageEnd`, then normal `TurnEnd` / `AgentEnd`.
- Stream ends without terminal event: synthesizes `StopReason::Error` with `error_message = "stream ended without terminal event"`.
- Tool not found: immediate `is_error = true` tool result with content `"Tool {name} not found"`.
- `cancel` fires mid-execution: in-progress tool calls return `is_error = true, "Operation aborted"`; serial mode exits immediately.

## `EventSink`

```rust
pub type EventSink = Arc<dyn Fn(AgentEvent) -> BoxFuture<'static, ()> + Send + Sync>;
```

Each event is awaited serially in emission order. Heavy downstream fan-out should `tokio::spawn` inside the sink.

`ToolExecutionUpdate` is fired by tools' `on_update` callback as fire-and-forget — internally `tokio::spawn(emit(event))`, so the tool body isn't blocked. If your sink needs strict ordering relative to other events, add your own sequencing.

## Using directly

```rust
use std::sync::Arc;
use futures::future::BoxFuture;
use grain_agent_core::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, ConvertToLlmFn, Message,
    StreamFn, run_agent_loop,
};
use tokio_util::sync::CancellationToken;

let convert_to_llm: ConvertToLlmFn = Arc::new(|msgs: Vec<AgentMessage>| {
    Box::pin(async move {
        msgs.into_iter()
            .filter_map(|m| match m {
                AgentMessage::Standard(m) => Some(m),
                AgentMessage::Custom(_) => None,
            })
            .collect::<Vec<Message>>()
    })
});

let config = AgentLoopConfig::new(model.clone(), convert_to_llm);
let emit = Arc::new(|event: AgentEvent| -> BoxFuture<'static, ()> {
    Box::pin(async move { println!("event: {event:?}"); })
});
let cancel = CancellationToken::new();

let new_msgs = run_agent_loop(
    vec![/* initial prompts */],
    AgentContext { system_prompt: "you are helpful".into(), messages: vec![], tools: vec![] },
    config,
    emit,
    cancel,
    stream_fn,
).await?;
```

`AgentLoopError` currently has only `Other(String)`, used for hard pre-condition failures.
