# Telemetry

可选的本地 JSONL 日志，每个 `AgentEvent` 一行。**不上网**。`--telemetry-file <path>` 不设就不启用。

English version: [../telemetry.md](../telemetry.md).

## 日志内容

每行一个 JSON 对象：

```json
{ "ts_ms": 1715000000000, "type": "tool_execution_start", "tool_call_id": "...", "tool_name": "read", "args": { "path": "src/main.rs" } }
```

`ts_ms` 之后是 `AgentEvent` 的 serde 序列化形态（见 [core-types.md](./core-types.md)）。所有 event variant 都原样写。

## 敏感数据警告

⚠️ **telemetry 内容包含用户 prompt 以及工具的参数/结果。** 如果你在 prompt 里输入过 API key，会进文件。工具读了 `.env` 也会进文件。**没有自动脱敏**——这是有意的，目的是让需要完整审计的人能拿到完整记录，但代价是：

- 把 telemetry 文件当 shell history 对待。
- 不要提交到 git。
- 分享前自己跑一遍脱敏。

## 程序里用

```rust
use grain_ai_agent_headless::TelemetrySink;

let sink = std::sync::Arc::new(TelemetrySink::open("./events.jsonl")?);
let sink_clone = sink.clone();
agent.subscribe(std::sync::Arc::new(move |event, _signal| {
    let s = sink_clone.clone();
    Box::pin(async move { s.record(&event) })
})).await;
```

`TelemetrySink::record` I/O 失败时不 panic；只往 stderr log 一下继续运行，写日志失败永远不会拖死 agent。

## 读日志

```bash
# 只看工具调用
jq 'select(.type == "tool_execution_start") | {ts: .ts_ms, tool: .tool_name, args: .args}' events.jsonl

# 每轮的 token 用量（有的话）
jq 'select(.type == "turn_end") | {ts: .ts_ms, usage: .message.usage}' events.jsonl
```
