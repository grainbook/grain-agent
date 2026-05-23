# WebAssembly Plugin Guide

Grain supports plugins compiled as WebAssembly Component Model modules. Ship a `.wasm` file alongside your `plugin.toml` and grain loads it at runtime — no recompiling grain to add a plugin.

Chinese version: [zh/plugins-wasm.md](./zh/plugins-wasm.md).

---

## Prerequisites

- grain built with `--features wasm-plugins` (both `grain-ai-agent-headless` and `grain-ai-agent-tui`)
- For writing plugins: [cargo-component](https://github.com/bytecodealliance/cargo-component) + `rustup target add wasm32-wasip2`

---

## How it works

```text
.grain/plugins/my-tool/
  plugin.toml         # manifest with optional [wasm] section
  plugin.wasm         # compiled Component Model module
```

At startup, the plugin engine:
1. Scans `<workspace>/.grain/plugins/` for directories with `plugin.toml`
2. For each plugin with a `.wasm` module (either `plugin.wasm` by default, or the path named in `[wasm].module`), loads it via [wasmtime](https://wasmtime.dev/)
3. Calls the plugin's `init` export to get metadata
4. Calls `list-tools` to enumerate the plugin's tools
5. Wraps each tool as an `AgentTool` and appends it to the agent's tool list

---

## Writing a plugin

### 1. Create the project

```sh
cargo component new my-plugin --lib
cd my-plugin
```

### 2. Copy the WIT contract

Copy `grain-plugin-wasm/wit/grain-plugin.wit` into your project's `wit/` directory. This defines the interface your plugin must implement.

### 3. Implement the exports

```rust
// src/lib.rs
wit_bindgen::generate!({
    world: "grain-plugin",
    path: "wit",
});

struct MyPlugin;

impl Guest for MyPlugin {
    fn init() -> Result<PluginInfo, String> {
        Ok(PluginInfo {
            name: "my-plugin".to_string(),
            version: "0.1.0".to_string(),
        })
    }

    fn list_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "my_tool".to_string(),
            label: "My Tool".to_string(),
            description: "Does something useful".to_string(),
            parameters_json: r#"{
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "The input text"
                    }
                },
                "required": ["input"]
            }"#.to_string(),
        }]
    }

    fn call_tool(name: String, args_json: String) -> ToolResult {
        match name.as_str() {
            "my_tool" => {
                // Parse args, do work, return result
                ToolResult {
                    content_json: format!(r#"{{"result": "processed"}}"#),
                    is_error: false,
                }
            }
            _ => ToolResult {
                content_json: format!(r#"{{"error": "unknown tool: {name}"}}"#),
                is_error: true,
            },
        }
    }
}

export!(MyPlugin);
```

### 4. Build

```sh
cargo component build --release
```

The compiled component lands at `target/wasm32-wasip2/release/my_plugin.wasm`.

### 5. Install

```sh
mkdir -p <workspace>/.grain/plugins/my-plugin/
cp target/wasm32-wasip2/release/my_plugin.wasm \
   <workspace>/.grain/plugins/my-plugin/plugin.wasm
```

Create the manifest:

```toml
# <workspace>/.grain/plugins/my-plugin/plugin.toml
name = "my-plugin"
version = "0.1.0"
description = "My custom tool"

[wasm]
module = "plugin.wasm"
capabilities = ["log"]
```

---

## Manifest: `[wasm]` section

| Field | Type | Default | Description |
|---|---|---|---|
| `module` | string | `"plugin.wasm"` | Path to `.wasm` file relative to plugin root |
| `capabilities` | list | `["log"]` | Host capabilities the plugin needs |

When the `[wasm]` section is omitted but `<root>/plugin.wasm` exists on disk, the engine auto-detects it with default capabilities (`["log"]` only).

---

## Host capabilities

Each host function is gated by the plugin's declared capabilities. Calls into a denied capability return an error to the guest.

| Capability | Functions | Description |
|---|---|---|
| `log` | `log(level, msg)` | Emit log lines to stderr with `[level] wasm plugin 'name': msg` |
| `env` | `env-get(key)` | Read environment variables. Returns `none` when denied. |
| `http` | `http-get(...)`, `http-post(...)` | HTTP requests. Returns error string when denied. |

### Security model

- Plugins run in a sandboxed wasmtime instance with **no filesystem access** (WASI is linked but the context is empty)
- Each tool invocation gets a fresh `Store` — no shared mutable state between calls
- Host HTTP calls go through the grain process's network stack (subject to the same proxy/firewall rules)
- Environment variable access is opt-in — `env` must be listed in capabilities
- The plugin cannot access any resources not explicitly provided through the host interface

---

## WIT contract

The full contract is at `grain-plugin-wasm/wit/grain-plugin.wit`. Key types:

```wit
// Plugin exports (what you implement)
interface plugin {
    record tool-def {
        name: string,
        label: string,
        description: string,
        parameters-json: string,   // JSON Schema
    }
    record tool-result {
        content-json: string,
        is-error: bool,
    }
    record plugin-info {
        name: string,
        version: string,
    }
    init: func() -> result<plugin-info, string>;
    list-tools: func() -> list<tool-def>;
    call-tool: func(name: string, args-json: string) -> tool-result;
}

// Host imports (what grain provides)
interface host {
    log: func(level: log-level, msg: string);
    env-get: func(key: string) -> option<string>;
    http-get: func(...) -> result<http-response, string>;
    http-post: func(...) -> result<http-response, string>;
}
```

---

## Tool execution path

```text
Agent calls tool "my_tool"
  -> WasmTool::execute(args)
       -> serialize args to JSON string
       -> tokio::task::spawn_blocking
            -> create fresh wasmtime Store
            -> instantiate Component
            -> call guest init()
            -> call guest call-tool("my_tool", args_json)
            -> deserialize result
       -> return AgentToolResult to agent loop
```

The `spawn_blocking` ensures the synchronous wasmtime execution doesn't block the async agent loop.

---

## Limitations

- **No async in guest**: the plugin runs synchronously inside wasmtime. Long-running operations block the `spawn_blocking` thread.
- **Single-threaded per plugin**: each tool call gets its own Store; no concurrent access to plugin state.
- **No streaming**: tool results are returned as a single JSON string, not streamed.
- **JSON-in/JSON-out**: arguments and results are serialized as JSON strings. No binary protocol.
- **Fresh state per call**: each invocation creates a new Store and calls `init` again. Plugins cannot persist state between calls.
- **wasmtime version**: grain uses wasmtime 40.0.4 (MSRV: rustc 1.89.0). Plugins must be compatible with this version of the Component Model.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `wasm plugin(s) found but binary was built without --features wasm-plugins` | Feature not enabled | Rebuild with `--features wasm-plugins` |
| `wasmtime runtime init failed` | Engine configuration issue | Check wasmtime compatibility with your platform |
| `plugin init failed: ...` | Plugin's `init()` returned `Err` | Fix the plugin's init function |
| `http capability not granted` | Plugin called `http-get`/`http-post` without `"http"` in capabilities | Add `"http"` to `capabilities` in `plugin.toml` |

---

## Module layout

| Crate | Role |
|---|---|
| `grain-plugin-wasm` | Wasmtime runtime, host functions, `WasmTool` adapter |
| `grain-plugin-wasm/wit/` | WIT contract (`grain:plugin` world) |
| `grain-ai-agent-headless::plugins` | `WasmConfig` in manifest, `Plugin::wasm_module()` |
| `grain-ai-agent-tui::agent_worker` | Discovery + loading wiring (behind `wasm-plugins` feature) |
