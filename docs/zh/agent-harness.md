# `AgentHarness`

顶层编排器，把 `Agent` + `Session` + tools + skills + queues + compaction + UI hooks 包成一个 façade。位于 `grain-agent-harness`（不是新 crate）。是 pi `AgentHarness` 的 Rust 移植 —— 设计文档见 [agent-harness-design.md](./agent-harness-design.md)。

English: [../agent-harness.md](../agent-harness.md).

---

## 为什么需要它

`AgentHarness` 之前，每个驱动 agent 的二进制都得手工：

```rust
let mut opts = AgentOptions::new(model, stream);
opts.tools = build_tools(...);
opts.transform_context = Some(context_guard);
// ...
let agent = Agent::new(opts);
agent.subscribe(SessionWriter::open(path)?).await;     // 镜像写盘
agent.subscribe(telemetry_sink).await;
agent.subscribe(event_printer).await;
agent.prompt_text("hello").await?;
```

这套样板今天散落在 `grain-headless::cli::run` 和 `grain-tui::agent_worker::spawn`。`AgentHarness::new(...)` 把它收成一行 + 一个 typed listener：

```rust
let harness = AgentHarness::new(opts).await;
harness.subscribe(my_listener).await;
harness.prompt_text("hello").await?;
```

它还**拥有** `Session`，自动把每个 `MessageEnd` 镜像回去，并暴露 bare `Agent` 没有的 pi 风格操作（`navigate_tree`、`compact`、`append_entry`、`prompt_from_template`、`skill`）。

---

## 快速上手

```rust
use grain_agent_harness::{AgentHarness, AgentHarnessOptions, Resources, SystemPrompt};
use grain_agent_harness::session::{InMemorySessionStorage, Session, SessionMetadata};
use grain_agent_core::Model;
use std::sync::Arc;

let session = Session::new(Arc::new(InMemorySessionStorage::new(SessionMetadata::new())));
let mut opts = AgentHarnessOptions::new(session, Model::unknown(), my_stream_fn());
opts.system_prompt = SystemPrompt::Static("You are a helpful coding agent.".into());
opts.tools = my_tools;
let harness = AgentHarness::new(opts).await;

harness.subscribe(Arc::new(|event, _signal| Box::pin(async move {
    println!("{:?}", event);
}))).await;

harness.prompt_text("Read main.rs and tell me what it does.").await?;
harness.wait_for_idle().await;
```

---

## 构造函数

```rust
pub struct AgentHarnessOptions {
    pub session: Session,
    pub model: Model,
    pub stream_fn: StreamFn,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub resources: Resources,
    pub system_prompt: SystemPrompt,
    pub thinking_level: ThinkingLevel,
    pub active_tool_names: Option<Vec<String>>,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub get_api_key: Option<GetApiKeyFn>,
    pub transform_context: Option<TransformContextFn>,
    pub tool_execution: ToolExecutionMode,
    pub session_id: Option<String>,
    pub transport: Option<String>,
    pub max_retry_delay_ms: Option<u64>,
}
```

`AgentHarnessOptions::new(session, model, stream_fn)` 给一组合理默认，剩下的字段你填。`AgentHarness::new(opts).await` 会从 `session.build_context()` 给 agent 喂初始 transcript，并装两个内部 listener（session 镜像 + harness 事件广播）。

---

## 公开方法

### Turn 触发

| 方法 | 行为 |
|------|------|
| `prompt_text(text)` | 用字符串作为 user prompt 提交。emit `BeforeAgentStart`。 |
| `prompt(Vec<AgentMessage>)` | 用一批消息提交（user + attachments 等）。 |
| `continue_()` | 从当前 transcript 继续。 |
| `prompt_from_template(name, args)` | 在 `Resources` 里找命名 `PromptTemplate`，用 `args` JSON 渲染后提交。找不到 → `UnknownTemplate`。 |
| `skill(name, args)` | 合成一个 prompt（`"Use the <name> skill with arguments: <json>"`）后提交。Phase 5+ 会改为严格的 validated invocation。 |

### 队列

| 方法 | 行为 |
|------|------|
| `steer(msg)` | 排队一个 steer 消息（下个 assistant turn 开始前送达）。 |
| `follow_up(msg)` | 排队 follow-up（当前 turn 工具调用完成后送达）。 |
| `next_turn(msg)` | Phase 2 暂时别名为 `follow_up`。 |

每个都 emit `QueueUpdate { has_queued }`。

### 运行时重配

| 方法 | 行为 |
|------|------|
| `set_model(model)` | 切换 active model。emit `ModelSelect`。 |
| `set_thinking_level(level)` | 切换 thinking level。emit `ThinkingLevelSelect`。 |
| `set_active_tools(&[name])` | 把 LLM 可见的工具限定到命名子集。未知名字返回 `UnknownTool`。emit `ActiveToolsSelect`。 |
| `set_resources(resources)` | 原子替换 skills + templates。emit `ResourcesUpdate { skills, templates }`。 |

### Session 控制

