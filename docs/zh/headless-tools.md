# 内置工具

`grain-headless` 默认注册的所有工具（也可以从你自己的 agent 里用 `coding_*_tools` 调）。路径校验通过 [`Workspace`](./harness-session.md) 强制——文件工具拒绝读/写工作目录之外。

English version: [../headless-tools.md](../headless-tools.md).

## 始终可用

### `read`

读 UTF-8 文本文件，支持按行截断。

```json
{ "path": "src/main.rs", "offset": 0, "limit": 200 }
```

默认 2000 行/50 KiB 上限；超量时追加 `[Truncated: kept N of M lines]`。

### `list`

列目录的直接子项。

```json
{ "path": "src" }
```

目录在前（后缀 `/`），然后是文件。包含隐藏文件，所以模型能看到 `.gitignore` 这种。

### `glob`

按 glob 找文件，尊重 .gitignore（用 `ignore::WalkBuilder`）。

```json
{ "pattern": "src/**/*.rs", "root": ".", "limit": 1000 }
```

### `grep`

跨文件 regex 搜索，尊重 .gitignore。返回 `path:line:col: text` 形式的匹配，per-file (200) 和 total (1000) 上限。

```json
{ "pattern": "TODO", "root": "src", "file_glob": "*.rs", "max_matches": 200, "max_total": 1000 }
```

### `source_info`

工作目录 git 状态——branch / commit / dirty 文件列表。**不需要** `--allow-bash`。

```json
{}
```

## `--allow-write`

### `write`

创建或覆盖文件。父目录必须存在。

```json
{ "path": "src/main.rs", "content": "fn main() {}\n" }
```

工具自身声明为 Sequential 执行模式——两个并行写同一个文件会 race。

### `edit`

原地纯字符串替换。如果 `old` 出现次数不匹配会大声失败。

```json
{ "path": "src/lib.rs", "old": "fn foo()", "new": "fn bar()", "expected_occurrences": 1 }
```

拒绝 no-op edit（`old == new`）。

## `--allow-bash`

### `bash`

通过 `/bin/sh -c` 跑 shell 命令。默认 30s 超时，最大 5min。stdout+stderr 合并后 tail-truncate 到 50 KiB。

```json
{ "command": "cargo test", "cwd": ".", "timeout_ms": 30000 }
```

`cwd` 经工作目录解析；命令本身能做任何 shell 能做的事——这**不是沙箱**。在能扔掉的项目或容器里跑。

## `--allow-web`

### `web_fetch`

HTTP/HTTPS GET + HTML 简化。拒绝 private / loopback / link-local / CGNAT 地址，每次 redirect 也走同一道校验，body 默认 512 KiB 上限（2 MiB 硬顶）通过 chunked stream 实现。

```json
{ "url": "https://example.com", "timeout_ms": 10000, "max_bytes": 524288 }
```

HTML entity 解码**先于**标签剥离，所以 escape 过的假 script 标签不会逃进 LLM 视野。

## `--allow-semantic-search`（需要 `--features rig`）

### `semantic_search`

OpenAI embedding 驱动的文件相似度搜索。首次调用时 lazy 建索引（一个文件一个文档，≤100 KiB，扩展名允许列表内），整个 session 复用。

```json
{ "query": "处理认证的函数", "top_n": 5 }
```

需要 `OPENAI_API_KEY` 做 embedding。

## 从代码组装

把工具接进自己的 agent 时用这些 runtime helper：

```rust
use grain_ai_agent_headless::{
    coding_read_tools, coding_write_tools, coding_bash_tools, coding_web_tools,
    coding_all_tools, coding_full_tools, Workspace,
};

let workspace = Arc::new(Workspace::new("./my-project")?);

opts.tools = coding_read_tools(workspace.clone());                  // 只读
// 或
opts.tools = coding_all_tools(workspace.clone());                   // + write/edit
// 或
let mut tools = coding_full_tools(workspace.clone());               // + bash
tools.extend(coding_web_tools());                                   // + web_fetch
opts.tools = tools;
```
