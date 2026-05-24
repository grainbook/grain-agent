//! Cache-stability building blocks for long sessions.
//!
//! Two pieces, both provider-agnostic:
//!
//! - [`PinnedSystemPrompt`] — a one-shot snapshot of the immutable
//!   session prefix (base system prompt + rendered `<available_skills>`
//!   block). Build it **once per session** and feed the same string to
//!   every turn so the upstream prefix-cache stays warm.
//!
//! - [`append_only_guard`] — a [`ConvertToLlmFn`] decorator that emits
//!   a `[warn]` when any previously-seen message changes between turns.
//!   The whole point of an append-only log is to be appended to; any
//!   in-place rewrite shifts bytes the provider had already cached.
//!
//! Inspired by DeepSeek-Reasonix's "Pillar 1 — Cache-First Loop"
//! design. The mechanism is generic: anything with a prefix cache
//! (Anthropic, OpenAI, Gemini, DeepSeek) benefits from the same
//! invariants.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use grain_agent_core::{AgentMessage, ConvertToLlmFn, Message};
use tokio::sync::Mutex;

use crate::system_prompt::{Skill, format_skills_for_system_prompt};

/// Frozen snapshot of the immutable session prefix.
///
/// The contents are a base system prompt concatenated with the
/// `<available_skills>` block rendered from a skills slice at the
/// moment of construction. The digest is a stable hash of the final
/// string — useful for asserting at end-of-session that nothing
/// re-rendered the prompt out from under the cache.
#[derive(Debug, Clone)]
pub struct PinnedSystemPrompt {
    text: Arc<str>,
    digest: u64,
}

impl PinnedSystemPrompt {
    /// Build the snapshot. `base` is the bare instruction prompt;
    /// `skills` are rendered into an `<available_skills>` block and
    /// appended (separated by a blank line). Pass an empty `skills`
    /// slice when there are none — the block is suppressed entirely.
    pub fn build(base: impl Into<String>, skills: &[Skill]) -> Self {
        let mut text = base.into();
        let block = format_skills_for_system_prompt(skills);
        if !block.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&block);
        }
        let digest = hash_str(&text);
        PinnedSystemPrompt {
            text: Arc::from(text.into_boxed_str()),
            digest,
        }
    }

    /// Borrow the pinned prompt text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Convert to an owned `String` for callers that need to feed it
    /// to `AgentOptions::system_prompt` (which is `String`-typed). This
    /// allocates; prefer [`Self::as_str`] elsewhere.
    pub fn to_string_owned(&self) -> String {
        self.text.as_ref().to_string()
    }

    /// Stable 64-bit digest of the pinned text. Two builds with the
    /// same base + skills produce the same digest; any mutation to
    /// either input changes it.
    pub fn digest(&self) -> u64 {
        self.digest
    }

    /// Total length in bytes — handy for diagnostics ("session pinned
    /// 12.4 KB of prompt").
    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Wrap a [`ConvertToLlmFn`] in a debug-only sentry that warns when a
/// previously-seen message changes between turns.
///
/// The wrapper is otherwise a passthrough: output bytes are identical
/// to `inner`. On each call it hashes every message except the tail
/// (which is allowed to grow / be replaced) and compares against the
/// hashes captured on the previous call. Any divergence emits a
/// single `[warn]` line per offending index.
///
/// **Cost**: one JSON serialization + one `DefaultHasher` per message
/// per call. Cheap enough for development; consider gating behind a
/// feature flag or `cfg(debug_assertions)` in hot release paths.
pub fn append_only_guard(inner: ConvertToLlmFn) -> ConvertToLlmFn {
    let state: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    Arc::new(move |messages: Vec<AgentMessage>| {
        let state = state.clone();
        let inner = inner.clone();
        Box::pin(async move {
            let out: Vec<Message> = inner(messages).await;
            check_and_record(&out, &state).await;
            out
        })
    })
}

async fn check_and_record(out: &[Message], state: &Arc<Mutex<Vec<u64>>>) {
    // Hash everything except the last message: the tail is the new
    // turn and is allowed to be different / longer than before.
    let trimmed_len = out.len().saturating_sub(1);
    let current: Vec<u64> = out[..trimmed_len].iter().map(stable_hash).collect();
    let mut prev = state.lock().await;
    for idx in diff_violations(&prev, &current) {
        eprintln!(
            "[warn] append-only-guard: message {idx} changed between turns; \
             prefix cache will miss for the rest of the session"
        );
    }
    *prev = current;
}

/// Indices `i` where `prev[i] != current[i]` within the common prefix.
/// Pure function — testable without the async wrapper.
fn diff_violations(prev: &[u64], current: &[u64]) -> Vec<usize> {
    let common = prev.len().min(current.len());
    (0..common).filter(|&i| prev[i] != current[i]).collect()
}

