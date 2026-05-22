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
use grain_llm_models::Registry;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::context_guard::{ActiveModelHandle, TokenEstimator};
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

// ---------------------------------------------------------------------------
// Token-budget compaction (ports compaction.ts shouldCompact + cut logic)
// ---------------------------------------------------------------------------

/// Settings controlling when and how token-budget compaction fires.
/// Mirrors `DEFAULT_COMPACTION_SETTINGS` from the TS reference.
#[derive(Debug, Clone)]
pub struct CompactionSettings {
    pub enabled: bool,
    /// Fixed token threshold. Values ≤ 0 → use fallback formula.
    pub threshold_tokens: i64,
    /// Percentage of context window. Valid range 1–99; values outside
    /// that range (including the default -1) → use fallback formula.
    pub threshold_percent: i32,
    /// Tokens reserved for the model's response in the fallback formula.
    pub reserve_tokens: u64,
    /// Minimum number of recent tokens to keep untouched when choosing
    /// the compaction cut boundary.
    pub keep_recent_tokens: u64,
}

/// Defaults matching the TS `DEFAULT_COMPACTION_SETTINGS`.
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    threshold_tokens: -1,
    threshold_percent: -1,
    reserve_tokens: 16384,
    keep_recent_tokens: 20000,
};

/// Resolve the effective threshold in tokens above which compaction fires.
///
/// Priority:
/// 1. `settings.threshold_tokens > 0` → clamp to `[1, ctx_window - 1]`
/// 2. `settings.threshold_percent` in 1..=99 → `ctx_window * pct / 100`
/// 3. Fallback → `ctx_window - max(15% * ctx_window, reserve_tokens)`
pub fn resolve_threshold_tokens(ctx_window: u64, settings: &CompactionSettings) -> u64 {
    if settings.threshold_tokens > 0 {
        let t = settings.threshold_tokens as u64;
        return t.clamp(1, ctx_window.saturating_sub(1).max(1));
    }
    if (1..=99).contains(&settings.threshold_percent) {
        let pct = settings.threshold_percent as u64;
        let t = ctx_window * pct / 100;
        // Clamp to [1% of window, 99% of window]
        let lo = ctx_window / 100;
        let hi = ctx_window * 99 / 100;
        return t.clamp(lo.max(1), hi.max(1));
    }
    // Fallback: ctx_window - max(15% * ctx_window, reserve_tokens)
    let fifteen_pct = ctx_window * 15 / 100;
    let reserve = fifteen_pct.max(settings.reserve_tokens);
    ctx_window.saturating_sub(reserve)
}

/// Returns `true` when the transcript's estimated token count exceeds
/// the compaction threshold for the given context window.
pub fn should_compact(ctx_tokens: u64, ctx_window: u64, settings: &CompactionSettings) -> bool {
    if !settings.enabled || ctx_window == 0 {
        return false;
    }
    ctx_tokens > resolve_threshold_tokens(ctx_window, settings)
}

/// Token-budget compaction policy: uses the model registry to look up
/// the active model's context window and decides whether to compact
/// based on estimated token counts.
///
/// Shares the same [`ActiveModelHandle`] as [`crate::ContextGuard`] so
/// a mid-session model switch is immediately visible.
pub struct TokenBudgetPolicy {
    registry: Arc<Registry>,
    model_handle: ActiveModelHandle,
    settings: CompactionSettings,
    estimator: TokenEstimator,
}

impl TokenBudgetPolicy {
    pub fn new(
        registry: Arc<Registry>,
        model_handle: ActiveModelHandle,
        settings: CompactionSettings,
        estimator: TokenEstimator,
    ) -> Self {
        TokenBudgetPolicy {
            registry,
            model_handle,
            settings,
            estimator,
        }
    }
}

