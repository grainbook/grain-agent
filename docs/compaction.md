# Context compaction

Long-running agents will eventually fill the model's context window. **Compaction** folds the old prefix of a transcript into a single summary message, freeing space without losing essential continuity. Lives in `grain_agent_harness::compaction`.

中文版：[zh/compaction.md](./zh/compaction.md).

## When to use this vs `context_guard`

| Approach | What it does | When to pick it |
|----------|-------------|-----------------|
| [`context_guard`](./context-guard.md) | Drops oldest messages outright | Cheap, no LLM call. Loses content. |
| `compaction` | Calls the LLM to *summarize* the old prefix, then drops the originals | Costs one summarization call per trigger. Keeps the gist. |

You can use both — context_guard as a final safety net, compaction as the preferred path.

## Quick wire-up

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
    stream_fn,                                    // reused as the summarizer
    policy,
    DEFAULT_COMPACTION_PROMPT.to_string(),
));
```

After each turn the wrapper checks the threshold; when exceeded it:

1. Calls the summarizer with the prefix to compact + the compaction prompt.
2. Inserts a `compactionSummary` custom message in place of the prefix.
3. Resumes the loop with the trimmed transcript.

## Default policy: `MessageCountPolicy`

```rust
pub struct MessageCountPolicy {
    pub threshold: usize,   // default 40 — trigger at this many messages
    pub keep_recent: usize, // default 8 — always keep last N intact
}
```

Refuses to compact if `prefix_len < 2` (no point summarizing one message).

## Custom policy

```rust
struct TokenAwarePolicy { ... }

impl CompactionPolicy for TokenAwarePolicy {
    fn evaluate(&self, messages: &[AgentMessage]) -> Option<usize> {
        // Return Some(n) to compact the first n messages, or None to skip.
    }
}
```

## Direct API

For full control (e.g. you want to compact based on a slash command instead of automatically), call `compact_transcript` directly:

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

## Caveats

- Summarization itself uses a chunk of the model's context window. Compact early enough that there's room.
- The summary's quality is the model's quality — bad prompt → bad summary → lost continuity. Tune `DEFAULT_COMPACTION_PROMPT` if your domain has specific things to preserve.
- An `AssistantMessageEvent::Error` from the summarizer is surfaced as `CompactionError::StreamFailed`; the wrapper logs a warning and skips this turn rather than corrupting the transcript with partial output.