fn stable_hash(m: &Message) -> u64 {
    // serde_json::to_string is deterministic for the structs in
    // grain_agent_core (no HashMap field shuffling that would otherwise
    // make this unstable). Fall back to an empty digest on the rare
    // serialization failure — at worst we miss a warning, never panic
    // a turn.
    let s = serde_json::to_string(m).unwrap_or_default();
    hash_str(&s)
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_core::{TextContent, UserContent, UserMessage};

    fn skill(name: &str, desc: &str) -> Skill {
        Skill {
            name: name.into(),
            description: desc.into(),
            file_path: String::new(),
            disable_model_invocation: false,
            body: String::new(),
        }
    }

    fn user_msg(text: &str) -> Message {
        Message::User(UserMessage {
            content: vec![UserContent::Text(TextContent { text: text.into() })],
            timestamp: 0,
        })
    }

    // -------- PinnedSystemPrompt ------------------------------------

    #[test]
    fn pinned_prompt_with_no_skills_is_just_base() {
        let p = PinnedSystemPrompt::build("you are a helpful agent", &[]);
        assert_eq!(p.as_str(), "you are a helpful agent");
        assert!(!p.is_empty());
    }

    #[test]
    fn pinned_prompt_includes_skills_block_separated_by_blank_line() {
        let skills = vec![skill("Code", "Edit code carefully")];
        let p = PinnedSystemPrompt::build("base", &skills);
        let text = p.as_str();
        // Base comes first.
        assert!(text.starts_with("base\n\n"));
        // Skills block follows.
        assert!(text.contains("Code"));
        assert!(text.contains("Edit code carefully"));
    }

    #[test]
    fn pinned_prompt_with_empty_base_renders_skills_alone() {
        let skills = vec![skill("Code", "x")];
        let p = PinnedSystemPrompt::build("", &skills);
        // No leading blank line when base is empty.
        assert!(!p.as_str().starts_with("\n"));
    }

    #[test]
    fn digest_is_deterministic_for_same_input() {
        let s = vec![skill("A", "a"), skill("B", "b")];
        let p1 = PinnedSystemPrompt::build("base", &s);
        let p2 = PinnedSystemPrompt::build("base", &s);
        assert_eq!(p1.digest(), p2.digest());
        assert_eq!(p1.as_str(), p2.as_str());
    }

    #[test]
    fn digest_changes_when_base_changes() {
        let s = vec![skill("A", "a")];
        let p1 = PinnedSystemPrompt::build("base v1", &s);
        let p2 = PinnedSystemPrompt::build("base v2", &s);
        assert_ne!(p1.digest(), p2.digest());
    }

    #[test]
    fn digest_changes_when_skills_change() {
        let p1 = PinnedSystemPrompt::build("base", &[skill("A", "a")]);
        let p2 = PinnedSystemPrompt::build("base", &[skill("A", "a"), skill("B", "b")]);
        assert_ne!(p1.digest(), p2.digest());
    }

    #[test]
    fn len_matches_string_len() {
        let p = PinnedSystemPrompt::build("hello", &[]);
        assert_eq!(p.len(), "hello".len());
    }

    // -------- diff_violations (pure helper) -------------------------

    #[test]
    fn diff_violations_empty_when_prefix_matches() {
        let prev = vec![1, 2, 3];
        let current = vec![1, 2, 3, 4]; // tail grew — fine
        assert!(diff_violations(&prev, &current).is_empty());
    }

    #[test]
    fn diff_violations_reports_changed_indices() {
        let prev = vec![1, 2, 3];
        let current = vec![1, 99, 3, 4];
        assert_eq!(diff_violations(&prev, &current), vec![1]);
    }

    #[test]
    fn diff_violations_only_checks_common_prefix() {
        // Previous run was longer (somehow), no current entry to
        // compare past index 2.
        let prev = vec![1, 2, 3, 4];
        let current = vec![1, 99];
        assert_eq!(diff_violations(&prev, &current), vec![1]);
    }

    // -------- append_only_guard wrapper -----------------------------

    fn passthrough_inner(out: Vec<Message>) -> ConvertToLlmFn {
        Arc::new(move |_| {
            let out = out.clone();
            Box::pin(async move { out })
        })
    }

    #[tokio::test]
    async fn guard_is_transparent_passthrough() {
        let inner_out = vec![user_msg("hello"), user_msg("world")];
        let guarded = append_only_guard(passthrough_inner(inner_out.clone()));
        let got = guarded(vec![]).await;
        assert_eq!(got.len(), inner_out.len());
        assert_eq!(stable_hash(&got[0]), stable_hash(&inner_out[0]));
    }

    #[tokio::test]
    async fn guard_records_then_compares_against_prior_call() {
        // Two-call sequence: first establishes baseline, second
        // mutates message 0 — guard must catch it.
        let state: Arc<Mutex<Vec<Message>>> =
            Arc::new(Mutex::new(vec![user_msg("alpha"), user_msg("tail-v1")]));
        let state_for_inner = state.clone();
        let inner: ConvertToLlmFn = Arc::new(move |_| {
            let state = state_for_inner.clone();
            Box::pin(async move { state.lock().await.clone() })
        });
        let guarded = append_only_guard(inner);

        let _ = guarded(vec![]).await; // populates baseline
        // Mutate prefix message 0 — would invalidate the cache.
        *state.lock().await = vec![user_msg("alpha-MUTATED"), user_msg("tail-v2")];
        let _ = guarded(vec![]).await; // would emit warn

        // We can't capture stderr here without extra plumbing, but
        // the surface we test is `diff_violations` (above). This
        // smoke just confirms the wrapper doesn't panic or block on
        // mutated input.
    }
}
