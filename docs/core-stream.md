# `grain_agent_core::stream`

`LlmStream` is the **only** seam between the agent loop and a concrete LLM provider. `grain-agent-core` carries no LLM SDK dependency; to talk to Anthropic, OpenAI, a local model, etc., implement this trait in a separate crate (`grain-llm-genai` is one ready-made example).

Corresponds to `streamFn` in the TypeScript reference.

中文版：[zh/core-stream.md](./zh/core-stream.md).

## Types

```rust
pub type AssistantStream = BoxStream<'static, AssistantMessageEvent>;
pub type StreamFn = Arc<dyn LlmStream>;

#[async_trait::async_trait]
pub trait LlmStream: Send + Sync {
    async fn stream(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
        cancel: CancellationToken,
    ) -> Result<AssistantStream, StreamError>;
}
```

`StreamOptions`:

```rust
pub struct StreamOptions {
    pub api_key: Option<String>,           // populated by GetApiKeyFn
    pub reasoning: Option<ThinkingLevel>,  // None = thinking disabled
    pub session_id: Option<String>,
    pub transport: Option<String>,
    pub max_retry_delay_ms: Option<u64>,
    pub extra: serde_json::Value,          // provider-specific opaque extension
}
```

`StreamError`: `Other(String)` or `Aborted`. Helper: `StreamError::msg("…")`.

## Implementation contract (must hold)

1. **Never** use `Err` to report request / model / runtime failures. Surface failures as a **terminal** `AssistantMessageEvent::Error { error, result }` (or `Done { result }`) on the returned stream, with `result.stop_reason` set to `StopReason::Error` / `StopReason::Aborted` and `result.error_message` populated.
2. The stream MUST end with **exactly one** terminal event (`Done` or `Error`).
3. Never panic.
4. `Err(StreamError)` is only for the extreme case where you can't even build a stream; the loop degrades it to a placeholder error terminal. Callers should not rely on this path.

`AssistantMessage`'s `api` / `provider` / `model` fields should be taken from the `Model` you receive, so events stay tied to the request.

## Minimal example (mock)

`grain-agent-core/tests/smoke.rs::MockStream` is the canonical reference; abbreviated:

```rust
use async_trait::async_trait;
use futures::StreamExt;
use grain_agent_core::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, AssistantStream,
    LlmContext, LlmStream, Model, StopReason, StreamError, StreamOptions, TextContent, Usage,
};
use tokio_util::sync::CancellationToken;

struct DummyStream;

#[async_trait]
impl LlmStream for DummyStream {
    async fn stream(
        &self,
        model: &Model,
        _ctx: &LlmContext,
        _opts: &StreamOptions,
        _cancel: CancellationToken,
    ) -> Result<AssistantStream, StreamError> {
        let final_msg = AssistantMessage {
            content: vec![AssistantContent::Text(TextContent { text: "hi".into() })],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        Ok(futures::stream::iter(vec![
            AssistantMessageEvent::Start { partial: final_msg.clone() },
            AssistantMessageEvent::Done { result: final_msg },
        ])
        .boxed())
    }
}
```

Plug into an `Agent`:

```rust
let stream_fn: grain_agent_core::StreamFn = Arc::new(DummyStream);
let agent = grain_agent_core::Agent::new(
    grain_agent_core::AgentOptions::new(model, stream_fn),
);
```

## Failure-handling template

```rust
fn fail(model: &Model, err: impl ToString) -> AssistantStream {
    let msg = AssistantMessage {
        content: vec![AssistantContent::Text(TextContent::default())],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some(err.to_string()),
        timestamp: 0,
    };
    futures::stream::iter(vec![AssistantMessageEvent::Error {
        error: msg.error_message.clone().unwrap(),
        result: msg,
    }])
    .boxed()
}
```

On cancellation (`cancel.cancelled()`), terminate with `stop_reason = Aborted` rather than dropping the stream silently.
