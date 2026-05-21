# grain-agent 使用文档

本仓库是 [`@earendil-works/pi-agent-core`](https://github.com/earendil-works/pi) 的 Rust 移植，由两个 crate 组成：

- **`grain-agent-core`** — 与具体 LLM SDK 解耦的 agent 运行时（消息、工具、事件、循环、`Agent` 封装）。
- **`grain-agent-harness`** — 工程化外壳（会话树、自定义消息、system prompt 装配、截断）。

## 模块索引

### grain-agent-core

| 模块 | 文档 | 简介 |
|------|------|------|
| `types` | [core-types.md](./core-types.md) | 消息、工具、事件、状态等基础数据类型 |
| `stream` | [core-stream.md](./core-stream.md) | `LlmStream` trait — LLM provider 的注入点 |
| `agent_loop` | [core-agent-loop.md](./core-agent-loop.md) | 底层 `run_agent_loop` / `run_agent_loop_continue` |
| `agent` | [core-agent.md](./core-agent.md) | 高层 `Agent` 封装：订阅 / 中断 / steering / follow-up |

### grain-agent-harness

| 模块 | 文档 | 简介 |
|------|------|------|
| `messages` | [harness-messages.md](./harness-messages.md) | 自定义消息（branch / compaction / custom）与 `convert_to_llm` |
| `session` | [harness-session.md](./harness-session.md) | 会话树、存储 trait、内存实现、分支与 fork |
| `system_prompt` | [harness-system-prompt.md](./harness-system-prompt.md) | `<available_skills>` XML 块生成 |
| `truncate` | [harness-truncate.md](./harness-truncate.md) | 工具输出 head/tail 截断工具 |

## 快速上手

```toml
# Cargo.toml
[dependencies]
grain-agent-core = { path = "grain-agent-core" }
grain-agent-harness = { path = "grain-agent-harness" }  # 可选
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use std::sync::Arc;
use grain_agent_core::{Agent, AgentOptions, Model};

#[tokio::main]
async fn main() {
    let stream_fn: grain_agent_core::StreamFn = Arc::new(MyProvider::default());
    let model = Model {
        id: "gpt-4o".into(),
        name: "gpt-4o".into(),
        api: "openai".into(),
        provider: "openai".into(),
        ..Default::default()
    };

    let agent = Agent::new(AgentOptions::new(model, stream_fn));
    agent.prompt_text("hello").await.unwrap();

    let state = agent.state().await;
    println!("{} messages", state.messages.len());
}
```

`MyProvider` 需实现 [`LlmStream`](./core-stream.md)。完整工作示例参考 `grain-agent-core/tests/smoke.rs` 中的 `MockStream`。
