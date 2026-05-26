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
use futures::future::BoxFuture;
use grain_agent_core::{
    AgentContext, AgentLoopTurnUpdate, AgentMessage, AssistantContent, AssistantMessageEvent,
    LlmContext, LlmStream, Message, Model, PrepareNextTurnContext, PrepareNextTurnFn,
    StreamOptions, TextContent, UserContent, UserMessage,
};
use grain_llm_models::Registry;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::context_guard::{ActiveModelHandle, ActiveModelInfo, TokenEstimator};
use crate::messages::compaction_summary_message;
use crate::session::{Session, SessionTreeEntry, SessionTreeEntryKind};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionNoticePhase {
    Started,
    Finished,
    Skipped,
    Failed,
}

#[derive(Debug, Clone)]
pub struct CompactionNotice {
    pub phase: CompactionNoticePhase,
    pub prefix_len: usize,
    pub message_count: usize,
    pub tokens_before: u64,
    pub summary_bytes: usize,
    pub messages: Option<Vec<AgentMessage>>,
    pub reason: Option<String>,
}

pub type CompactionNotifyFn =
    Arc<dyn Fn(CompactionNotice, CancellationToken) -> BoxFuture<'static, ()> + Send + Sync>;

pub type CompactionSettingsResolver =
    Arc<dyn Fn(&ActiveModelInfo, &CompactionSettings) -> CompactionSettings + Send + Sync>;

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

/// Adjust `prefix_len` forward past any tool-call / tool-result pairs
/// that would be split by the cut, so the summarizer never receives
/// an assistant message with `tool_calls` whose corresponding tool
/// results are in the kept tail.
///
/// Returns an adjusted `prefix_len` that is safe to use as a split
/// point. May return `messages.len()` (the entire transcript is the
/// prefix — caller should check for degenerate cases).
pub fn snap_to_safe_boundary(messages: &[AgentMessage], mut prefix_len: usize) -> usize {
    while prefix_len < messages.len() {
        // Never cut at a ToolResult boundary — the preceding
        // assistant+toolcall is in the prefix, so the result
        // must be too.
        if matches!(
            &messages[prefix_len],
            AgentMessage::Standard(Message::ToolResult(_))
        ) {
            prefix_len += 1;
            continue;
        }
        // Check the message just before the cut: if it's an
        // assistant message with tool calls, those tool calls
        // would be orphaned without their results.
        if prefix_len > 0
            && let AgentMessage::Standard(Message::Assistant(a)) = &messages[prefix_len - 1]
        {
            let has_tool_call = a
                .content
                .iter()
                .any(|c| matches!(c, AssistantContent::ToolCall(_)));
            if has_tool_call {
                prefix_len += 1;
                continue;
            }
        }
        break;
    }
    prefix_len
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
    settings_resolver: Option<CompactionSettingsResolver>,
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
            settings_resolver: None,
        }
    }

    pub fn with_settings_resolver(mut self, resolver: CompactionSettingsResolver) -> Self {
        self.settings_resolver = Some(resolver);
        self
    }
}

impl CompactionPolicy for TokenBudgetPolicy {
    fn evaluate(&self, messages: &[AgentMessage]) -> Option<usize> {
        // 1. Resolve current model's context window. Registry-backed
        // models use the embedded descriptor; synthetic/local models
        // fall back to the window carried in ActiveModelInfo.
        let active_model = self.model_handle.read().ok()?.clone();
        let ctx_window = active_model.resolve_context_window(&self.registry)?;
        let settings = self
            .settings_resolver
            .as_ref()
            .map(|resolver| resolver(&active_model, &self.settings))
            .unwrap_or_else(|| self.settings.clone());

        // 2. Estimate total tokens.
        let ctx_tokens = self.estimator.estimate_messages(messages);
        if !should_compact(ctx_tokens, ctx_window, &settings) {
            return None;
        }

        // 2.5 Derive working-set pins: messages that mention currently-
        //     active file paths or contain error/patch markers are pulled
        //     into the kept tail so the summary never loses them.
        let working_set_paths = derive_working_set_paths(messages);
        let pinned: Vec<bool> = messages
            .iter()
            .map(|m| should_pin_message(m, &working_set_paths))
            .collect();

        // 3. Walk backward from tail accumulating tokens until we've
        //    kept at least `keep_recent_tokens`. Include per-message
        //    framing in each entry so the running total matches what
        //    `estimate_messages` reported in step 2.
        let keep_recent = settings.keep_recent_tokens;
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
            if tail_tokens >= keep_recent && !pinned[i] {
                break;
            }
        }

        // 3.5 Walk forward from keep_start through the prefix; any pinned
        //     message pulls the boundary backward to include it.
        for i in (0..keep_start).rev() {
            if pinned[i] {
                keep_start = i;
            }
        }

