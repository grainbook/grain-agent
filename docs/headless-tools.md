# Built-in tools

Every tool registered with `grain-headless` (or callable from your own agent via `coding_*_tools`). Path validation is enforced through a workspace-anchored `Workspace` struct (in `grain_ai_agent_headless::workspace`) — file tools refuse to read or write outside the workspace root.

中文版：[zh/headless-tools.md](./zh/headless-tools.md).

## Always available

### `read`

Read a UTF-8 text file with optional line-range trim.

```json
{ "path": "src/main.rs", "offset": 0, "limit": 200 }
```

Defaults: 2000-line head, 50 KiB cap. Output is suffixed with `[Truncated: kept N of M lines]` when over budget.

### `list`

List a directory's immediate entries.

```json
{ "path": "src" }
```

Returns directories first (suffix `/`), then files. Hidden entries included so the model sees `.gitignore` etc.

### `glob`

Find files by glob pattern. gitignore-aware via `ignore::WalkBuilder`.

```json
{ "pattern": "src/**/*.rs", "root": ".", "limit": 1000 }
```

### `grep`

Regex search across files. gitignore-aware. Returns `path:line:col: text` matches with per-file (200) and total (1000) caps.

```json
{ "pattern": "TODO", "root": "src", "file_glob": "*.rs", "max_matches": 200, "max_total": 1000 }
```

### `source_info`

Workspace git status — branch, commit, dirty file list. No `--allow-bash` required.

```json
{}
```

## `--allow-write`

### `write`

Create or overwrite a file. Parent directory must exist.

```json
{ "path": "src/main.rs", "content": "fn main() {}\n" }
```

Sequential execution mode by design — two parallel writes to the same path would race.

### `edit`

In-place plain-string search-and-replace. Fails loudly if `old` appears the wrong number of times.

```json
{ "path": "src/lib.rs", "old": "fn foo()", "new": "fn bar()", "expected_occurrences": 1 }
```

Refuses no-op edits (`old == new`).

## `--allow-bash`

### `bash`

Run a shell command via `/bin/sh -c`. Default 30s timeout, 5min hard cap. Combined stdout+stderr tail-truncated to 50 KiB.

```json
{ "command": "cargo test", "cwd": ".", "timeout_ms": 30000 }
```

`cwd` is resolved through the workspace; the command body itself can do anything a shell can do — this is **not a sandbox**. Run in a throwaway project or container.

## `--allow-web`

### `web_fetch`

HTTP/HTTPS GET with HTML stripping. Refuses private / loopback / link-local / CGNAT addresses, validates every redirect target the same way, caps the body at 512 KiB by default (2 MiB hard cap) via chunked streaming.

```json
{ "url": "https://example.com", "timeout_ms": 10000, "max_bytes": 524288 }
```

HTML entities are decoded **before** tag-stripping so escaped script tags can't survive into the LLM-visible output.

## `--allow-semantic-search` (requires `--features rig`)

### `semantic_search`

OpenAI-embedding-backed file similarity search. Lazy index build on first call (one document per file, ≤100 KiB, allowed extensions only); reused for the rest of the session.

```json
{ "query": "the function that handles authentication", "top_n": 5 }
```

Requires `OPENAI_API_KEY` for embeddings.

## Composition from code

Use the runtime helpers when wiring tools into your own agent:

```rust
use grain_ai_agent_headless::{
    coding_read_tools, coding_write_tools, coding_bash_tools, coding_web_tools,
    coding_all_tools, coding_full_tools, Workspace,
};

let workspace = Arc::new(Workspace::new("./my-project")?);

opts.tools = coding_read_tools(workspace.clone());                  // read-only
// or
opts.tools = coding_all_tools(workspace.clone());                   // + write/edit
// or
let mut tools = coding_full_tools(workspace.clone());               // + bash
tools.extend(coding_web_tools());                                   // + web_fetch
opts.tools = tools;
```
