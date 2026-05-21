# Telemetry

Opt-in local JSONL log of every `AgentEvent`. No network. Off unless `--telemetry-file <path>` is set.

中文版：[zh/telemetry.md](./zh/telemetry.md).

## What gets logged

Each line is one JSON object:

```json
{ "ts_ms": 1715000000000, "type": "tool_execution_start", "tool_call_id": "...", "tool_name": "read", "args": { "path": "src/main.rs" } }
```

The body after `ts_ms` is the `AgentEvent` serde-serialized form ([core-types.md](./core-types.md)). Every event variant flows through verbatim.

## Sensitive data warning

⚠️ **Telemetry payloads include user prompts and tool arguments / results.** If you typed an API key as part of a prompt, it ends up in the file. If a tool returned the contents of `.env`, that's in the file too. No redaction is applied — that's intentional, so callers needing a complete audit trail get one, but it means:

- Treat the telemetry file like shell history.
- Don't commit it to git.
- Run your own scrubbing pass before sharing.

## Programmatic use

```rust
use grain_ai_agent_headless::TelemetrySink;

let sink = std::sync::Arc::new(TelemetrySink::open("./events.jsonl")?);
let sink_clone = sink.clone();
agent.subscribe(std::sync::Arc::new(move |event, _signal| {
    let s = sink_clone.clone();
    Box::pin(async move { s.record(&event) })
})).await;
```

`TelemetrySink::record` never panics on I/O failure; it logs the problem to stderr and continues, so a failing append never takes the agent down.

## Reading the log

```bash
# Just see tool calls
jq 'select(.type == "tool_execution_start") | {ts: .ts_ms, tool: .tool_name, args: .args}' events.jsonl

# Per-turn token usage (when present)
jq 'select(.type == "turn_end") | {ts: .ts_ms, usage: .message.usage}' events.jsonl
```
