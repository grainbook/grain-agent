# `grain_agent_core::types`

定义贯穿整个 agent 运行时的数据模型：消息、工具、事件、上下文与状态快照。所有 wire 类型与 TS 参考实现的 `packages/agent/src/types.ts` 一致，serde 默认使用 `camelCase`。

## 内容块（content blocks）

```rust
pub struct TextContent { pub text: String }
pub struct ImageContent { pub data: String, pub mime_type: String }
pub struct ThinkingContent { pub thinking: String, pub signature: Option<String> }
pub struct ToolCall { pub id: String, pub name: String, pub arguments: serde_json::Value }
```

按角色拆成两种枚举：

- `AssistantContent` — assistant 可发出的内容：`Text` / `Image` / `Thinking` / `ToolCall`。
- `UserContent` — user 或 tool result 可携带的内容：`Text` / `Image`。便捷构造：`UserContent::text("...")`。

## 消息

```rust
pub enum Message {        // 给 LLM 看的标准消息
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

pub enum AgentMessage {    // 完整 transcript 的消息条目
    Standard(Message),
    Custom(serde_json::Value),
}
```

**关键约定**：`AgentMessage` 是 transcript 的存储形态；`Message` 是要送进 LLM 的形态。`Custom` 携带任意应用层 JSON，必须含 `role` 字段做判别——`AgentMessage::role()` 用它来分类。`AgentMessage::Custom` 不会自动进入 LLM 请求，需要由 `ConvertToLlmFn` 决定如何映射（参考 [harness-messages](./harness-messages.md)）。

便捷构造：

```rust
AgentMessage::user(user_msg);
AgentMessage::assistant(asst_msg);
AgentMessage::tool_result(trm);

if let Some(asst) = agent_msg.as_assistant() { ... }
```

## 用量 / 成本 / 模型

```rust
pub struct Cost { input, output, cache_read, cache_write, total: f64 }
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
Model::unknown()  // 占位实现
```

## 枚举

```rust
pub enum StopReason     { Stop, ToolUse, Length, Error, Aborted, Refused }
pub enum ThinkingLevel  { Off, Minimal, Low, Medium, High, XHigh }   // Default = Off
pub enum ToolExecutionMode { Sequential, Parallel }                   // Default = Parallel
pub enum QueueMode      { All, OneAtATime }                           // Default = OneAtATime
```

## 工具

实现一个工具：

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

`ToolDefinition`：

```rust
ToolDefinition {
    name: "echo".into(),
    label: "Echo".into(),
    description: "Echo back the value".into(),
    parameters: serde_json::json!({          // JSON Schema
        "type": "object",
        "properties": { "value": { "type": "string" } },
        "required": ["value"]
    }),
    execution_mode: None,                    // None = 沿用全局；Some(Sequential) 会强制整批串行
}
```

可选 hook：`prepare_arguments`（schema 校验前的参数预处理）、`validate_arguments`（默认接受任何参数）。

`AgentToolResult`：

```rust
AgentToolResult::text("...")     // 单段 text
AgentToolResult::error("...")    // 与 text 等价（is_error 由调用方决定）
AgentToolResult {
    content: vec![UserContent::text("hi")],
    details: serde_json::json!({...}),
    terminate: Some(true),       // 该批工具全部 terminate=true → 循环结束
}
```

`AgentToolError`：`Message(String)` / `Aborted` / `Validation(String)`，便捷构造 `AgentToolError::msg("…")`。

`ToolUpdateCallback` 是 `Arc<dyn Fn(AgentToolResult) + Send + Sync>`，传给 `execute` 用于发出流式中间结果；事件最终会变成 `AgentEvent::ToolExecutionUpdate`。

## 流式事件

`AssistantMessageEvent`（按 snake_case 标签序列化）：

| 变体 | 说明 |
|------|------|
| `Start` | 进入新 assistant 消息 |
| `TextStart` / `TextDelta` / `TextEnd` | 文本块 |
| `ThinkingStart` / `ThinkingDelta` / `ThinkingEnd` | 推理块 |
| `ToolcallStart` / `ToolcallDelta` / `ToolcallEnd` | 工具调用块 |
| `Done { result }` | 终止事件（成功） |
| `Error { error, result }` | 终止事件（失败） |

辅助方法：`event.partial()`（拿到非终止事件的部分消息）、`event.is_terminal()`。

## Agent 级事件

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

这些事件由 [`agent_loop`](./core-agent-loop.md) 经 `EventSink` 顺序发出，[`Agent`](./core-agent.md) 会进一步广播给 `subscribe` 注册的监听器。

## 上下文 / 状态

```rust
pub struct LlmContext {                // 送给 LLM 的快照
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

pub struct AgentContext {              // 送给 agent_loop 的快照
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

pub struct AgentState {                // Agent::state() 的快照
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

`AgentState` / `AgentContext` 都实现了 `Debug`（工具被压缩为 `name` 列表展示）。
