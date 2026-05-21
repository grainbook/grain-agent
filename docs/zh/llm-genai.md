# `grain_llm_genai`

基于 [`genai`](https://crates.io/crates/genai) 0.5 crate 的 `grain_agent_core::LlmStream` 实现。把 transport-agnostic 的 agent 循环桥接到 genai 的多 provider chat API。

这是你接入真实 LLM 时用来连到 `AgentOptions::stream_fn` 的 crate。

> English version: [../llm-genai.md](../llm-genai.md).

## 快速开始

```rust
use std::sync::Arc;
use grain_agent_core::{Agent, AgentOptions};
use grain_llm_genai::GenaiStream;
use grain_llm_models::Registry;

let stream: Arc<GenaiStream> = Arc::new(GenaiStream::new());
let model = Registry::from_embedded_snapshot()
    .to_core_model("anthropic/claude-sonnet-4-5")
    .unwrap();

let agent = Agent::new(AgentOptions::new(model, stream));
agent.prompt_text("hello").await?;
```

`GenaiStream::new()` 用 `genai::Client::default()`（env-var 认证、prefix-based provider 检测）+ 我们的 [`baseline_chat_options`]。

需要自定义的话用 builder。

## Builder

```rust
use grain_llm_genai::{GenaiStream, GenaiStreamBuilder, OpenAiCompatPreset};
use std::sync::Arc;
use grain_llm_models::Registry;

let stream = GenaiStream::builder()
    .with_openai_compat_preset(OpenAiCompatPreset::Common)   // kimi + siliconflow
    .with_env_override("openai", "MY_OPENAI_KEY")            // 覆盖 env var 名
    .with_registry(Arc::new(Registry::from_embedded_snapshot()))
    .build();
```

builder 配置 `genai::Client`，挂上 auth resolver（基于 env-var 的 key 查找）和 service-target resolver（OpenAI-compat endpoint 重写）。默认值合理：`EnvKeyResolver::default_mapping()` 覆盖所有 genai 原生 provider；`OpenAiCompatPreset::None`（空）；`ProviderRouter::default()` 将 `google` → `gemini`、`zhipu` → `bigmodel`、`moonshot` → `kimi`。

## 模型 id 格式

grain 的 id 形如 `"<provider>/<model>"`（如 `"anthropic/claude-sonnet-4-5"`）；genai 按 `"<namespace>::<model>"` 分派。`stream()` 内部自动翻译：

| grain id | translated | genai adapter |
|---|---|---|
| `anthropic/claude-sonnet-4-5` | `anthropic::claude-sonnet-4-5` | Anthropic native |
| `openai/gpt-4o` | `openai::gpt-4o` | OpenAI native |
| `google/gemini-2.0-flash` | `gemini::gemini-2.0-flash` | Gemini native（router 重命名） |
| `zhipu/glm-4-plus` | `bigmodel::glm-4-plus` | BigModel native（router 重命名） |
| `kimi/moonshot-v1-128k` | `kimi::moonshot-v1-128k` | OpenAI adapter，Kimi endpoint（compat preset） |
| `gpt-4o`（无 `/`） | `gpt-4o` | genai 自动检测 |

通过 `with_provider_router(ProviderRouter::new().with_override(...))` 自定义路由。

## OpenAI-compat 路由

当模型 id 的 namespace 命中注册过的 `OpenAiCompatEndpoint` 时，builder 的 `service_target_resolver` 改写请求：

- `endpoint` → preset 的 `base_url`
- `auth` → 读 preset 的 env var
- `adapter_kind` → `OpenAI`（说 OpenAI 的 wire 协议）
- 模型名 → 去掉 namespace 前缀再发送

`OpenAiCompatPreset::Common` 现成的：

| id | base URL | env var |
|---|---|---|
| `kimi` | `https://api.moonshot.cn/v1` | `MOONSHOT_API_KEY` |
| `siliconflow` | `https://api.siliconflow.cn/v1` | `SILICONFLOW_API_KEY` |

要加更多：

```rust
.with_openai_compat(OpenAiCompatEndpoint::new(
    "my-host", "https://api.example.com/v1", "MY_HOST_API_KEY",
))
```

genai 0.5 原生支持 Anthropic、OpenAI、Gemini、DeepSeek、Groq、Mimo、Nebius、xAI、Zai、BigModel（Zhipu）、Cohere、Together、Fireworks、Ollama——它们**有意不在** OpenAI-compat preset 里，覆盖它们会覆掉原生 adapter 的 per-provider quirks。

## env-based API key

`EnvKeyResolver::default_mapping()` 覆盖 19 个 provider（所有 genai 原生 + OpenAI-compat preset）。自定义：

```rust
let resolver = grain_llm_genai::EnvKeyResolver::default_mapping()
    .with_override("openai", "MY_OPENAI_KEY")
    .with_override("acme",   "ACME_LLM_KEY");

let stream = GenaiStream::builder().with_env_resolver(resolver).build();
```

builder 的 `auth_resolver` 优先查这张表，未命中回退到 genai 自己的默认查找。

## 流式事件

`GenaiStream::stream(...)` 返回 `Pin<Box<dyn Stream<Item = AssistantMessageEvent>>>`，事件序列合规：

1. 仅一次 `Start { partial }`
2. 每个内容块：`TextStart` / `TextDelta` / `TextEnd`、`ThinkingStart` / `ThinkingDelta` / `ThinkingEnd`、或 `ToolcallStart` / `ToolcallEnd`
3. 仅一次终止 `Done { result }` 或 `Error { error, result }`

内部 `mapping::inbound::InboundState` 是个小状态机：
- 同类型连续 chunk 聚合成一个块
- 类型切换时关闭当前块
- Anthropic 风格 `ThoughtSignatureChunk` 静默合入开放的 `Thinking` 块的 `signature` 字段
- 从累积 content 推断 stop_reason（任意 tool call → `ToolUse`，否则 `Stop`）合成 `Done`

## Thinking / reasoning 回放

双向都接通：

- **入站**：`ReasoningChunk` → `AssistantContent::Thinking`；`ThoughtSignatureChunk` 写到 `signature`。PR 3b 状态机处理记账。
- **出站**：当 `AssistantMessage` 有带 signature 的 `Thinking` 块时，signature 挂到**第一个**出站 `ToolCall::thought_signatures`。这就是 Anthropic 用来验证多轮签名 thinking 的字段——少了它，多轮签名流程会崩。

**reasoning 文本有意不回送给 provider。** genai 0.5 没有出站 `reasoning_content` 槽位，并且 provider 每轮自己重新生成 reasoning（OpenAI o-series、DeepSeek-R1）。文本仍在 grain transcript 里供 app 用（UI 展示、审计……），但不上 wire。

## 取消

实现把 genai stream 与你传入的 `CancellationToken` 在 `tokio::select!` 里赛跑。取消触发时：

1. 内部 genai stream 被丢弃（不再被 poll）。
2. 状态机产出终止 `Error` 事件，`stop_reason = Aborted`、`error_message = "aborted"`，已接收的 partial content 保留。

## Live tests

`tests/live.rs` 含 5 个 `#[ignore]` 门控的真实 provider endpoint 测试（OpenAI、Anthropic、OpenAI-compat Kimi、外加取消竞速）。当你改 outbound mapper、inbound 状态机、或 builder 时手动跑一下：

```bash
ANTHROPIC_API_KEY=... cargo test -p grain-llm-genai --test live -- --ignored
```

每个测试在缺 env var 时会打印 skip 提示，所以 `--ignored` 任何时候传都是安全的。

## 注意事项

- `genai 0.5` 的 `ServiceTarget` resolver 是 sync 的，auth/target 解析时跑不了 async work（DNS、OAuth 刷新）。如果你需要 async key 查找，先在 agent 调用前做好再以 env / custom resolver 注入。
- provider router 只处理 namespace 翻译；重命名也存在 `grain-llm-models::Registry` 里（如 `provider: "google"`、`api: "gemini"`）。两边自定义时记得同步。
- OpenAI-compat preset 用 `"kimi"` 和 `"siliconflow"` 作为 id；models.dev 的 catalog 用 `"moonshotai"` 表示 Kimi 原生 endpoint。如果你直接用 models.dev 的 id，请额外注册 `OpenAiCompatEndpoint { id: "moonshotai", ... }`。
