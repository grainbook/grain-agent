# JSONL session persistence

`grain_agent_harness::session_jsonl` gives you a directory-on-disk implementation of the [`SessionRepo` / `SessionStorage`](./harness-session.md) traits. Sessions survive across CLI invocations and process restarts.

中文版：[zh/session-jsonl.md](./zh/session-jsonl.md).

## On-disk layout

```
<root>/
  <session_id>/
    meta.json       # SessionMetadata
    entries.jsonl   # one SessionTreeEntry per line, append-only
    state.json      # mutable: { "leafId": Option<String> }
```

- `entries.jsonl` is the source of truth. Lines are immutable; new entries get appended atomically.
- `state.json` is written via temp + rename, so crashes can't leave it half-written.
- Labels are reconstructed by replaying `SessionTreeEntryKind::Label` entries on load — no separate file.

## Crash recovery

The append flow writes to `entries.jsonl` *before* updating `state.json`, so a crash between the two leaves the leaf cursor pointing at the previous tip while the new entry is already on disk. `JsonlSessionStorage::open_or_init` detects this and trusts `entries.jsonl` — the file is the source of truth, the leaf cursor is just a cache.

## Programmatic use

```rust
use grain_agent_harness::{JsonlSessionRepo, SessionRepo};
use grain_agent_core::{AgentMessage, TextContent, UserContent, UserMessage};

let repo = JsonlSessionRepo::new("./.grain/sessions")?;

// Resume an existing session, or create a new one.
let session = repo.create(Some("my-chat".into())).await?;
session.append_message(AgentMessage::user(UserMessage {
    content: vec![UserContent::Text(TextContent { text: "hi".into() })],
    timestamp: 0,
})).await?;

// Persist branching: fork off from a specific entry.
let forked = repo
    .fork(&session.metadata().await, Some(&entry_id), ForkPosition::At, Some("alt".into()))
    .await?;

// Reload at process startup:
let restored = repo.open(&SessionMetadata::with_id("my-chat")).await?;
let ctx = restored.build_context().await;
let mut opts = AgentOptions::new(model, stream_fn);
opts.messages = ctx.messages;        // pick up where you left off
```

## Listing + cleanup

```rust
let metas = repo.list().await?;                  // sorted by session id
repo.delete(&SessionMetadata::with_id("trash")).await?;  // rm -rf the session dir
```

## See also

- [harness-session.md](./harness-session.md) — the tree data model + `Session` API
- [harness-messages.md](./harness-messages.md) — custom-message variants that survive across sessions
- [compaction.md](./compaction.md) — keep long sessions inside the context window
