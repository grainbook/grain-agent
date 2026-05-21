# 五分钟做出你的第一个 Agent

这篇假设你用过 ChatGPT 或 Claude **作为用户**——不需要任何 agent 知识，也不需要懂 Rust async。读完你会：

1. 让一个真的能在你电脑上跑的 coding-agent 跟 Claude 对话（5 分钟）。
2. 搞懂背后到底发生了什么（10 分钟）。
3. 用 Rust 写一个**带自己工具**的自定义 agent（30 分钟）。

English version: [../getting-started.md](../getting-started.md).

---

## 第一部分 — Agent 是什么？

普通聊天机器人就是个"问一句答一句"的盒子：

> **你**：`main.rs` 写了什么？
> **Claude**：我看不到你电脑上的文件。

**Agent** 是带工具腰带的聊天机器人。你的代码给 LLM 一份工具清单（可以调的函数），然后把对话放进一个循环里跑：

```
你         "main.rs 写了什么？"
  ↓
LLM        "我需要读一下" → 想调：read(path="main.rs")
  ↓
你的代码    读 main.rs 并返回:    "<文件内容>"
  ↓
LLM        "这是个打印 hello world 的程序，因为……"
```

LLM 决定调哪个工具，你的代码负责真的去做。这个来回循环，就是 `grain-agent` 给你的东西。

这个 repo 里有：

- **`grain-headless`** —— 一个已经能跑的 coding-agent（自带读/列/搜索/写/shell 工具）。
- **`grain-agent-core` + `grain-agent-harness`** —— 给你自己构造 agent 的 Rust 库。

两个我们都会用。先跑 CLI 看效果，再自己写。

---

## 第二部分 — 五分钟跑起来

### 2.1 — 编译

需要：Rust stable + 一个 LLM 提供商 API key（我们用 Claude，因为默认模型是 Claude）。

```bash
git clone <this-repo> grain-agent
cd grain-agent
cargo build --release -p grain-ai-agent-headless --bin grain-headless
```

二进制在 `./target/release/grain-headless`。喜欢的话往 `$PATH` 里建个软链，或者就用全路径。

### 2.2 — 设置 API key

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

Grain 会按环境变量自动识别 key。其它支持的：`OPENAI_API_KEY`、`GEMINI_API_KEY`、`DEEPSEEK_API_KEY`、`MOONSHOT_API_KEY` 等——完整表见 [llm-genai.md](./llm-genai.md)。

### 2.3 — 让它读一个真实项目

把 CLI 指向任意一个本地代码目录，问点问题：

```bash
./target/release/grain-headless \
    -C ~/code/some-project \
    --prompt "这个项目是干嘛的？看看 README 和主入口。"
```

你会看到 agent 实时的思考过程：工具调用 `→ read(...)`、工具结果 `← read ...`、最后的回答。它替你读了文件，把内容喂给 Claude，Claude 看到上下文后给出了答案。

### 2.4 — 让它改代码

默认只读。加 `--allow-write` 才能动文件：

```bash
grain-headless -C ~/code/some-project --allow-write \
    --prompt "加一个 CHANGELOG.md，里面初始写一个 'Unreleased' 段落。"
```

要让它跑 shell（比如 `cargo test`）：

```bash
grain-headless -C ~/code/some-project --allow-write --allow-bash \
    --prompt "找到失败的测试并修好它。"
```

> ⚠️ `--allow-bash` 让 agent 能跑**任何** shell 命令。在没信任模型表现之前，先在能扔掉的项目或者容器里跑。

### 2.5 — 多轮对话

要持续问问题，加 `--interactive`：

```bash
grain-headless -C ~/code/some-project --interactive
```

你会看到 `> ` 提示符。打字、回车。内置的 slash 命令：`/skills`、`/doctor`、`/source`、`/clear`、`/exit`。先打 `/help` 看完整列表。

### 2.6 — 环境自检

不确定环境配好没？跑：

```bash
grain-headless -C . --doctor
```

它会打印工作目录、注册的模型数、当前 env 里检测到的 provider key、git 状态。**不调用任何 LLM**，所以没 key 也能跑。

---

## 第三部分 — 它是怎么运作的（四个零件）

跑过 CLI 之后，记住这张表就行。整个系统只有四个零件：

| 零件 | 作用 | 在 `grain-agent` 里 |
|------|------|---------------------|
| **Model（模型）** | LLM 本身（Claude / GPT-4o ...） | `grain_agent_core::Model` |
| **Tools（工具）** | LLM 可以调的函数 | 实现 `AgentTool` |
| **System prompt（系统提示）** | 一开始给 LLM 的指令 | `AgentOptions::system_prompt` |
| **Loop（循环）** | 跑对话的代码 | `grain_agent_core::Agent` |

