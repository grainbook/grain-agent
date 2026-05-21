# `grain_agent_core::agent_loop`

底层 agent 循环。所有事件路由、工具调度、steering / follow-up 队列处理、prepare-next-turn / should-stop-after-turn 钩子都在这里。常规应用应优先使用更易用的 [`Agent`](./core-agent.md) 封装；这一层暴露给希望完全控制生命周期的调用者。

## 入口

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

- `run_agent_loop` 用一批新 prompt 启动循环，会先发出 `AgentStart` + `TurnStart` 以及每条 prompt 的 `MessageStart` / `MessageEnd`。
- `run_agent_loop_continue` 在已有 transcript 上继续。**前置条件**：`context.messages` 非空，且最后一条不能是 `assistant`（否则 provider 会拒绝），否则返回 `AgentLoopError::Other`。

两者都返回 *本次循环新增的* `AgentMessage[]`（不是完整 transcript）。

## `AgentLoopConfig`

```rust
pub struct AgentLoopConfig {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
    pub tool_execution: ToolExecutionMode,

    pub convert_to_llm: ConvertToLlmFn,                   // 必填
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

便捷构造：

```rust
let mut config = AgentLoopConfig::new(model, convert_to_llm);
config.thinking_level = ThinkingLevel::Medium;
config.tool_execution = ToolExecutionMode::Parallel;
```

## 钩子签名

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

调用顺序（参见“一轮的生命周期”）：
1. `get_steering_messages()` — 注入 user / 工具结果消息进入 transcript。
2. `transform_context` → `convert_to_llm` → `get_api_key` → `stream_fn.stream`。
3. 流逐事件发到 `emit`，结束后得到 `AssistantMessage`。
4. 对每个 `ToolCall`：`tool.prepare_arguments` → `tool.validate_arguments` → `before_tool_call` → `tool.execute` → `after_tool_call`。
5. 发 `TurnEnd`。
6. `prepare_next_turn` — 允许替换 `context` / `model` / `thinking_level`。
7. `should_stop_after_turn` — 返回 `true` 立即发 `AgentEnd` 并退出。
8. 拉新 steering；若仍空且没有更多工具调用，再拉 `get_follow_up_messages`；都空则 `AgentEnd`。

### Before / After 钩子

```rust
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub args: serde_json::Value,
    pub context: Arc<AgentContext>,
}

pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
}

pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub args: serde_json::Value,
    pub result: AgentToolResult,
    pub is_error: bool,
    pub context: Arc<AgentContext>,
}

pub struct AfterToolCallResult {
    pub content: Option<Vec<UserContent>>,    // 覆写 result.content
    pub details: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}
```

- `before_tool_call` 返回 `Some(result)` 且 `result.block = true` 会把这次 tool 调用变成一条 `is_error = true` 的 tool result，`reason` 写入文本（默认 `"Tool execution was blocked"`）；后续 `tool.execute` 不会被调用。
- `after_tool_call` 用于修改最终发回模型的 tool result（脱敏、压缩、覆盖 `terminate` 等）。

### 终止整个循环

让某个工具批次后立刻退出循环：让该批每个工具的 `AgentToolResult.terminate = Some(true)`，循环就不再发起下一轮 user/tool 消息。

### `PrepareNextTurnFn` 与 `ShouldStopAfterTurnFn`

```rust
pub struct ShouldStopAfterTurnContext {  // PrepareNextTurnContext 是它的别名
    pub message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub context: Arc<AgentContext>,
    pub new_messages: Vec<AgentMessage>,
}

pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,           // 整段替换
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}
```

- 用 `prepare_next_turn` 实现压缩 / 总结、模型升降级、提升 thinking_level 等。
- `should_stop_after_turn = true` 的退出是“正常退出”，会发 `AgentEnd`，但不会发额外的 `TurnEnd`。

## 工具执行模式

- 默认 `ToolExecutionMode::Parallel`：并行执行，但**保留消息顺序**——内部用 `FuturesOrdered`，`ToolResultMessage[]` 与 assistant 中的 `ToolCall` 顺序一致。
- 任意被调工具的 `definition().execution_mode == Some(Sequential)` 会让**整批**降级到串行。整体配置 `tool_execution = Sequential` 同样如此。
- 串行模式下一旦 `cancel.is_cancelled()` 会跳过剩余工具。

## 错误与终止事件

- `stream_fn.stream(...).Err(_)`：循环将合成一条 `StopReason::Error` 的 placeholder assistant，并发 `MessageStart` + `MessageEnd`，然后正常进入 `TurnEnd` / `AgentEnd`。
- 流提前结束（没有终止事件）：同样合成一条 `StopReason::Error`、`error_message = "stream ended without terminal event"`。
- 工具未找到：直接走 `Preparation::Immediate` 路径，返回 `is_error = true`、`content = "Tool {name} not found"`。
- `cancel` 取消：进行中的 `before_tool_call` / `prepare_tool_call` 检查会把工具变成 `is_error = true`、`"Operation aborted"`；串行模式立即退出。

## `EventSink`

```rust
pub type EventSink = Arc<dyn Fn(AgentEvent) -> BoxFuture<'static, ()> + Send + Sync>;
```

所有事件按发生顺序串行 `await`。在循环里同步处理事件（例如把消息追加到 transcript）请直接在 sink 内做；昂贵的下游分发可以 `tokio::spawn`。

注意 `ToolExecutionUpdate` 是由工具的 `on_update` 回调 *fire-and-forget* 触发的——它会 `tokio::spawn(emit(event))`，不阻塞工具体执行；如果你的 sink 依赖严格顺序，要自己再加序号或队列。

## 直接使用示例

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
    vec![/* 初始 prompts */],
    AgentContext { system_prompt: "you are helpful".into(), messages: vec![], tools: vec![] },
    config,
    emit,
    cancel,
    stream_fn,
).await?;
```

`AgentLoopError` 当前只有 `Other(String)`，仅用于硬错（例如 `run_agent_loop_continue` 的前置条件失败）。
