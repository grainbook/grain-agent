//! Stream abstraction for LLM provider integration.
//!
//! The agent loop is parameterized over [`LlmStream`] so the core crate has no
//! dependency on any concrete LLM SDK. Apps inject an implementation; a default
//! provider (e.g. Anthropic, OpenAI) lives in a separate crate.
//!
//! This corresponds to the `streamFn` injection point in the TypeScript
//! `@earendil-works/pi-agent-core` package.

use std::sync::Arc;

use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::types::{AssistantMessageEvent, LlmContext, Model, ThinkingLevel};

/// Boxed stream of streaming-protocol events ending with `Done` or `Error`.
pub type AssistantStream = BoxStream<'static, AssistantMessageEvent>;

/// Options passed to an [`LlmStream`] implementation for a single request.
#[derive(Debug, Default, Clone)]
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub reasoning: Option<ThinkingLevel>,
    pub session_id: Option<String>,
    pub transport: Option<String>,
    pub max_retry_delay_ms: Option<u64>,
    /// Provider-specific extras forwarded as opaque JSON.
    pub extra: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("{0}")]
    Other(String),
    #[error("aborted")]
    Aborted,
}

impl StreamError {
    /// Convenience constructor wrapping a message into [`StreamError::Other`].
    pub fn msg(s: impl Into<String>) -> Self {
        StreamError::Other(s.into())
    }
}

/// Trait implemented by an LLM provider adapter.
///
/// Contract (mirroring the TS `StreamFn`):
/// - Must not panic or return an `Err` for request/model/runtime failures.
/// - Must surface failures in the returned stream via a terminal
///   [`AssistantMessageEvent::Error`] (or [`AssistantMessageEvent::Done`])
///   carrying a final [`crate::types::AssistantMessage`] with
///   [`crate::types::StopReason::Error`] or [`crate::types::StopReason::Aborted`]
///   and a populated `error_message`.
/// - The stream MUST end with exactly one terminal event.
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

/// Shared, type-erased stream handle.
pub type StreamFn = Arc<dyn LlmStream>;