impl CompactionPolicy for TokenBudgetPolicy {
    fn evaluate(&self, messages: &[AgentMessage]) -> Option<usize> {
        // 1. Look up current model's context window.
        let model_id = self.model_handle.read().ok()?.clone();
        let ctx_window = self.registry.lookup(&model_id)?.context_window;
        if ctx_window == 0 {
            return None;
        }

        // 2. Estimate total tokens.
        let ctx_tokens = self.estimator.estimate_messages(messages);
        if !should_compact(ctx_tokens, ctx_window, &self.settings) {
            return None;
        }

        // 3. Walk backward from tail accumulating tokens until we've
        //    kept at least `keep_recent_tokens`. Include per-message
        //    framing in each entry so the running total matches what
        //    `estimate_messages` reported in step 2.
        let keep_recent = self.settings.keep_recent_tokens;
        let framing = self.estimator.per_message_overhead();
        let per_msg: Vec<u64> = messages
            .iter()
            .map(|m| self.estimator.estimate_message(m) + framing)
            .collect();
        let mut tail_tokens: u64 = 0;
        let mut keep_start = messages.len(); // index of first kept message
        for i in (0..messages.len()).rev() {
            tail_tokens += per_msg[i];
            keep_start = i;
            if tail_tokens >= keep_recent {
                break;
            }
        }

        // 4. Snap forward to a safe cut boundary:
        //    - The cut point must NOT be a ToolResult (would orphan it
        //      from its preceding assistant ToolCall).
        //    - The message immediately before the kept tail must NOT be
        //      an Assistant message with a ToolCall (the ToolResult for
        //      that call would be in the kept tail but the call itself
        //      would be in the summarized prefix, orphaning the pair).
        while keep_start < messages.len() {
            // Never cut at a ToolResult boundary.
            if matches!(
                &messages[keep_start],
                AgentMessage::Standard(Message::ToolResult(_))
            ) {
                keep_start += 1;
                continue;
            }
            // Check the message just before keep_start: if it's an
            // assistant message with tool calls, we'd orphan them.
            if keep_start > 0
                && let AgentMessage::Standard(Message::Assistant(a)) = &messages[keep_start - 1]
            {
                let has_tool_call = a
                    .content
                    .iter()
                    .any(|c| matches!(c, AssistantContent::ToolCall(_)));
                if has_tool_call {
                    keep_start += 1;
                    continue;
                }
            }
            break;
        }

        let prefix_len = keep_start;

        // 5. Refuse degenerate compactions.
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

    // --- TokenBudgetPolicy + threshold formula tests -------------------------

    #[test]
    fn resolve_threshold_fixed_tokens() {
        let s = CompactionSettings {
            threshold_tokens: 5000,
            ..DEFAULT_COMPACTION_SETTINGS
        };
        assert_eq!(resolve_threshold_tokens(10000, &s), 5000);
    }

    #[test]
    fn resolve_threshold_fixed_tokens_clamped_to_window() {
        let s = CompactionSettings {
            threshold_tokens: 99999,
            ..DEFAULT_COMPACTION_SETTINGS
        };
        // ctx_window = 10000, threshold = 99999 → clamped to 9999
        assert_eq!(resolve_threshold_tokens(10000, &s), 9999);
    }

    #[test]
    fn resolve_threshold_percent() {
        let s = CompactionSettings {
            threshold_percent: 80,
            ..DEFAULT_COMPACTION_SETTINGS
        };
        // 80% of 100_000 = 80_000
        assert_eq!(resolve_threshold_tokens(100_000, &s), 80_000);
    }

    #[test]
    fn resolve_threshold_fallback_uses_reserve() {
        let s = DEFAULT_COMPACTION_SETTINGS;
        // ctx_window = 100_000
        // 15% = 15_000, reserve = 16384 → max = 16384
        // threshold = 100_000 - 16384 = 83616
        assert_eq!(resolve_threshold_tokens(100_000, &s), 83616);
    }

    #[test]
    fn resolve_threshold_fallback_fifteen_pct_wins_over_reserve() {
        let s = DEFAULT_COMPACTION_SETTINGS;
        // ctx_window = 200_000
        // 15% = 30_000, reserve = 16384 → max = 30_000
        // threshold = 200_000 - 30_000 = 170_000
        assert_eq!(resolve_threshold_tokens(200_000, &s), 170_000);
    }

    #[test]
    fn should_compact_disabled() {
        let s = CompactionSettings {
            enabled: false,
            ..DEFAULT_COMPACTION_SETTINGS
        };
        assert!(!should_compact(999_999, 100_000, &s));
    }

    #[test]
    fn should_compact_zero_window() {
        assert!(!should_compact(50_000, 0, &DEFAULT_COMPACTION_SETTINGS));
    }

    #[test]
    fn should_compact_below_threshold() {
        // Default threshold for 100k window = 83616
        assert!(!should_compact(80_000, 100_000, &DEFAULT_COMPACTION_SETTINGS));
    }

    #[test]
    fn should_compact_above_threshold() {
        assert!(should_compact(90_000, 100_000, &DEFAULT_COMPACTION_SETTINGS));
    }

    // Helpers for TokenBudgetPolicy tests
    use std::sync::RwLock;

    fn make_test_registry(
        models: Vec<grain_llm_models::descriptor::ModelDescriptor>,
    ) -> Arc<Registry> {
        Arc::new(Registry::from_descriptors(models).unwrap())
    }

    fn test_model_descriptor(
        id: &str,
        context_window: u64,
    ) -> grain_llm_models::descriptor::ModelDescriptor {
        use grain_llm_models::descriptor::*;
        ModelDescriptor {
            id: id.into(),
            name: id.into(),
            provider: ProviderId::Other { id: "test".into() },
            api: ApiKind::OpenAi,
            context_window,
            max_output_tokens: 4096,
            cost: grain_agent_core::Cost::default(),
            capabilities: Capabilities::default(),
            thinking: ThinkingProfile::default(),
            extra: serde_json::Value::Null,
        }
    }

    fn tool_call_assistant(text: &str, tool_call_id: &str, tool_name: &str) -> AgentMessage {
        AgentMessage::assistant(AssistantMessage {
            content: vec![
                AssistantContent::Text(TextContent { text: text.into() }),
                AssistantContent::ToolCall(grain_agent_core::ToolCall {
                    id: tool_call_id.into(),
                    name: tool_name.into(),
                    arguments: serde_json::json!({}),
                }),
            ],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        })
    }

    fn tool_result(tool_call_id: &str, text: &str) -> AgentMessage {
        AgentMessage::tool_result(grain_agent_core::ToolResultMessage {
            tool_call_id: tool_call_id.into(),
            tool_name: "test_tool".into(),
            content: vec![UserContent::Text(TextContent { text: text.into() })],
            details: serde_json::Value::Null,
            is_error: false,
            timestamp: 0,
        })
    }

    #[test]
    fn token_budget_policy_no_compact_below_threshold() {
        let registry = make_test_registry(vec![test_model_descriptor("test/big", 1_000_000)]);
        let handle: ActiveModelHandle = Arc::new(RwLock::new("test/big".into()));
        let policy = TokenBudgetPolicy::new(
            registry,
            handle,
            DEFAULT_COMPACTION_SETTINGS,
            TokenEstimator::approximate(),
        );
        // Small transcript → well below 1M window threshold.
        let msgs: Vec<AgentMessage> = (0..5).map(|i| user(&format!("u{i}"))).collect();
        assert!(policy.evaluate(&msgs).is_none());
    }

    #[test]
    fn token_budget_policy_compacts_when_above_threshold() {
        // Small window model (2000 tokens), transcript that exceeds it.
        let registry = make_test_registry(vec![test_model_descriptor("test/tiny", 2000)]);
        let handle: ActiveModelHandle = Arc::new(RwLock::new("test/tiny".into()));
        let policy = TokenBudgetPolicy::new(
            registry,
            handle,
            CompactionSettings {
                keep_recent_tokens: 100,
                reserve_tokens: 200,
                ..DEFAULT_COMPACTION_SETTINGS
            },
            TokenEstimator::approximate(),
        );
        // Each message ~250 tokens (1000 chars / 4). 10 messages → ~2500 tokens.
        // Threshold ≈ 2000 - max(300, 200) = 1700 → 2500 > 1700, should compact.
        let msgs: Vec<AgentMessage> = (0..10)
            .map(|i| user(&format!("msg{i}: {}", "x".repeat(1000))))
            .collect();
        let prefix_len = policy.evaluate(&msgs);
        assert!(prefix_len.is_some(), "should compact");
        let n = prefix_len.unwrap();
        assert!(n >= 2, "prefix_len must be >= 2");
        assert!(n < msgs.len(), "must keep some tail");
    }

    #[test]
    fn token_budget_policy_never_cuts_mid_tool_pair() {
        // Transcript: [user, assistant+toolcall, toolresult, user]
        // The policy should never split between assistant+toolcall and toolresult.
        let registry = make_test_registry(vec![test_model_descriptor("test/tiny", 500)]);
        let handle: ActiveModelHandle = Arc::new(RwLock::new("test/tiny".into()));
        let policy = TokenBudgetPolicy::new(
            registry,
            handle,
            CompactionSettings {
                keep_recent_tokens: 10,
                reserve_tokens: 50,
                ..DEFAULT_COMPACTION_SETTINGS
            },
            TokenEstimator::approximate(),
        );
        let msgs = vec![
            user(&"x".repeat(800)),             // 0: ~200 tokens
            tool_call_assistant("think", "tc1", "read_file"), // 1: assistant+toolcall
            tool_result("tc1", &"y".repeat(400)), // 2: tool result
            user("follow up"),                    // 3: user
        ];
        if let Some(prefix_len) = policy.evaluate(&msgs) {
            // The cut must NOT land at index 2 (tool result) or leave
            // index 1 (assistant+toolcall) as the last message before
            // the kept tail. Valid cuts: 0 (degenerate, rejected),
            // or >= 3 (after the tool result).
            assert!(
                prefix_len != 2,
                "must not cut at tool result boundary"
            );
            // If prefix_len == 1, the preceding message (index 0) is a
            // user message, which is fine. If prefix_len == 3, the
            // preceding message (index 2) is a tool result which is
            // fine (the assistant+toolcall at 1 is in the prefix and
            // will be summarized together with its result).
            if prefix_len > 1 {
                // Verify the message just before the cut isn't an
                // assistant with tool calls (would orphan them).
                let prev = &msgs[prefix_len - 1];
                if let AgentMessage::Standard(Message::Assistant(a)) = prev {
                    let has_tc = a.content.iter().any(|c| matches!(c, AssistantContent::ToolCall(_)));
                    assert!(!has_tc, "must not leave orphaned tool call at boundary");
                }
            }
        }
        // If None, the policy decided not to compact at all — also valid.
    }

    #[test]
    fn token_budget_policy_refuses_degenerate() {
        let registry = make_test_registry(vec![test_model_descriptor("test/tiny", 200)]);
        let handle: ActiveModelHandle = Arc::new(RwLock::new("test/tiny".into()));
        let policy = TokenBudgetPolicy::new(
            registry,
            handle,
            CompactionSettings {
                keep_recent_tokens: 10,
                reserve_tokens: 20,
                ..DEFAULT_COMPACTION_SETTINGS
            },
            TokenEstimator::approximate(),
        );
        // Only 2 messages — even if over threshold, prefix_len would be < 2.
        let msgs = vec![user(&"x".repeat(400)), user(&"y".repeat(400))];
        // Either None or Some(n >= 2).
        if let Some(n) = policy.evaluate(&msgs) {
            assert!(n >= 2);
        }
    }

    #[test]
    fn token_budget_policy_unknown_model_returns_none() {
        let registry = make_test_registry(vec![test_model_descriptor("test/known", 100_000)]);
        let handle: ActiveModelHandle = Arc::new(RwLock::new("test/unknown".into()));
        let policy = TokenBudgetPolicy::new(
            registry,
            handle,
            DEFAULT_COMPACTION_SETTINGS,
            TokenEstimator::approximate(),
        );
        let msgs: Vec<AgentMessage> = (0..20)
            .map(|i| user(&format!("msg{i}: {}", "x".repeat(4000))))
            .collect();
        assert!(policy.evaluate(&msgs).is_none());
    }
}
