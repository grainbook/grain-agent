//! Reactive retry wrapper for context-window overflow errors.
//!
//! Ports the "retry on `ContextWindowExceeded`" pattern from
//! `codex-rs/core/src/compact.rs` in the openai/codex project.
//!
//! Instead of relying solely on client-side token estimation (which
//! never matches provider tokenization closely enough for tight windows),
//! this wrapper catches the 400-class overflow error the provider
//! returns, drops the oldest safe message from the context, and retries.
//! The proactive [`crate::context_guard::ContextGuard`] remains the
//! primary line of defence; this wrapper is the safety net.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use grain_agent_core::{
    AssistantContent, AssistantMessageEvent, AssistantStream, LlmContext, LlmStream, Message,
    Model, StreamError, StreamOptions,
};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Custom overflow detector: returns `true` when the error string
/// indicates a context-window overflow.
pub type OverflowDetector = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Notification callback invoked each time the wrapper trims + retries.
/// Args: `attempt` (1-based current retry), `max_retries`, `dropped`
/// (messages dropped this round). Use it to render a transient UI
/// status line instead of writing to stderr (which corrupts a TUI's
/// alt screen).
pub type RetryNotify = Arc<dyn Fn(usize, usize, usize) + Send + Sync>;

/// Configuration for [`RetryOnOverflowStream`].
#[derive(Clone)]
pub struct RetryOnOverflowConfig {
    /// Max number of retries before giving up and surfacing the error.
    /// Default 8. Prevents infinite loops on misclassified errors.
    pub max_retries: usize,
    /// Don't trim past this floor -- leaves the agent able to keep working
    /// once it's down to "newest user message + nothing else".
    /// Default 2 (last user/assistant pair).
    pub min_messages: usize,
    /// Optional override for the pattern matcher that decides "this error
    /// is an overflow". Default: a small set of known provider patterns.
    pub is_overflow: Option<OverflowDetector>,
    /// Optional notifier fired on each retry. When set the wrapper
    /// suppresses its built-in `eprintln!` so callers (typically the
    /// TUI) can render their own ephemeral status without seeing
    /// double output corrupt the alt screen.
    pub on_retry: Option<RetryNotify>,
}

impl std::fmt::Debug for RetryOnOverflowConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryOnOverflowConfig")
            .field("max_retries", &self.max_retries)
            .field("min_messages", &self.min_messages)
            .field("is_overflow", &self.is_overflow.as_ref().map(|_| ".."))
            .field("on_retry", &self.on_retry.as_ref().map(|_| ".."))
            .finish()
    }
}

