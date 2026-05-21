# `grain_agent_harness::messages`

提供三种 harness 内置的自定义消息（统一存为 `AgentMessage::Custom`），以及一个 harness 感知的 `convert_to_llm` 实现，把这些自定义消息转成发给模型的 `user` 文本。

对应 TS 参考实现 `packages/agent/src/harness/messages.ts`。Rust 端不能像 TS 那样做声明合并扩展 `CustomAgentMessages`，所以**所有**自定义变体都落到一个 JSON 值里、用顶层 `role` 字段做判别。

## 内置自定义消息

### `branchSummary`

切换到旧分支后，把被弃用分支的总结塞进新分支上下文，让模型知道“之前那条线说过啥”。

```rust
use grain_agent_harness::branch_summary_message;

let msg = branch_summary_message("用户最终选择了方案 A", "entry-abc", 1715000000000);
session.append_message(msg).await?;
```

转成 LLM 时被包成：

```
The following is a summary of a branch that this conversation came back from:

<summary>
用户最终选择了方案 A</summary>
```

（注意 `BRANCH_SUMMARY_SUFFIX` 没有前置换行——`prefix` 末尾的 `\n` 已经处理换行。）

### `compactionSummary`

会话被压缩（compaction）时，把被丢弃的早期消息总结成一段文本。`build_session_context` 在检测到 `Compaction` 树条目时会自动 prepend 一条 `compactionSummary` 消息（见 [session](./harness-session.md)）。也可以手动构造：

```rust
let msg = grain_agent_harness::compaction_summary_message(
    "用户问候、自我介绍、确认偏好",   // summary
    12_345,                            // tokens_before
    1715000000000,                     // timestamp
);
```

转成：

```
The conversation history before this point was compacted into the following summary:

<summary>
用户问候、自我介绍、确认偏好
</summary>
```

### `custom`

任意应用层 payload。`content` 可以是字符串、也可以是 `UserContent` 数组（要符合 `[{"type":"text",...}, {"type":"image",...}]` 形态）。

```rust
use grain_agent_harness::custom_message;
use serde_json::json;

let msg_text = custom_message(
    "artifact",
    json!("一段说明文本"),       // 字符串 content
    /* display */ true,
    /* details */ None,
    timestamp_ms,
);

let msg_blocks = custom_message(
    "artifact",
    json!([
        { "type": "text", "text": "图说" },
        { "type": "image", "data": "...", "mimeType": "image/png" }
    ]),
    true,
    None,
    timestamp_ms,
);
```

`display` 字段用于 UI：harness 端不做处理，应用决定要不要把它渲染给用户。`details` 是应用私有的元数据。

`custom_type` 用于业务侧分类（如 `"artifact"` / `"tool_invocation_summary"` …）；harness `convert_to_llm` 不会基于 `custom_type` 分流——所有 `custom` 都按统一规则变成 user 消息。

## `convert_to_llm`

```rust
pub fn convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message>;
```

行为：

- `AgentMessage::Standard(m)` → 原样保留。
- `AgentMessage::Custom(value)`：根据 `role`：
  - `"branchSummary"` → 包裹成 `BRANCH_SUMMARY_PREFIX` + `summary` + `BRANCH_SUMMARY_SUFFIX` 的 user 文本。
  - `"compactionSummary"` → 包裹成 `COMPACTION_SUMMARY_PREFIX` + `summary` + `COMPACTION_SUMMARY_SUFFIX` 的 user 文本。
  - `"custom"` → 如果 `content` 是字符串则转成单段 text；是 `UserContent` 数组则解析为多段；解析失败或为空就**整条丢弃**（不会出现在 LLM 上下文里）。
  - 其它 / 缺 `role` → 丢弃。

接入 `Agent`：

```rust
use std::sync::Arc;
use grain_agent_core::{AgentOptions, ConvertToLlmFn};
use grain_agent_harness::convert_to_llm;

let convert: ConvertToLlmFn = Arc::new(|msgs| Box::pin(async move { convert_to_llm(msgs) }));

let mut opts = AgentOptions::new(model, stream_fn);
opts.convert_to_llm = Some(convert);
```

## 常量

```rust
pub const BRANCH_SUMMARY_PREFIX: &str = "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
```

如果你要从外面定位 / 拆解这些 wrapper，直接用这些常量做 `starts_with` / `strip_prefix`，不要拷字面量。

## 扩展自定义消息

要新增一种 harness 级 custom 类型：

1. 在自己的 crate 里写 typed 构造函数，把消息序列化成 `serde_json::Value`，包到 `AgentMessage::Custom` 里——`role` 字段定义清楚。
2. 写自己的 `ConvertToLlmFn`：先尝试匹配自定义 `role`，未命中再 fallthrough 调 `grain_agent_harness::convert_to_llm`（或重新实现）。
3. 注入 `AgentOptions::convert_to_llm`。

不要尝试修改 `messages.rs` 把新变体硬塞进 harness 内置集合——`convert_to_llm` 的契约是只识别 `branchSummary` / `compactionSummary` / `custom`，扩展点放在调用方。
