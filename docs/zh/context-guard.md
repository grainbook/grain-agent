# `grain_agent_harness::context_guard`

一个 [`grain_agent_core::TransformContextFn`] —— 查 `grain_llm_models::Registry` 拿到模型的 context window，在每一轮请求前截断 transcript，确保 LLM 请求永远不会超预算。

挂在 `AgentOptions::transform_context`；在 `convert_to_llm` 之前每轮跑一次。

> English version: [../context-guard.md](../context-guard.md).

## 接入

```rust
use std::sync::Arc;
use grain_agent_core::AgentOptions;
use grain_agent_harness::context_guard::{ContextGuard, ContextGuardPolicy};
use grain_llm_models::Registry;

let registry = Arc::new(Registry::from_embedded_snapshot());

let guard = ContextGuard::new(registry, "anthropic/claude-sonnet-4-5")
    .with_policy(ContextGuardPolicy::DropOldest)
    .with_headroom_tokens(2048)    // 给 system prompt + 回复留余地
    .into_transform_fn();

let mut opts = AgentOptions::new(model, stream_fn);
opts.transform_context = Some(guard);
```

`headroom_tokens` 默认 1024——够小 system prompt + 短回复。system prompt 大或者预期长回复时调大。

## 策略

```rust
pub enum ContextGuardPolicy {
    DropOldest,           // 从头丢直到不超预算（默认）
    KeepRecent(usize),    // 超预算时只保留最后 N 条
    Identity,             // 永不截断（observe-only）
}
```

- `DropOldest` **永远保留至少 1 条** 消息——即使这条单独已经超预算；丢光会让 agent 循环跑不下去。
- `KeepRecent(n)` 仅在超预算时触发；不超预算时无论多少条都直接放行。
- `Identity` 是给只想观察 / 记录 overflow 但不动 transcript 的调用方（例如发个 metric 然后在别处处理）。

## token 估算

`TokenEstimator` 默认使用本地 tiktoken BPE 估算。`ContextGuard::new` 会尽量按模型选择 OpenAI tokenizer（例如 `gpt-4` → `cl100k_base`，GPT-4o/GPT-5/Codex 系列 → `o200k_base`），识别不到时回退到 `o200k_base`。如果要做确定性测试或特定 provider 调参，仍然可以使用 bytes-per-token fallback。

```rust
use grain_agent_harness::TokenEstimator;

let est = TokenEstimator::for_model("openai/gpt-5.1-codex");
let fallback = TokenEstimator::approximate();             // UTF-8 bytes / 4.0
let custom = TokenEstimator::with_bytes_per_token(2.5);    // provider 特定 fallback

let tokens = est.estimate_string("hello world");
let total  = est.estimate_messages(&transcript);
```

各 content 类型的成本：

| `AgentMessage` content | 计算方式 |
|---|---|
| `UserContent::Text` | 选中的 tokenizer，或 bytes / fallback 比率 |
| `UserContent::Image` | 定额 100 tokens |
| `AssistantContent::Text` | 选中的 tokenizer，或 bytes / fallback 比率 |
| `AssistantContent::Thinking` | 思考文本 + signature |
| `AssistantContent::ToolCall` | name + JSON 序列化后的 args |
| `AssistantContent::Image` | 定额 100 tokens |
| `ToolResultMessage`（文本 content） | 选中的 tokenizer，或 bytes / fallback 比率 |
| `AgentMessage::Custom(value)` | JSON 序列化后的 value |

在 guard 上覆盖 estimator：

```rust
ContextGuard::new(registry, "anthropic/claude-sonnet-4-5")
    .with_estimator(TokenEstimator::with_bytes_per_token(2.5))
    .into_transform_fn();
```

## 行为总览

1. 在 registry 里查 `model_id`。查不到 → guard 变成 **no-op**（防御式——绝不破坏循环）。
2. 计算 `budget = context_window - headroom_tokens - system_overhead_tokens`。
3. 估算 transcript 总 tokens。
4. 不超预算就原样返回。
5. 否则按策略处理。

`headroom` 只在 budget 为正时生效；`context_window = 0`（descriptor 字段未设）也短路成 no-op。

## 注意事项

- 本地 tokenizer 仍然不是 provider 的最终真值：非 OpenAI provider 可能用不同方式序列化 role、tools、images、reasoning。它适合做 preflight 安全阀；响应回来后仍应优先相信 provider reported `usage`。
- system prompt 和 tool schemas 不在 `messages` slice 里。请用 `with_system_overhead_tokens` 预扣；否则本地 transcript 看起来没超，真正序列化请求时仍可能溢出。
- hook 收到的是**过滤前**的 `AgentMessage` 快照。后续 `convert_to_llm` 可能再丢消息（例如 `Agent` 默认丢所有 custom），所以实际请求会比估算小——偏保守是更安全的方向。
