# JSONL 会话持久化

`grain_agent_harness::session_jsonl` 给 [`SessionRepo` / `SessionStorage`](./harness-session.md) trait 提供了一个目录到磁盘的实现。会话跨 CLI 调用、跨进程重启都不丢。

English version: [../session-jsonl.md](../session-jsonl.md).

## 磁盘布局

```
<root>/
  <session_id>/
    meta.json       # SessionMetadata
    entries.jsonl   # 一行一个 SessionTreeEntry，append-only
    state.json      # 可变的：{ "leafId": Option<String> }
```

- `entries.jsonl` 是事实来源。每行不可变，新 entry 原子追加。
- `state.json` 通过 temp + rename 写，crash 不会留半成品。
- Label 不需要单独文件——加载时重放 `SessionTreeEntryKind::Label` entries 重建。

## 崩溃恢复

append 流程是先写 `entries.jsonl` 再更新 `state.json`，所以中间崩溃会让 leaf cursor 指着旧的尖端、新 entry 已经在磁盘上了。`JsonlSessionStorage::open_or_init` 检测到不一致就**信** `entries.jsonl`——文件是事实来源，leaf cursor 只是缓存。

## 程序里用

```rust
use grain_agent_harness::{JsonlSessionRepo, SessionRepo};
use grain_agent_core::{AgentMessage, TextContent, UserContent, UserMessage};

let repo = JsonlSessionRepo::new("./.grain/sessions")?;

// 恢复已有会话，或创建新的。
let session = repo.create(Some("my-chat".into())).await?;
session.append_message(AgentMessage::user(UserMessage {
    content: vec![UserContent::Text(TextContent { text: "hi".into() })],
    timestamp: 0,
})).await?;

// 持久化分支：从某个 entry fork 出来。
let forked = repo
    .fork(&session.metadata().await, Some(&entry_id), ForkPosition::At, Some("alt".into()))
    .await?;

// 进程启动时重新打开：
let restored = repo.open(&SessionMetadata::with_id("my-chat")).await?;
let ctx = restored.build_context().await;
let mut opts = AgentOptions::new(model, stream_fn);
opts.messages = ctx.messages;        // 从上次的位置继续
```

## 列表 + 清理

```rust
let metas = repo.list().await?;                            // 按 id 排序
repo.delete(&SessionMetadata::with_id("trash")).await?;    // rm -rf 会话目录
```

## 另见

- [harness-session.md](./harness-session.md) — 树形数据模型 + `Session` API
- [harness-messages.md](./harness-messages.md) — 跨会话保留的自定义消息变体
- [compaction.md](./compaction.md) — 让长会话留在 context window 里
