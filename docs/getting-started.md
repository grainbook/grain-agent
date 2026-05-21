# Your first agent (5 minutes)

This tutorial assumes you've used ChatGPT or Claude as a user. **No prior agent / Rust-async experience needed.** By the end you'll have:

1. A working coding-agent talking to Claude on your machine (5 min).
2. A clear mental model of what's happening (10 min).
3. A custom agent in Rust code with your own tool (30 min).

中文版：[zh/getting-started.md](./zh/getting-started.md).

---

## Part 1 — What's an agent?

A regular chatbot is a one-shot answer machine:

> **You**: "What's in main.rs?"
> **Claude**: "I can't see files on your computer."

An **agent** is a chatbot with a tool belt. Your code gives the LLM a list of tools (functions it can call) and runs the conversation in a loop:

```
You         "What does main.rs do?"
  ↓
LLM         "I'll need to read it." → wants to call: read(path="main.rs")
  ↓
Your code   reads main.rs, sends back:    "<file contents>"
  ↓
LLM         "It's a hello-world program that..."
```

The LLM decides which tool to call; your code carries out the call. That back-and-forth loop is what `grain-agent` provides.

This repo gives you:

- **`grain-headless`** — a ready-to-run coding agent (read / list / search / write / shell tools, all built in).
- **`grain-agent-core` + `grain-agent-harness`** — Rust libraries to build your own agent.

We'll use both. Start with the CLI to see it work, then write your own agent.

---

## Part 2 — Run a coding agent in 5 minutes

### 2.1 — Build it

You need Rust (stable) and an Anthropic API key (any provider works — we'll use Claude here because the default model is Claude).

```bash
git clone <this-repo> grain-agent
cd grain-agent
cargo build --release -p grain-ai-agent-headless --bin grain-headless
```

The binary lands at `./target/release/grain-headless`. Symlink it onto your PATH if you like, or use the full path.

### 2.2 — Set your key

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

Grain auto-detects keys by environment variable. Other supported providers: `OPENAI_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `MOONSHOT_API_KEY`, more — see [llm-genai.md](./llm-genai.md) for the full table.

### 2.3 — Ask it about a real project

Point the CLI at any local code repo and ask a question:

```bash
./target/release/grain-headless \
    -C ~/code/some-project \
    --prompt "What does this project do? Look at the README and the main entry point."
```

You'll see the agent's reasoning live: tool calls (`→ read(...)`), tool results (`← read ...`), and the final answer. It read files on your behalf, fed them to Claude, and Claude answered with context.

### 2.4 — Let it edit code

Read-only by default. To let it touch files, pass `--allow-write`:

```bash
grain-headless -C ~/code/some-project --allow-write \
    --prompt "Add a CHANGELOG.md with an initial 'Unreleased' section."
```

To also let it run shell commands (e.g. `cargo test`):

```bash
grain-headless -C ~/code/some-project --allow-write --allow-bash \
    --prompt "Find and fix the failing test."
```

> ⚠️ `--allow-bash` lets the agent run any shell command. Run it in a project you can throw away, or inside a container, until you trust the model's behavior.

### 2.5 — Multi-turn chat

For longer back-and-forth, add `--interactive`:

```bash
grain-headless -C ~/code/some-project --interactive
```

You'll get a `> ` prompt. Type questions, press Enter. Type `/help` for built-in slash commands (`/skills`, `/doctor`, `/source`, `/clear`, `/exit`).

### 2.6 — Sanity check + diagnostics

Not sure if your environment is wired up? Run:

```bash
grain-headless -C . --doctor
```

It prints the workspace path, registered model count, which provider keys it sees in your env, and your git source info. No LLM calls — safe to run before keys are configured.

---

## Part 3 — How it works (the four pieces)

Now that you've seen it run, here's the mental model. There are only four moving parts:

| Piece | What it is | In `grain-agent` |
|------|-----------|------------------|
| **Model** | The LLM (Claude, GPT-4o, …) | `grain_agent_core::Model` |
| **Tools** | Functions the LLM can call | implement `AgentTool` |
| **System prompt** | Instructions the LLM gets at the start | `AgentOptions::system_prompt` |
| **Loop** | Code that runs the conversation | `grain_agent_core::Agent` |

Run the CLI again with **`--show-thinking`** to see the LLM's chain-of-thought streamed in dim text alongside its final answer. That's all the magic — the LLM thinks, picks a tool, your code runs the tool, the LLM continues.

---

## Part 4 — Build your own agent (with a custom tool)

We'll write a tiny agent that knows the current weather. It demonstrates the full pattern — add a tool, register it, run a prompt.

### 4.1 — New project

```bash
cargo new my-agent
cd my-agent
```

Edit `Cargo.toml`:

```toml
[package]
name = "my-agent"
version = "0.1.0"
edition = "2024"

[dependencies]
grain-agent-core    = { path = "../grain-agent/grain-agent-core" }
grain-llm-genai     = { path = "../grain-agent/grain-llm-genai" }
grain-llm-models    = { path = "../grain-agent/grain-llm-models" }
tokio        = { version = "1", features = ["rt-multi-thread", "macros"] }
async-trait  = "0.1"
tokio-util   = "0.7"
serde_json   = "1"
```

(Adjust the `path = "..."` to wherever you cloned grain-agent.)

### 4.2 — Write a tool

A tool is a struct that implements `AgentTool`. The trait has two pieces:

- **`definition()`** — name + description + JSON-Schema for arguments. The LLM reads this to decide when to call you.
- **`execute(args)`** — the actual function. Returns text the LLM will see as the tool result.

Replace `src/main.rs`:

```rust
use std::sync::Arc;
use async_trait::async_trait;
use grain_agent_core::{
    Agent, AgentOptions, AgentTool, AgentToolError, AgentToolResult,
    ToolDefinition, ToolUpdateCallback, UserContent,
};
use grain_llm_genai::GenaiStream;
use grain_llm_models::Registry;
use tokio_util::sync::CancellationToken;

/// Pretend weather lookup. In a real agent this would call a real API.
struct WeatherTool { def: ToolDefinition }

impl WeatherTool {
    fn new() -> Self {
        WeatherTool {
            def: ToolDefinition {
                name: "current_weather".into(),
                label: "Current Weather".into(),
                description: "Return today's weather for a city. Returns a one-line summary.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "city": { "type": "string", "description": "City name, e.g. \"Tokyo\"" }
                    },
                    "required": ["city"]
                }),
                execution_mode: None,
            },
        }
    }
}

