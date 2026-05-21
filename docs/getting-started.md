# Getting Started

Walks you from zero through a full grain-agent setup: implement a mock LLM provider, register a tool, subscribe to events, persist sessions, and finally swap in a real LLM via `grain-llm-genai`.

中文版：[zh/getting-started.md](./zh/getting-started.md).

> No external API keys required for sections 1–6; every example runs against an in-process mock. Section 7 shows how to plug in a real provider.

---

## 0. Prerequisites

- Rust stable. The workspace uses edition 2024 + resolver 3.
- In the repo root:

  ```bash
  cargo build
  cargo test
  ```

If both pass, you're set.

---

## 1. New binary crate

Outside the repo:

```bash
cargo new --bin my-agent
cd my-agent
```

`Cargo.toml`:

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

Path adjustments depend on where your local checkout lives.

---

## 2. Hello world: mock provider + single prompt

`grain-agent-core` doesn't depend on any LLM SDK — you have to provide an `LlmStream` implementation. The minimal one below always replies `"hello from mock"`:

`src/main.rs`:

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

    agent.prompt_text("hi").await.unwrap();
    for m in agent.state().await.messages {
        println!("{}", m.role());
    }
}
```

```bash
cargo run
```

Should print two roles — your `user` prompt and the mock `assistant`.

**Hard contracts** for any `LlmStream` implementation:

1. **Never** return `Err` for request / model / runtime failures. Surface failures as a terminal `AssistantMessageEvent::Error { result, .. }` (or `Done`) on the returned stream.
2. The stream must end with **exactly one** terminal event.
3. Fill `result.stop_reason` and `result.error_message` accurately — `Agent::finish_run` and subscribers both read them.

Full contract: [core-stream.md](./core-stream.md).

---

## 3. Add a tool

Two-turn flow: first turn requests an `echo` tool call, second turn stops. Statefulness:

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

Tool implementation:

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
                    "required": ["value"]
                }),
                execution_mode: None,
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

Wire up in `main`:

```rust
let mut opts = AgentOptions::new(model, Arc::new(MockStream { calls: AtomicUsize::new(0) }));
opts.tools = vec![Arc::new(EchoTool::new())];
let agent = Agent::new(opts);

agent.prompt_text("ping").await.unwrap();
assert_eq!(agent.state().await.messages.len(), 4);
// 0: user("ping")  1: assistant(tool_use)  2: tool_result  3: assistant("done")
```

Key points:

- Tool `parameters` is JSON Schema, forwarded verbatim to the LLM.
- `AgentToolResult.terminate = Some(true)` on **every** call in a batch ends the loop immediately.
- `on_update` lets you emit partial results during long-running tools (surfaces as `ToolExecutionUpdate` events).

More: [core-types.md](./core-types.md), [core-agent-loop.md](./core-agent-loop.md).

---

## 4. Subscribe, steer, abort

```rust
use grain_agent_core::AgentEvent;

let unsub = agent.subscribe(Arc::new(|event: AgentEvent, _signal| {
    Box::pin(async move {
        match event {
            AgentEvent::ToolExecutionStart { tool_name, .. } => println!("→ {tool_name}"),
            AgentEvent::ToolExecutionEnd { tool_name, is_error, .. } =>
                println!("← {tool_name} done (error={is_error})"),
            AgentEvent::AgentEnd { messages } => println!("agent end, {} new", messages.len()),
            _ => {}
        }
    })
})).await;

agent.prompt_text("ping").await.unwrap();
unsub.cancel().await;
```

**Subscribers are awaited serially per event** — do heavy work in `tokio::spawn`.

Cancel mid-stream:

```rust
agent.abort().await;  // signals the inner stream + tool::execute via shared CancellationToken
```

Steer (insert mid-turn) vs follow-up (re-run after the agent looks done):

```rust
agent.steer(AgentMessage::user(/* ... */)).await;
agent.follow_up(AgentMessage::user(/* ... */)).await;
```

Full API: [core-agent.md](./core-agent.md).

---

## 5. Harness: sessions + custom messages

`grain-agent-harness` adds three independent pieces on top of core:

1. Session tree (`Session`/`SessionRepo`) — tree-shaped transcript, supports branching / fork / compaction.
2. Custom messages (`branchSummary`/`compactionSummary`/`custom`) + `convert_to_llm` that projects them to LLM user messages.
3. System-prompt assembly (`<available_skills>` XML block renderer).

### 5.1 Sessions

```rust
use grain_agent_harness::{InMemorySessionRepo, SessionRepo};
use grain_agent_core::{AgentMessage, TextContent, UserContent, UserMessage};

let repo = InMemorySessionRepo::new();
let session = repo.create(None).await.unwrap();
let meta = session.metadata().await;

session.append_message(AgentMessage::user(UserMessage {
    content: vec![UserContent::Text(TextContent { text: "hi".into() })],
    timestamp: 0,
})).await.unwrap();
session.append_session_name("first chat").await.unwrap();

let reopened = repo.open(&meta).await.unwrap();
let ctx = reopened.build_context().await;     // SessionContext { messages, thinking_level, model }
```

`build_context()` automatically handles compaction entries (only entries from `first_kept_entry_id` are kept; a `compactionSummary` is prepended). Full reference: [harness-session.md](./harness-session.md).

### 5.2 Custom messages

By default `Agent` drops every `AgentMessage::Custom`. Wire the harness `convert_to_llm` to project `branchSummary` / `compactionSummary` / `custom` to LLM user messages:

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

Reference: [harness-messages.md](./harness-messages.md).

### 5.3 Skills in the system prompt

```rust
use grain_agent_harness::{format_skills_for_system_prompt, system_prompt::Skill};

