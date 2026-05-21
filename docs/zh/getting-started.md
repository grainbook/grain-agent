# Getting Started

本教程带你从零跑通 grain-agent：实现一个 mock LLM provider、注册一个工具、订阅事件、用 harness 持久化会话。读完你应该能把它接到任何真实的 LLM 服务上。

> 本教程不依赖外部 API key——所有示例都用本地 mock 跑通。具体类型说明请配合 [模块文档](./README.md)。

---

## 0. 前置条件

- Rust 工具链（建议 stable，仓库本身用 **edition 2024 + resolver 3**）。
- 已 clone 本仓库，能在根目录执行：

  ```bash
  cargo build
  cargo test
  ```

如果上面两条 OK，环境就绪。

---

## 1. 新建一个二进制 crate

在仓库**外**新建一个项目（也可以在仓库里加新 workspace member）：

```bash
cargo new --bin my-agent
cd my-agent
```

`Cargo.toml`：

```toml
[package]
name = "my-agent"
version = "0.1.0"
edition = "2024"

[dependencies]
grain-agent-core    = { path = "../grain-agent/grain-agent-core" }
grain-agent-harness = { path = "../grain-agent/grain-agent-harness" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
async-trait = "0.1"
futures = "0.3"
serde_json = "1"
tokio-util = "0.7"
```

路径按你本地实际位置调整。

---

## 2. Hello world：mock provider + 单轮 prompt

`grain-agent-core` 完全不依赖任何 LLM SDK——你需要自己实现 `LlmStream`。下面这个 mock 总是回一句 `"hello from mock"` 然后正常 stop。

`src/main.rs`：

```rust
use std::sync::Arc;
use async_trait::async_trait;
use futures::StreamExt;
use grain_agent_core::{
    Agent, AgentOptions, AssistantContent, AssistantMessage, AssistantMessageEvent,
    AssistantStream, LlmContext, LlmStream, Model, StopReason, StreamError, StreamFn,
    StreamOptions, TextContent, Usage,
};
use tokio_util::sync::CancellationToken;

struct MockStream;

#[async_trait]
impl LlmStream for MockStream {
    async fn stream(
        &self,
        model: &Model,
        _ctx: &LlmContext,
        _opts: &StreamOptions,
        _cancel: CancellationToken,
    ) -> Result<AssistantStream, StreamError> {
        let final_msg = AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: "hello from mock".into(),
            })],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        Ok(futures::stream::iter(vec![
            AssistantMessageEvent::Start { partial: final_msg.clone() },
            AssistantMessageEvent::Done { result: final_msg },
        ])
        .boxed())
    }
}

#[tokio::main]
async fn main() {
    let stream_fn: StreamFn = Arc::new(MockStream);

    let model = Model {
        id: "mock-1".into(),
        name: "mock".into(),
        api: "mock".into(),
        provider: "mock".into(),
        ..Default::default()
    };

    let mut opts = AgentOptions::new(model, stream_fn);
    opts.system_prompt = "You are a helpful agent.".into();
    let agent = Agent::new(opts);

    agent.prompt_text("你好").await.unwrap();

    let state = agent.state().await;
    for (i, m) in state.messages.iter().enumerate() {
        println!("[{}] {} → {:?}", i, m.role(), m);
    }
}
```

```bash
cargo run
```

应输出两条消息：你的 `user` prompt 和 mock 回的 `assistant`。

**关键契约**——你自己实现 `LlmStream` 时必须遵守：

1. **不要** `Err` 报告请求/模型错误。所有失败转成 `AssistantMessageEvent::Error { result, .. }`（或 `Done`）作为流的**终止事件**。
2. 流必须以**恰好一个**终止事件结尾。
3. `result.stop_reason` 与 `result.error_message` 要填准——`Agent::finish_run` 和事件订阅者都会读它。

完整契约见 [`core-stream.md`](./core-stream.md)。

---

## 3. 加一个工具

让 mock 第一轮请求一次 `echo` 工具，第二轮再 stop——这才能完整验证“assistant ↔ tool result”往返。

把 `MockStream` 改成有状态：

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use grain_agent_core::ToolCall;