impl Default for RetryOnOverflowConfig {
    fn default() -> Self {
        RetryOnOverflowConfig {
            max_retries: 8,
            min_messages: 2,
            is_overflow: None,
            on_retry: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Default overflow detector
// ---------------------------------------------------------------------------

/// Known provider error substrings (case-insensitive) that signal the
/// request exceeded the model's context window. Tested against real
/// kimi-k2.6 / OpenAI / Anthropic / DeepSeek error bodies.
const OVERFLOW_PATTERNS: &[&str] = &[
    "prompt is too long",
    "maximum context length",
    "context_length_exceeded",
    "context window exceeded",
    "context length is too long",
];

/// Returns `true` when `error` looks like a context-window overflow.
fn default_is_overflow(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    OVERFLOW_PATTERNS.iter().any(|p| lower.contains(p))
}

// ---------------------------------------------------------------------------
// RetryOnOverflowStream
// ---------------------------------------------------------------------------

/// An [`LlmStream`] wrapper that catches context-window overflow errors
/// and retries with a progressively smaller context.
pub struct RetryOnOverflowStream {
    inner: Arc<dyn LlmStream>,
    config: RetryOnOverflowConfig,
}

impl RetryOnOverflowStream {
    /// Wrap `inner` with the default configuration (8 retries, floor of 2
    /// messages).
    pub fn new(inner: Arc<dyn LlmStream>) -> Self {
        RetryOnOverflowStream {
            inner,
            config: RetryOnOverflowConfig::default(),
        }
    }

    /// Wrap `inner` with a custom configuration.
    pub fn with_config(inner: Arc<dyn LlmStream>, config: RetryOnOverflowConfig) -> Self {
        RetryOnOverflowStream { inner, config }
    }

    /// Check whether `error` matches the overflow detector.
    fn is_overflow(&self, error: &str) -> bool {
        match &self.config.is_overflow {
            Some(f) => f(error),
            None => default_is_overflow(error),
        }
    }
}

#[async_trait]
impl LlmStream for RetryOnOverflowStream {
    async fn stream(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
        cancel: CancellationToken,
    ) -> Result<AssistantStream, StreamError> {
        let mut ctx = context.clone();
        let mut last_events: Vec<AssistantMessageEvent> = Vec::new();

        for attempt in 0..=self.config.max_retries {
            // Buffer all events from the inner stream.
            let mut events: Vec<AssistantMessageEvent> = Vec::new();
            let mut stream = self.inner.stream(model, &ctx, options, cancel.clone()).await?;
            while let Some(evt) = stream.next().await {
                events.push(evt);
            }

            // Find the terminal event.
            let terminal = events.last();
            match terminal {
                Some(AssistantMessageEvent::Done { .. }) => {
                    // Success -- forward all buffered events.
                    return Ok(Box::pin(futures::stream::iter(events)));
                }
                Some(AssistantMessageEvent::Error { error, .. })
                    if self.is_overflow(error)
                        && ctx.messages.len() > self.config.min_messages
                        && attempt < self.config.max_retries =>
                {
                    // Overflow -- drop the oldest safe message and retry.
                    let dropped = drop_oldest_safe(&mut ctx.messages);
                    // Notify the host (typically the TUI) so it can
                    // render a transient status line. Fall back to
                    // stderr only when no notifier is wired -- writing
                    // to stderr concurrently with a ratatui alt-screen
                    // session garbles the UI.
                    if let Some(notify) = &self.config.on_retry {
                        notify(attempt + 1, self.config.max_retries, dropped);
                    } else {
                        eprintln!(
                            "[warn] retry-on-overflow: dropped {dropped} message(s), \
                             retrying (attempt {}/{})",
                            attempt + 1,
                            self.config.max_retries
                        );
                    }
                    last_events = events;
                    continue;
                }
                _ => {
                    // Non-overflow error or terminal, or at floor -- forward as-is.
                    return Ok(Box::pin(futures::stream::iter(events)));
                }
            }
        }

        // Exhausted retries -- return the last error.
        Ok(Box::pin(futures::stream::iter(last_events)))
    }
}

// ---------------------------------------------------------------------------
// "Drop oldest safe" logic
// ---------------------------------------------------------------------------

/// Drop the oldest message from `messages` without orphaning tool results.
///
/// Returns the number of messages removed.
///
/// Strategy:
/// 1. The first message is the candidate to drop.
/// 2. If it's a `ToolResult`, it's already orphaned (its matching
///    `ToolCall` was dropped in a prior iteration or never existed in
///    this window). Drop it alone.
/// 3. If it's an `Assistant` message containing `ToolCall` content,
///    also drop all immediately following `ToolResult` messages whose
///    `tool_call_id` matches one of those tool calls (they'd be
///    orphaned without the assistant message).
/// 4. Otherwise (e.g. `User`), drop just that one message.
///
/// Mirrors the orphan-cleanup invariant from
/// [`crate::context_guard::remove_orphan_tool_results`].
fn drop_oldest_safe(messages: &mut Vec<Message>) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let first = &messages[0];

    match first {
        // Case: leading ToolResult -- already orphaned, just drop it.
        Message::ToolResult(_) => {
            messages.remove(0);
            1
        }
        // Case: Assistant with tool calls -- drop it plus trailing
        // tool results that reference its calls.
        Message::Assistant(a) => {
            let call_ids: std::collections::HashSet<&str> = a
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::ToolCall(tc) => Some(tc.id.as_str()),
                    _ => None,
                })
                .collect();

            if call_ids.is_empty() {
                // No tool calls -- safe to just drop the assistant message.
                messages.remove(0);
                return 1;
            }

            // Count how many trailing ToolResults belong to this assistant.
            let mut drop_count = 1usize; // the assistant message itself
            for msg in messages.iter().skip(1) {
                match msg {
                    Message::ToolResult(tr) if call_ids.contains(tr.tool_call_id.as_str()) => {
                        drop_count += 1;
                    }
                    _ => break,
                }
            }

            messages.drain(..drop_count);
            drop_count
        }
        // Case: User or anything else -- drop one.
        Message::User(_) => {
            messages.remove(0);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_core::{
        AssistantMessage, StopReason, TextContent, ToolCall, ToolResultMessage, Usage,
        UserContent, UserMessage,
    };

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn user_msg(text: &str) -> Message {
        Message::User(UserMessage {
            content: vec![UserContent::text(text)],
            timestamp: 0,
        })
    }

    fn assistant_msg(text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.into(),
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        })
    }

