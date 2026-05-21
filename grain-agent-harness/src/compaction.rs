//! Context compaction: collapse old transcript prefix into a summary so
//! long-running agents can keep running without blowing the model's
//! context window.
//!
//! Ports `packages/agent/src/harness/compaction/*` from pi (minus the UI
//! callbacks pi uses for progress reporting).
//!
//! ## How it works
//!
//! 1. A [`CompactionPolicy`] decides whether the current transcript needs
//!    compaction and, if so, how many leading messages to summarize.
//! 2. [`compact_transcript`] calls a provided [`LlmStream`] with a
//!    dedicated summarization prompt, waits for the terminal event, and
//!    extracts the summary text.
//! 3. The compacted prefix is replaced with a
//!    [`compaction_summary_message`] (the existing harness custom-message
//!    variant — `convert_to_llm` already projects it into a wrapped user
//!    message for the next turn).
//!
//! The resulting transcript looks like:
//!
//! ```text
//! [compactionSummary]   <- generated, replaces the dropped prefix
//! [kept message K]
//! [kept message K+1]
//! …
//! [most recent message]
//! ```
//!
//! ## Wiring into [`grain_agent_core::Agent`]
//!
//! Use [`compaction_prepare_next_turn`] to wrap a [`CompactionPolicy`] +
//! summarizer into the [`grain_agent_core::PrepareNextTurnFn`] hook. After
//! each turn the wrapper checks the threshold and, if exceeded, performs
//! the compaction synchronously before the next turn begins.

use std::sync::Arc;