#[async_trait]
impl AgentTool for WeatherTool {
    fn definition(&self) -> &ToolDefinition { &self.def }

    async fn execute(
        &self,
        _id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let city = args.get("city").and_then(|c| c.as_str()).unwrap_or("Unknown");
        // Fake answer for the tutorial. Replace with a real HTTP call.
        let summary = format!("In {city} today: sunny, 22°C, light breeze.");
        Ok(AgentToolResult {
            content: vec![UserContent::text(summary)],
            details: serde_json::json!({ "city": city }),
            terminate: None,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 1. The LLM provider (genai-backed; reads ANTHROPIC_API_KEY etc. from env)
    let stream = Arc::new(GenaiStream::builder().build());

    // 2. Pick a model — the registry knows its context window, capabilities, etc.
    let registry = Registry::from_embedded_snapshot();
    let model = registry
        .to_core_model("anthropic/claude-sonnet-4-5")
        .expect("model in registry");

    // 3. Build an Agent with our custom tool and a focused system prompt.
    let mut opts = AgentOptions::new(model, stream);
    opts.system_prompt =
        "You are a weather assistant. Use the current_weather tool to look up cities. \
         Respond in one sentence.".into();
    opts.tools = vec![Arc::new(WeatherTool::new())];

    let agent = Agent::new(opts);

    // 4. Subscribe to see what's happening (optional — pretty-prints events).
    agent.subscribe(Arc::new(|event, _cancel| {
        Box::pin(async move { println!("[event] {event:?}"); })
    })).await;

    // 5. Run a single prompt and wait for the loop to finish.
    agent.prompt_text("What's the weather in Kyoto?").await?;

    // 6. Inspect the final transcript.
    let state = agent.state().await;
    for msg in &state.messages {
        println!("--- {} ---", msg.role());
    }
    Ok(())
}
```

### 4.3 — Run it

```bash
export ANTHROPIC_API_KEY=...
cargo run
```

You'll see:

- An `[event] ToolExecutionStart { tool_name: "current_weather", ... }`
- Followed by `[event] ToolExecutionEnd { ... }`
- And a final `[event] MessageEnd` containing Claude's one-sentence answer.

That's the agent loop. The LLM saw your `current_weather` tool, decided to call it, your code returned a fake weather string, the LLM rolled that into its final answer.

### 4.4 — What you can change next

- **Real HTTP call** in `execute()` — swap `format!(...)` for a `reqwest::get(...)`.
- **More tools** — add a `forecast_tool`, a `historical_temperature_tool`. Push each onto `opts.tools`.
- **Persistence** — set `opts.session_id` and use `grain_agent_harness::JsonlSessionRepo` to save the transcript to disk between runs ([harness-session.md](./harness-session.md)).
- **Long conversations** — wire `grain_agent_harness::compaction_prepare_next_turn` into `opts.prepare_next_turn` so old turns get summarized before they blow the context window ([context-guard.md](./context-guard.md)).
- **Other providers** — point at `openai/gpt-4o` or `kimi/moonshot-v1-8k` instead. The OpenAI-compat preset handles Kimi out of the box ([llm-genai.md](./llm-genai.md)).

---

## Part 5 — When you hit a wall

| Symptom | Most likely cause | Where to look |
|---------|------------------|--------------|
| "no api key" / "auth failed" | env var not set | run `grain-headless --doctor` to see what's detected |
| "unknown model" | typo'd model id | check `Registry::from_embedded_snapshot().iter()` or [llm-models.md](./llm-models.md) |
| Agent loops forever | tool always returns same result, model never gets unstuck | add `terminate: Some(true)` to your tool result or shorten your prompt |
| Lost context after long chat | context window overflowed | enable [context-guard](./context-guard.md) or [compaction](./harness-messages.md) |
| Tool didn't get called | description / JSON-schema unclear | rewrite the tool description like you'd describe it to a colleague |
| Strange output mid-stream | UTF-8 / terminal issue | try `--output json` and pipe to `jq` |

When you're really stuck, run `grain-headless --doctor` first — it prints workspace + env + registry health in one shot.

---

## Part 6 — Reference docs

You now know enough to read the rest. Pick what you need:

### Concepts and core API

- [core-types.md](./core-types.md) — every data type
- [core-stream.md](./core-stream.md) — the `LlmStream` contract (only matters if you implement a custom provider)
- [core-agent-loop.md](./core-agent-loop.md) — the low-level loop, hooks
- [core-agent.md](./core-agent.md) — the high-level `Agent` wrapper

### Harness (sessions, prompts, budget enforcement)

- [harness-messages.md](./harness-messages.md) — custom-message extension
- [harness-session.md](./harness-session.md) — session tree + fork
- [harness-system-prompt.md](./harness-system-prompt.md) — `<available_skills>` rendering
- [harness-truncate.md](./harness-truncate.md) — output truncation
- [context-guard.md](./context-guard.md) — context-window budget enforcement

### LLM provider stack

- [llm-models.md](./llm-models.md) — the model registry
- [llm-genai.md](./llm-genai.md) — the `genai`-backed `LlmStream` implementation

### `grain-headless` (the CLI) deep dives

Every Phase added new CLI capabilities; the inline `--help` is the canonical reference:

```bash
grain-headless --help
```

Highlights:

- Built-in tools: `read` / `list` / `glob` / `grep` / `source_info` (always), `write` / `edit` (with `--allow-write`), `bash` (with `--allow-bash`), `web_fetch` (with `--allow-web`), `semantic_search` (with `--allow-semantic-search` + `--features rig` build).
- Outputs: `--output text` (human) or `--output json` (one event per line, for piping into jq).
- Persistence: `--session <path>` JSONL transcript across runs; `--telemetry-file <path>` opt-in audit log.
- Slash commands in `--interactive`: `/help`, `/clear`, `/skills`, `/doctor`, `/source`, `/compact`, `/exit`.
- Config files: `.grain/config.toml` (per-workspace) or `~/.config/grain/config.toml` (user) — see [config](./config.md).