let skills = vec![Skill {
    name: "Bash".into(),
    description: "Runs shell commands".into(),
    file_path: "/skills/bash/SKILL.md".into(),
    disable_model_invocation: false,
}];

let block = format_skills_for_system_prompt(&skills);
agent.set_system_prompt(format!("You are helpful.\n\n{block}")).await;
```

Skills with `disable_model_invocation = true` are filtered out of the rendered block. Reference: [harness-system-prompt.md](./harness-system-prompt.md).

### 5.4 Context-window guard

When the transcript grows close to the model's context window, `context_guard` truncates before the next request:

```rust
use std::sync::Arc;
use grain_agent_harness::context_guard::{ContextGuard, ContextGuardPolicy};
use grain_llm_models::Registry;

let registry = Arc::new(Registry::from_embedded_snapshot());
let guard = ContextGuard::new(registry, "anthropic/claude-sonnet-4-5")
    .with_policy(ContextGuardPolicy::DropOldest)
    .with_headroom_tokens(2048)
    .into_transform_fn();

let mut opts = AgentOptions::new(model, stream_fn);
opts.transform_context = Some(guard);
```

Reference: [context-guard.md](./context-guard.md).

---

## 6. Patterns to know

### Rewrite a tool result (after_tool_call)

```rust
use grain_agent_core::{AfterToolCallFn, AfterToolCallResult, UserContent};

let after: AfterToolCallFn = Arc::new(|ctx, _cancel| Box::pin(async move {
    if ctx.tool_call.name == "echo" {
        Some(AfterToolCallResult {
            content: Some(vec![UserContent::text("[redacted]")]),
            ..Default::default()
        })
    } else {
        None
    }
}));
opts.after_tool_call = Some(after);
```

### Swap model / thinking between turns

```rust
use grain_agent_core::{AgentLoopTurnUpdate, PrepareNextTurnFn, ThinkingLevel};

let prepare: PrepareNextTurnFn = Arc::new(|ctx| Box::pin(async move {
    if ctx.message.usage.total_tokens > 5_000 {
        Some(AgentLoopTurnUpdate {
            thinking_level: Some(ThinkingLevel::High),
            ..Default::default()
        })
    } else {
        None
    }
}));
```

Setters like `Agent::set_model(..)` only affect the **next** loop construction; running loops can only swap via `prepare_next_turn`.

### Parallel vs sequential tool execution

Default is `ToolExecutionMode::Parallel`. If any invoked tool's `execution_mode == Some(Sequential)`, the **whole batch** degrades to serial. Parallel mode still preserves source order in the resulting `ToolResultMessage[]`.

---

## 7. Real LLM via `grain-llm-genai`

Stop reading from `MockStream` — `grain-llm-genai` is the production `LlmStream` implementation.

```toml
[dependencies]
grain-llm-models  = { path = "..." }
grain-llm-genai   = { path = "..." }
```

```rust
use std::sync::Arc;
use grain_agent_core::{Agent, AgentOptions};
use grain_llm_genai::{GenaiStream, OpenAiCompatPreset};
use grain_llm_models::Registry;

let stream = Arc::new(
    GenaiStream::builder()
        .with_openai_compat_preset(OpenAiCompatPreset::Common)    // kimi + siliconflow
        .build(),
);

let model = Registry::from_embedded_snapshot()
    .to_core_model("anthropic/claude-sonnet-4-5")
    .unwrap();

let agent = Agent::new(AgentOptions::new(model, stream));
agent.prompt_text("Hello!").await.unwrap();
```

Required env var depends on the model. genai's auto-resolution + our `EnvKeyResolver::default_mapping()` together cover the canonical names (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `MOONSHOT_API_KEY`, …).

Custom env var per provider:

```rust
GenaiStream::builder()
    .with_env_override("openai", "MY_OPENAI_KEY")
    .build();
```

Custom OpenAI-compat host:

```rust
use grain_llm_genai::OpenAiCompatEndpoint;
GenaiStream::builder()
    .with_openai_compat(OpenAiCompatEndpoint::new(
        "my-host", "https://api.example.com/v1", "MY_HOST_API_KEY",
    ))
    .build();
```

Full surface: [llm-genai.md](./llm-genai.md), [llm-models.md](./llm-models.md).

---

## 8. Further reading

- [core-types.md](./core-types.md) — every data type / event
- [core-stream.md](./core-stream.md) — provider implementation contract
- [core-agent-loop.md](./core-agent-loop.md) — loop lifecycle, hook details
- [core-agent.md](./core-agent.md) — `Agent` API
- [harness-messages.md](./harness-messages.md) — custom message extension points
- [harness-session.md](./harness-session.md) — session tree, fork, move-to
- [harness-system-prompt.md](./harness-system-prompt.md) — skills XML rendering
- [harness-truncate.md](./harness-truncate.md) — output truncation utilities
- [context-guard.md](./context-guard.md) — context-window enforcement
- [llm-models.md](./llm-models.md) — model registry / models.dev integration
- [llm-genai.md](./llm-genai.md) — genai-backed `LlmStream` implementation

Runnable minimal examples in the repo:

```bash
cargo test -p grain-agent-core    smoke -- --nocapture
cargo test -p grain-agent-harness smoke -- --nocapture
```
