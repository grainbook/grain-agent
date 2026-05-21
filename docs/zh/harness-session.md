# `grain_agent_harness::session`

会话持久化的基础设施：把对话表示成一棵带 `parent_id` 链接的树，提供 `leaf_id` 游标标记“当前分支末端”，可以分支、fork、移动游标、压缩。

对应 TS 参考实现 `packages/agent/src/harness/session/*`（**不**包含 `jsonl-repo.ts`——磁盘持久化尚未移植，详见 `grain_agent_harness::lib` 顶部注释）。

## 数据模型

```rust
pub struct SessionMetadata {
    pub id: String,            // 默认 UUIDv7
    pub created_at: String,    // RFC3339-ish, 例 "2024-05-21T10:00:00.000Z"
    pub extra: serde_json::Value,  // backend 私有字段（例如文件路径）
}

pub struct SessionTreeEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    pub kind: SessionTreeEntryKind,
}
```

`SessionTreeEntryKind`（序列化为 `{ "type": "snake_case", ... }`）：

| 变体 | 含义 |
|------|------|
| `Message { message }` | 一条 `AgentMessage`（user / assistant / tool_result / custom） |
| `ThinkingLevelChange { thinking_level }` | 切换 thinking level |
| `ModelChange { provider, model_id }` | 切换模型 |
| `Compaction { summary, first_kept_entry_id, tokens_before, details?, from_hook? }` | 压缩：丢弃 `first_kept_entry_id` 之前的消息，留 summary |
| `Custom { custom_type, data? }` | 透明的应用层条目（不进入 LLM 上下文） |
| `CustomMessage { custom_type, content, display, details? }` | 自定义消息条目，会被 `build_session_context` 转成 `AgentMessage::Custom` 写入 transcript |
| `Label { target_id, label? }` | 给某条 entry 贴标签（`label = None` 表示移除） |
| `SessionInfo { name }` | 会话标题 |
| `BranchSummary { from_id, summary, details?, from_hook? }` | 来自旧分支的总结（被 `move_to(Some(summary))` 自动生成） |

每个 entry 拿 `kind.type_tag()` 取字符串名（用于 `find_entries(type_tag)`）。

## `SessionStorage` trait

```rust
#[async_trait]
pub trait SessionStorage: Send + Sync {
    async fn get_metadata(&self) -> SessionMetadata;
    async fn get_leaf_id(&self) -> Option<String>;
    async fn set_leaf_id(&self, leaf_id: Option<String>) -> Result<(), SessionError>;
    async fn get_entry(&self, id: &str) -> Option<SessionTreeEntry>;
    async fn get_entries(&self) -> Vec<SessionTreeEntry>;
    async fn get_path_to_root(&self, leaf_id: Option<&str>) -> Vec<SessionTreeEntry>;
    async fn append_entry(&self, entry: SessionTreeEntry) -> Result<(), SessionError>;
    async fn find_entries(&self, type_tag: &str) -> Vec<SessionTreeEntry>;
    async fn get_label(&self, id: &str) -> Option<String>;
    async fn create_entry_id(&self) -> String;
}
```

实现要求：

- `get_path_to_root(Some(leaf))` 沿 `parent_id` 链向上走，返回按时间顺序（root → leaf）的 entries；`get_path_to_root(None)` 返回空。
- `append_entry` 必须拒绝重复 id，并把 leaf 设为新 entry id；遇到 `Label` 条目同步维护 `labels` 映射。
- `create_entry_id` 默认用 UUIDv7（`grain_agent_harness::uuidv7()`）。

仓库目前提供 `InMemorySessionStorage`（`tokio::sync::Mutex` 保护内部状态）和 `InMemorySessionRepo`。JSONL 持久化按计划用同一个 trait 实现，**不要随意改 trait 形态**。

## `SessionRepo` trait

```rust
#[async_trait]
pub trait SessionRepo: Send + Sync {
    async fn create(&self, id: Option<String>) -> Result<Session, SessionError>;
    async fn open(&self, metadata: &SessionMetadata) -> Result<Session, SessionError>;
    async fn list(&self) -> Result<Vec<SessionMetadata>, SessionError>;
    async fn delete(&self, metadata: &SessionMetadata) -> Result<(), SessionError>;
    async fn fork(
        &self,
        source: &SessionMetadata,
        entry_id: Option<&str>,
        position: ForkPosition,
        id: Option<String>,
    ) -> Result<Session, SessionError>;
}

pub enum ForkPosition { Before, At }   // Default = Before
```

`fork` 行为：

- `entry_id = None` → 拷贝全部 entries。
- `position = At` → 新 session 的初始分支是 `path_to_root(entry_id)`，即包含 `entry_id` 本身。
- `position = Before` → 仅当 `entry_id` 是一条 user 消息时有效（其它情况返回 `SessionError::InvalidForkTarget`），新分支是该 entry 父节点的 `path_to_root`。
- 找不到 entry → `SessionError::InvalidForkTarget`。

