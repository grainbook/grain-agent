# `grain_agent_core::agent`

`Agent` is the high-level wrapper around [`agent_loop`](./core-agent-loop.md): owns mutable state (messages, tools, model, subscribers), enforces single-active-run, supports steering / follow-up queues, event subscription, and cancellation.

Corresponds to `packages/agent/src/agent.ts` in the TypeScript reference.

中文版：[zh/core-agent.md](./zh/core-agent.md).

## Construction

```rust
use std::sync::Arc;
use grain_agent_core::{Agent, AgentOptions, Model, QueueMode, ThinkingLevel, ToolExecutionMode};

let mut opts = AgentOptions::new(model, stream_fn);
opts.system_prompt = "you are helpful".into();
opts.thinking_level = ThinkingLevel::Medium;
opts.tools = vec![Arc::new(MyTool::new())];
opts.steering_mode = QueueMode::OneAtATime;        // default
opts.follow_up_mode = QueueMode::OneAtATime;
opts.tool_execution = ToolExecutionMode::Parallel; // default
opts.session_id = Some("session-abc".into());

let agent = Agent::new(opts);
```

`AgentOptions::new(model, stream_fn)` covers required fields; everything else uses `Default`. Optional hooks all accept `None`:

```rust
pub struct AgentOptions {
    pub system_prompt: String,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<AgentMessage>,

    pub convert_to_llm: Option<ConvertToLlmFn>,        // None → drop all Custom
    pub transform_context: Option<TransformContextFn>,
    pub stream_fn: StreamFn,
    pub get_api_key: Option<GetApiKeyFn>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,

    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub session_id: Option<String>,
    pub transport: Option<String>,
    pub max_retry_delay_ms: Option<u64>,
    pub tool_execution: ToolExecutionMode,
}
```

To let `Custom` messages actually reach the LLM, supply your own `convert_to_llm` (see [`harness::convert_to_llm`](./harness-messages.md)).

## Running: `prompt` / `continue_`

```rust
agent.prompt_text("hi").await?;                  // one user-text shortcut
agent.prompt(vec![message_a, message_b]).await?; // multi-prompt batch
agent.continue_().await?;                        // continue without injecting
```

Errors:

- `AgentError::AlreadyRunning` — only one active loop allowed at a time.
- `AgentError::NoMessagesToContinue` — `continue_` called on empty transcript.
- `AgentError::CannotContinueFromAssistant` — `continue_` while last is an assistant AND both queues are empty. If queues are non-empty, their content is drained and used as the new prompt.

`prompt` / `prompt_text` **skip the initial steering poll** (so queued steering messages don't get prepended to the just-sent prompt); subsequent turns pull normally.

## State, subscriptions, abort

```rust
let state = agent.state().await;              // cloned AgentState snapshot

let unsub = agent.subscribe(Arc::new(move |event, cancel| {
    Box::pin(async move {
        println!("event: {event:?}");
    })
})).await;
// ...
unsub.cancel().await;

let signal = agent.signal().await;            // Option<CancellationToken>
agent.abort().await;                          // cancel the active loop
```

Subscriber concurrency: each event is awaited **serially** across all listeners before moving on. Heavy work inside a listener should `tokio::spawn`. `subscribe` uses index-based storage, so concurrent add+remove can invalidate your handle — avoid interleaving them.

`abort` triggers cancellation propagated to `stream_fn` and tool `execute`. `LlmStream` impls should respond by emitting a `StopReason::Aborted` terminal event.

## Runtime configuration

All setters are async (internal `Mutex`):

```rust
agent.set_system_prompt("...".into()).await;
agent.set_model(new_model).await;
agent.set_thinking_level(ThinkingLevel::High).await;
agent.set_tools(vec![Arc::new(MyTool::new())]).await;
agent.set_messages(restored_transcript).await;
agent.set_steering_mode(QueueMode::All).await;
agent.set_follow_up_mode(QueueMode::OneAtATime).await;
```

These only mutate `Inner`; running loops see the change only on the next `snapshot_context` / `build_loop_config`. To swap mid-loop, return `AgentLoopTurnUpdate { model: Some(_), .. }` from `prepare_next_turn`.

`agent.reset().await` clears transcript, streaming state, queues, and error — does NOT remove subscribers.

## Steering / follow-up

```rust
agent.steer(AgentMessage::user(user_msg)).await;       // injects at next turn start
agent.follow_up(AgentMessage::user(user_msg)).await;   // injected when loop would end

agent.has_queued_messages().await;
agent.clear_steering_queue().await;
agent.clear_follow_up_queue().await;
agent.clear_all_queues().await;
```

- `steering_queue` is consumed by `get_steering_messages` at the **start of each turn**.
- `follow_up_queue` is consumed only when the loop is about to end (no more tool calls, steering empty) — non-empty follow-ups extend the loop by one more turn.
- Drain behavior follows `QueueMode`: `OneAtATime` (default) pops the head; `All` drains everything.

## Failure event sequence

When the underlying loop returns `Err`, `Agent::finish_run` synthesizes a `StopReason::Error` / `StopReason::Aborted` assistant message and emits:

1. `MessageStart` (synthetic)
2. `MessageEnd`
3. `TurnEnd { message, tool_results: [] }`
4. `AgentEnd { messages: [synthetic] }`

Subscribers always see a coherent terminal sequence regardless of success / failure. `AgentState.error_message` is populated in parallel.

## End-to-end example (from `tests/smoke.rs`)

```rust
use std::sync::Arc;
use grain_agent_core::{Agent, AgentEvent, AgentOptions, Model};

let stream = Arc::new(MockStream::default());
let mut opts = AgentOptions::new(
    Model { id: "mock".into(), name: "mock".into(),
            api: "mock".into(), provider: "mock".into(), ..Default::default() },
    stream.clone(),
);
opts.tools = vec![Arc::new(EchoTool::new())];

let agent = Agent::new(opts);
agent.subscribe(Arc::new(|event: AgentEvent, _signal| {
    Box::pin(async move { println!("{event:?}"); })
})).await;

agent.prompt_text("hello").await?;
assert_eq!(agent.state().await.messages.len(), 4);
```