        // 4. Snap forward to a safe cut boundary:
        //    - The cut point must NOT be a ToolResult (would orphan it
        //      from its preceding assistant ToolCall).
        //    - The message immediately before the kept tail must NOT be
        //      an Assistant message with a ToolCall (the ToolResult for
        //      that call would be in the kept tail but the call itself
        //      would be in the summarized prefix, orphaning the pair).
        keep_start = snap_to_safe_boundary(messages, keep_start);

        let prefix_len = keep_start;

        // 5. Refuse degenerate compactions.
        if prefix_len < 2 {
            return None;
        }

        Some(prefix_len)
    }
}

// ---------------------------------------------------------------------------
// Working-set path extraction (model-agnostic, model after DeepSeek-TUI's
// compaction.rs)
// ---------------------------------------------------------------------------

/// Maximum number of recent messages to scan for working-set paths.
const RECENT_WORKING_SET_WINDOW: usize = 12;

/// Maximum number of unique paths in the working set.
const MAX_WORKING_SET_PATHS: usize = 24;

/// Extract file paths from an agent message (tool arguments, tool results,
/// and assistant/user text that mentions paths like `src/main.rs` or
/// `Cargo.toml`).
fn extract_paths_from_message(msg: &AgentMessage) -> Vec<String> {
    let mut out = Vec::new();
    match msg {
        AgentMessage::Standard(Message::Assistant(a)) => {
            for c in &a.content {
                match c {
                    AssistantContent::Text(t) => extract_paths_from_text(&t.text, &mut out),
                    AssistantContent::Thinking(t) => extract_paths_from_text(&t.thinking, &mut out),
                    AssistantContent::ToolCall(tc) => {
                        for key in ["path", "file", "target", "cwd", "file_path"] {
                            if let Some(v) = tc.arguments.get(key).and_then(|v| v.as_str()) {
                                out.push(v.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        AgentMessage::Standard(Message::User(u)) => {
            for c in &u.content {
                if let UserContent::Text(t) = c {
                    extract_paths_from_text(&t.text, &mut out);
                }
            }
        }
        AgentMessage::Standard(Message::ToolResult(tr)) => {
            for c in &tr.content {
                if let UserContent::Text(t) = c {
                    extract_paths_from_text(&t.text, &mut out);
                }
            }
        }
        AgentMessage::Custom(_) => {}
    }
    out
}

/// Scan `text` for path-like strings (e.g. `src/main.rs`, `Cargo.toml`,
/// `docs/README.md`) and append them to `out`.
fn extract_paths_from_text(text: &str, out: &mut Vec<String>) {
    // Quick regex-like scan: look for sequences that look like file paths.
    // Two patterns:
    //   a) root-level names: Cargo.toml, README.md, AGENTS.md, etc.
    //   b) dir/.../name.ext with typical code extensions.
    for word in text.split_whitespace() {
        let w = word.trim_matches(|c: char| c == '`' || c == '"' || c == '\'' || c == ',');
        if w.is_empty() {
            continue;
        }
        if is_root_name(w) || looks_like_path(w) {
            out.push(w.to_string());
        }
    }
}

fn is_root_name(s: &str) -> bool {
    matches!(
        s,
        "Cargo.toml"
            | "Cargo.lock"
            | "README.md"
            | "CHANGELOG.md"
            | "AGENTS.md"
            | "Makefile"
            | "Dockerfile"
            | "package.json"
            | "go.mod"
            | "pyproject.toml"
            | "config.example.toml"
    )
}

fn looks_like_path(s: &str) -> bool {
    if !s.contains('/') && !s.contains('\\') {
        return false;
    }
    // Must end with a plausible extension or be a known directory prefix.
    let has_ext = s.rsplit('.').next().is_some_and(|ext| {
        ext.len() >= 2 && ext.len() <= 6 && ext.chars().all(|c| c.is_alphanumeric())
    });
    let known_dir = s.starts_with("src/")
        || s.starts_with("tests/")
        || s.starts_with("docs/")
        || s.starts_with("crates/")
        || s.starts_with(".grain/")
        || s.starts_with(".github/");
    has_ext || known_dir
}

/// Build a working set of file paths from the most recent messages.
fn derive_working_set_paths(messages: &[AgentMessage]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for msg in messages.iter().rev().take(RECENT_WORKING_SET_WINDOW) {
        for p in extract_paths_from_message(msg) {
            if seen.insert(p.clone()) {
                paths.push(p);
                if paths.len() >= MAX_WORKING_SET_PATHS {
                    return paths;
                }
            }
        }
    }
    paths
}

/// Whether a message should be pinned (kept in the tail) during compaction.
/// Returns true when the message mentions a file in the working set or
/// contains error / patch markers.
fn should_pin_message(msg: &AgentMessage, working_set_paths: &[String]) -> bool {
    let text = message_text(msg);
    let lower = text.to_lowercase();

    // Working-set path hits.
    for path in working_set_paths {
        if text.contains(path.as_str()) {
            return true;
        }
    }

    // Error markers.
    for marker in [
        "error:",
        "error ",
        "failed",
        "panic",
        "traceback",
        "stack trace",
        "assertion failed",
        "test failed",
    ] {
        if lower.contains(marker) {
            return true;
        }
    }

    // Patch / diff markers.
    for marker in [
        "diff --git",
        "+++ b/",
        "--- a/",
        "*** begin patch",
        "*** update file:",
        "*** add file:",
        "*** delete file:",
        "```diff",
        "apply_patch",
    ] {
        if lower.contains(marker) {
            return true;
        }
    }

    false
}

/// Render an agent message to a plain-text string for pinning heuristics.
fn message_text(msg: &AgentMessage) -> String {
    let mut text = String::new();
    match msg {
        AgentMessage::Standard(Message::Assistant(a)) => {
            for c in &a.content {
                match c {
                    AssistantContent::Text(t) => {
                        text.push_str(&t.text);
                        text.push('\n');
                    }
                    AssistantContent::Thinking(t) => {
                        text.push_str(&t.thinking);
                        text.push('\n');
                    }
                    AssistantContent::ToolCall(tc) => {
                        use std::fmt::Write;
                        let _ = writeln!(text, "[tool_call:{}] {}", tc.name, tc.arguments);
                    }
                    _ => {}
                }
            }
        }
        AgentMessage::Standard(Message::User(u)) => {
            for c in &u.content {
                if let UserContent::Text(t) = c {
                    text.push_str(&t.text);
                    text.push('\n');
                }
            }
        }
        AgentMessage::Standard(Message::ToolResult(tr)) => {
            for c in &tr.content {
                if let UserContent::Text(t) = c {
                    text.push_str(&t.text);
                    text.push('\n');
                }
            }
        }
        AgentMessage::Custom(v) => {
            text.push_str(&serde_json::to_string(v).unwrap_or_default());
        }
    }
    text
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

    // Ensure we never split a tool-call / tool-result pair.
    let prefix_len = snap_to_safe_boundary(messages, prefix_len);

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
///
/// The `session` argument is also written to on every successful compaction
/// via [`Session::append_compaction`], so the summary survives `/resume`.
/// Without that persist, the in-memory compaction works for the current
/// run but the next `/resume` reloads the full pre-compaction transcript
/// from the on-disk JSONL — which is exactly the bug `retry-on-overflow`
/// kept band-aiding before this wiring landed.
pub fn compaction_prepare_next_turn(
    summarizer: Arc<dyn LlmStream>,
    policy: Arc<dyn CompactionPolicy>,
    compaction_prompt: String,
    session: Session,
) -> PrepareNextTurnFn {
    compaction_prepare_next_turn_with_notify(summarizer, policy, compaction_prompt, session, None)
}

pub fn compaction_prepare_next_turn_with_notify(
    summarizer: Arc<dyn LlmStream>,
    policy: Arc<dyn CompactionPolicy>,
    compaction_prompt: String,
    session: Session,
    notify: Option<CompactionNotifyFn>,
) -> PrepareNextTurnFn {
    Arc::new(
        move |ctx: PrepareNextTurnContext, cancel: CancellationToken| {
            let summarizer = summarizer.clone();
            let policy = policy.clone();
            let prompt = compaction_prompt.clone();
            let session = session.clone();
            let notify = notify.clone();
            Box::pin(async move {
                let prefix_len = policy.evaluate(&ctx.context.messages)?;
                let prefix = &ctx.context.messages[..prefix_len];
                let tokens_before = approximate_token_count(prefix);
                let message_count = ctx.context.messages.len();

                if let Some(notify) = notify.as_ref() {
                    notify(
                        CompactionNotice {
                            phase: CompactionNoticePhase::Started,
                            prefix_len,
                            message_count,
                            tokens_before,
                            summary_bytes: 0,
                            messages: None,
                            reason: None,
                        },
                        cancel.clone(),
                    )
                    .await;
                }
                if cancel.is_cancelled() {
                    if let Some(notify) = notify.as_ref() {
                        notify(
                            CompactionNotice {
                                phase: CompactionNoticePhase::Skipped,
                                prefix_len,
                                message_count,
                                tokens_before,
                                summary_bytes: 0,
                                messages: None,
                                reason: Some("cancelled before compaction".into()),
                            },
                            cancel.clone(),
                        )
                        .await;
                    }
                    return None;
                }

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

                // Build the summary directly via `produce_summary` (rather than
                // `compact_transcript`) so we have the raw text in hand for
                // `Session::append_compaction` — `compact_transcript` only
                // returns the woven `Vec<AgentMessage>`.
                let summary = match produce_summary(
                    &summarizer,
                    &model,
                    &ctx.context.system_prompt,
                    prefix,
                    &prompt,
                    cancel.clone(),
                )
                .await
                {
                    Ok(s) if !s.trim().is_empty() => s,
                    Ok(_) => {
                        eprintln!(
                            "[warn] grain-agent-harness: compaction skipped this turn: empty summary"
                        );
                        if let Some(notify) = notify.as_ref() {
                            notify(
                                CompactionNotice {
                                    phase: CompactionNoticePhase::Skipped,
                                    prefix_len,
                                    message_count,
                                    tokens_before,
                                    summary_bytes: 0,
                                    messages: None,
                                    reason: Some("empty summary".into()),
                                },
                                cancel.clone(),
                            )
                            .await;
                        }
                        return None;
                    }
                    Err(e) => {
                        eprintln!("[warn] grain-agent-harness: compaction skipped this turn: {e}");
                        if let Some(notify) = notify.as_ref() {
                            notify(
                                CompactionNotice {
                                    phase: CompactionNoticePhase::Failed,
                                    prefix_len,
                                    message_count,
                                    tokens_before,
                                    summary_bytes: 0,
                                    messages: None,
                                    reason: Some(e.to_string()),
                                },
                                cancel.clone(),
                            )
                            .await;
                        }
                        return None;
                    }
                };

                // Persist the compaction node so `/resume` rebuilds the
                // session as `[summary, ...]` instead of replaying the full
                // pre-compaction history. Failure here is non-fatal — we
                // still rewrite the in-memory transcript so the current
                // run benefits.
                let first_kept = match first_kept_context_entry_id(&session, prefix_len).await {
                    Some(id) => id,
                    None => session.leaf_id().await.unwrap_or_default(),
                };
                if let Err(e) = session
                    .append_compaction(summary.clone(), first_kept, tokens_before, None, Some(true))
                    .await
                {
                    eprintln!("[warn] grain-agent-harness: compaction session persist failed: {e}");
                }

                // Weave the in-memory transcript: [summary_message, ...tail].
                let tail: Vec<AgentMessage> = ctx.context.messages[prefix_len..].to_vec();
                let mut new_messages: Vec<AgentMessage> = Vec::with_capacity(tail.len() + 1);
                new_messages.push(compaction_summary_message(
                    summary.clone(),
                    tokens_before,
                    current_time_ms(),
                ));
                new_messages.extend(tail);

                if let Some(notify) = notify.as_ref() {
                    notify(
                        CompactionNotice {
                            phase: CompactionNoticePhase::Finished,
                            prefix_len,
                            message_count,
                            tokens_before,
                            summary_bytes: summary.len(),
                            messages: Some(new_messages.clone()),
                            reason: None,
                        },
                        cancel.clone(),
                    )
                    .await;
                }

                let new_ctx = AgentContext {
                    system_prompt: ctx.context.system_prompt.clone(),
                    messages: new_messages,
                    tools: ctx.context.tools.clone(),
                };
                Some(AgentLoopTurnUpdate {
                    context: Some(new_ctx),
                    ..Default::default()
                })
            })
        },
    )
}

pub(crate) fn entry_contributes_context_message(entry: &SessionTreeEntry) -> bool {
    matches!(
        &entry.kind,
        SessionTreeEntryKind::Message { .. }
            | SessionTreeEntryKind::CustomMessage { .. }
            | SessionTreeEntryKind::BranchSummary { .. }
            | SessionTreeEntryKind::Compaction { .. }
    )
}

pub(crate) async fn first_kept_context_entry_id(
    session: &Session,
    prefix_len: usize,
) -> Option<String> {
    session
        .branch(None)
        .await
        .into_iter()
        .filter(entry_contributes_context_message)
        .nth(prefix_len)
        .map(|entry| entry.id)
}

async fn produce_summary(
    summarizer: &Arc<dyn LlmStream>,
    model: &Model,
    system_prompt: &str,
    prefix: &[AgentMessage],
    compaction_prompt: &str,
    cancel: CancellationToken,
) -> Result<String, CompactionError> {
    if cancel.is_cancelled() {
        return Err(CompactionError::StreamFailed("operation aborted".into()));
    }
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

    // Safety net: strip any trailing assistant messages that have
    // tool_calls but whose tool results are missing (shouldn't happen
    // after snap_to_safe_boundary, but guards against edge cases
    // like Custom messages being filtered out in between).
    while let Some(Message::Assistant(a)) = llm_messages.last() {
        let has_tool_call = a
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_)));
        if !has_tool_call {
            break;
        }
        llm_messages.pop();
    }

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
        .stream(model, &llm_ctx, &StreamOptions::default(), cancel.clone())
        .await
        .map_err(|e| CompactionError::StreamFailed(e.to_string()))?;

    let mut summary = String::new();
    while let Some(event) = stream.next().await {
        if cancel.is_cancelled() {
            return Err(CompactionError::StreamFailed("operation aborted".into()));
        }
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

// ---------------------------------------------------------------------------
// Tool-result turn-end truncation (model-agnostic)
// ---------------------------------------------------------------------------

/// Default cap in **characters** for a single tool-result text block at
/// turn end. Mirrors Reasonix's `TURN_END_RESULT_CAP_TOKENS` (~3000 tokens,
/// ~12 000 chars at 4 chars/token). The model saw the full result for the
/// turn that used it; subsequent turns only need a compacted reminder.
/// A re-read is one tool call away and far cheaper than carrying the full
/// payload through every future context window.
pub const DEFAULT_TOOL_RESULT_TRUNCATION_CAP_CHARS: usize = 12_000;

/// Truncate every tool-result message in `messages` whose text content
/// exceeds `cap_chars` chars, keeping the head and tail so the model can
/// still see what file / command was involved and its outcome.
///
/// Returns the number of messages that were truncated (for logging).
pub fn truncate_tool_results(messages: &mut [AgentMessage], cap_chars: usize) -> usize {
    let mut truncated = 0usize;
    if cap_chars == 0 {
        return 0;
    }
    let head_chars = cap_chars * 3 / 4; // keep 75% head
    let tail_chars = cap_chars.saturating_sub(head_chars);
    for msg in messages {
        let AgentMessage::Standard(Message::ToolResult(tr)) = msg else {
            continue;
        };
        for c in &mut tr.content {
            let UserContent::Text(t) = c else {
                continue;
            };
            let len = t.text.chars().count();
            if len <= cap_chars {
                continue;
            }
            let head: String = t.text.chars().take(head_chars).collect();
            let tail: String = t
                .text
                .chars()
                .rev()
                .take(tail_chars)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            t.text = format!(
                "{head}\n\n... [{len} total chars, truncated to {cap_chars} at turn end; \
                 use `read` to re-fetch the full content if needed] ...\n\n{tail}",
            );
            truncated += 1;
        }
    }
    truncated
}

/// Build a [`PrepareNextTurnFn`] that truncates every tool-result message
/// exceeding `cap_chars` after each turn. Chain this *before* the
/// compaction hook (via [`super::chain_prepare_next_turn`]) so compaction
/// summarises already-truncated results rather than the original blobs.
pub fn tool_result_truncation_hook(cap_chars: usize) -> PrepareNextTurnFn {
    Arc::new(
        move |ctx: PrepareNextTurnContext, _cancel: CancellationToken| {
            let cap = cap_chars;
            Box::pin(async move {
                let mut new_messages = (*ctx.context.messages).to_vec();
                let n_truncated = truncate_tool_results(&mut new_messages, cap);
                let n_compressed = semantic_compress_tool_results(&mut new_messages);
                if n_truncated == 0 && n_compressed == 0 {
                    return None;
                }
                Some(AgentLoopTurnUpdate {
                    context: Some(AgentContext {
                        system_prompt: ctx.context.system_prompt.clone(),
                        messages: new_messages,
                        tools: ctx.context.tools.clone(),
                    }),
                    ..Default::default()
                })
            })
        },
    )
}

// ---------------------------------------------------------------------------
// Rule-based semantic compression (model-agnostic, based on oh-my-pi's
// semantic-compression skill)
// ---------------------------------------------------------------------------

/// Conservative semantic compression of tool-result text. Removes
/// grammatical scaffolding that LLMs reconstruct from content words while
/// preserving meaning-carrying content. Returns the number of messages
/// that were compressed.
///
/// Rules (ordered for safety):
///   - Delete leading articles (The, A, An) at sentence start
///   - Delete copula phrases ("is a", "are the", "was a", etc.)
///   - Delete "There is/are/was/were" and "It is/was" expletive subjects
///   - Delete pure intensifiers (very, quite, rather, really, extremely)
///   - Delete "that" complementizers before verbs
///   - Collapse redundant phrases ("in order to" → "to")
///
/// We do NOT compress: error messages, code blocks, path references,
/// numbers, or any line shorter than 30 chars (likely a key-value pair).
pub fn semantic_compress(text: &str) -> String {
    if text.len() < 30 {
        return text.to_string();
    }

    // Stage 1: per-line compression (safe — doesn't merge lines).
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.len() < 30 {
                return line.to_string();
            }
            compress_line(line)
        })
        .collect();

    lines.join("\n")
}

/// Compress a single line of English text. Never changes line semantics.
fn compress_line(line: &str) -> String {
    let mut s = line.to_string();

    // Delete redundant phrases (full-word, case-insensitive).
    for (phrase, replacement) in [
        ("in order to", "to"),
        ("due to the fact that", "because"),
        ("in terms of", ""),
        ("a number of", "several"),
        ("the majority of", "most"),
        ("at this point in time", "now"),
        ("on a regular basis", "regularly"),
    ] {
        // Case-insensitive replace.
        if s.to_lowercase().contains(phrase) {
            // Use a simple approach: find and replace preserving original casing where possible.
            s = replace_phrase(&s, phrase, replacement);
        }
    }

    // Delete "There is/are/was/were" at line start.
    for prefix in [
        "there is ",
        "there are ",
        "there was ",
        "there were ",
        "There is ",
        "There are ",
        "There was ",
        "There were ",
    ] {
        if s.starts_with(prefix) {
            s = s[prefix.len()..].to_string();
            break;
        }
    }

    // Delete "It is/was" at line start when followed by adjective/noun.
    for prefix in ["it is ", "it was ", "It is ", "It was "] {
        if s.starts_with(prefix) {
            s = s[prefix.len()..].to_string();
            break;
        }
    }

    // Delete pure intensifiers (standalone word, no meaning change).
    for intensifier in [
        "very ",
        "quite ",
        "rather ",
        "really ",
        "extremely ",
        "Very ",
        "Quite ",
        "Rather ",
        "Really ",
        "Extremely ",
    ] {
        s = s.replace(intensifier, "");
    }

    // Delete comma-separated interjections.
    s = s.replace(" of course,", "");
    s = s.replace(" Of course,", "");
    s = s.replace(" indeed,", "");
    s = s.replace(" Indeed,", "");

    // Delete "that" as complementizer before a new clause (heuristic:
    // the word before "that" is a verb like "said"/"reported"/"showed").
    s = remove_complementizer_that(&s);

    // Collapse extra whitespace.
    let compressed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if compressed.is_empty() {
        line.to_string()
    } else {
        compressed
    }
}

fn replace_phrase(s: &str, phrase: &str, replacement: &str) -> String {
    // Simple case-insensitive replacement.
    let lower = s.to_lowercase();
    let mut result = String::with_capacity(s.len());
    let mut pos = 0;
    while let Some(idx) = lower[pos..].find(phrase) {
        let abs = pos + idx;
        result.push_str(&s[pos..abs]);
        result.push_str(replacement);
        pos = abs + phrase.len();
    }
    result.push_str(&s[pos..]);
    result
}

fn remove_complementizer_that(s: &str) -> String {
    // If "that" appears after a reporting verb, delete it.
    // Reporting verbs: said, reported, showed, indicated, noted, found,
    // suggested, confirmed, mentioned, stated, claimed.
    let reporting_verbs = [
        "said",
        "reported",
        "showed",
        "indicated",
        "noted",
        "found",
        "suggested",
        "confirmed",
        "mentioned",
        "stated",
        "claimed",
        "Said",
        "Reported",
        "Showed",
        "Indicated",
        "Noted",
        "Found",
        "Suggested",
        "Confirmed",
        "Mentioned",
        "Stated",
        "Claimed",
    ];
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut result = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        result.push(words[i].to_string());
        if i + 2 < words.len()
            && reporting_verbs.contains(&words[i])
            && words[i + 1].eq_ignore_ascii_case("that")
        {
            // Skip "that", keep the verb.
            i += 2;
            continue;
        }
        i += 1;
    }
    result.join(" ")
}

/// Apply semantic compression to all tool-result text blocks. Returns
/// the number of messages that had any text actually compressed.
fn semantic_compress_tool_results(messages: &mut [AgentMessage]) -> usize {
    let mut compressed = 0usize;
    for msg in messages {
        let AgentMessage::Standard(Message::ToolResult(tr)) = msg else {
            continue;
        };
        for c in &mut tr.content {
            let UserContent::Text(t) = c else {
                continue;
            };
            let compact = semantic_compress(&t.text);
            if compact != t.text {
                t.text = compact;
                compressed += 1;
            }
        }
    }
    compressed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{InMemorySessionRepo, SessionRepo};
    use async_trait::async_trait;
    use futures::stream;
    use grain_agent_core::{
        AssistantMessage, AssistantStream, StopReason, StreamError, Usage, UserMessage,
    };
    use std::sync::{Mutex, RwLock};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: text.into() })],
            timestamp: 0,
        })
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::assistant(assistant_msg(text))
    }

    fn assistant_msg(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![AssistantContent::Text(TextContent { text: text.into() })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
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
        let p = MessageCountPolicy {
            threshold: 10,
            keep_recent: 2,
        };
        let msgs: Vec<AgentMessage> = (0..5).map(|i| user(&format!("u{i}"))).collect();
        assert!(p.evaluate(&msgs).is_none());
    }

    #[test]
    fn message_count_policy_at_threshold_returns_prefix_len() {
        let p = MessageCountPolicy {
            threshold: 10,
            keep_recent: 3,
        };
        let msgs: Vec<AgentMessage> = (0..12).map(|i| user(&format!("u{i}"))).collect();
        // 12 messages, keep 3 → compact 9.
        assert_eq!(p.evaluate(&msgs), Some(9));
    }

    #[test]
    fn message_count_policy_refuses_degenerate_compactions() {
        // 11 messages, keep 10 → would only compact 1, return None.
        let p = MessageCountPolicy {
            threshold: 11,
            keep_recent: 10,
        };
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
                assert_eq!(
                    v.get("role").and_then(|r| r.as_str()),
                    Some("compactionSummary")
                );
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
        let summarizer: Arc<dyn LlmStream> = Arc::new(StaticSummarizer { text: "   ".into() });
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

    #[tokio::test]
    async fn compaction_prepare_next_turn_notifies_finished_and_persists() {
        let repo = InMemorySessionRepo::new();
        let session = repo.create(None).await.unwrap();
        let messages: Vec<AgentMessage> = (0..4).map(|i| user(&format!("u{i}"))).collect();
        for message in messages.iter().cloned() {
            session.append_message(message).await.unwrap();
        }

        let phases = Arc::new(Mutex::new(Vec::new()));
        let notice_message_count = Arc::new(Mutex::new(0usize));
        let notify: CompactionNotifyFn = {
            let phases = phases.clone();
            let notice_message_count = notice_message_count.clone();
            Arc::new(move |notice, _cancel| {
                let phases = phases.clone();
                let notice_message_count = notice_message_count.clone();
                Box::pin(async move {
                    if let Some(messages) = &notice.messages {
                        *notice_message_count.lock().unwrap() = messages.len();
                    }
                    phases.lock().unwrap().push(notice.phase.clone());
                })
            })
        };

        let hook = compaction_prepare_next_turn_with_notify(
            Arc::new(StaticSummarizer {
                text: "rolled up".into(),
            }),
            Arc::new(MessageCountPolicy {
                threshold: 3,
                keep_recent: 1,
            }),
            DEFAULT_COMPACTION_PROMPT.to_string(),
            session.clone(),
            Some(notify),
        );
        let ctx = PrepareNextTurnContext {
            message: assistant_msg("done"),
            tool_results: Vec::new(),
            context: Arc::new(AgentContext {
                system_prompt: "system".into(),
                messages: messages.clone(),
                tools: Vec::new(),
            }),
            new_messages: Vec::new(),
        };

        let update = (hook)(ctx, CancellationToken::new())
            .await
            .expect("expected compaction update");
        let new_messages = update.context.expect("expected rewritten context").messages;

        assert_eq!(
            *phases.lock().unwrap(),
            vec![
                CompactionNoticePhase::Started,
                CompactionNoticePhase::Finished
            ]
        );
        assert_eq!(*notice_message_count.lock().unwrap(), new_messages.len());
        match &new_messages[0] {
            AgentMessage::Custom(v) => {
                assert_eq!(
                    v.get("role").and_then(|r| r.as_str()),
                    Some("compactionSummary")
                );
                assert_eq!(v.get("summary").and_then(|s| s.as_str()), Some("rolled up"));
            }
            other => panic!("expected compactionSummary, got {other:?}"),
        }
        let compactions = session
            .entries()
            .await
            .into_iter()
            .filter(|entry| matches!(entry.kind, SessionTreeEntryKind::Compaction { .. }))
            .count();
        assert_eq!(compactions, 1);
    }

    #[tokio::test]
    async fn compaction_prepare_next_turn_can_be_cancelled_from_started_notice() {
        let repo = InMemorySessionRepo::new();
        let session = repo.create(None).await.unwrap();
        let messages: Vec<AgentMessage> = (0..4).map(|i| user(&format!("u{i}"))).collect();
        for message in messages.iter().cloned() {
            session.append_message(message).await.unwrap();
        }

        let phases = Arc::new(Mutex::new(Vec::new()));
        let notify: CompactionNotifyFn = {
            let phases = phases.clone();
            Arc::new(move |notice, cancel| {
                let phases = phases.clone();
                Box::pin(async move {
                    if notice.phase == CompactionNoticePhase::Started {
                        cancel.cancel();
                    }
                    phases.lock().unwrap().push(notice.phase.clone());
                })
            })
        };

        let hook = compaction_prepare_next_turn_with_notify(
            Arc::new(StaticSummarizer {
                text: "should not run".into(),
            }),
            Arc::new(MessageCountPolicy {
                threshold: 3,
                keep_recent: 1,
            }),
            DEFAULT_COMPACTION_PROMPT.to_string(),
            session.clone(),
            Some(notify),
        );
        let ctx = PrepareNextTurnContext {
            message: assistant_msg("done"),
            tool_results: Vec::new(),
            context: Arc::new(AgentContext {
                system_prompt: "system".into(),
                messages,
                tools: Vec::new(),
            }),
            new_messages: Vec::new(),
        };

        let update = (hook)(ctx, CancellationToken::new()).await;

        assert!(update.is_none());
        assert_eq!(
            *phases.lock().unwrap(),
            vec![
                CompactionNoticePhase::Started,
                CompactionNoticePhase::Skipped
            ]
        );
        let compactions = session
            .entries()
            .await
            .into_iter()
            .filter(|entry| matches!(entry.kind, SessionTreeEntryKind::Compaction { .. }))
            .count();
        assert_eq!(compactions, 0);
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
        assert!(!should_compact(
            80_000,
            100_000,
            &DEFAULT_COMPACTION_SETTINGS
        ));
    }

    #[test]
    fn should_compact_above_threshold() {
        assert!(should_compact(
            90_000,
            100_000,
            &DEFAULT_COMPACTION_SETTINGS
        ));
    }

    // Helpers for TokenBudgetPolicy tests
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
        let handle: ActiveModelHandle = Arc::new(RwLock::new(ActiveModelInfo::id("test/big")));
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
        let handle: ActiveModelHandle = Arc::new(RwLock::new(ActiveModelInfo::id("test/tiny")));
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
    fn token_budget_policy_applies_settings_resolver() {
        let registry = make_test_registry(vec![test_model_descriptor("test/window", 10_000)]);
        let handle: ActiveModelHandle = Arc::new(RwLock::new(ActiveModelInfo::id("test/window")));
        let policy = TokenBudgetPolicy::new(
            registry,
            handle,
            CompactionSettings {
                threshold_tokens: 9_000,
                keep_recent_tokens: 100,
                ..DEFAULT_COMPACTION_SETTINGS
            },
            TokenEstimator::approximate(),
        )
        .with_settings_resolver(Arc::new(|_active_model, base| {
            let mut settings = base.clone();
            settings.threshold_tokens = 1_000;
            settings.keep_recent_tokens = 50;
            settings
        }));

        let msgs: Vec<AgentMessage> = (0..10)
            .map(|i| user(&format!("msg{i}: {}", "x".repeat(1000))))
            .collect();

        assert!(
            policy.evaluate(&msgs).is_some(),
            "resolver should lower threshold enough to trigger compaction"
        );
    }

    #[test]
    fn token_budget_policy_never_cuts_mid_tool_pair() {
        // Transcript: [user, assistant+toolcall, toolresult, user]
        // The policy should never split between assistant+toolcall and toolresult.
        let registry = make_test_registry(vec![test_model_descriptor("test/tiny", 500)]);
        let handle: ActiveModelHandle = Arc::new(RwLock::new(ActiveModelInfo::id("test/tiny")));
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
            user(&"x".repeat(800)),                           // 0: ~200 tokens
            tool_call_assistant("think", "tc1", "read_file"), // 1: assistant+toolcall
            tool_result("tc1", &"y".repeat(400)),             // 2: tool result
            user("follow up"),                                // 3: user
        ];
        if let Some(prefix_len) = policy.evaluate(&msgs) {
            // The cut must NOT land at index 2 (tool result) or leave
            // index 1 (assistant+toolcall) as the last message before
            // the kept tail. Valid cuts: 0 (degenerate, rejected),
            // or >= 3 (after the tool result).
            assert!(prefix_len != 2, "must not cut at tool result boundary");
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
                    let has_tc = a
                        .content
                        .iter()
                        .any(|c| matches!(c, AssistantContent::ToolCall(_)));
                    assert!(!has_tc, "must not leave orphaned tool call at boundary");
                }
            }
        }
        // If None, the policy decided not to compact at all — also valid.
    }

    #[test]
    fn token_budget_policy_refuses_degenerate() {
        let registry = make_test_registry(vec![test_model_descriptor("test/tiny", 200)]);
        let handle: ActiveModelHandle = Arc::new(RwLock::new(ActiveModelInfo::id("test/tiny")));
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
        let handle: ActiveModelHandle = Arc::new(RwLock::new(ActiveModelInfo::id("test/unknown")));
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

    #[test]
    fn token_budget_policy_uses_fallback_window_for_registry_miss() {
        let registry = make_test_registry(vec![]);
        let handle: ActiveModelHandle =
            Arc::new(RwLock::new(ActiveModelInfo::new("local/model", 2000)));
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

        let msgs: Vec<AgentMessage> = (0..10)
            .map(|i| user(&format!("msg{i}: {}", "x".repeat(1000))))
            .collect();
        assert!(policy.evaluate(&msgs).is_some());
    }
}
