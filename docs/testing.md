# Testing

How to run, gate, and extend the test suites in this workspace.

中文版：[zh/testing.md](./zh/testing.md).

## Quick reference

| Command | What it runs |
|---------|-------------|
| `cargo test --workspace` | Every unit + integration test. Doesn't touch the network. |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lints + fmt-adjacent checks; CI gate. |
| `cargo test -p grain-llm-models --features fetch` | Adds tests for `Registry::fetch_models_dev` (still no network in the test). |
| `cargo test -p grain-ai-agent-headless --features rig` | Adds tests for the `semantic_search` tool wrapper (still offline). |
| `cargo test -p <crate> --test live -- --ignored` | Runs the live-integration suite against a real LLM provider — opt-in only, see below. |

The default `cargo test --workspace` invocation runs every gated and ungated test except the live suites. CI should run this plus the clippy lint.

## Live integration tests

Some crates have a `tests/live.rs` file with end-to-end tests that talk to a real LLM provider. They are **`#[ignore]`-gated** so they only run when you opt in with `--ignored`, and they additionally **skip with a printed note** when the required env var is missing — so `--ignored` is always safe to pass even with a partial setup.

### Setup

1. Copy the example file:
   ```bash
   cp .env.test.example .env.test
   ```
2. Fill in at least `DEEPSEEK_API_KEY=...` in `.env.test`. The headless live suite uses DeepSeek by default — it's cheap, fast, and supports tool calls.
3. `.env.test` is `.gitignored` — your key never leaves your machine.

Optional environment overrides (set in `.env.test` or your shell):

| Var | Default | Notes |
|-----|---------|-------|
| `GRAIN_LIVE_TEST_MODEL` | `deepseek/deepseek-chat` | Any model id from the embedded `models.dev` snapshot |
| `GRAIN_LIVE_TEST_WORKSPACE` | (cargo manifest dir of crate) | Pointed-at directory for tool-using live tests |
| `ANTHROPIC_API_KEY` | (unset) | Lets the Anthropic-specific live tests run |
| `OPENAI_API_KEY` | (unset) | Same for OpenAI |
| `MOONSHOT_API_KEY` | (unset) | Same for the OpenAI-compat Kimi test |
| `SILICONFLOW_API_KEY` | (unset) | Same for SiliconFlow |

### Run

```bash
# Just the headless live suite (DeepSeek round-trips, tool-call, workspace agent)
cargo test -p grain-ai-agent-headless --test live -- --ignored

# Single-threaded with full output (useful when debugging streaming behavior)
cargo test -p grain-ai-agent-headless --test live -- --ignored --nocapture --test-threads=1

# Just the genai live suite (also Anthropic / Kimi etc., depending on keys)
cargo test -p grain-llm-genai --test live -- --ignored
```

### What the headless live suite covers

`grain-ai-agent-headless/tests/live.rs` ships three end-to-end tests:

- **`live_simple_prompt_round_trip`** — bare text reply ("say pong"). Verifies the chat-only path.
- **`live_tool_call_round_trip`** — registers a synthetic `echo` tool, asserts the model invokes it with `stop_reason == ToolUse`. Verifies the outbound tool-schema + inbound tool-call event handling on a real provider.
- **`live_agent_with_workspace_tools_round_trip`** — builds a real `Agent` with `coding_read_tools` against a tempdir project; sends a "what language is this?" prompt; asserts the transcript contains at least one tool result without error.

Both genai-0.6 streaming quirks (cumulative argument chunks, string-encoded arguments) are exercised end-to-end here. If you ever pin to a newer genai version, run this suite — it catches breaking provider-side changes that unit tests miss.

## Provider-specific notes

### DeepSeek

- Fast and cheap; recommended default for the live suite.
- The 0.6 streaming behavior surfaced two real bugs in our code that didn't appear with 0.5 mock tests. Both are now fixed and continuously verified by `live_agent_with_workspace_tools_round_trip`.

### Anthropic (Claude)

- Use `GRAIN_LIVE_TEST_MODEL=anthropic/claude-haiku-4-5` for the cheapest live tests.
- Signed-thinking blocks are round-tripped via `thought_signatures` on the first outgoing tool call — see [llm-genai.md](./llm-genai.md).

### OpenAI

- Use `GRAIN_LIVE_TEST_MODEL=openai/gpt-4o-mini` for the cheapest live tests.

### Kimi (OpenAI-compatible)

- The `OpenAiCompatPreset::Common` preset must be enabled in your `GenaiStream::builder()` for the `kimi/...` model ids to route correctly. The headless `--allow-semantic-search` flag and the default GenaiStream do this automatically.

## Writing your own live tests

Follow the pattern in `grain-ai-agent-headless/tests/live.rs`:

```rust
fn require_env(key: &str, test_name: &str) -> Option<String> {
    load_env_test();
    let val = std::env::var(key).ok().filter(|s| !s.is_empty());
    if val.is_none() {
        eprintln!("[skip] {test_name}: {key} not set");
    }
    val
}

#[tokio::test]
#[ignore = "requires .env.test with a real provider key"]
async fn my_live_test() {
    let Some(_key) = require_env("DEEPSEEK_API_KEY", "my_live_test") else {
        return;
    };
    // … rest of the test
}
```

The `#[ignore]` attribute keeps the test out of the default `cargo test` run; the `require_env` skip path keeps `--ignored` runs safe when keys are missing.