use futures::StreamExt;
use grain_agent_core::{
    AgentContext, AgentLoopTurnUpdate, AgentMessage, AssistantContent, AssistantMessageEvent,
    LlmContext, LlmStream, Message, Model, PrepareNextTurnContext, PrepareNextTurnFn,
    StreamOptions, TextContent, UserContent, UserMessage,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::messages::compaction_summary_message;

/// Default amount of recent transcript to leave untouched. Compaction
/// always preserves at least this many tail messages — older messages
/// are the ones we summarize.
pub const DEFAULT_KEEP_RECENT: usize = 8;

/// Default high-water mark: trigger compaction when the transcript has at
/// least this many messages. Apps with token-aware policies should plug in
/// their own [`CompactionPolicy`] implementation.
pub const DEFAULT_MESSAGE_THRESHOLD: usize = 40;

/// Default summarization prompt — terse, instructional. Apps can override.
pub const DEFAULT_COMPACTION_PROMPT: &str = "\
Summarize the conversation so far in 2-4 paragraphs. Cover:
- The user's primary goals and any constraints they specified.
- Decisions already made and code / files already inspected or modified.
- Open questions, blockers, and any state the next turn needs to know.

Be specific (file paths, function names, error messages, decisions). Do not invent details that weren't in the conversation. Output only the summary text — no preamble or sign-off.
";

/// Policy: given the current transcript, decide whether to compact and how
/// many leading messages to fold into the summary.
pub trait CompactionPolicy: Send + Sync {
    /// `None` → don't compact this turn. `Some(n)` → summarize the first
    /// `n` messages and replace them with one `compactionSummary` entry.
    fn evaluate(&self, messages: &[AgentMessage]) -> Option<usize>;
}

/// Simple message-count policy: compact when the transcript reaches
/// `threshold` messages, replacing everything except the most-recent
/// `keep_recent` messages with a single summary.
#[derive(Debug, Clone, Copy)]
pub struct MessageCountPolicy {
    pub threshold: usize,
    pub keep_recent: usize,
}

impl Default for MessageCountPolicy {
    fn default() -> Self {
        MessageCountPolicy {
            threshold: DEFAULT_MESSAGE_THRESHOLD,
            keep_recent: DEFAULT_KEEP_RECENT,
        }
    }
}

impl CompactionPolicy for MessageCountPolicy {
    fn evaluate(&self, messages: &[AgentMessage]) -> Option<usize> {
        if messages.len() < self.threshold {
            return None;
        }
        let prefix_len = messages.len().saturating_sub(self.keep_recent);
        // Avoid degenerate compactions — at least 2 messages worth folding.
        if prefix_len < 2 {
            return None;
        }
        Some(prefix_len)
    }
}

#[derive(Debug, Error)]
pub enum CompactionError {
    #[error("summarization stream produced no usable text")]
    EmptySummary,
    #[error("summarization stream failed: {0}")]
    StreamFailed(String),
}

/// Perform the compaction itself: call the summarizer, weave the
/// resulting `compactionSummary` into the transcript, return the new
/// transcript. The caller is expected to install the result via
/// [`AgentLoopTurnUpdate::context`].
pub async fn compact_transcript(
    summarizer: &Arc<dyn LlmStream>,
    model: &Model,
    system_prompt: &str,
    messages: &[AgentMessage],
    prefix_len: usize,
    compaction_prompt: &str,
    cancel: CancellationToken,
) -> Result<Vec<AgentMessage>, CompactionError> {
    debug_assert!(prefix_len <= messages.len());

    let prefix = &messages[..prefix_len];
    let tail: Vec<AgentMessage> = messages[prefix_len..].to_vec();

    let prefix_token_estimate = approximate_token_count(prefix);
    let summary = produce_summary(
        summarizer,
        model,
        system_prompt,
        prefix,
        compaction_prompt,
        cancel,
    )
    .await?;
    if summary.trim().is_empty() {
        return Err(CompactionError::EmptySummary);
    }

    let mut out: Vec<AgentMessage> = Vec::with_capacity(tail.len() + 1);
    out.push(compaction_summary_message(
        summary,
        prefix_token_estimate,
        current_time_ms(),
    ));
    out.extend(tail);
    Ok(out)
}

/// Wrap a policy + summarizer into a [`PrepareNextTurnFn`] that compaction-
/// rewrites the transcript between turns. Drop into
/// [`grain_agent_core::AgentOptions::prepare_next_turn`].
pub fn compaction_prepare_next_turn(
    summarizer: Arc<dyn LlmStream>,
    policy: Arc<dyn CompactionPolicy>,
    compaction_prompt: String,
) -> PrepareNextTurnFn {
    Arc::new(move |ctx: PrepareNextTurnContext| {
        let summarizer = summarizer.clone();
        let policy = policy.clone();
        let prompt = compaction_prompt.clone();
        Box::pin(async move {
            let prefix_len = policy.evaluate(&ctx.context.messages)?;

            // We need an owned `Model` for the summarizer call. Reuse the
            // assistant message's model when present; otherwise fall back
            // to `Model::unknown()` — the summarizer can override.
            let model = if !ctx.message.model.is_empty() {
                Model {
                    id: ctx.message.model.clone(),
                    name: ctx.message.model.clone(),
                    api: ctx.message.api.clone(),
                    provider: ctx.message.provider.clone(),
                    ..Default::default()
                }
            } else {
                Model::unknown()
            };

            match compact_transcript(
                &summarizer,
                &model,
                &ctx.context.system_prompt,
                &ctx.context.messages,
                prefix_len,
                &prompt,
                CancellationToken::new(),
            )
            .await
            {
                Ok(new_messages) => {
                    let new_ctx = AgentContext {
                        system_prompt: ctx.context.system_prompt.clone(),
                        messages: new_messages,
                        tools: ctx.context.tools.clone(),
                    };
                    Some(AgentLoopTurnUpdate {
                        context: Some(new_ctx),
                        ..Default::default()
                    })
                }
                Err(e) => {
                    eprintln!(
                        "[warn] grain-agent-harness: compaction skipped this turn: {e}"
                    );
                    None
                }
            }
        })
    })
}

async fn produce_summary(
    summarizer: &Arc<dyn LlmStream>,
    model: &Model,
    system_prompt: &str,
    prefix: &[AgentMessage],
    compaction_prompt: &str,
    cancel: CancellationToken,
) -> Result<String, CompactionError> {
    // Build the LLM context fed to the summarizer:
    // - Reuse the agent's system prompt so the model already has context.
    // - Project the prefix to plain LLM messages (drop Custom variants;
    //   they're not what we want to summarize).
    // - Append a final user message asking for the summary.
    let mut llm_messages: Vec<Message> = prefix
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Standard(m) => Some(m.clone()),
            AgentMessage::Custom(_) => None,
        })
        .collect();
    llm_messages.push(Message::User(UserMessage {
        content: vec![UserContent::Text(TextContent {
            text: compaction_prompt.to_string(),
        })],
        timestamp: current_time_ms(),
    }));

    let llm_ctx = LlmContext {
        system_prompt: system_prompt.to_string(),
        messages: llm_messages,
        tools: Vec::new(),
    };

    let mut stream = summarizer
        .stream(model, &llm_ctx, &StreamOptions::default(), cancel)
        .await
        .map_err(|e| CompactionError::StreamFailed(e.to_string()))?;

    let mut summary = String::new();
    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::Done { result } => {
                for c in result.content {
                    if let AssistantContent::Text(t) = c {
                        summary.push_str(&t.text);
                    }
                }
                break;
            }
            // The summarizer hit an error. Don't silently take whatever
            // partial / error text the provider emitted as a "summary"
            // and replace the real transcript prefix with it — that
            // would lose context permanently. Surface the failure
            // cleanly; `compaction_prepare_next_turn` downgrades to
            // "skip this turn" without breaking the loop.
            AssistantMessageEvent::Error { error, .. } => {
                return Err(CompactionError::StreamFailed(error));
            }
            _ => {}
        }
    }
    Ok(summary)
}

