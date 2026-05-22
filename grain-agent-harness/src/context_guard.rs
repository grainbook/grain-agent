//! Context-window guard: a [`grain_agent_core::TransformContextFn`] that
//! consults a [`grain_llm_models::Registry`] for the model's context window
//! and applies a truncation policy before each turn so the request never
//! exceeds the budget.
//!
//! Token counts here are **estimated** with a fixed chars-per-token ratio —
//! good enough for budget enforcement without dragging in a tokenizer crate.
//! Swap in a real tokenizer later by providing a custom [`TokenEstimator`].
//!
//! Wiring:
//!
//! ```ignore
//! use std::sync::Arc;
//! use grain_agent_core::AgentOptions;
//! use grain_agent_harness::context_guard::{ContextGuard, ContextGuardPolicy};
//! use grain_llm_models::Registry;
//!
//! let registry = Arc::new(Registry::from_embedded_snapshot());
//! let guard = ContextGuard::new(registry, "anthropic/claude-sonnet-4-5")
//!     .with_policy(ContextGuardPolicy::DropOldest)
//!     .with_headroom_tokens(2048)
//!     .into_transform_fn();
//!
//! let mut opts = AgentOptions::new(model, stream_fn);
//! opts.transform_context = Some(guard);
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use grain_agent_core::{
    AgentMessage, AssistantContent, Message, ToolResultMessage, TransformContextFn,
    UserContent, UserMessage,
};
use grain_llm_models::Registry;

/// How to handle a transcript that exceeds the model's context budget.
#[derive(Debug, Clone, Default)]
pub enum ContextGuardPolicy {
    /// Drop messages from the head (oldest first) until the remaining
    /// transcript fits, but always keep at least one message so the
    /// agent loop can still make a request.
    #[default]
    DropOldest,
    /// Keep only the last `n` messages. Useful for "rolling window"
    /// conversations where older context is intentionally forgotten.
    KeepRecent(usize),
    /// Never truncate. Lets the hook observe overflow without acting on it
    /// (e.g. for logging / metrics callers that take action elsewhere).
    Identity,
}

/// Fixed chars-per-token estimator.
///
/// The default ratio (4.0 chars/token) is a generous overestimate for
/// English-heavy LLM transcripts. Tighten it (e.g. 2.5) for CJK-heavy
/// traffic to be more conservative, or pass a real tokenizer-backed
/// estimator once that becomes worth its weight.
#[derive(Debug, Clone, Copy)]
pub struct TokenEstimator {
    chars_per_token: f64,
}

impl Default for TokenEstimator {
    fn default() -> Self {
        TokenEstimator::approximate()
    }
}

impl TokenEstimator {
    /// Standard chars-per-token approximation (4.0).
    pub const fn approximate() -> Self {
        TokenEstimator { chars_per_token: 4.0 }
    }

    /// Customize the ratio. Values ≤ 0 are clamped to 1.0 to avoid divide-by-zero.
    pub fn with_chars_per_token(n: f64) -> Self {
        let n = if n <= 0.0 { 1.0 } else { n };
        TokenEstimator { chars_per_token: n }
    }

    /// Tokens for a UTF-8 string (character count divided by ratio).
    pub fn estimate_string(&self, s: &str) -> u64 {
        let chars = s.chars().count();
        (chars as f64 / self.chars_per_token).ceil() as u64
    }

    /// Tokens for one [`AgentMessage`]. Images count as a flat 100 tokens.
    pub fn estimate_message(&self, m: &AgentMessage) -> u64 {
        match m {
            AgentMessage::Standard(Message::User(u)) => self.estimate_user_message(u),
            AgentMessage::Standard(Message::Assistant(a)) => self.estimate_assistant_message(a),
            AgentMessage::Standard(Message::ToolResult(t)) => self.estimate_tool_result(t),
            AgentMessage::Custom(v) => {
                let s = serde_json::to_string(v).unwrap_or_default();
                self.estimate_string(&s)
            }
        }
    }

    /// Tokens for a whole transcript.
    pub fn estimate_messages(&self, ms: &[AgentMessage]) -> u64 {
        ms.iter().map(|m| self.estimate_message(m)).sum()
    }

    fn estimate_user_message(&self, m: &UserMessage) -> u64 {
        self.estimate_user_content(&m.content)
    }

    fn estimate_assistant_message(
        &self,
        m: &grain_agent_core::AssistantMessage,
    ) -> u64 {
        let mut total: u64 = 0;
        for c in &m.content {
            total += match c {
                AssistantContent::Text(t) => self.estimate_string(&t.text),
                AssistantContent::Thinking(t) => {
                    let mut n = self.estimate_string(&t.thinking);
                    if let Some(sig) = &t.signature {
                        n += self.estimate_string(sig);
                    }
                    n
                }
                AssistantContent::Image(_) => 100,
                AssistantContent::ToolCall(tc) => {
                    let args = serde_json::to_string(&tc.arguments).unwrap_or_default();
                    self.estimate_string(&tc.name) + self.estimate_string(&args)
                }
            };
        }
        total
    }

    fn estimate_tool_result(&self, m: &ToolResultMessage) -> u64 {
        self.estimate_user_content(&m.content)
    }

    fn estimate_user_content(&self, content: &[UserContent]) -> u64 {
        content
            .iter()
            .map(|c| match c {
                UserContent::Text(t) => self.estimate_string(&t.text),
                UserContent::Image(_) => 100,
            })
            .sum()
    }
}

