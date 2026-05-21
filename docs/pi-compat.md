# pi extension compatibility (`grain-pi-compat`)

Run [pi.dev-style extensions](https://pi.dev/docs/latest/extensions) on the grain agent runtime. Source-level shim on top of [`grain-script-boa`](./scripting.md) — no separate JS engine.

中文版：[zh/pi-compat.md](./zh/pi-compat.md).

---

## What works today

| pi API | Status |
|--------|--------|
| `export default (pi) => {...}` factory entry | ✅ |
| `pi.registerTool({ name, description, parameters, execute })` | ✅ |
| `pi.registerCommand(name, { description, handler })` | ✅ |
| `pi.registerShortcut(keys, { description, handler })` | ✅ (lib side; TUI key-dispatch wiring is a separate task) |
| `pi.on(event, handler)` | ✅ — see "events" below |
| `pi.ui.notify(text)` | ✅ — fire-and-forget toast |
| `pi.ui.confirm(prompt)` | ✅ — synchronous yes/no |
| `pi.ui.input(prompt)` | ✅ — synchronous text input |
| `pi.ui.select(prompt, items)` | ✅ — synchronous list pick |
| TypeScript (`.ts`) source | ⏸ blocked on dep conflict (swc ↔ ratatui share `unicode-width`); rename to `.js` for now |
| `ctx.newSession / fork / switchSession` | ❌ not in this layer |
| `pi.appendEntry` (session state) | ❌ |
| `session_start` / `session_shutdown` / `before_agent_start` / `input` events | ❌ — no direct grain equivalent |
| npm package extensions | ❌ — out of scope |

---

## Discovery

The loader scans (first existing path wins; ordering is alphabetical within each):

1. `<workspace>/.pi/extensions/*.js` — per-project.
2. `~/.pi/agent/extensions/*.js` — user-wide.

Missing directories are not errors. Use `PiExtension::from_dirs(&[...])` if you want explicit paths.

---

## How the shim works

For every pi extension file, the loader prepends a small JS shim that defines `pi` as an object whose methods translate pi's camelCase + field names (`parameters`, `execute`, …) onto grain's snake_case + field names (`schema`, `run`, …). When the source begins with `export default <expr>`, the shim also strips the `export default` keyword and wraps the expression in `(<expr>)(pi);` so the factory gets invoked with our `pi` object.

The transformed source is written to a `tempfile::TempDir`, then handed to `BoaExtension::from_scripts_dir(...)` — so all the heavy lifting (Boa worker, tool registration, callback dispatch) is shared with the [scripting layer](./scripting.md).

This means a pi extension copy-pasted from pi.dev's docs **runs unmodified** for the supported API, save for the `.ts` → `.js` rename.

---

## Events bridged into `pi.on(...)`

| pi event name | grain source | Payload shape |
|---------------|--------------|---------------|
| `agent_start` | `AgentEvent::AgentStart` | `{}` |
| `agent_end` | `AgentEvent::AgentEnd { messages }` | `{ message_count }` |
| `message_start` | `AgentEvent::MessageStart` | `{ role }` |
| `message_end` | `AgentEvent::MessageEnd` | `{ role }` |
| `tool_call` | `AgentEvent::ToolExecutionStart` | `{ tool_call_id, tool_name, args }` |
| `tool_result` | `AgentEvent::ToolExecutionEnd` | `{ tool_call_id, tool_name, is_error, content }` |

Unsupported names (`session_*`, `before_agent_start`, `input`) silently no-op when subscribed.

Wire one listener once at startup:

```rust
for listener in pi_ext.listeners() {
    agent.subscribe(listener).await;
}
```

---

## Rust API (`PiExtension`)

```rust
use grain_pi_compat::{PiExtension, PiNotification};

// Discovery
let ext = PiExtension::from_pi_dirs(workspace_root)?;
// or explicit:
let ext = PiExtension::from_dirs(&[PathBuf::from("./.pi/extensions")])?;

// Tools for AgentOptions::tools
let tools = ext.tools();

// Event bridge — wire into Agent::subscribe
for listener in ext.listeners() { agent.subscribe(listener).await; }

// Slash commands (TUI: merge with SLASH_CATALOG)
for cmd in ext.commands() { /* show in palette */ }
ext.invoke_command("audit", serde_json::json!({})).await?;

// Shortcuts (TUI: match against KeyEvent)
for sc in ext.shortcuts() { /* register with key dispatcher */ }
ext.invoke_shortcut("ctrl+x").await?;

// UI notification queue (poll per UI tick)
for note in ext.drain_notifications() {
    match note {
        PiNotification::Notify { text } => { /* render toast */ }
        PiNotification::Confirm { request_id, prompt } => {
            // show modal, then:
            ext.resolve_modal(request_id, serde_json::json!(true))?;
        }
        PiNotification::Input { request_id, prompt } => { /* … */ }
        PiNotification::Select { request_id, prompt, items } => { /* … */ }
    }
}
```

---

## Example pi extension (runs unmodified)

```js
// .pi/extensions/example.js
export default (pi) => {
  pi.registerTool({
    name: "shout",
    description: "Uppercases text",
    parameters: { type: "object", properties: { text: { type: "string" }}, required: ["text"] },
    execute: (a) => a.text.toUpperCase(),
  });

  pi.registerCommand("greet", {
    description: "Ask + greet",
    handler: () => {
      const who = pi.ui.input("Who are you?");
      const ok = pi.ui.confirm(`Hello, ${who}. Continue?`);
      if (!ok) { pi.ui.notify("aborted"); return; }
      const fruit = pi.ui.select("Favorite fruit?", ["apple", "banana", "cherry"]);
      pi.ui.notify(`${who} picked ${fruit}`);
    },
  });

  pi.registerShortcut("ctrl+x", {
    description: "Custom action",
    handler: () => { pi.ui.notify("ctrl+x pressed"); },
  });

  pi.on("agent_end", (event) => {
    pi.ui.notify(`agent finished after ${event.message_count} messages`);
  });
};
```

Every call in that file is wired and tested.

---

## Modal caveat

`pi.ui.confirm / input / select` block the Boa worker until the host resolves them. The host MUST eventually call `PiExtension::resolve_modal(request_id, value)` or the worker is stuck forever.

- **Safe inside command/shortcut handlers.** The user explicitly invoked the command and is sitting at the modal waiting.
- **Risky inside `pi.on(...)` listeners.** The agent's event listener is awaiting your `BoxFuture`; if it never resolves the modal, the agent stalls. Don't open modals from listeners unless you guarantee the host resolves them quickly.

---

## Testing

```bash
cargo test -p grain-pi-compat
```

20 unit + integration tests cover the full surface above: factory + top-level entry shapes, JS error surfacing, all event types, command + shortcut registration/dispatch, all four `ui.*` flows including the modal round-trip.

---

## Roadmap (deferred)

- **TypeScript source** — needs either a swc dep tree compatible with ratatui's `unicode-width` pin, or a switch to oxc / hand-rolled type stripper.
- **`ctx.*` parameter threading** — currently `pi.ui.notify(...)` is callable directly on `pi`. Pi extensions that explicitly use the `ctx` arg passed to handlers need a small adaptation.
- **`pi.appendEntry` / session state** — depends on session-aware extension persistence in `grain-agent-harness`.
- **Hot reload** — `notify`-based watcher around the tempdir + Boa context replacement.