struct MockStream { calls: AtomicUsize }

#[async_trait]
impl LlmStream for MockStream {
    async fn stream(
        &self,
        model: &Model,
        _ctx: &LlmContext,
        _opts: &StreamOptions,
        _cancel: CancellationToken,
    ) -> Result<AssistantStream, StreamError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let msg = if n == 0 {
            AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({ "value": "ping" }),
                })],
                stop_reason: StopReason::ToolUse,
                ..base(model)
            }
        } else {
            AssistantMessage {
                content: vec![AssistantContent::Text(TextContent { text: "done".into() })],
                stop_reason: StopReason::Stop,
                ..base(model)
            }
        };
        Ok(futures::stream::iter(vec![
            AssistantMessageEvent::Start { partial: msg.clone() },
            AssistantMessageEvent::Done { result: msg },
        ])
        .boxed())
    }
}

fn base(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: vec![],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    }
}
```

实现 `EchoTool`：

```rust
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};

struct EchoTool { def: ToolDefinition }

impl EchoTool {
    fn new() -> Self {
        EchoTool {
            def: ToolDefinition {
                name: "echo".into(),
                label: "Echo".into(),
                description: "Echo back the value".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": ["value"],
                }),
                execution_mode: None,   // 跟随全局；想强制串行就给 Some(Sequential)
            },
        }
    }
}

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
            content: vec![UserContent::text(format!("echo: {v}"))],
            details: serde_json::json!({}),
            terminate: None,
        })
    }
}
```

在 `main` 里挂上：

```rust
let stream_fn: StreamFn = Arc::new(MockStream { calls: AtomicUsize::new(0) });
let mut opts = AgentOptions::new(model, stream_fn);
opts.tools = vec![Arc::new(EchoTool::new())];
let agent = Agent::new(opts);

agent.prompt_text("ping").await.unwrap();

let state = agent.state().await;
assert_eq!(state.messages.len(), 4);
// 0: user("ping")  1: assistant(tool_use)  2: tool_result("echo: ping")  3: assistant("done")
```

要点：

- 工具的 `parameters` 是 JSON Schema，会被原样转发给 LLM。`validate_arguments` 默认接受任何输入；要校验自己重写。
- `AgentToolResult::terminate = Some(true)` 当一批工具**全部**为 `true` 时会立刻结束循环。
- `on_update` 回调可在 `execute` 内多次调用，用来发出流式中间结果——它会通过 `AgentEvent::ToolExecutionUpdate` 广播给订阅者。

更多见 [`core-types.md`](./core-types.md) 的“工具”一节、[`core-agent-loop.md`](./core-agent-loop.md) 的工具执行模式说明。

---

## 4. 订阅事件、steering、abort

```rust
use grain_agent_core::AgentEvent;

let unsub = agent.subscribe(Arc::new(|event: AgentEvent, _signal| {
    Box::pin(async move {
        match event {
            AgentEvent::ToolExecutionStart { tool_name, .. } =>
                println!("→ calling {tool_name}"),
            AgentEvent::ToolExecutionEnd { tool_name, is_error, .. } =>
                println!("← {tool_name} done (error={is_error})"),
            AgentEvent::AgentEnd { messages } =>
                println!("agent end, {} new messages", messages.len()),
            _ => {}
        }
    })
})).await;

agent.prompt_text("ping").await.unwrap();
unsub.cancel().await;
```

**注意**：所有监听器对每个事件**串行** await。在监听器内做重活要 `tokio::spawn`，否则会拖慢循环。

中途打断 / 插话：

```rust
// 异步打断当前循环（会传播到 stream_fn 与 tool::execute 的 cancel）
tokio::spawn({
    let agent_handle = agent.clone(); // Agent: Clone（内部 Arc）
    async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        agent_handle.abort().await;
    }
});

// steering：当前轮结束后立即插入 user 消息
agent.steer(grain_agent_core::AgentMessage::user(/* ... */)).await;

