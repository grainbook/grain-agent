# Context Compaction（上下文压缩）

长时间运行的 agent 早晚会塞满模型的 context window。**Compaction** 把对话历史的旧前缀折叠成一条总结，腾出空间又不丢主线。位于 `grain_agent_harness::compaction`。

English version: [../compaction.md](../compaction.md).

## 什么时候用它 vs `context_guard`

| 方式 | 做什么 | 啥时候选 |
|------|--------|---------|
| [`context_guard`](./context-guard.md) | 直接丢掉最老的消息 | 便宜，不调 LLM。会丢内容。 |
| `compaction` | 调 LLM **总结**旧前缀，再丢原始消息 | 每次触发要一次 summarization 调用。保留要点。 |

两者可以同时用——context_guard 当兜底，compaction 当首选路径。

## 一行接入

```rust
use std::sync::Arc;
use grain_agent_core::AgentOptions;
use grain_agent_harness::{
    MessageCountPolicy, compaction_prepare_next_turn, DEFAULT_COMPACTION_PROMPT,
};

let policy: Arc<dyn grain_agent_harness::CompactionPolicy> = Arc::new(
    MessageCountPolicy { threshold: 50, keep_recent: 10 }
);

let mut opts = AgentOptions::new(model, stream_fn.clone());
opts.prepare_next_turn = Some(compaction_prepare_next_turn(
    stream_fn,                                    // 复用同一个 provider 做总结
    policy,
    DEFAULT_COMPACTION_PROMPT.to_string(),
));
```

每轮结束后 wrapper 会检查阈值；超过就：

1. 用 prefix-to-compact + compaction prompt 调 summarizer。
2. 把 prefix 替换成一条 `compactionSummary` 自定义消息。
3. 在裁剪后的 transcript 上继续 loop。

## 默认策略：`MessageCountPolicy`

```rust
pub struct MessageCountPolicy {
    pub threshold: usize,   // 默认 40——消息条数到这个值触发
    pub keep_recent: usize, // 默认 8——最后 N 条始终保留
}
```

`prefix_len < 2` 时拒绝（没意义总结一条消息）。

## 自定义策略

```rust
struct TokenAwarePolicy { ... }

impl CompactionPolicy for TokenAwarePolicy {
    fn evaluate(&self, messages: &[AgentMessage]) -> Option<usize> {
        // 返回 Some(n) 表示压缩前 n 条；None 表示跳过本轮。
    }
}
```

## 直接 API

要完全控制（比如想用 slash 命令而不是自动触发），直接调 `compact_transcript`：

```rust
use grain_agent_harness::compact_transcript;

let new_messages = compact_transcript(
    &stream_fn,
    &model,
    &system_prompt,
    &current_messages,
    prefix_len,
    DEFAULT_COMPACTION_PROMPT,
    cancel_token,
).await?;
```

## 注意事项

- Summarization 本身要占模型 context 一部分。要早点压，留出空间。
- 总结质量 = 模型质量。prompt 写得烂 → 总结烂 → 主线丢。你的领域如果有要保留的关键信息，调整 `DEFAULT_COMPACTION_PROMPT`。
- Summarizer 流上 `AssistantMessageEvent::Error` 会被表面成 `CompactionError::StreamFailed`；wrapper 打一条 warning 并跳过本轮压缩，而不是用半截输出污染 transcript。
