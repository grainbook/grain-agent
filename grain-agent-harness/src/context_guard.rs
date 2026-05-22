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
use std::sync::{Arc, RwLock};

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

/// Fixed UTF-8-bytes-per-token estimator.
///
/// Counts **bytes**, not Unicode characters: BPE tokenizers (cl100k,
/// o200k) charge roughly 3-4 bytes/token across English, code, and
/// CJK alike, while chars/token swings wildly (4 for English, ~1 for
/// Chinese). At the default 4.0 ratio, ASCII estimates are identical
/// to the legacy chars/4 behavior; CJK is now ~0.75 tokens/char
/// instead of 0.25 — close to the real ~1 token/char BPE charges.
#[derive(Debug, Clone, Copy)]
pub struct TokenEstimator {
    bytes_per_token: f64,
    /// Tokens charged on top of content for each transcript message —
    /// covers the JSON framing (`{"role":...,"content":[...]}`) and the
    /// per-message structural tokens BPE tokenizers assess. Without
    /// this, transcripts with many small messages (612-message
    /// debugging sessions, e.g.) drift several thousand tokens below
    /// reality because the framing is invisible to content estimation.
    per_message_overhead: u64,
}

impl Default for TokenEstimator {
    fn default() -> Self {
        TokenEstimator::approximate()
    }
}

impl TokenEstimator {
    /// Standard bytes-per-token approximation (4.0). ASCII content
    /// matches legacy chars/4 estimates; CJK is much closer to truth
    /// than the old chars-based variant. Defaults to 16 tokens
    /// per-message overhead — a conservative middle estimate for
    /// OpenAI/Anthropic-style JSON framing (real values land in the
    /// 7-30 range depending on message type).
    pub const fn approximate() -> Self {
        TokenEstimator {
            bytes_per_token: 4.0,
            per_message_overhead: 16,
        }
    }

    /// Customize the ratio. Values ≤ 0 are clamped to 1.0 to avoid divide-by-zero.
    pub fn with_bytes_per_token(n: f64) -> Self {
        let n = if n <= 0.0 { 1.0 } else { n };
        TokenEstimator {
            bytes_per_token: n,
            per_message_overhead: Self::approximate().per_message_overhead,
        }
    }

    /// Backwards-compat alias for [`Self::with_bytes_per_token`]. The
    /// "chars" name is a misnomer post the byte-counting switch but
    /// kept to avoid churning external callers.
    pub fn with_chars_per_token(n: f64) -> Self {
        Self::with_bytes_per_token(n)
    }

    /// Override the per-message JSON-framing overhead. Set to 0 in
    /// unit tests that care about pure content accounting.
    pub fn with_per_message_overhead(mut self, n: u64) -> Self {
        self.per_message_overhead = n;
        self
    }

    /// The configured per-message JSON-framing overhead. Used by
    /// [`apply_policy`] and [`crate::compaction::TokenBudgetPolicy`]
    /// to charge framing tokens against the budget alongside content.
    pub fn per_message_overhead(&self) -> u64 {
        self.per_message_overhead
    }

    /// Tokens for a UTF-8 string (byte count divided by ratio).
    /// Counts bytes rather than chars so CJK content (3 bytes/char) is
    /// estimated near its real BPE token count instead of 4× under.
    pub fn estimate_string(&self, s: &str) -> u64 {
        let bytes = s.len();
        (bytes as f64 / self.bytes_per_token).ceil() as u64
    }

    /// Tokens for one [`AgentMessage`] — content only, no framing.
    /// Framing is added by [`Self::estimate_messages`] / policy code,
    /// not here, so single-message semantics stay focused on content.
    /// Images count as a flat 100 tokens.
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

