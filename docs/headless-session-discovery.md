# `grain_ai_agent_headless::session_discovery`

Scans a directory of JSONL session files and returns metadata for each — title preview, model id, mtime. Used by the TUI's `/resume` overlay and available to any CLI tool that wants to list past sessions.

Pure I/O + parsing; no UI concerns.

---

## `SessionMeta`

```rust
pub struct SessionMeta {
    /// Session id derived from filename stem (the `<uuidv7>` part of `<uuidv7>.jsonl`).
    pub id: String,
    /// Absolute path on disk.
    pub path: PathBuf,
    /// First user prompt's text, clamped to `TITLE_PREVIEW_MAX` (80) chars.
    /// `None` when the session never recorded a user message.
    pub title: Option<String>,
    /// First assistant message's `model` field.
    /// `None` when no assistant turn has finished yet.
    pub model: Option<String>,
    /// Number of finalized messages on disk (user / assistant / tool-result combined).
    pub message_count: usize,
    /// File mtime — the picker sorts by this descending (most recent first).
    pub modified_at: SystemTime,
}
```

`SessionMeta::title_or_placeholder()` returns `title` or `"(empty session)"` for picker UIs.

---

## `new_session_path`

```rust
pub fn new_session_path(dir: &Path) -> PathBuf;
```

Generates a UUIDv7 id (sortable by creation time) and returns `dir / <id>.jsonl`. Caller is responsible for `create_dir_all(dir)` before opening the file.

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

Scans `dir` for `*.jsonl` files, parses each into a `SessionMeta`, and returns the list sorted by `modified_at` descending (most recent first).

- Non-`.jsonl` files are skipped.
- Files that fail to read / parse are skipped with a `[warn]` line — corruption in one doesn't hide the rest.
- Missing directory → returns an empty `Vec` so callers can use the path as "create on first session".

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

Reads a single JSONL session file and derives its metadata in one pass:

1. Opens the file and reads its `modified_at` mtime.
2. Scans each line for `AgentMessage` JSON.
3. Picks the first `User` message as `title` (clamped + whitespace-collapsed to 80 chars).
4. Picks the first `Assistant` message with a non-empty `model` field.
5. Counts all parseable messages.

Malformed individual lines are silently skipped — a file with no parseable messages still returns a valid `SessionMeta` with `title = None` and `message_count = 0`.

### Errors

Returns an I/O error when the file can't be opened / read. Malformed lines inside the file are **not** errors — they're skipped.

---

## `TITLE_PREVIEW_MAX`

```rust
pub const TITLE_PREVIEW_MAX: usize = 80;
```

Cap on `SessionMeta::title` length. Picker rows are width-limited; 80 chars wraps cleanly in most overlays.

---

## How the TUI uses it

1. On startup, if `--session` is not given, `grain-tui` calls `new_session_path(&sessions_dir)` and creates a fresh JSONL transcript — every run leaves a recoverable file.

2. When the user opens `/resume`, the worker calls `list_sessions(&sessions_dir)` and returns the list via `TuiEvent::SessionsListed`. The overlay renders each row with `title_or_placeholder()`, model, message count, and a humanized mtime.

3. Enter on a row prints a relaunch hint (`grain-tui --session <path>`) to the transcript. In-place session swap (Phase 4) will use `parse_session_meta` + `SessionWriter::open` to hot-reload without restarting.