/// Crude token estimate (chars / 4). Matches the heuristic used by
/// `grain-agent-harness::context_guard`'s default `TokenEstimator`.
fn approximate_token_count(messages: &[AgentMessage]) -> u64 {
    let mut chars = 0usize;
    for m in messages {
        match m {
            AgentMessage::Standard(Message::User(u)) => {
                for c in &u.content {
                    if let UserContent::Text(t) = c {
                        chars += t.text.chars().count();
                    }
                }
            }
            AgentMessage::Standard(Message::Assistant(a)) => {
                for c in &a.content {
                    match c {
                        AssistantContent::Text(t) => chars += t.text.chars().count(),
                        AssistantContent::Thinking(t) => chars += t.thinking.chars().count(),
                        AssistantContent::ToolCall(tc) => {
                            chars += tc.name.chars().count();
                            chars += serde_json::to_string(&tc.arguments)
                                .map(|s| s.chars().count())
                                .unwrap_or(0);
                        }
                        _ => {}
                    }
                }
            }
            AgentMessage::Standard(Message::ToolResult(t)) => {
                for c in &t.content {
                    if let UserContent::Text(t) = c {
                        chars += t.text.chars().count();
                    }
                }
            }
            AgentMessage::Custom(value) => {
                chars += serde_json::to_string(value)
                    .map(|s| s.chars().count())
                    .unwrap_or(0);
            }
        }
    }
    (chars as u64).div_ceil(4)
}

fn current_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream;
    use grain_agent_core::{
        AssistantMessage, AssistantStream, StopReason, StreamError, Usage, UserMessage,
    };

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: text.into() })],
            timestamp: 0,
        })
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent { text: text.into() })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        })
    }

    struct StaticSummarizer {
        text: String,
    }

    #[async_trait]
    impl LlmStream for StaticSummarizer {
        async fn stream(
            &self,
            model: &Model,
            _ctx: &LlmContext,
            _opts: &StreamOptions,
            _cancel: CancellationToken,
        ) -> Result<AssistantStream, StreamError> {
            let final_msg = AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: self.text.clone(),
                })],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            };
            Ok(Box::pin(stream::iter(vec![
                AssistantMessageEvent::Start {
                    partial: final_msg.clone(),
                },
                AssistantMessageEvent::Done { result: final_msg },
            ])))
        }
    }

    #[test]
    fn message_count_policy_below_threshold_returns_none() {
        let p = MessageCountPolicy { threshold: 10, keep_recent: 2 };
        let msgs: Vec<AgentMessage> = (0..5).map(|i| user(&format!("u{i}"))).collect();
        assert!(p.evaluate(&msgs).is_none());
    }

    #[test]
    fn message_count_policy_at_threshold_returns_prefix_len() {
        let p = MessageCountPolicy { threshold: 10, keep_recent: 3 };
        let msgs: Vec<AgentMessage> = (0..12).map(|i| user(&format!("u{i}"))).collect();
        // 12 messages, keep 3 → compact 9.
        assert_eq!(p.evaluate(&msgs), Some(9));
    }

    #[test]
    fn message_count_policy_refuses_degenerate_compactions() {
        // 11 messages, keep 10 → would only compact 1, return None.
        let p = MessageCountPolicy { threshold: 11, keep_recent: 10 };
        let msgs: Vec<AgentMessage> = (0..11).map(|i| user(&format!("u{i}"))).collect();
        assert!(p.evaluate(&msgs).is_none());
    }

    #[tokio::test]
    async fn compact_transcript_replaces_prefix_with_summary() {
        let summarizer: Arc<dyn LlmStream> = Arc::new(StaticSummarizer {
            text: "this is the rolled-up summary".into(),
        });
        let model = Model {
            id: "test-model".into(),
            name: "test".into(),
            api: "test".into(),
            provider: "test".into(),
            ..Default::default()
        };
        let mut messages = Vec::new();
        for i in 0..6 {
            messages.push(user(&format!("u{i}")));
            messages.push(assistant(&format!("a{i}")));
        }
        // 12 messages total → compact first 8, keep last 4.
        let out = compact_transcript(
            &summarizer,
            &model,
            "you are helpful",
            &messages,
            8,
            DEFAULT_COMPACTION_PROMPT,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        // Summary entry + 4 kept messages.
        assert_eq!(out.len(), 5);
        match &out[0] {
            AgentMessage::Custom(v) => {
                assert_eq!(v.get("role").and_then(|r| r.as_str()), Some("compactionSummary"));
                assert_eq!(
                    v.get("summary").and_then(|s| s.as_str()),
                    Some("this is the rolled-up summary")
                );
            }
            other => panic!("expected compactionSummary, got {other:?}"),
        }
        // The kept tail starts at u4.
        match &out[1] {
            AgentMessage::Standard(Message::User(u)) => match &u.content[0] {
                UserContent::Text(t) => assert_eq!(t.text, "u4"),
                _ => panic!(),
            },
            other => panic!("expected user(u4), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn compact_transcript_errors_on_empty_summary() {
        let summarizer: Arc<dyn LlmStream> =
            Arc::new(StaticSummarizer { text: "   ".into() });
        let model = Model::unknown();
        let messages: Vec<AgentMessage> = (0..6).map(|i| user(&format!("u{i}"))).collect();
        let err = compact_transcript(
            &summarizer,
            &model,
            "",
            &messages,
            4,
            DEFAULT_COMPACTION_PROMPT,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CompactionError::EmptySummary));
    }

    #[test]
    fn approximate_token_count_scales_with_content_size() {
        let small = vec![user("hi")];
        let large = vec![user(&"x".repeat(400))];
        assert!(approximate_token_count(&large) > approximate_token_count(&small));
    }
}
