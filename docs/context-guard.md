# `grain_agent_harness::context_guard`

A [`grain_agent_core::TransformContextFn`] that consults a `grain_llm_models::Registry` for the model's context window and truncates the transcript before each turn so the LLM request never exceeds budget.

Wired in as `AgentOptions::transform_context`; runs once per turn just before `convert_to_llm`.

## Wiring

```rust
use std::sync::Arc;
use grain_agent_core::AgentOptions;
use grain_agent_harness::context_guard::{ContextGuard, ContextGuardPolicy};
use grain_llm_models::Registry;

let registry = Arc::new(Registry::from_embedded_snapshot());

let guard = ContextGuard::new(registry, "anthropic/claude-sonnet-4-5")
    .with_policy(ContextGuardPolicy::DropOldest)
    .with_headroom_tokens(2048)    // reserve for system prompt + completion
    .into_transform_fn();

let mut opts = AgentOptions::new(model, stream_fn);
opts.transform_context = Some(guard);
```

`headroom_tokens` defaults to 1024 — enough for a small system prompt and a short reply. Bump it if you have a large system prompt or expect long completions.

## Policies

```rust
pub enum ContextGuardPolicy {
    DropOldest,           // drop from the head until under budget (default)
    KeepRecent(usize),    // keep only the last N messages when over budget
    Identity,             // never truncate (observe-only)
}
```

- `DropOldest` **always keeps at least one message** even when that lone message blows the budget — losing the entire transcript would break the agent loop.
- `KeepRecent(n)` only kicks in when the transcript exceeds the budget; under budget, it passes through unchanged regardless of message count.
- `Identity` is for callers that want to inspect / log overflow without mutating the transcript (e.g. emitting a metric, then handling overflow elsewhere via another hook).

## Token estimation

`TokenEstimator` is a fixed chars-per-token approximation (default 4.0). Good enough for budget enforcement; swap in a tokenizer-backed estimator when worth the cost.

```rust
use grain_agent_harness::TokenEstimator;

let est = TokenEstimator::approximate();         // 4.0 chars / token
let custom = TokenEstimator::with_chars_per_token(2.5);  // CJK-heavy

let tokens = est.estimate_string("hello world");
let total  = est.estimate_messages(&transcript);
```

Per-content cost:

| `AgentMessage` content | counted as |
|---|---|
| `UserContent::Text` | chars / ratio |
| `UserContent::Image` | flat 100 tokens |
| `AssistantContent::Text` | chars / ratio |
| `AssistantContent::Thinking` | thinking text + signature, each chars / ratio |
| `AssistantContent::ToolCall` | name + JSON-serialized args, chars / ratio |
| `AssistantContent::Image` | flat 100 tokens |
| `ToolResultMessage` (text content) | chars / ratio |
| `AgentMessage::Custom(value)` | JSON-serialized value, chars / ratio |

Override the estimator on the guard:

```rust
ContextGuard::new(registry, "anthropic/claude-sonnet-4-5")
    .with_estimator(TokenEstimator::with_chars_per_token(2.5))
    .into_transform_fn();
```

## Behavior summary

1. Look up `model_id` in the registry. If not found, the guard becomes a **no-op** (defensive — never breaks the loop).
2. Compute `budget = context_window - headroom_tokens`.
3. Estimate total transcript tokens.
4. If under budget, return unchanged.
5. Otherwise apply the policy.

The headroom only matters when the budget is positive; `context_window = 0` (an unset descriptor field) also short-circuits to a no-op.

## Caveats

- The chars-per-token approximation is **conservative-on-average but not bounded**: CJK or code can push real tokenization 1.5–2× higher than the estimate. Raise `chars_per_token` or shrink the headroom if you see truncation that's too aggressive (or, conversely, requests that still overflow).
- The system prompt isn't visible at this hook (it lives on `AgentContext.system_prompt`, not in `messages`). Leave room for it via `headroom_tokens`.
- The hook receives the **filtered** `AgentMessage` snapshot. If a `convert_to_llm` later drops messages (e.g. all custom variants in `Agent`'s default), the actual request will be smaller than what we counted — being over-conservative is the safer direction.