| 方法 | 行为 |
|------|------|
| `append_entry(type_tag, data)` | 追加一个 `Custom` session entry（**扩展状态**，不进 LLM context）。返回 entry id，emit `AppendEntry`。 |
| `navigate_tree(target_leaf)` | 切换 active leaf，然后从新分支的 `build_context()` 重写 agent transcript。emit `SessionBeforeTree` → `SessionTree`。 |
| `compact(keep_recent)` | 跑一次 compaction：把最后 `keep_recent` 条之前的消息全部 summarize，替换 transcript 为 summary + tail，写 `Compaction` session entry。emit `SessionBeforeCompact` → `SessionCompact`。 |
| `session()` | 廉价 clone 的 `Session` handle（Arc-backed）。 |

### 订阅 + 控制

| 方法 | 行为 |
|------|------|
| `subscribe(listener)` | 注册 listener。返回 `HarnessUnsubscribe`，`.cancel().await` 取消订阅。 |
| `abort()` | 中断当前 turn（若有）。 |
| `wait_for_idle()` | 轮询直到 agent 的 run signal 为 `None`。 |
| `agent()` | 逃生口 —— 返回 `&Arc<Agent>` 让你访问还没在 harness 一等公民化的行为。后续 phase 会逐步缩小。 |

---

## 事件总览

```rust
pub enum AgentHarnessEvent {
    // 转发自 grain-agent-core::AgentEvent
    AgentStart,
    AgentEnd { messages: Vec<AgentMessage> },
    TurnStart,
    TurnEnd { message: AssistantMessage, tool_results: Vec<ToolResultMessage> },
    MessageStart { message: AgentMessage },
    MessageUpdate { message: AssistantMessage, assistant_message_event: AssistantMessageEvent },
    MessageEnd { message: AgentMessage },
    ToolExecutionStart { tool_call_id, tool_name, args },
    ToolExecutionUpdate { tool_call_id, tool_name, args, partial_result },
    ToolExecutionEnd { tool_call_id, tool_name, result, is_error },

    // Harness 自有
    Abort,
    Settled,                                                    // AgentEnd 之后
    BeforeAgentStart { system_prompt, messages, tool_names },   // turn 派发前
    QueueUpdate { has_queued: bool },
    ModelSelect { model: Model },
    ThinkingLevelSelect { level: ThinkingLevel },
    ActiveToolsSelect { names: Vec<String> },
    AppendEntry { entry_id: String, type_tag: String },
    SessionBeforeCompact { messages: Vec<AgentMessage> },
    SessionCompact { kept_from: Option<String> },
    SessionBeforeTree { from: Option<String>, to: Option<String> },
    SessionTree { current_leaf: Option<String> },
    ResourcesUpdate { skills: usize, templates: usize },
}
```

所有变体 `Serialize + Deserialize`（沿用既有 `serde(tag = "type", rename_all = "snake_case")` 约定）。

---

## 从手工 `Agent::new` 迁移

老套路：

```rust
let mut opts = AgentOptions::new(model, stream);
opts.tools = tools;
let agent = Agent::new(opts);
let session_writer = Arc::new(SessionWriter::open(&path)?);
agent.subscribe(Arc::new(move |event, _| {
    let w = session_writer.clone();
    Box::pin(async move {
        if let AgentEvent::MessageEnd { message } = event {
            let _ = w.append(&message);
        }
    })
})).await;
```

新写法 —— session 镜像内置：

```rust
let session = Session::new(JsonlSessionStorage::open(&path)?);
let mut opts = AgentHarnessOptions::new(session, model, stream);
opts.tools = tools;
let harness = AgentHarness::new(opts).await;
```

`grain-headless` 和 `grain-tui` 暂时还没迁移过来 —— 设计文档把 callsite swap 留作最后一步。Harness Phase 1-4 已经实现完毕；什么时候做消费者侧的切换由你决定，工作量很小。

---

## 还没做的事

| 项 | 进度 / Phase |
|----|------|
| Pi `Context` 事件（`transform_context` 之后） | 需要 agent loop 内部的 hook；未实现 |
| Pi `BeforeProviderRequest` / `BeforeProviderPayload` / `AfterProviderResponse` | 需要 `LlmStream` 更丰富的 hook |
| `SessionCompact` 携带 summary 文本 | 当前只 emit `kept_from`；summary 内容存在新的 `MessageEnd` 里 |
| Pending-write 批量（per-turn 原子提交） | Phase 5+；当前写入直接落盘 |
| 动态 `SystemPrompt::Dynamic` 重渲染 | 存储但状态变化时不会自动重新求值 |
| 三种独立队列（`steering` / `follow_up` / `next_turn` 各自一桶） | 当前 `next_turn` 别名到 `follow_up` |
| `Resources::skills` 在 `skill()` 里的严格校验 | 当前只是合成 prompt |
| `Agent::fork`（创建新 session） | 需要 `SessionRepo` 引用，harness 还没拿 |

这些都在 [agent-harness-design.md](./agent-harness-design.md) 里跟踪，每条都独立可发布。

---

## 测试

```bash
cargo test -p grain-agent-harness agent_harness
```

20 个单元测试覆盖：构造、四个 phase 的全部公开 surface、event 广播。