加 `--show-thinking` 再跑一次 CLI，你会看到 LLM 的内心思考（暗色字体）和最终回答并排显示。整个 agent 就这么回事：LLM 思考 → 选个工具 → 你的代码执行 → LLM 继续。

---

## 第四部分 — 写一个自己的 agent（带自定义工具）

我们写一个小 agent，会查"今天的天气"。它演示完整模式——写工具、注册工具、跑一次 prompt。

### 4.1 — 新项目

```bash
cargo new my-agent
cd my-agent
```

改 `Cargo.toml`：

```toml
[package]
name = "my-agent"
version = "0.1.0"
edition = "2024"

[dependencies]
grain-agent-core    = { path = "../grain-agent/grain-agent-core" }
grain-llm-genai     = { path = "../grain-agent/grain-llm-genai" }
grain-llm-models    = { path = "../grain-agent/grain-llm-models" }
tokio        = { version = "1", features = ["rt-multi-thread", "macros"] }
async-trait  = "0.1"
tokio-util   = "0.7"
serde_json   = "1"
```

（`path = "..."` 改成你本地 grain-agent 实际位置。）

### 4.2 — 写一个工具

工具就是个实现了 `AgentTool` trait 的 struct。trait 有两块：

- **`definition()`** —— 名字 + 描述 + 参数的 JSON-Schema。LLM 读这些来决定什么时候调你。
- **`execute(args)`** —— 真正的函数。返回的文本就是 LLM 看到的工具结果。

把 `src/main.rs` 替换成：

```rust
use std::sync::Arc;
use async_trait::async_trait;
use grain_agent_core::{
    Agent, AgentOptions, AgentTool, AgentToolError, AgentToolResult,
    ToolDefinition, ToolUpdateCallback, UserContent,
};
use grain_llm_genai::GenaiStream;
use grain_llm_models::Registry;
use tokio_util::sync::CancellationToken;

/// 假的天气查询。真实 agent 里这里调真 API。
struct WeatherTool { def: ToolDefinition }

impl WeatherTool {
    fn new() -> Self {
        WeatherTool {
            def: ToolDefinition {
                name: "current_weather".into(),
                label: "Current Weather".into(),
                description: "查指定城市今天的天气，返回一句话总结。".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "city": { "type": "string", "description": "城市名，例如 \"Tokyo\"" }
                    },
                    "required": ["city"]
                }),
                execution_mode: None,
            },
        }
    }
}

#[async_trait]
impl AgentTool for WeatherTool {
    fn definition(&self) -> &ToolDefinition { &self.def }

    async fn execute(
        &self,
        _id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let city = args.get("city").and_then(|c| c.as_str()).unwrap_or("Unknown");
        // 教程里写死。真实场景换成 reqwest 调 OpenWeather 或类似。
        let summary = format!("{city} 今天：晴，22°C，微风。");
        Ok(AgentToolResult {
            content: vec![UserContent::text(summary)],
            details: serde_json::json!({ "city": city }),
            terminate: None,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 1. LLM provider（genai-backed；自动从环境变量读 ANTHROPIC_API_KEY 等）
    let stream = Arc::new(GenaiStream::builder().build());

    // 2. 挑模型——registry 知道它的 context window / 能力 / 价格等
    let registry = Registry::from_embedded_snapshot();
    let model = registry
        .to_core_model("anthropic/claude-sonnet-4-5")
        .expect("model in registry");

    // 3. 用自定义工具 + 一个聚焦的系统提示构造 Agent
    let mut opts = AgentOptions::new(model, stream);
    opts.system_prompt =
        "You are a weather assistant. Use the current_weather tool to look up cities. \
         Respond in one sentence.".into();
    opts.tools = vec![Arc::new(WeatherTool::new())];

    let agent = Agent::new(opts);

    // 4. 订阅事件流（可选——观察 agent 的实时动作）
    agent.subscribe(Arc::new(|event, _cancel| {
        Box::pin(async move { println!("[event] {event:?}"); })
    })).await;

    // 5. 跑一次 prompt，等循环跑完
    agent.prompt_text("京都今天天气怎么样？").await?;

    // 6. 看看最后的 transcript
    let state = agent.state().await;
    for msg in &state.messages {
        println!("--- {} ---", msg.role());
    }
    Ok(())
}
```

### 4.3 — 跑起来

