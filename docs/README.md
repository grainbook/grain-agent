# grain-agent docs

Rust port of [`@earendil-works/pi-agent-core`](https://github.com/earendil-works/pi). Five workspace crates:

- **`grain-agent-core`** — provider-agnostic agent runtime (messages, tools, events, loop, `Agent` wrapper).
- **`grain-agent-harness`** — engineering plumbing (session tree, custom messages, system prompt assembly, output truncation, context-window guard, **compaction**, **JSONL persistence**).
- **`grain-llm-models`** — standardized model registry (`models.dev`-backed snapshot, descriptor, capability flags, pricing).
- **`grain-llm-genai`** — `LlmStream` implementation backed by the [`genai`](https://crates.io/crates/genai) crate; builder + env-key resolver + OpenAI-compat presets.
- **`grain-ai-agent-headless`** — the `grain-headless` CLI binary + a coding-agent toolkit (file / shell / web / semantic-search tools, skills loader, JSONL session, telemetry, …).

中文版文档：[docs/zh/](./zh/).

---

## 👋 Start here

**Never built an agent before?** Read [getting-started.md](./getting-started.md). It walks from "what's an agent?" → run the bundled CLI → write your own custom tool in Rust. ~30 minutes total.

**Want the CLI reference right now?** [headless-cli.md](./headless-cli.md).

**Building a custom agent?** Skip to [core-agent.md](./core-agent.md) after the tutorial.

---

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
| `session_jsonl` | [session-jsonl.md](./session-jsonl.md) | JSONL directory-on-disk session persistence |
| `system_prompt` | [harness-system-prompt.md](./harness-system-prompt.md) | `<available_skills>` XML block renderer |
| `truncate` | [harness-truncate.md](./harness-truncate.md) | Tool-output head/tail truncation utilities |
| `context_guard` | [context-guard.md](./context-guard.md) | Registry-driven `transform_context` budget enforcement |
| `compaction` | [compaction.md](./compaction.md) | LLM-driven context summarization between turns |

### LLM integration

| Crate | Doc | What it is |
|-------|-----|------------|
| `grain-llm-models` | [llm-models.md](./llm-models.md) | Model descriptor + registry, vendored models.dev snapshot, optional runtime fetch |
| `grain-llm-genai` | [llm-genai.md](./llm-genai.md) | `LlmStream` impl on top of `genai` 0.5: builder, env keys, OpenAI-compat routing |

### grain-ai-agent-headless

| Surface | Doc | What it is |
|---------|-----|------------|
| `grain-headless` binary | [headless-cli.md](./headless-cli.md) | The ready-to-run CLI; all flags + slash commands |
| Tools (built-in) | [headless-tools.md](./headless-tools.md) | Every tool the CLI / library ships, with example arguments |
| Config file | [config.md](./config.md) | TOML config at `<workspace>/.grain/config.toml` + `~/.config/grain/config.toml` |
| Telemetry | [telemetry.md](./telemetry.md) | Opt-in local JSONL event log (with sensitive-data warning) |

---

## Quick start

### Just use the CLI

```bash
cargo build --release -p grain-ai-agent-headless --bin grain-headless
export ANTHROPIC_API_KEY=...
./target/release/grain-headless -C ./my-project --prompt "What does main.rs do?"
```

Add `--allow-write` to let it edit; `--allow-bash` to let it run shell; `--interactive` for multi-turn chat. Full reference: [headless-cli.md](./headless-cli.md).

### Build your own agent in code

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
use grain_llm_genai::GenaiStream;
use grain_llm_models::Registry;

#[tokio::main]
async fn main() {
    let stream = Arc::new(GenaiStream::builder().build());
    let model = Registry::from_embedded_snapshot()
        .to_core_model("anthropic/claude-sonnet-4-5")
        .unwrap();

    let agent = Agent::new(AgentOptions::new(model, stream));
    agent.prompt_text("hello").await.unwrap();
}
```

Full walkthrough with a custom tool: [getting-started.md](./getting-started.md).
