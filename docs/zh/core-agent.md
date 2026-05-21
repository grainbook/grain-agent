# `grain_agent_core::agent`

`Agent` 是 [`agent_loop`](./core-agent-loop.md) 的高层封装：持有可变状态（消息、工具、模型、订阅者）、保证同时只跑一次循环、提供 steering / follow-up 队列、订阅事件、中断。

对应 TS 参考实现里的 `packages/agent/src/agent.ts`。

## 构造

```rust
use std::sync::Arc;
use grain_agent_core::{Agent, AgentOptions, Model, QueueMode, ThinkingLevel, ToolExecutionMode};

let mut opts = AgentOptions::new(model, stream_fn);
opts.system_prompt = "you are helpful".into();
opts.thinking_level = ThinkingLevel::Medium;
opts.tools = vec![Arc::new(MyTool::new())];
opts.steering_mode = QueueMode::OneAtATime;       // 默认
opts.follow_up_mode = QueueMode::OneAtATime;
opts.tool_execution = ToolExecutionMode::Parallel; // 默认
opts.session_id = Some("session-abc".into());

let agent = Agent::new(opts);
```

`AgentOptions::new(model, stream_fn)` 只填必填项；其余字段由 `Default` 自然零值。可选 hook 全部允许 `None`：

```rust
pub struct AgentOptions {
    pub system_prompt: String,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<AgentMessage>,

    pub convert_to_llm: Option<ConvertToLlmFn>,        // None → 丢弃所有 Custom
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

如果要让 `Custom` 消息真正进入 LLM 上下文，传入自己的 `convert_to_llm`（参考 [`harness::convert_to_llm`](./harness-messages.md)）。

## 运行：`prompt` / `continue_`

```rust
agent.prompt_text("你好").await?;            // 便捷封装一条 user 文本
agent.prompt(vec![message_a, message_b]).await?;  // 多条 prompt 一起入
agent.continue_().await?;                          // 不加新消息，从 transcript 继续
```

错误：

- `AgentError::AlreadyRunning` — 同一时刻只允许一个活动循环。
- `AgentError::NoMessagesToContinue` — `continue_` 时 transcript 为空。
- `AgentError::CannotContinueFromAssistant` — `continue_` 时最后一条是 assistant 且 steering / follow-up 队列都空。若有 queued steering 或 follow-up，会先把队列 drain 出来当作 prompt 重启循环。

`prompt` / `prompt_text` 启动时会**跳过首次 steering 轮询**（即原本要在循环开头注入的 queued steering 消息不会立刻被插到你刚发的 prompt 前面）；之后每一轮再正常拉队列。这避免了“刚发 prompt 又被 queued 的旧消息抢前缀”的怪现象。

## 状态、订阅与中断

```rust
let state = agent.state().await;            // AgentState 快照（克隆）

let unsub = agent.subscribe(Arc::new(move |event, cancel| {
    Box::pin(async move {
        println!("event: {event:?}");
    })
})).await;
// ...
unsub.cancel().await;                       // 取消订阅

let signal = agent.signal().await;          // Option<CancellationToken> —— 活动循环的取消句柄
agent.abort().await;                        // 立即取消当前循环
```

监听器的并发模型：每个事件**串行** await 所有已注册监听器，再传给下一事件。监听器内部如果有重活，请自行 `tokio::spawn`。`subscribe` 当前实现按 `Vec` 索引存储 listener，`Unsubscribe::cancel` 会按下标移除——添加 listener 的同时移除其它 listener 可能让你的下标失效，避免并发地反复增删。

`abort` 通过共享的 `CancellationToken` 通知底层 stream / 工具 `execute` 取消。`StreamFn` 实现应在 `cancel.cancelled()` 时尽快产出 `StopReason::Aborted` 的终止事件。

## 修改运行时配置

所有 setter 都是 async（因为内部 `Mutex`）：

```rust
agent.set_system_prompt("...".into()).await;
agent.set_model(new_model).await;
agent.set_thinking_level(ThinkingLevel::High).await;
agent.set_tools(vec![Arc::new(MyTool::new())]).await;
agent.set_messages(restored_transcript).await;
agent.set_steering_mode(QueueMode::All).await;
agent.set_follow_up_mode(QueueMode::OneAtATime).await;
```

注意：这些 setter 只改 `Inner` 字段，正在跑的循环不会自动看到——只有下一轮 `snapshot_context` / `build_loop_config` 时才会生效。要立即换模型，结合 `prepare_next_turn` 钩子返回 `AgentLoopTurnUpdate { model: Some(_), .. }` 才能在循环内部下一轮立即换。

`agent.reset().await` 清空 transcript、流式状态、队列、错误——但不解绑订阅者。

## Steering / Follow-up 队列

```rust
// 在循环跑的过程中插入“打断”消息（每轮开头取 1 条 / 全部）
agent.steer(AgentMessage::user(user_msg)).await;

// 让 agent 看似停下后，再追加一条让它继续工作
agent.follow_up(AgentMessage::user(user_msg)).await;

agent.has_queued_messages().await;
agent.clear_steering_queue().await;
agent.clear_follow_up_queue().await;
agent.clear_all_queues().await;
```

- `steering_queue` 由 `get_steering_messages` 在**每一轮** assistant 开始前消费。
- `follow_up_queue` 只在循环本来要结束时消费——agent 没有更多 tool calls、steering 也空了，才看看 follow-up 是否非空，非空就继续跑一轮。
- 两个队列的 drain 行为受 `QueueMode` 控制：`OneAtATime`（默认）每次只出队头那一条；`All` 出全部。

## 失败时的事件序列

底层循环返回 `Err` 时，`Agent::finish_run` 会**合成**一条 `StopReason::Error` / `StopReason::Aborted` 的 assistant 消息，并依次发：

1. `MessageStart`（这条合成消息）
2. `MessageEnd`
3. `TurnEnd { message, tool_results: [] }`
4. `AgentEnd { messages: [synthetic] }`

这样订阅者无论成功失败都能看到一个一致的终止序列。`AgentState.error_message` 也会同步被填上。

## 端到端示例（节选自 `tests/smoke.rs`）

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

let state = agent.state().await;
assert!(!state.is_streaming);
assert_eq!(state.messages.len(), 4); // user + assistant(tool_use) + tool_result + assistant(stop)
```
