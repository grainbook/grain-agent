# 测试

如何在这个 workspace 里跑、门控、扩展测试。

English version: [../testing.md](../testing.md).

## 速查

| 命令 | 跑什么 |
|------|--------|
| `cargo test --workspace` | 所有 unit + integration 测试。**不联网**。 |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint + 格式相关检查；CI 必过。 |
| `cargo test -p grain-llm-models --features fetch` | 加上 `Registry::fetch_models_dev` 的测试（仍不联网）。 |
| `cargo test -p grain-ai-agent-headless --features rig` | 加上 `semantic_search` 工具的 wrapper 测试（仍 offline）。 |
| `cargo test -p <crate> --test live -- --ignored` | 真模型集成测试——opt-in，看下面。 |

默认 `cargo test --workspace` 跑所有 gate / 不 gate 的测试，**不**跑 live 套件。CI 应该跑这条 + clippy。

## Live 集成测试

某些 crate 有 `tests/live.rs`，里面是真模型端到端测试。它们全部 `#[ignore]` 门控——只有 `--ignored` 才跑；缺 key 时**单条测试自动 skip 并打印提示**，所以 `--ignored` 任何时候传都安全，即使只配了部分 provider。

### 准备

1. 复制 example：
   ```bash
   cp .env.test.example .env.test
   ```
2. 至少在 `.env.test` 填 `DEEPSEEK_API_KEY=...`。headless live 套件默认用 DeepSeek——便宜、快、支持 tool call。
3. `.env.test` 已 `.gitignore` —— key 不会进 commit。

可选环境变量（设在 `.env.test` 或 shell 里）：

| 变量 | 默认 | 说明 |
|------|------|------|
| `GRAIN_LIVE_TEST_MODEL` | `deepseek/deepseek-chat` | 任何内嵌 `models.dev` snapshot 里的 id |
| `GRAIN_LIVE_TEST_WORKSPACE` | (crate 的 cargo manifest dir) | 工具用 live 测试的目标目录 |
| `ANTHROPIC_API_KEY` | (未设) | 让 Anthropic 专属测试能跑 |
| `OPENAI_API_KEY` | (未设) | 同上，OpenAI |
| `MOONSHOT_API_KEY` | (未设) | OpenAI-compat Kimi 测试 |
| `SILICONFLOW_API_KEY` | (未设) | SiliconFlow 测试 |

### 跑

```bash
# 只跑 headless live 套件（DeepSeek round-trip / tool-call / workspace agent）
cargo test -p grain-ai-agent-headless --test live -- --ignored

# 单线程 + 全输出（调 streaming 行为时有用）
cargo test -p grain-ai-agent-headless --test live -- --ignored --nocapture --test-threads=1

# 只跑 genai 自己的 live 套件（看 key 选 Anthropic / Kimi 等）
cargo test -p grain-llm-genai --test live -- --ignored
```

### Headless live 套件覆盖

`grain-ai-agent-headless/tests/live.rs` 三个端到端测试：

- **`live_simple_prompt_round_trip`** —— 纯文本回答（"say pong"）。验证纯聊天路径。
- **`live_tool_call_round_trip`** —— 注册合成 `echo` 工具，断言模型调它且 `stop_reason == ToolUse`。验证 outbound 工具 schema + inbound tool-call 事件处理在真 provider 下工作。
- **`live_agent_with_workspace_tools_round_trip`** —— 用 `coding_read_tools` + tempdir 项目造真 Agent，发 "what language is this?" prompt，断言 transcript 里有 tool result 且无错误。

genai 0.6 两个 streaming quirks（累积 args chunks、string-encoded args）都靠这个套件端到端验证。如果以后升 genai 版本，跑一下这套件——unit test 抓不到的 provider-side 行为变化它能抓到。

## Provider 专属说明

### DeepSeek

- 快、便宜；live 套件默认。
- 0.6 streaming 行为暴露过我们代码两个真 bug（0.5 mock 测试没抓到）。现在都修了并由 `live_agent_with_workspace_tools_round_trip` 持续验证。

### Anthropic (Claude)

- `GRAIN_LIVE_TEST_MODEL=anthropic/claude-haiku-4-5` 最便宜。
- Signed-thinking block 通过首个外发 tool call 的 `thought_signatures` 回放——见 [llm-genai.md](./llm-genai.md)。

### OpenAI

- `GRAIN_LIVE_TEST_MODEL=openai/gpt-4o-mini` 最便宜。

### Kimi（OpenAI-compatible）

- `GenaiStream::builder()` 必须启用 `OpenAiCompatPreset::Common` 才能让 `kimi/...` model id 路由正确。headless 默认 GenaiStream 已经做了这个。

## 自己写 live 测试

照搬 `grain-ai-agent-headless/tests/live.rs` 的模式：

```rust
fn require_env(key: &str, test_name: &str) -> Option<String> {
    load_env_test();
    let val = std::env::var(key).ok().filter(|s| !s.is_empty());
    if val.is_none() {
        eprintln!("[skip] {test_name}: {key} not set");
    }
    val
}

#[tokio::test]
#[ignore = "requires .env.test with a real provider key"]
async fn my_live_test() {
    let Some(_key) = require_env("DEEPSEEK_API_KEY", "my_live_test") else {
        return;
    };
    // …测试逻辑
}
```

`#[ignore]` 把它从默认 `cargo test` 跑里排除；`require_env` 的 skip 路径让 `--ignored` 在缺 key 时不崩。
