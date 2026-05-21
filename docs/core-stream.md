# `grain_agent_core::stream`

`LlmStream` 是 agent 与具体 LLM provider 之间**唯一**的注入点。`grain-agent-core` 不依赖任何 LLM SDK；要接入 Anthropic / OpenAI / 本地模型，请在独立 crate 中实现这个 trait。

对应 TS 参考实现里的 `streamFn`。

## 类型

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

`StreamOptions`：

```rust
pub struct StreamOptions {
    pub api_key: Option<String>,             // 由 GetApiKeyFn 注入
    pub reasoning: Option<ThinkingLevel>,    // None 表示 thinking 关闭
    pub session_id: Option<String>,
    pub transport: Option<String>,
    pub max_retry_delay_ms: Option<u64>,
    pub extra: serde_json::Value,            // provider 私有的不透明扩展
}
```

`StreamError`：`Other(String)` 或 `Aborted`。便捷构造 `StreamError::msg("…")`。

## 实现契约（务必遵守）

1. **不要** 用 `Err` 报告请求 / 模型 / 运行时错误。所有失败都要变成流上的**终止事件** `AssistantMessageEvent::Error { error, result }`（或 `Done { result }`），其中 `result.stop_reason` 设为 `StopReason::Error` / `StopReason::Aborted`，`result.error_message` 填上失败原因。
2. 流必须以**恰好一个**终止事件（`Done` 或 `Error`）结尾。
3. 不要 panic。
4. `Err(StreamError)` 仅用于无法构造 stream 的极端场景；`agent_loop` 会把它降级成一个 placeholder 的 error 终止消息，调用方不应依赖这条路径。

`AssistantMessage` 内的 `api` / `provider` / `model` 字段应取自传入的 `Model`，让事件与请求源对得上。

## 最小示例（mock）

`grain-agent-core/tests/smoke.rs::MockStream` 就是参考实现，节选：

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

注入 `Agent`：

```rust
let stream_fn: grain_agent_core::StreamFn = Arc::new(DummyStream);
let agent = grain_agent_core::Agent::new(
    grain_agent_core::AgentOptions::new(model, stream_fn),
);
```

## 失败处理建议

provider 实现里碰到错误时的通用做法：

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

取消（`cancel.cancelled()`）应当让流尽快产出一条 `stop_reason = Aborted` 的终止事件，而不是直接结束流。
