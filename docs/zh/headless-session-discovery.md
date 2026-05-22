# `grain_ai_agent_headless::session_discovery`

扫描 JSONL 会话文件目录并返回每个文件的元数据 —— 标题预览、模型 id、修改时间。供 TUI 的 `/resume` 弹层使用，也可被任何需要列出历史会话的 CLI 工具调用。

纯 I/O + 解析；无 UI 依赖。

---

## `SessionMeta`

```rust
pub struct SessionMeta {
    /// 从文件名主干提取的 session id（`<uuidv7>.jsonl` 中的 `<uuidv7>` 部分）。
    pub id: String,
    /// 磁盘上的绝对路径。
    pub path: PathBuf,
    /// 第一条用户 prompt 文本，截断到 `TITLE_PREVIEW_MAX`（80）字符。
    /// 如果会话从未记录用户消息则为 `None`。
    pub title: Option<String>,
    /// 第一条 assistant 消息的 `model` 字段。
    /// 如果还没有完成的 assistant turn 则为 `None`。
    pub model: Option<String>,
    /// 磁盘上已完成的消息数（user / assistant / tool-result 合计）。
    pub message_count: usize,
    /// 文件修改时间 —— 选择器按此字段降序排列（最新在前）。
    pub modified_at: SystemTime,
}
```

`SessionMeta::title_or_placeholder()` 返回 `title` 或 `"(empty session)"`，供选择器 UI 使用。

---

## `new_session_path`

```rust
pub fn new_session_path(dir: &Path) -> PathBuf;
```

生成 UUIDv7 id（可按创建时间排序），返回 `dir / <id>.jsonl`。调用方需在打开文件之前执行 `create_dir_all(dir)`。

```rust
use grain_ai_agent_headless::new_session_path;
use std::path::Path;

let dir = Path::new("/tmp/sessions");
let path = new_session_path(dir);
assert_eq!(path.extension().and_then(|s| s.to_str()), Some("jsonl"));
```

---

## `list_sessions`

```rust
pub fn list_sessions(dir: &Path) -> Vec<SessionMeta>;
```

扫描 `dir` 中的 `*.jsonl` 文件，逐个解析为 `SessionMeta`，按 `modified_at` 降序（最新在前）返回。

- 非 `.jsonl` 文件被跳过。
- 读取 / 解析失败的文件以 `[warn]` 行跳过 —— 一个文件损坏不会影响其他文件。
- 目录不存在 → 返回空 `Vec`，调用方可将该路径视为「首次会话时创建」。

```rust
use grain_ai_agent_headless::list_sessions;

let sessions = list_sessions(Path::new("/tmp/sessions"));
for s in &sessions {
    println!("{} — {:?}", s.id, s.title);
}
```

---

## `parse_session_meta`

```rust
pub fn parse_session_meta(path: &Path) -> std::io::Result<SessionMeta>;
```

读取单个 JSONL 会话文件并在一次遍历中提取元数据：

1. 打开文件并读取 `modified_at` 时间戳。
2. 逐行扫描 `AgentMessage` JSON。
3. 取第一条 `User` 消息作为 `title`（截断并合并空白到 80 字符）。
4. 取第一条 `model` 非空的 `Assistant` 消息。
5. 统计所有可解析的消息数。

格式有误的单独行会被静默跳过 —— 没有任何可解析消息的文件仍会返回有效的 `SessionMeta`（`title = None`，`message_count = 0`）。

### 错误

文件无法打开 / 读取时返回 I/O 错误。文件内的格式有误行**不是**错误 —— 它们会被跳过。

---

## `TITLE_PREVIEW_MAX`

```rust
pub const TITLE_PREVIEW_MAX: usize = 80;
```

`SessionMeta::title` 的长度上限。选择器行宽有限；80 字符在大多数弹层中都能干净换行。

---

## TUI 如何使用

1. 启动时，如果未传 `--session`，`grain-tui` 调用 `new_session_path(&sessions_dir)` 创建新的 JSONL 会话记录 —— 每次运行都会留下可恢复的文件。

2. 用户打开 `/resume` 时，worker 调用 `list_sessions(&sessions_dir)` 并通过 `TuiEvent::SessionsListed` 回传列表。弹层逐行渲染 `title_or_placeholder()`、模型、消息数、人性化的修改时间。

3. 在某行上按 Enter 会向 transcript 打印重启提示（`grain-tui --session <路径>`）。第四阶段会实现就地会话切换，届时将使用 `parse_session_meta` + `SessionWriter::open` 实现热重载，无需重启。