// follow-up：循环“似乎结束”时才被消费，让 agent 再跑一轮
agent.follow_up(grain_agent_core::AgentMessage::user(/* ... */)).await;
```

`Agent` 不是 `Clone`——上面这个示例是简化版，实际请把 `Arc<Agent>` 共享出去，或者在外部持有 `agent.signal().await` 的 `CancellationToken`。

完整 API 见 [`core-agent.md`](./core-agent.md)。

---

## 5. 加上 harness：会话持久化 + 自定义消息

`grain-agent-harness` 在 core 之上加了三块工程化能力：

1. **会话树**（`Session` / `SessionRepo`）——树形 transcript，支持分支、fork、压缩、移动游标。
2. **自定义消息**（`branchSummary` / `compactionSummary` / `custom`）+ `convert_to_llm` 实现。
3. **system prompt 装配**——skill 列表渲染成 `<available_skills>` XML 块。

### 5.1 会话基础

```rust
use grain_agent_harness::{InMemorySessionRepo, SessionRepo};
use grain_agent_core::{AgentMessage, TextContent, UserContent, UserMessage};

let repo = InMemorySessionRepo::new();
let session = repo.create(None).await.unwrap();
let meta = session.metadata().await;

session.append_message(AgentMessage::user(UserMessage {
    content: vec![UserContent::Text(TextContent { text: "你好".into() })],
    timestamp: 0,
})).await.unwrap();
session.append_session_name("first chat").await.unwrap();

let reopened = repo.open(&meta).await.unwrap();
let ctx = reopened.build_context().await;     // SessionContext { messages, thinking_level, model }
println!("{} messages in branch", ctx.messages.len());
```

把会话 transcript 喂给 `Agent`：

```rust
let mut opts = AgentOptions::new(model, stream_fn);
opts.messages = ctx.messages;
let agent = Agent::new(opts);
```

`build_session_context` 会自动处理 `Compaction` 条目（只保留 `first_kept_entry_id` 之后的消息，并 prepend 一条 `compactionSummary`）。详见 [`harness-session.md`](./harness-session.md)。

### 5.2 用 harness 的 `convert_to_llm`

默认 `Agent` 会丢弃所有 `AgentMessage::Custom`。要让 `branchSummary` / `compactionSummary` / `custom` 进入 LLM 上下文，注入 harness 实现：

```rust
use std::sync::Arc;
use grain_agent_core::ConvertToLlmFn;
use grain_agent_harness::convert_to_llm;

let convert: ConvertToLlmFn = Arc::new(|msgs| {
    Box::pin(async move { convert_to_llm(msgs) })
});

let mut opts = AgentOptions::new(model, stream_fn);
opts.convert_to_llm = Some(convert);
```

构造一条 custom 消息（典型用途：UI artifact、压缩摘要）：

```rust
use grain_agent_harness::custom_message;
let msg = custom_message(
    "artifact",
    serde_json::json!("结构化输出说明"),
    /* display */ true,
    /* details */ None,
    /* timestamp */ 0,
);
session.append_message(msg).await.unwrap();
```

完整规则见 [`harness-messages.md`](./harness-messages.md)。

### 5.3 把 skills 写进 system prompt

```rust
use grain_agent_harness::{format_skills_for_system_prompt, system_prompt::Skill};

let skills = vec![Skill {
    name: "Bash".into(),
    description: "Runs shell commands".into(),
    file_path: "/skills/bash/SKILL.md".into(),
    disable_model_invocation: false,
}];

let block = format_skills_for_system_prompt(&skills);
let prompt = if block.is_empty() {
    "You are helpful.".into()
} else {
    format!("You are helpful.\n\n{block}")
};

agent.set_system_prompt(prompt).await;
```

`disable_model_invocation = true` 的 skill 会被过滤掉、不出现在 prompt 里。详见 [`harness-system-prompt.md`](./harness-system-prompt.md)。

> 注：harness **尚未**移植磁盘 skill 加载、JSONL 持久化、context compaction、execution environment、`AgentHarness` 总壳——你当前要自己组装。

---

## 6. 常见模式

### 用 hook 改写 tool result（after_tool_call）

```rust
use std::sync::Arc;
use grain_agent_core::{AfterToolCallFn, AfterToolCallResult, UserContent};

