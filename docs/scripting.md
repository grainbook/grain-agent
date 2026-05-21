# JavaScript scripting (`grain-script-boa`)

A Boa-powered JS layer that lets users drop `.js` files into a directory and have them register agent tools (and more) at runtime — no rebuild required.

中文版：[zh/scripting.md](./zh/scripting.md).

For **pi.dev-style extensions** (`export default (pi) => {...}`, `pi.registerTool`, etc.), see [pi-compat.md](./pi-compat.md). pi-compat is a thin source-transform shim on top of this crate.

---

## What it is

- New workspace crate: `grain-script-boa`.
- Embeds [`boa_engine`](https://github.com/boa-dev/boa) (Rust-native ES2022+ interpreter, no JIT).
- Spawns a dedicated worker thread that owns one `boa_engine::Context` for its lifetime — the engine is `!Send`, so it can't be shared across tokio tasks.
- Talks to the rest of the agent via `mpsc` channels.
- Exposes itself as a set of `Arc<dyn AgentTool>` to be merged into the agent's tool list.

---

## Quickstart

Build the binary with the `scripts-boa` cargo feature on either `grain-ai-agent-headless` or `grain-ai-agent-tui`:

```bash
cargo install --path grain-ai-agent-headless --features scripts-boa
# or for the TUI
cargo build --release -p grain-ai-agent-tui --bin grain-tui --features scripts-boa
```

Drop a script into `<workspace>/.grain/scripts/`:

```js
// .grain/scripts/shout.js
grain.register_tool({
  name: "shout",
  description: "Uppercases the input text",
  schema: { type: "object", properties: { text: { type: "string" }}, required: ["text"] },
  run: (args) => args.text.toUpperCase(),
});
```

Run the agent:

```bash
DEEPSEEK_API_KEY=... grain-headless --prompt "use the shout tool to scream 'hello'"
```

When the agent starts, it logs `[info] loaded 1 JS tool(s) from .grain/scripts` and the `shout` tool is available alongside the built-in read/write/bash tools.

---

## CLI flags

| Flag | Default | Notes |
|------|---------|-------|
| `--scripts-dir <DIR>` | `<workspace>/.grain/scripts` | Override the discovery path. |
| `--features scripts-boa` (build time) | off | Gate. Without it, `--scripts-dir` is accepted but every script load surfaces a warning. |

Same flags work on `grain-headless` and `grain-tui`.

---

## JS API (the `grain` global)

The worker installs one global object `grain` with these methods:

| Method | Purpose |
|--------|---------|
| `grain.register_tool({ name, description, schema, run })` | Register an `AgentTool`. `run(args)` returns a string or `{ content, is_error? }`. |
| `grain.register_callback(name, fn)` | Generic named-callback slot. Higher layers (e.g. pi-compat events) use this. |
| `grain.register_meta(kind, name, attrs)` | Generic `(kind, name) → attrs` descriptor bag. Used by pi-compat for commands / shortcuts. |
| `grain.push_notification(payload)` | Fire-and-forget — host drains via `BoaExtension::drain_notifications()`. |
| `grain.modal_request(kind, payload)` | **Synchronous** round-trip. Blocks the worker until the host calls `resolve_modal`. Returns the resolved value to JS. |

The Boa runtime accepts ES2022+ (top-level `let`/`const`, arrow functions, destructuring, spread, optional chaining, modules). No JIT, no Node std lib, no npm — just the JS language.

---

## Rust API (`BoaExtension`)

```rust
use grain_script_boa::BoaExtension;

// Construction. Missing dir is fine (returns an empty extension).
// `.js` syntax errors → BoaExtensionError::ScriptLoad with diagnostic.
let ext = BoaExtension::from_scripts_dir("./.grain/scripts")?;

// Tools the scripts registered, ready to drop into AgentOptions::tools.
let tools: Vec<Arc<dyn AgentTool>> = ext.tools();

// Dispatch a named callback into JS.
let res: Result<(), String> = ext.invoke_callback("on:tool_call", json).await;

// Pump fire-and-forget notifications (e.g. once per UI tick).
let queued: Vec<serde_json::Value> = ext.drain_notifications();

// Snapshot kind-scoped descriptors (used by pi-compat for commands).
let metas: Vec<(String, Value)> = ext.list_metas("command");

// Resolve a synchronous modal previously initiated via grain.modal_request.
ext.resolve_modal(request_id, serde_json::json!(true))?;
```

`BoaExtension` is `Send + Sync` — `Arc<BoaExtension>` is the idiomatic shape for handing it to tasks / listeners.

---

## Concurrency model

- The Boa context lives on a single dedicated `std::thread`.
- Parent ↔ worker communication uses two `mpsc` channels:
  - `cmd_tx` for LoadScript / ListTools / InvokeTool / InvokeCallback / ListMetas.
  - `modal_tx` for `resolve_modal` responses.
- Notifications go through a **parent-side** channel (`notify_rx`), so `drain_notifications()` is safe to call even while the worker is blocked inside a synchronous modal.

**Modal caveat:** `grain.modal_request(...)` blocks the worker thread until the host resolves it. That means:

- Inside a **tool call** or **command handler** — fine. The user is already waiting for the agent's response.
- Inside an **event listener** — risky. The agent's progress (waiting on the listener's BoxFuture) will pause until the modal resolves. Don't call `pi.ui.confirm/input/select` from `pi.on(...)` handlers unless you're sure the host will resolve quickly.

---

## What's NOT in this crate

- **No file / HTTP / Node std**: scripts can't directly read files, fetch URLs, or call `require`. That keeps the sandbox tight. Add tools via Rust and have scripts call them.
- **No TypeScript**: today the worker reads `*.js` files literally. TS transpile via swc was attempted but blocked on a `unicode-width` version conflict with ratatui 0.29. See pi-compat docs for the workaround.
- **No async JS**: `grain.modal_request` is synchronous (blocks the worker). Promises work *inside* JS but no host-side awaiting.
- **No hot reload yet**: file changes don't auto-reload — restart the binary. A future patch can add `notify`-based watching.

---

## Testing

```bash
cargo test -p grain-script-boa
```

3 unit tests cover registration, execution, missing-dir, and syntax-error paths. Modal / callback / meta paths are tested through `grain-pi-compat`'s integration tests (20+ tests).