    fn assistant_with_tool_call(call_id: &str, tool_name: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: call_id.into(),
                name: tool_name.into(),
                arguments: serde_json::Value::Null,
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        })
    }

    fn tool_result_msg(call_id: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: call_id.into(),
            tool_name: "test_tool".into(),
            content: vec![UserContent::text("result")],
            details: serde_json::Value::Null,
            is_error: false,
            timestamp: 0,
        })
    }

    fn done_msg() -> AssistantMessage {
        AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: "ok".into(),
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
    }

    fn error_msg() -> AssistantMessage {
        AssistantMessage {
            content: vec![],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some("overflow".into()),
            timestamp: 0,
        }
    }

    fn dummy_model() -> Model {
        Model {
            id: "test".into(),
            name: "test".into(),
            api: "test".into(),
            provider: "test".into(),
            ..Default::default()
        }
    }

    fn dummy_options() -> StreamOptions {
        StreamOptions::default()
    }

    /// Mock `LlmStream` that fails N times with an overflow error,
    /// then succeeds.
    struct FailThenSucceed {
        failures_remaining: std::sync::atomic::AtomicUsize,
        error_text: String,
    }

    impl FailThenSucceed {
        fn new(n: usize, error: &str) -> Self {
            FailThenSucceed {
                failures_remaining: std::sync::atomic::AtomicUsize::new(n),
                error_text: error.into(),
            }
        }
    }

    #[async_trait]
    impl LlmStream for FailThenSucceed {
        async fn stream(
            &self,
            _model: &Model,
            _context: &LlmContext,
            _options: &StreamOptions,
            _cancel: CancellationToken,
        ) -> Result<AssistantStream, StreamError> {
            let remaining = self
                .failures_remaining
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| if n > 0 { Some(n - 1) } else { None },
                )
                .unwrap_or(0);

            if remaining > 0 {
                let evt = AssistantMessageEvent::Error {
                    error: self.error_text.clone(),
                    result: error_msg(),
                };
                Ok(Box::pin(futures::stream::iter(vec![evt])))
            } else {
                let evt = AssistantMessageEvent::Done {
                    result: done_msg(),
                };
                Ok(Box::pin(futures::stream::iter(vec![evt])))
            }
        }
    }

    /// Mock that always fails with a specific error.
    struct AlwaysFail {
        error_text: String,
    }

    #[async_trait]
    impl LlmStream for AlwaysFail {
        async fn stream(
            &self,
            _model: &Model,
            _context: &LlmContext,
            _options: &StreamOptions,
            _cancel: CancellationToken,
        ) -> Result<AssistantStream, StreamError> {
            let evt = AssistantMessageEvent::Error {
                error: self.error_text.clone(),
                result: error_msg(),
            };
            Ok(Box::pin(futures::stream::iter(vec![evt])))
        }
    }

    /// Mock that records how many messages were in the context on each call.
    struct RecordingStream {
        inner: FailThenSucceed,
        message_counts: std::sync::Mutex<Vec<usize>>,
    }

    impl RecordingStream {
        fn new(failures: usize, error: &str) -> Self {
            RecordingStream {
                inner: FailThenSucceed::new(failures, error),
                message_counts: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn counts(&self) -> Vec<usize> {
            self.message_counts.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmStream for RecordingStream {
        async fn stream(
            &self,
            model: &Model,
            context: &LlmContext,
            options: &StreamOptions,
            cancel: CancellationToken,
        ) -> Result<AssistantStream, StreamError> {
            self.message_counts
                .lock()
                .unwrap()
                .push(context.messages.len());
            self.inner.stream(model, context, options, cancel).await
        }
    }

    // -----------------------------------------------------------------------
    // drop_oldest_safe unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn drop_oldest_user_message() {
        let mut msgs = vec![user_msg("a"), user_msg("b"), user_msg("c")];
        let n = drop_oldest_safe(&mut msgs);
        assert_eq!(n, 1);
        assert_eq!(msgs.len(), 2);
        // First remaining should be "b".
        match &msgs[0] {
            Message::User(u) => {
                assert_eq!(u.content[0], UserContent::text("b"));
            }
            _ => panic!("expected user"),
        }
    }

    #[test]
    fn drop_oldest_assistant_with_tool_calls_also_drops_results() {
        let mut msgs = vec![
            assistant_with_tool_call("c1", "grep"),
            tool_result_msg("c1"),
            user_msg("next"),
        ];
        let n = drop_oldest_safe(&mut msgs);
        assert_eq!(n, 2, "should drop assistant + its tool result");
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], Message::User(_)));
    }

    #[test]
    fn drop_oldest_orphan_tool_result() {
        let mut msgs = vec![tool_result_msg("orphan"), user_msg("b")];
        let n = drop_oldest_safe(&mut msgs);
        assert_eq!(n, 1);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], Message::User(_)));
    }

    #[test]
    fn drop_oldest_assistant_without_tool_calls() {
        let mut msgs = vec![assistant_msg("thinking..."), user_msg("ok")];
        let n = drop_oldest_safe(&mut msgs);
        assert_eq!(n, 1);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], Message::User(_)));
    }

    // -----------------------------------------------------------------------
    // RetryOnOverflowStream integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn retry_once_on_overflow_then_succeed() {
        let inner = Arc::new(FailThenSucceed::new(
            1,
            "The prompt is too long: 262879, model maximum context length: 262143",
        ));
        let wrapper = RetryOnOverflowStream::new(inner);
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: vec![user_msg("a"), user_msg("b"), user_msg("c")],
            tools: vec![],
        };
        let mut stream = wrapper
            .stream(&dummy_model(), &ctx, &dummy_options(), CancellationToken::new())
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Some(evt) = stream.next().await {
            events.push(evt);
        }

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], AssistantMessageEvent::Done { .. }));
    }

    #[tokio::test]
    async fn retry_twice_on_overflow_then_succeed() {
        let inner = Arc::new(FailThenSucceed::new(
            2,
            "context_length_exceeded",
        ));
        let wrapper = RetryOnOverflowStream::new(inner);
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: vec![
                user_msg("a"),
                user_msg("b"),
                user_msg("c"),
                user_msg("d"),
            ],
            tools: vec![],
        };
        let mut stream = wrapper
            .stream(&dummy_model(), &ctx, &dummy_options(), CancellationToken::new())
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Some(evt) = stream.next().await {
            events.push(evt);
        }

        assert!(matches!(events.last(), Some(AssistantMessageEvent::Done { .. })));
    }

    #[tokio::test]
    async fn max_retries_exceeded_surfaces_last_error() {
        let inner = Arc::new(AlwaysFail {
            error_text: "prompt is too long".into(),
        });
        let config = RetryOnOverflowConfig {
            max_retries: 3,
            min_messages: 1,
            ..Default::default()
        };
        let wrapper = RetryOnOverflowStream::with_config(inner, config);
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: vec![
                user_msg("a"),
                user_msg("b"),
                user_msg("c"),
                user_msg("d"),
                user_msg("e"),
            ],
            tools: vec![],
        };
        let mut stream = wrapper
            .stream(&dummy_model(), &ctx, &dummy_options(), CancellationToken::new())
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Some(evt) = stream.next().await {
            events.push(evt);
        }

        // Should surface the error after max_retries.
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Error { .. })
        ));
    }

    #[tokio::test]
    async fn unrelated_error_forwarded_immediately() {
        let recording = Arc::new(RecordingStream {
            inner: FailThenSucceed::new(999, "rate limit exceeded"),
            message_counts: std::sync::Mutex::new(Vec::new()),
        });
        let wrapper = RetryOnOverflowStream::new(recording.clone());
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: vec![user_msg("a"), user_msg("b"), user_msg("c")],
            tools: vec![],
        };
        let mut stream = wrapper
            .stream(&dummy_model(), &ctx, &dummy_options(), CancellationToken::new())
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Some(evt) = stream.next().await {
            events.push(evt);
        }

        // Should forward the error without retrying.
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Error { .. })
        ));
        // Only one call to inner stream (no retries).
        assert_eq!(recording.counts().len(), 1);
    }

    #[tokio::test]
    async fn at_min_messages_floor_forwards_error() {
        let inner = Arc::new(AlwaysFail {
            error_text: "prompt is too long: 999999".into(),
        });
        let config = RetryOnOverflowConfig {
            max_retries: 8,
            min_messages: 2,
            ..Default::default()
        };
        let wrapper = RetryOnOverflowStream::with_config(inner, config);
        // Only 2 messages -- at the floor, cannot trim.
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: vec![user_msg("a"), user_msg("b")],
            tools: vec![],
        };
        let mut stream = wrapper
            .stream(&dummy_model(), &ctx, &dummy_options(), CancellationToken::new())
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Some(evt) = stream.next().await {
            events.push(evt);
        }

        // Should forward the overflow error (can't trim below floor).
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Error { .. })
        ));
    }

    #[tokio::test]
    async fn drop_oldest_does_not_orphan_tool_results() {
        let recording = Arc::new(RecordingStream::new(
            1,
            "maximum context length exceeded",
        ));
        let wrapper = RetryOnOverflowStream::new(recording.clone());
        // Transcript: [assistant(tool_call c1), tool_result(c1), user("next")]
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: vec![
                assistant_with_tool_call("c1", "grep"),
                tool_result_msg("c1"),
                user_msg("next"),
            ],
            tools: vec![],
        };
        let mut stream = wrapper
            .stream(&dummy_model(), &ctx, &dummy_options(), CancellationToken::new())
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Some(evt) = stream.next().await {
            events.push(evt);
        }

        assert!(matches!(events.last(), Some(AssistantMessageEvent::Done { .. })));
        // First call: 3 messages. After dropping assistant+tool_result: 1 message.
        let counts = recording.counts();
        assert_eq!(counts[0], 3);
        assert_eq!(counts[1], 1);
    }

    #[tokio::test]
    async fn messages_shrink_on_each_retry() {
        let recording = Arc::new(RecordingStream::new(
            3,
            "prompt is too long",
        ));
        let wrapper = RetryOnOverflowStream::new(recording.clone());
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: vec![
                user_msg("a"),
                user_msg("b"),
                user_msg("c"),
                user_msg("d"),
                user_msg("e"),
            ],
            tools: vec![],
        };
        let mut stream = wrapper
            .stream(&dummy_model(), &ctx, &dummy_options(), CancellationToken::new())
            .await
            .unwrap();

        while stream.next().await.is_some() {}

        let counts = recording.counts();
        // 4 calls: 5, 4, 3, 2 messages
        assert_eq!(counts, vec![5, 4, 3, 2]);
    }

    // -----------------------------------------------------------------------
    // default_is_overflow pattern tests
    // -----------------------------------------------------------------------

    #[test]
    fn detects_kimi_overflow() {
        assert!(default_is_overflow(
            "genai stream error: Web stream error for model 'kimi-k2.6 (adapter: OpenAI)'.\n\
             Cause: HTTP error.\nStatus: 400 Bad Request Bad Request\n\
             Body: {\"error\":{\"message\":\"Error from provider: \
             The prompt is too long: 262879, model maximum context length: 262143\"}}"
        ));
    }

    #[test]
    fn detects_openai_context_length_exceeded() {
        assert!(default_is_overflow(
            "context_length_exceeded: This model's maximum context length is 128000 tokens"
        ));
    }

    #[test]
    fn detects_anthropic_style() {
        assert!(default_is_overflow("maximum context length is 200000 tokens"));
    }

    #[test]
    fn detects_context_window_exceeded() {
        assert!(default_is_overflow("Context window exceeded"));
    }

    #[test]
    fn detects_context_length_too_long() {
        assert!(default_is_overflow(
            "The context length is too long for this model"
        ));
    }

    #[test]
    fn does_not_match_unrelated_errors() {
        assert!(!default_is_overflow("rate limit exceeded"));
        assert!(!default_is_overflow("internal server error"));
        assert!(!default_is_overflow("authentication failed"));
    }
}