let after: AfterToolCallFn = Arc::new(|ctx, _cancel| {
    Box::pin(async move {
        if ctx.tool_call.name == "echo" {
            Some(AfterToolCallResult {
                content: Some(vec![UserContent::text("[redacted]")]),
                ..Default::default()
            })
        } else {
            None  // 不改写
        }
    })
});

let mut opts = AgentOptions::new(model, stream_fn);
opts.after_tool_call = Some(after);
```

`before_tool_call` 也是同样模式，返回 `Some(BeforeToolCallResult { block: true, .. })` 会把这次调用直接变成一条 `is_error = true` 的 tool result，不会调到 `execute`。

### 在轮间换模型 / thinking_level

```rust
use grain_agent_core::{AgentLoopTurnUpdate, PrepareNextTurnFn, ThinkingLevel};

let prepare: PrepareNextTurnFn = Arc::new(|ctx| {
    Box::pin(async move {
        // 比如基于 token 用量升级到更强的模型
        if ctx.message.usage.total_tokens > 5_000 {
            Some(AgentLoopTurnUpdate {
                thinking_level: Some(ThinkingLevel::High),
                ..Default::default()
            })
        } else {
            None
        }
    })
});
```

`Agent::set_model(..)` 这类 setter 只改下次构造 `AgentLoopConfig` 时的快照——**正在运行的循环**只能通过 `prepare_next_turn` 在轮间换。

### 并行 vs 串行

默认 `ToolExecutionMode::Parallel`；只要**任意**被调工具的 `definition().execution_mode == Some(Sequential)`，**整批**就降级到串行。要全局强制串行：

```rust
opts.tool_execution = ToolExecutionMode::Sequential;
```

并行模式下 `ToolResultMessage[]` 仍然按 assistant 中 `ToolCall` 的顺序排列（内部用 `FuturesOrdered`），这一点不需要担心。

---

## 7. 走向真实 LLM provider

按下面这条 checklist 把 `MockStream` 换成真实 provider：

1. **新建独立 crate**（不要污染 `grain-agent-core`），依赖你选的 SDK（`async-anthropic` / `async-openai` …）。
2. 实现 `LlmStream::stream`：
   - 把 `LlmContext.messages` / `tools` / `system_prompt` 映射成 SDK 的请求。
   - 把 SDK 的流事件**逐个**映射成 `AssistantMessageEvent::TextDelta` / `ToolcallDelta` / `ThinkingDelta` 等。
   - 结束时发一个 `Done` 或 `Error`，把 `AssistantMessage` 填全（`api` / `provider` / `model` 来自传入的 `Model`，`usage` 来自 SDK 返回的 token 计数，`stop_reason` 按上游响应映射）。
3. 用 `StreamError::Other(...)` 只在“**根本无法**返回 stream”的情况下返回 `Err`——别的全部走终止事件。
4. **不要 panic**。
5. 用 `cancel.cancelled()` 在 `tokio::select!` 里中断 SDK 的请求，最后发一条 `StopReason::Aborted` 终止事件。

`get_api_key` hook 可以异步拉短期凭证（OAuth、IAM token），返回的字符串会写入 `StreamOptions::api_key`，由你的 provider 实现读取。

---

## 8. 进一步阅读

- [`core-types.md`](./core-types.md) — 所有数据类型 / 事件
- [`core-stream.md`](./core-stream.md) — provider 实现契约 + 失败处理模板
- [`core-agent-loop.md`](./core-agent-loop.md) — 循环生命周期、所有钩子
- [`core-agent.md`](./core-agent.md) — `Agent` API 全景
- [`harness-messages.md`](./harness-messages.md) — 自定义消息扩展点
- [`harness-session.md`](./harness-session.md) — 会话树语义、fork / move-to
- [`harness-system-prompt.md`](./harness-system-prompt.md) — skills 块渲染
- [`harness-truncate.md`](./harness-truncate.md) — 工具输出截断

仓库内可直接运行的最小示例：

```bash
cargo test -p grain-agent-core    smoke -- --nocapture
cargo test -p grain-agent-harness smoke -- --nocapture
```