    /// Tokens for a whole transcript including per-message framing.
    pub fn estimate_messages(&self, ms: &[AgentMessage]) -> u64 {
        let content: u64 = ms.iter().map(|m| self.estimate_message(m)).sum();
        content + self.per_message_overhead * (ms.len() as u64)
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

/// Shared handle to the active model id. Both [`ContextGuard`] and
/// [`crate::compaction::TokenBudgetPolicy`] read from the same handle so
/// a model switch mid-session is immediately visible to both subsystems.
pub type ActiveModelHandle = Arc<RwLock<String>>;

/// Builder + factory for a context-guard [`TransformContextFn`].
#[derive(Debug, Clone)]
pub struct ContextGuard {
    registry: Arc<Registry>,
    /// Shared mutable model id — read on every invocation of the
    /// produced [`TransformContextFn`] so a mid-session model switch
    /// takes effect immediately.
    model_handle: ActiveModelHandle,
    policy: ContextGuardPolicy,
    estimator: TokenEstimator,
    /// Tokens reserved for the model's response (output budget).
    /// Defaults to 1024 — bump this if you let the model produce long
    /// answers or use heavy reasoning.
    headroom_tokens: u64,
    /// Tokens spent on per-request fixed overhead the guard CAN'T see:
    /// system prompt, `<available_skills>` block, tool JSON schemas,
    /// and per-message framing the provider adds on top of message
    /// content. The transform fn only receives `Vec<AgentMessage>`,
    /// so anything outside that vector has to be subtracted up front
    /// or the budget calculation runs hot.
    ///
    /// Compute this once at agent boot — see
    /// [`Self::with_system_overhead_tokens`].
    system_overhead_tokens: u64,
}

impl ContextGuard {
    /// Create a guard for `model_id` (looked up in `registry`).
    ///
    /// If the model isn't in the registry at hook time, the guard becomes a
    /// no-op rather than failing — easier to wire defensively.
    pub fn new(registry: Arc<Registry>, model_id: impl Into<String>) -> Self {
        ContextGuard {
            registry,
            model_handle: Arc::new(RwLock::new(model_id.into())),
            policy: ContextGuardPolicy::default(),
            estimator: TokenEstimator::approximate(),
            headroom_tokens: 1024,
            system_overhead_tokens: 0,
        }
    }

    /// Create a guard that reads from a pre-existing shared model handle.
    ///
    /// Use this when you need multiple subsystems (context guard, compaction
    /// policy) to track the same active model — pass the same
    /// [`ActiveModelHandle`] to each.
    pub fn with_active_model_handle(
        registry: Arc<Registry>,
        handle: ActiveModelHandle,
    ) -> Self {
        ContextGuard {
            registry,
            model_handle: handle,
            policy: ContextGuardPolicy::default(),
            estimator: TokenEstimator::approximate(),
            headroom_tokens: 1024,
            system_overhead_tokens: 0,
        }
    }

    /// Update the active model id. Takes effect on the next invocation
    /// of the [`TransformContextFn`] produced by [`Self::into_transform_fn`].
    pub fn set_active_model(&self, id: impl Into<String>) {
        if let Ok(mut guard) = self.model_handle.write() {
            *guard = id.into();
        }
    }

    /// Return a clone of the shared model handle. Useful for passing to
    /// other subsystems that need to read the same active model.
    pub fn model_handle(&self) -> ActiveModelHandle {
        self.model_handle.clone()
    }

    /// Set the guard policy (default: [`ContextGuardPolicy::DropOldest`]).
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

    /// Pre-charge the budget by the per-request fixed overhead the guard
    /// can't see directly — typically `system_prompt` + tool JSON
    /// schemas. Without this, the guard trims the transcript to just
    /// under the model's window, then the provider tacks on
    /// system+tools and the request lands over budget.
    ///
    /// Compute at boot, e.g.:
    /// ```ignore
    /// let estimator = TokenEstimator::approximate();
    /// let mut overhead = estimator.estimate_string(&system_prompt);
    /// for t in &tools {
    ///     overhead += estimator.estimate_string(
    ///         &serde_json::to_string(&t.definition().input_schema)
    ///             .unwrap_or_default(),
    ///     );
    /// }
    /// guard.with_system_overhead_tokens(overhead);
    /// ```
    pub fn with_system_overhead_tokens(mut self, n: u64) -> Self {
        self.system_overhead_tokens = n;
        self
    }

    /// Materialize a [`TransformContextFn`] you can drop into
    /// [`grain_agent_core::AgentOptions::transform_context`].
    pub fn into_transform_fn(self) -> TransformContextFn {
        let ContextGuard {
            registry,
            model_handle,
            policy,
            estimator,
            headroom_tokens,
            system_overhead_tokens,
        } = self;
        Arc::new(move |messages, _cancel| {
            let registry = registry.clone();
            // Read the *current* model id on every invocation — not a
            // stale snapshot from construction time.
            let model_id = model_handle
                .read()
                .map(|g| g.clone())
                .unwrap_or_default();
            let policy = policy.clone();
            Box::pin(async move {
                let budget = match registry.lookup(&model_id) {
                    Some(m) if m.context_window > 0 => m
                        .context_window
                        .saturating_sub(headroom_tokens)
                        .saturating_sub(system_overhead_tokens),
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
    // (the old implementation was O(n²)). Each entry includes the
    // estimator's per-message framing charge so DropOldest's running
    // total stays consistent with `estimator.estimate_messages(...)`
    // without needing to track framing separately.
    let framing = estimator.per_message_overhead();
    let per_message: Vec<u64> = messages
        .iter()
        .map(|m| estimator.estimate_message(m) + framing)
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

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_core::{TextContent, UserContent, UserMessage};
    use grain_llm_models::descriptor::{ApiKind, ModelDescriptor, ProviderId};
    use tokio_util::sync::CancellationToken;

    fn make_registry(models: Vec<ModelDescriptor>) -> Arc<Registry> {
        Arc::new(Registry::from_descriptors(models).unwrap())
    }

    fn test_descriptor(id: &str, name: &str, context_window: u64) -> ModelDescriptor {
        use grain_llm_models::descriptor::{Capabilities, ThinkingProfile};
        ModelDescriptor {
            id: id.into(),
            name: name.into(),
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

    fn small_model() -> ModelDescriptor {
        test_descriptor("test/small", "Small", 1000)
    }

    fn big_model() -> ModelDescriptor {
        test_descriptor("test/big", "Big", 1_000_000)
    }

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: text.into() })],
            timestamp: 0,
        })
    }

    /// Build a transcript whose estimated token count exceeds `small_model`'s
    /// context window (1000 tokens) but fits `big_model` (1M tokens).
    fn large_transcript() -> Vec<AgentMessage> {
        // Each message ~1000 chars → ~250 tokens at 4 chars/token.
        // 6 messages → ~1500 tokens, exceeds the small model's 1000-token budget.
        (0..6)
            .map(|i| user_msg(&format!("msg{i}: {}", "x".repeat(1000))))
            .collect()
    }

    #[tokio::test]
    async fn guard_uses_current_model_not_construction_time() {
        let registry = make_registry(vec![small_model(), big_model()]);
        let guard = ContextGuard::new(registry, "test/small")
            .with_policy(ContextGuardPolicy::DropOldest)
            .with_headroom_tokens(0);

        let transform = guard.into_transform_fn();
        let messages = large_transcript();

        // With small model (1000 token window), the transcript should be truncated.
        let result = transform(messages.clone(), CancellationToken::new()).await;
        assert!(
            result.len() < messages.len(),
            "small model should truncate: got {} messages, expected fewer than {}",
            result.len(),
            messages.len()
        );
    }

    #[tokio::test]
    async fn set_active_model_switches_budget_dynamically() {
        let registry = make_registry(vec![small_model(), big_model()]);
        let guard = ContextGuard::new(registry, "test/small")
            .with_policy(ContextGuardPolicy::DropOldest)
            .with_headroom_tokens(0);

        // Grab a handle before consuming the guard.
        let handle = guard.model_handle();
        let transform = guard.into_transform_fn();
        let messages = large_transcript();

        // Small model → truncation.
        let result = transform(messages.clone(), CancellationToken::new()).await;
        assert!(result.len() < messages.len(), "small model should truncate");

        // Switch to big model via the handle.
        {
            let mut w = handle.write().unwrap();
            *w = "test/big".into();
        }

        // Big model → no truncation.
        let result = transform(messages.clone(), CancellationToken::new()).await;
        assert_eq!(
            result.len(),
            messages.len(),
            "big model should NOT truncate"
        );
    }

    #[tokio::test]
    async fn with_active_model_handle_shares_state() {
        let registry = make_registry(vec![small_model(), big_model()]);
        let shared_handle: ActiveModelHandle =
            Arc::new(RwLock::new("test/small".into()));

        let guard = ContextGuard::with_active_model_handle(
            registry,
            shared_handle.clone(),
        )
        .with_policy(ContextGuardPolicy::DropOldest)
        .with_headroom_tokens(0);

        let transform = guard.into_transform_fn();
        let messages = large_transcript();

        // Verify truncation with small model.
        let result = transform(messages.clone(), CancellationToken::new()).await;
        assert!(result.len() < messages.len());

        // External code writes through the shared handle.
        *shared_handle.write().unwrap() = "test/big".into();

        // Transform now sees the big model.
        let result = transform(messages.clone(), CancellationToken::new()).await;
        assert_eq!(result.len(), messages.len());
    }

    #[tokio::test]
    async fn unknown_model_is_noop() {
        let registry = make_registry(vec![small_model()]);
        let guard = ContextGuard::new(registry, "nonexistent/model")
            .with_policy(ContextGuardPolicy::DropOldest)
            .with_headroom_tokens(0);

        let transform = guard.into_transform_fn();
        let messages = large_transcript();
        let result = transform(messages.clone(), CancellationToken::new()).await;
        assert_eq!(result.len(), messages.len(), "unknown model should be no-op");
    }
}