/// Builder + factory for a context-guard [`TransformContextFn`].
#[derive(Debug, Clone)]
pub struct ContextGuard {
    registry: Arc<Registry>,
    model_id: String,
    policy: ContextGuardPolicy,
    estimator: TokenEstimator,
    /// Tokens reserved for the system prompt + the model's response.
    /// Defaults to 1024 — enough for a small system prompt and a short reply.
    headroom_tokens: u64,
}

impl ContextGuard {
    /// Create a guard for `model_id` (looked up in `registry`).
    ///
    /// If the model isn't in the registry at hook time, the guard becomes a
    /// no-op rather than failing — easier to wire defensively.
    pub fn new(registry: Arc<Registry>, model_id: impl Into<String>) -> Self {
        ContextGuard {
            registry,
            model_id: model_id.into(),
            policy: ContextGuardPolicy::default(),
            estimator: TokenEstimator::approximate(),
            headroom_tokens: 1024,
        }
    }

    /// Set the guard policy (default: [`ContextGuardPolicy::TruncateHead`]).
    pub fn with_policy(mut self, policy: ContextGuardPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set the token estimator (default: [`TokenEstimator::approximate()`]).
    pub fn with_estimator(mut self, estimator: TokenEstimator) -> Self {
        self.estimator = estimator;
        self
    }

    /// Override headroom tokens reserved for the assistant response.
    /// Default is 1024.
    pub fn with_headroom_tokens(mut self, n: u64) -> Self {
        self.headroom_tokens = n;
        self
    }

    /// Materialize a [`TransformContextFn`] you can drop into
    /// [`grain_agent_core::AgentOptions::transform_context`].
    pub fn into_transform_fn(self) -> TransformContextFn {
        let ContextGuard {
            registry,
            model_id,
            policy,
            estimator,
            headroom_tokens,
        } = self;
        Arc::new(move |messages, _cancel| {
            let registry = registry.clone();
            let model_id = model_id.clone();
            let policy = policy.clone();
            Box::pin(async move {
                let budget = match registry.lookup(&model_id) {
                    Some(m) if m.context_window > 0 => {
                        m.context_window.saturating_sub(headroom_tokens)
                    }
                    _ => return messages, // unknown model — no-op
                };
                apply_policy(messages, budget, &policy, &estimator)
            })
        })
    }
}

/// Apply the policy and return the resulting transcript.
///
/// After truncating by policy, any [`ToolResultMessage`] whose
/// [`ToolResultMessage::tool_call_id`] no longer references a preceding
/// assistant tool-call is removed — orphaned tool results are rejected by
/// most providers (Anthropic returns 400).
fn apply_policy(
    messages: Vec<AgentMessage>,
    budget: u64,
    policy: &ContextGuardPolicy,
    estimator: &TokenEstimator,
) -> Vec<AgentMessage> {
    // Per-message token estimates — compute once and reuse instead of
    // re-summing the whole transcript on every iteration of DropOldest
    // (the old implementation was O(n²)).
    let per_message: Vec<u64> = messages
        .iter()
        .map(|m| estimator.estimate_message(m))
        .collect();
    let total: u64 = per_message.iter().sum();
    if total <= budget {
        return messages;
    }

    let mut truncated = match policy {
        ContextGuardPolicy::Identity => messages,
        ContextGuardPolicy::KeepRecent(n) => {
            // Drop from the head to keep the last N. If those N still
            // exceed budget we additionally peel oldest off the front
            // until we fit (or only one message remains).
            let keep = (*n).min(messages.len());
            let drop_n = messages.len() - keep;
            let mut kept_total: u64 = per_message[drop_n..].iter().sum();
            let mut messages: Vec<AgentMessage> = messages.into_iter().skip(drop_n).collect();
            let mut per_message: Vec<u64> = per_message[drop_n..].to_vec();
            let mut head = 0usize;
            while messages.len() - head > 1 && kept_total > budget {
                kept_total -= per_message[head];
                head += 1;
            }
            if head > 0 {
                messages.drain(..head);
                per_message.drain(..head);
            }
            messages
        }
        ContextGuardPolicy::DropOldest => {
            // Running total: subtract dropped messages' estimates as we
            // peel them off instead of rescanning. Single O(n) pass.
            let mut running = total;
            let mut head = 0usize;
            while messages.len() - head > 1 && running > budget {
                running -= per_message[head];
                head += 1;
            }
            let mut messages = messages;
            messages.drain(..head);
            messages
        }
    };

    remove_orphan_tool_results(&mut truncated);
    truncated
}

/// After truncation, drop any tool-result whose `tool_call_id` no longer
/// has a matching tool-call in an earlier assistant message in the trimmed
/// transcript. Orphan tool results trip provider validation (Anthropic 400,
/// OpenAI silent failures).
fn remove_orphan_tool_results(messages: &mut Vec<AgentMessage>) {
    let mut known_ids: HashSet<String> = HashSet::new();
    let mut keep: Vec<bool> = Vec::with_capacity(messages.len());
    for m in messages.iter() {
        match m {
            AgentMessage::Standard(Message::Assistant(a)) => {
                for c in &a.content {
                    if let AssistantContent::ToolCall(tc) = c {
                        known_ids.insert(tc.id.clone());
                    }
                }
                keep.push(true);
            }
            AgentMessage::Standard(Message::ToolResult(tr)) => {
                keep.push(known_ids.contains(&tr.tool_call_id));
            }
            _ => keep.push(true),
        }
    }
    let mut idx = 0usize;
    messages.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
}