## `Session` API

```rust
let session = repo.create(None).await?;
let meta = session.metadata().await;

session.append_message(agent_message).await?;
session.append_thinking_level_change("medium").await?;
session.append_model_change("openai", "gpt-4o").await?;
session.append_compaction("总结", first_kept_id, 12_345, None, None).await?;
session.append_custom("trace_marker", Some(json!({...}))).await?;
session.append_custom_message("artifact", json!("text"), /* display */ true, None).await?;
session.append_label(target_id, Some("important".into())).await?;
session.append_session_name("my chat").await?;

let id = session.leaf_id().await;
let entry = session.entry(&id.unwrap()).await;
let all   = session.entries().await;
let branch = session.branch(None).await;            // 当前 leaf 的 path-to-root
let other  = session.branch(Some(other_leaf)).await;
let label  = session.label(target_id).await;
let name   = session.session_name().await;          // 最后一条 SessionInfo
```

所有 `append_*` 返回新 entry 的 id，便于稍后引用（贴 label、compaction `first_kept_entry_id` 等）。

### 切分支与 branch summary

```rust
use grain_agent_harness::session::MoveToSummary;

session.move_to(Some(other_entry_id), None).await?;     // 仅移动游标
session.move_to(Some(other_entry_id), Some(MoveToSummary {
    summary: "用户最终选择了 A".into(),
    details: None,
    from_hook: None,
})).await?;                                              // 同时记录一条 BranchSummary
session.move_to(None, None).await?;                      // 回到 "before root"
```

`move_to(Some(id), Some(summary))` 会在目标 entry 之后追加一条 `BranchSummary` entry，`parent_id` 指向目标 entry——下次 `build_session_context` 时这条 BranchSummary 会被转成 [`branchSummary` 自定义消息](./harness-messages.md)。

如果 `entry_id` 不存在返回 `SessionError::NotFound`。

## `build_session_context`

把一条分支线性化成一个 `SessionContext`：

```rust
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,             // 默认 "off"
    pub model: Option<(String, String)>,    // (provider, model_id)
}

let ctx = session.build_context().await;        // 等价 build_session_context(&session.branch(None).await)
```

规则：

1. 遍历整条 path，按时序更新 `thinking_level`、`model`（`ModelChange` 或 assistant 消息内自带的 provider/model 都会更新）。
2. 寻找**最后一个** `Compaction` entry。
3. 若有 compaction：先 push 一条 `compactionSummary` 自定义消息，再从 `first_kept_entry_id` 起把后续 entries 收集进 `messages`；compaction 之后的 entries 整体追加。
4. 若没有 compaction：按顺序收集所有 `Message` / `CustomMessage` / `BranchSummary`（非空 summary）。`ThinkingLevelChange` / `ModelChange` / `Custom` / `Label` / `SessionInfo` 不进入 messages。

返回的 `SessionContext.messages` 直接喂给 `AgentOptions { messages, .. }` 或 `AgentContext.messages`。

## 错误

```rust
pub enum SessionError {
    NotFound(String),
    InvalidForkTarget(String),
    Storage(String),
    Other(String),
}
```

`append_entry` 检测到重复 id 返回 `Storage("duplicate entry id: ...")`。`append_label` 在 `target_id` 不存在时返回 `NotFound`。

## ID / 时间

```rust
pub fn uuidv7() -> String;                  // UUIDv7，按创建时间单调
```

时间戳用自定义 RFC3339 mini formatter（避免引入 `chrono`）——格式固定 `"YYYY-MM-DDTHH:MM:SS.mmmZ"`。`build_session_context` 内部把这种字符串解析回毫秒（`parse_iso_to_ms`），其它代码不依赖时区。

## 端到端示例

```rust
use grain_agent_harness::{InMemorySessionRepo, SessionRepo};
use grain_agent_core::{AgentMessage, TextContent, UserContent, UserMessage};

let repo = InMemorySessionRepo::new();
let session = repo.create(None).await?;
let meta = session.metadata().await;

session.append_message(AgentMessage::user(UserMessage {
    content: vec![UserContent::Text(TextContent { text: "hi".into() })],
    timestamp: 0,
})).await?;

session.append_session_name("first chat").await?;

let reopened = repo.open(&meta).await?;
let branch = reopened.branch(None).await;
assert_eq!(branch.len(), 2);   // 1 user 消息 + 1 session_info entry
assert_eq!(reopened.session_name().await.as_deref(), Some("first chat"));

let ctx = reopened.build_context().await;
assert_eq!(ctx.messages.len(), 1); // session_info 不进入 messages
```