```bash
export ANTHROPIC_API_KEY=...
cargo run
```

你会看到：

- `[event] ToolExecutionStart { tool_name: "current_weather", ... }`
- 接着 `[event] ToolExecutionEnd { ... }`
- 最后 `[event] MessageEnd`，里面是 Claude 的一句话回答。

这就是 agent 循环。LLM 看到你的 `current_weather` 工具，决定调它，你的代码返回假天气字符串，LLM 把它编进了最终回答。

### 4.4 — 接下来可以改什么

- **真的 HTTP 调用** —— 把 `execute()` 里的 `format!(...)` 换成 `reqwest::get(...)`。
- **更多工具** —— 加 `forecast_tool` / `historical_temperature_tool`，每个 push 进 `opts.tools`。
- **持久化** —— 设 `opts.session_id` + 用 `grain_agent_harness::JsonlSessionRepo` 把对话存到磁盘（[harness-session.md](./harness-session.md)）。
- **长对话不爆 context** —— 把 `grain_agent_harness::compaction_prepare_next_turn` 挂到 `opts.prepare_next_turn`，超长之前会自动总结老对话（[context-guard.md](./context-guard.md)）。
- **换 provider** —— 把模型 id 换成 `openai/gpt-4o` 或 `kimi/moonshot-v1-8k`。Kimi 走 OpenAI-compat preset 自动识别（[llm-genai.md](./llm-genai.md)）。

---

## 第五部分 — 卡住的时候

| 现象 | 八成原因 | 去哪查 |
|------|---------|--------|
| "no api key" / "auth failed" | 环境变量没设 | `grain-headless --doctor` 看检测到哪些 key |
| "unknown model" | 模型 id 写错了 | 用 `Registry::from_embedded_snapshot().iter()` 列一下，或者看 [llm-models.md](./llm-models.md) |
| Agent 死循环 | 工具老返回同样结果导致 LLM 卡在那儿 | 工具结果加 `terminate: Some(true)`，或者改短 prompt |
| 长对话之后丢上下文 | context window 满了 | 启用 [context-guard](./context-guard.md) 或 [compaction](./harness-messages.md) |
| 工具没被调 | 描述 / JSON-schema 不清楚 | 把工具描述写得像跟同事讲一样清楚 |
| stream 输出乱码 | UTF-8 / 终端问题 | 试 `--output json` 然后 `jq` 解析 |

真卡住了，先跑 `grain-headless --doctor`——一行命令能看到 workspace + env + registry 的全部状态。

---

## 第六部分 — 进阶文档

到这一步你已经会用了，剩下的看需要：

### 概念和核心 API

- [core-types.md](./core-types.md) —— 所有数据类型
- [core-stream.md](./core-stream.md) —— `LlmStream` trait（自己实现 provider 时才看）
- [core-agent-loop.md](./core-agent-loop.md) —— 底层循环 + 钩子
- [core-agent.md](./core-agent.md) —— 高层 `Agent` 封装

### Harness（会话、prompt、预算）

- [harness-messages.md](./harness-messages.md) —— 自定义消息扩展
- [harness-session.md](./harness-session.md) —— 会话树 + fork
- [harness-system-prompt.md](./harness-system-prompt.md) —— `<available_skills>` 块
- [harness-truncate.md](./harness-truncate.md) —— 输出截断
- [context-guard.md](./context-guard.md) —— context window 预算

### LLM provider 栈

- [llm-models.md](./llm-models.md) —— 模型 registry
- [llm-genai.md](./llm-genai.md) —— genai-backed `LlmStream`

### `grain-headless` 深入

每个 Phase 都加了 CLI 新能力，最权威的参考是 inline `--help`：

```bash
grain-headless --help
```

要点：

- 内置工具：始终在的 `read` / `list` / `glob` / `grep` / `source_info`；`--allow-write` 启用 `write` / `edit`；`--allow-bash` 启用 `bash`；`--allow-web` 启用 `web_fetch`；`--allow-semantic-search`（+ `--features rig` 编译时）启用 `semantic_search`。
- 输出：`--output text`（人读）或 `--output json`（每行一个事件，方便管道到 jq）。
- 持久化：`--session <path>` JSONL 对话跨次保留；`--telemetry-file <path>` 可选审计日志。
- `--interactive` 里的 slash 命令：`/help`、`/clear`、`/skills`、`/doctor`、`/source`、`/compact`、`/exit`。
- 配置文件：`.grain/config.toml`（per-workspace）或 `~/.config/grain/config.toml`（user）。
