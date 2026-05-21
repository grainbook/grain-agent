# grain-agent 使用文档

> English version: [../README.md](../README.md). 此目录是完整的中文本地化版本。

本仓库是 [`@earendil-works/pi-agent-core`](https://github.com/earendil-works/pi) 的 Rust 移植，由五个 workspace crate 组成：

- **`grain-agent-core`** — 与具体 LLM SDK 解耦的 agent 运行时（消息、工具、事件、循环、`Agent` 封装）。
- **`grain-agent-harness`** — 工程化外壳（会话树、自定义消息、system prompt 装配、截断、context guard、**compaction**、**JSONL 持久化**）。
- **`grain-llm-models`** — 标准化模型注册表（models.dev 数据，descriptor + capability + 价格）。
- **`grain-llm-genai`** — 基于 [`genai`](https://crates.io/crates/genai) 的 `LlmStream` 实现（builder、env-key resolver、OpenAI-compat preset）。
- **`grain-ai-agent-headless`** — `grain-headless` CLI 二进制 + coding-agent 工具包（文件 / shell / web / 语义搜索工具、skills loader、JSONL session、telemetry 等）。

---

## 👋 入门

**没用过 agent？** 看 [getting-started.md](./getting-started.md)。从"agent 是什么"讲起，再跑内置 CLI，最后写一个自定义工具。大约 30 分钟。

**直接要 CLI 参考？** [headless-cli.md](./headless-cli.md)。

**写自定义 agent？** 看完入门后直接看 [core-agent.md](./core-agent.md)。

---

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
| `session_jsonl` | [session-jsonl.md](./session-jsonl.md) | JSONL 目录形式的会话磁盘持久化 |
| `system_prompt` | [harness-system-prompt.md](./harness-system-prompt.md) | `<available_skills>` XML 块生成 |
| `truncate` | [harness-truncate.md](./harness-truncate.md) | 工具输出 head/tail 截断工具 |
| `context_guard` | [context-guard.md](./context-guard.md) | 基于 Registry 的 `transform_context` 预算守门 |
| `compaction` | [compaction.md](./compaction.md) | 跨轮的 LLM 上下文总结 |

### LLM 集成

| Crate | 文档 | 简介 |
|-------|------|------|
| `grain-llm-models` | [llm-models.md](./llm-models.md) | 模型 descriptor + registry、vendored models.dev snapshot、可选 runtime fetch |
| `grain-llm-genai` | [llm-genai.md](./llm-genai.md) | 基于 `genai` 0.5 的 `LlmStream`：builder、env keys、OpenAI-compat 路由 |

### grain-ai-agent-headless

| 面 | 文档 | 简介 |
|-----|------|------|
| `grain-headless` 二进制 | [headless-cli.md](./headless-cli.md) | 即开即用 CLI；所有 flag + slash 命令 |
| 内置工具 | [headless-tools.md](./headless-tools.md) | CLI / 库提供的每个工具及参数示例 |
| 配置文件 | [config.md](./config.md) | `<workspace>/.grain/config.toml` + `~/.config/grain/config.toml` TOML |
| Telemetry | [telemetry.md](./telemetry.md) | 可选的本地 JSONL 事件日志（含敏感数据警告） |

### grain-ai-agent-tui

| 面 | 文档 | 简介 |
|-----|------|------|
| `grain-tui` 二进制 | [headless-tui.md](./headless-tui.md) | 基于 ratatui 的终端 UI — 主题、slash 补齐、prompt 历史、provider 选择器 |

### 扩展

| Crate | 文档 | 简介 |
|-------|------|------|
| `grain-script-boa` | [scripting.md](./scripting.md) | 基于 Boa 的 JS 脚本层 —— `<workspace>/.grain/scripts/` 丢 `.js` 即可运行时注册 agent 工具 |
| `grain-pi-compat` | [pi-compat.md](./pi-compat.md) | [pi.dev 风格扩展](https://pi.dev/docs/latest/extensions)的兼容层 —— 支持 `registerTool` / `registerCommand` / `registerShortcut` / `on` / `ui.notify` / `ui.confirm` / `ui.input` / `ui.select` |

### 跨模块

| 主题 | 文档 | 简介 |
|------|------|------|
| Provider profiles | [providers.md](./providers.md) | 多厂商 / 多账号 / OAuth 订阅配置；`grain-headless` 和 `grain-tui` 共享 |

### 全 workspace

| 主题 | 文档 |
|------|------|
| 测试 & CI | [testing.md](./testing.md) —— 跑 unit / clippy / 用 `.env.test` 门控的真模型 live 测试 |

---

## 快速上手

### 直接用 CLI

```bash
cargo build --release -p grain-ai-agent-headless --bin grain-headless
export ANTHROPIC_API_KEY=...
./target/release/grain-headless -C ./my-project --prompt "main.rs 写了什么？"
```

加 `--allow-write` 让它改文件；加 `--allow-bash` 让它跑 shell；加 `--interactive` 进多轮对话。完整参考：[headless-cli.md](./headless-cli.md)。

### 自己写一个 agent

```toml
[dependencies]
grain-agent-core    = { path = "grain-agent-core" }
grain-llm-models    = { path = "grain-llm-models" }
grain-llm-genai     = { path = "grain-llm-genai" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use std::sync::Arc;
use grain_agent_core::{Agent, AgentOptions};
use grain_llm_genai::GenaiStream;
use grain_llm_models::Registry;

#[tokio::main]
async fn main() {
    let stream = Arc::new(GenaiStream::builder().build());
    let model = Registry::from_embedded_snapshot()
        .to_core_model("anthropic/claude-sonnet-4-5")
        .unwrap();

    let agent = Agent::new(AgentOptions::new(model, stream));
    agent.prompt_text("hello").await.unwrap();
}
```

完整 walkthrough 带自定义工具：[getting-started.md](./getting-started.md)。
