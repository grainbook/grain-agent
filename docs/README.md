# grain-agent docs

Rust port of [`@earendil-works/pi-agent-core`](https://github.com/earendil-works/pi). Four workspace crates:

- **`grain-agent-core`** — provider-agnostic agent runtime (messages, tools, events, loop, `Agent` wrapper).
- **`grain-agent-harness`** — engineering plumbing (session tree, custom messages, system prompt assembly, output truncation, context-window guard).
- **`grain-llm-models`** — standardized model registry (`models.dev`-backed snapshot, descriptor, capability flags, pricing).
- **`grain-llm-genai`** — `LlmStream` implementation backed by the [`genai`](https://crates.io/crates/genai) crate; builder + env-key resolver + OpenAI-compat presets.

中文版文档：[docs/zh/](./zh/).

## Start here

If you're new, read [getting-started.md](./getting-started.md) — walks from a runnable mock provider through tools, event subscription, session persistence, and a checklist for plugging in a real LLM provider.

## Module index

### grain-agent-core

| Module | Doc | What it is |
|--------|-----|------------|
| `types` | [core-types.md](./core-types.md) | Messages, tools, events, state primitives |
| `stream` | [core-stream.md](./core-stream.md) | `LlmStream` trait — the LLM provider injection point |
| `agent_loop` | [core-agent-loop.md](./core-agent-loop.md) | Low-level `run_agent_loop` / `run_agent_loop_continue` |
| `agent` | [core-agent.md](./core-agent.md) | High-level `Agent` wrapper: subscribe / abort / steer / follow-up |

### grain-agent-harness

| Module | Doc | What it is |
|--------|-----|------------|
| `messages` | [harness-messages.md](./harness-messages.md) | Custom messages (branch / compaction / custom) + harness `convert_to_llm` |
| `session` | [harness-session.md](./harness-session.md) | Session tree, storage trait, in-memory impl, branching + fork |
| `system_prompt` | [harness-system-prompt.md](./harness-system-prompt.md) | `<available_skills>` XML block renderer |
| `truncate` | [harness-truncate.md](./harness-truncate.md) | Tool-output head/tail truncation utilities |
| `context_guard` | [context-guard.md](./context-guard.md) | Registry-driven `transform_context` budget enforcement |

### LLM integration

| Crate | Doc | What it is |
|-------|-----|------------|
| `grain-llm-models` | [llm-models.md](./llm-models.md) | Model descriptor + registry, vendored models.dev snapshot, optional runtime fetch |
| `grain-llm-genai` | [llm-genai.md](./llm-genai.md) | `LlmStream` impl on top of `genai` 0.5: builder, env keys, OpenAI-compat routing |

## Quick start (mock provider)

```toml
# Cargo.toml
[dependencies]
grain-agent-core = { path = "grain-agent-core" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use std::sync::Arc;
use grain_agent_core::{Agent, AgentOptions, Model};

#[tokio::main]
async fn main() {
    let stream_fn: grain_agent_core::StreamFn = Arc::new(MyProvider::default());
    let model = Model {
        id: "gpt-4o".into(),
        name: "gpt-4o".into(),
        api: "openai".into(),
        provider: "openai".into(),
        ..Default::default()
    };

    let agent = Agent::new(AgentOptions::new(model, stream_fn));
    agent.prompt_text("hello").await.unwrap();
}
```

`MyProvider` implements [`LlmStream`](./core-stream.md). A full working example lives in `grain-agent-core/tests/smoke.rs` (`MockStream`).

## Quick start (real LLM via genai)

```toml
[dependencies]
grain-agent-core    = { path = "grain-agent-core" }
grain-llm-models    = { path = "grain-llm-models" }
grain-llm-genai     = { path = "grain-llm-genai" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use std::sync::Arc;
use grain_agent_core::{Agent, AgentOptions};
use grain_llm_genai::{GenaiStream, OpenAiCompatPreset};
use grain_llm_models::Registry;

#[tokio::main]
async fn main() {
    let stream = Arc::new(
        GenaiStream::builder()
            .with_openai_compat_preset(OpenAiCompatPreset::Common)
            .build(),
    );

    let model = Registry::from_embedded_snapshot()
        .to_core_model("anthropic/claude-sonnet-4-5")
        .unwrap();

    let agent = Agent::new(AgentOptions::new(model, stream));
    agent.prompt_text("hello").await.unwrap();
}
```

`ANTHROPIC_API_KEY` (or whichever provider) must be set in the environment. See [llm-genai.md](./llm-genai.md) for the full builder surface (env overrides, OpenAI-compat providers, provider router).
