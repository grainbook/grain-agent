# `grain_agent_harness::messages`

Three built-in custom-message variants (stored as `AgentMessage::Custom`) plus a harness-aware `convert_to_llm` that projects them into LLM user messages.

Corresponds to `packages/agent/src/harness/messages.ts` in the TS reference. Rust can't do TS-style declaration merging on `CustomAgentMessages`, so **every** custom variant lives in one JSON value with a top-level `role` discriminator.

中文版：[zh/harness-messages.md](./zh/harness-messages.md).

## Built-in custom messages

### `branchSummary`

Used after `move_to` switches the leaf cursor away from a branch — captures what the old branch was about so the new branch can be re-grounded.

```rust
use grain_agent_harness::branch_summary_message;

let msg = branch_summary_message("user chose option A", "entry-abc", 1715000000000);
session.append_message(msg).await?;
```

Wrapped on the wire as:

```
The following is a summary of a branch that this conversation came back from:

<summary>
user chose option A</summary>
```

### `compactionSummary`

Used by `build_session_context` when a `Compaction` tree entry exists — prepended automatically with the prior summary so the model still has context for what was dropped.

```rust
let msg = grain_agent_harness::compaction_summary_message(
    "user greeting, intros, preferences",
    12_345,                                // tokens_before
    1715000000000,                         // timestamp
);
```

Wrapped as:

```
The conversation history before this point was compacted into the following summary:

<summary>
user greeting, intros, preferences
</summary>
```

### `custom`

Arbitrary app-level payload. `content` can be a string or a `UserContent` array (must match `[{"type":"text",...}, {"type":"image",...}]` shape):

```rust
use grain_agent_harness::custom_message;
use serde_json::json;

let msg_text = custom_message(
    "artifact",
    json!("structured output blurb"),
    /* display */ true,
    /* details */ None,
    timestamp_ms,
);

let msg_blocks = custom_message(
    "artifact",
    json!([
        { "type": "text", "text": "caption" },
        { "type": "image", "data": "...", "mimeType": "image/png" }
    ]),
    true,
    None,
    timestamp_ms,
);
```

`display` is a UI hint — the harness doesn't act on it. `details` is opaque app metadata. `custom_type` is used by the app to categorize; the harness `convert_to_llm` treats all `custom` entries uniformly.

## `convert_to_llm`

```rust
pub fn convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message>;
```

- `AgentMessage::Standard(m)` → passed through.
- `AgentMessage::Custom(value)` by `role`:
  - `"branchSummary"` → wrapped user text with `BRANCH_SUMMARY_PREFIX`/`SUFFIX`.
  - `"compactionSummary"` → wrapped user text with `COMPACTION_SUMMARY_PREFIX`/`SUFFIX`.
  - `"custom"` → string `content` becomes single-text user message; array `content` parses to `UserContent`; empty / unparseable is **dropped**.
  - Other / no `role` → dropped.

Wiring into `Agent`:

```rust
use std::sync::Arc;
use grain_agent_core::{AgentOptions, ConvertToLlmFn};
use grain_agent_harness::convert_to_llm;

let convert: ConvertToLlmFn = Arc::new(|msgs| Box::pin(async move { convert_to_llm(msgs) }));

let mut opts = AgentOptions::new(model, stream_fn);
opts.convert_to_llm = Some(convert);
```

## Constants

```rust
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
```

Use these (rather than re-typing the literal text) when parsing or detecting these wrappers downstream.

## Extending with your own custom types

To add a new harness-level custom variant:

1. In your own crate, write a typed constructor that serializes into a `serde_json::Value` and wraps it in `AgentMessage::Custom`. Pick a unique `role` discriminator.
2. Write your own `ConvertToLlmFn`: match your `role`s first, then fall through to `grain_agent_harness::convert_to_llm` (or re-implement).
3. Inject your `ConvertToLlmFn` into `AgentOptions::convert_to_llm`.

Don't try to add new variants by modifying `messages.rs` — `convert_to_llm`'s contract is fixed (`branchSummary` / `compactionSummary` / `custom`); extension belongs at the caller.
