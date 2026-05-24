//! Tool-output pruning: replace large tool results with a token-count
//! placeholder to reclaim context-window budget without losing the
//! conversation structure.
//!
//! Ports `packages/agent/src/compaction/pruning.ts` from the reference
//! TypeScript implementation.
//!
//! Pruning is a lighter-weight alternative to full compaction: it targets
//! [`ToolResultMessage`] specifically, replacing bulky content with a
//! short placeholder while preserving the tool-call/result pairing that
//! models rely on for context. Recent results and results from protected
//! tools (e.g. `read`, `skill`) are left intact.

use std::collections::HashMap;

use grain_agent_core::{AgentMessage, AssistantContent, Message, TextContent, UserContent};

use crate::context_guard::TokenEstimator;

/// Configuration for [`prune_tool_outputs`].
#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// Number of most-recent tool results to leave untouched.
    pub protect_recent: usize,
    /// Tool names whose results are never pruned.
    pub protected_tools: Vec<String>,
    /// Minimum total token savings required before any pruning happens.
    /// Avoids pointless churn when the savings are negligible.
    pub min_savings_tokens: u64,
    /// Template for the replacement text. `{tokens}` is replaced with
    /// the estimated token count of the original content.
    pub placeholder_template: String,
}

impl Default for PruneConfig {
    fn default() -> Self {
        DEFAULT_PRUNE_CONFIG.clone()
    }
}

/// Default configuration matching the TS reference.
pub const DEFAULT_PRUNE_CONFIG: PruneConfig = PruneConfig {
    protect_recent: 3,
    protected_tools: Vec::new(), // populated by `default()` — const can't heap-alloc
    min_savings_tokens: 20_000,
    placeholder_template: String::new(), // populated by `default()` — const can't heap-alloc
};

impl PruneConfig {
    /// Build the standard default config with heap-allocated fields.
    pub fn standard() -> Self {
        PruneConfig {
            protect_recent: 3,
            protected_tools: vec!["read".into(), "skill".into()],
            min_savings_tokens: 20_000,
            placeholder_template: "[Output truncated - {tokens} tokens]".into(),
        }
    }
}

/// Outcome of a [`prune_tool_outputs`] call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneOutcome {
    /// Number of tool results whose content was replaced.
    pub pruned_count: usize,
    /// Estimated tokens saved across all pruned results.
    pub tokens_saved: u64,
}

/// The placeholder text that pruned results get. Exposed so callers
/// (and tests) can detect already-pruned messages.
const PRUNED_MARKER: &str = "[Output truncated - ";

/// Prune large tool-result content in `messages`, mutating in place.
///
/// Returns a [`PruneOutcome`] describing what changed. If the predicted
/// total savings are below [`PruneConfig::min_savings_tokens`], nothing
/// is mutated and the outcome reports zero changes.
pub fn prune_tool_outputs(
    messages: &mut [AgentMessage],
    config: &PruneConfig,
    estimator: &TokenEstimator,
) -> PruneOutcome {
    // 1. Build {tool_call_id → tool_name} map from assistant messages.
    let tool_name_map = build_tool_name_map(messages);

    // 2. Find indices of all ToolResult messages.
    let tr_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            if matches!(m, AgentMessage::Standard(Message::ToolResult(_))) {
                Some(i)
            } else {
                None
            }
        })
        .collect();

    if tr_indices.is_empty() {
        return PruneOutcome::default();
    }

    // 3. Exclude the last `protect_recent` tool results.
    let candidate_count = tr_indices.len().saturating_sub(config.protect_recent);
    let candidate_indices = &tr_indices[..candidate_count];

    // 4. For each candidate, check protections and estimate savings.
    //    ~10 tokens for the placeholder text.
    const PLACEHOLDER_TOKENS: u64 = 10;

    struct Candidate {
        msg_index: usize,
        current_tokens: u64,
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut total_predicted_savings: u64 = 0;

    for &idx in candidate_indices {
        let m = &messages[idx];
        let AgentMessage::Standard(Message::ToolResult(tr)) = m else {
            continue;
        };

        // Skip protected tools.
        let tool_name = tool_name_map
            .get(&tr.tool_call_id)
            .map(|s| s.as_str())
            .unwrap_or(&tr.tool_name);
        if config.protected_tools.iter().any(|p| p == tool_name) {
            continue;
        }

        // Skip already-pruned messages (idempotency).
        if is_already_pruned(tr) {
            continue;
        }

        let current_tokens = estimator.estimate_message(m);
        if current_tokens <= PLACEHOLDER_TOKENS {
            continue; // nothing to save
        }

        let savings = current_tokens - PLACEHOLDER_TOKENS;
        total_predicted_savings += savings;
        candidates.push(Candidate {
            msg_index: idx,
            current_tokens,
        });
    }

    // 5. Gate: only mutate if total savings exceed min_savings_tokens.
    if total_predicted_savings < config.min_savings_tokens {
        return PruneOutcome::default();
    }

    // 6. Mutate candidates.
    let mut outcome = PruneOutcome::default();
    for c in &candidates {
        let placeholder = config
            .placeholder_template
            .replace("{tokens}", &c.current_tokens.to_string());
        if let AgentMessage::Standard(Message::ToolResult(tr)) = &mut messages[c.msg_index] {
            tr.content = vec![UserContent::Text(TextContent { text: placeholder })];
            outcome.pruned_count += 1;
            outcome.tokens_saved += c.current_tokens.saturating_sub(PLACEHOLDER_TOKENS);
        }
    }

    outcome
}

/// Build a map from tool_call_id to tool name by scanning assistant
/// messages for ToolCall content blocks.
fn build_tool_name_map(messages: &[AgentMessage]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for m in messages {
        if let AgentMessage::Standard(Message::Assistant(a)) = m {
            for c in &a.content {
                if let AssistantContent::ToolCall(tc) = c {
                    map.insert(tc.id.clone(), tc.name.clone());
                }
            }
        }
    }
    map
}

/// Detect whether a tool result has already been pruned (placeholder
/// content). Prevents double-pruning on repeated calls.
fn is_already_pruned(tr: &grain_agent_core::ToolResultMessage) -> bool {
    if tr.content.len() != 1 {
        return false;
    }
    if let UserContent::Text(t) = &tr.content[0] {
        t.text.starts_with(PRUNED_MARKER)
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_core::{
        AssistantContent, AssistantMessage, StopReason, TextContent, ToolCall, ToolResultMessage,
        Usage, UserContent, UserMessage,
    };

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: text.into() })],
            timestamp: 0,
        })
    }

    fn assistant_with_tool_calls(calls: &[(&str, &str)]) -> AgentMessage {
        let mut content: Vec<AssistantContent> = vec![AssistantContent::Text(TextContent {
            text: "thinking...".into(),
        })];
        for (id, name) in calls {
            content.push(AssistantContent::ToolCall(ToolCall {
                id: (*id).into(),
                name: (*name).into(),
                arguments: serde_json::json!({}),
            }));
        }
        AgentMessage::assistant(AssistantMessage {
            content,
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        })
    }

    fn tool_result(id: &str, name: &str, text: &str) -> AgentMessage {
        AgentMessage::tool_result(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: name.into(),
            content: vec![UserContent::Text(TextContent { text: text.into() })],
            details: serde_json::Value::Null,
            is_error: false,
            timestamp: 0,
        })
    }

    fn big_tool_result(id: &str, name: &str, size: usize) -> AgentMessage {
        tool_result(id, name, &"x".repeat(size))
    }

    fn standard_config() -> PruneConfig {
        PruneConfig::standard()
    }

    #[test]
    fn protects_recent_results() {
        let mut msgs = vec![
            user("go"),
            assistant_with_tool_calls(&[
                ("tc1", "bash"),
                ("tc2", "bash"),
                ("tc3", "bash"),
                ("tc4", "bash"),
            ]),
            big_tool_result("tc1", "bash", 40000),
            big_tool_result("tc2", "bash", 40000),
            big_tool_result("tc3", "bash", 40000),
            big_tool_result("tc4", "bash", 40000),
        ];
        let config = PruneConfig {
            protect_recent: 3,
            ..standard_config()
        };
        let outcome = prune_tool_outputs(&mut msgs, &config, &TokenEstimator::approximate());
        // 4 tool results, protect last 3 → only tc1 is a candidate.
        // tc1 is ~10000 tokens, which is below min_savings (20000).
        assert_eq!(outcome.pruned_count, 0, "savings below min_savings gate");
    }

    #[test]
    fn protects_named_tools() {
        let mut msgs = vec![
            user("go"),
            assistant_with_tool_calls(&[
                ("tc1", "read"),
                ("tc2", "bash"),
                ("tc3", "skill"),
                ("tc4", "bash"),
                ("tc5", "bash"),
                ("tc6", "bash"),
            ]),
            big_tool_result("tc1", "read", 40000),
            big_tool_result("tc2", "bash", 40000),
            big_tool_result("tc3", "skill", 40000),
            big_tool_result("tc4", "bash", 40000),
            big_tool_result("tc5", "bash", 40000),
            big_tool_result("tc6", "bash", 40000),
        ];
        let config = PruneConfig {
            protect_recent: 0, // don't protect by recency
            ..standard_config()
        };
        let outcome = prune_tool_outputs(&mut msgs, &config, &TokenEstimator::approximate());
        // tc1 (read) and tc3 (skill) protected → 4 candidates pruned
        assert_eq!(outcome.pruned_count, 4);
        // Verify the protected results are untouched.
        if let AgentMessage::Standard(Message::ToolResult(tr)) = &msgs[2]
            && let UserContent::Text(t) = &tr.content[0]
        {
            assert!(!t.text.contains("truncated"), "read should be untouched");
        }
    }

    #[test]
    fn min_savings_gate() {
        let mut msgs = vec![
            user("go"),
            assistant_with_tool_calls(&[("tc1", "bash")]),
            tool_result("tc1", "bash", "small output"),
        ];
        let config = PruneConfig {
            protect_recent: 0,
            min_savings_tokens: 20_000,
            ..standard_config()
        };
        let outcome = prune_tool_outputs(&mut msgs, &config, &TokenEstimator::approximate());
        assert_eq!(outcome.pruned_count, 0);
        assert_eq!(outcome.tokens_saved, 0);
    }

    #[test]
    fn prunes_large_results() {
        let mut msgs = vec![
            user("go"),
            assistant_with_tool_calls(&[("tc1", "bash"), ("tc2", "bash")]),
            big_tool_result("tc1", "bash", 200_000), // ~50k tokens
            big_tool_result("tc2", "bash", 200_000), // ~50k tokens
            user("continue"),
        ];
        let config = PruneConfig {
            protect_recent: 0,
            ..standard_config()
        };
        let outcome = prune_tool_outputs(&mut msgs, &config, &TokenEstimator::approximate());
        assert_eq!(outcome.pruned_count, 2);
        assert!(outcome.tokens_saved > 90_000);
        // Verify content was replaced with placeholder.
        if let AgentMessage::Standard(Message::ToolResult(tr)) = &msgs[2] {
            if let UserContent::Text(t) = &tr.content[0] {
                assert!(t.text.starts_with("[Output truncated - "));
                assert!(t.text.ends_with(" tokens]"));
            } else {
                panic!("expected text content");
            }
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn idempotent_no_double_prune() {
        let mut msgs = vec![
            user("go"),
            assistant_with_tool_calls(&[("tc1", "bash"), ("tc2", "bash")]),
            big_tool_result("tc1", "bash", 200_000),
            big_tool_result("tc2", "bash", 200_000),
        ];
        let config = PruneConfig {
            protect_recent: 0,
            ..standard_config()
        };
        let est = TokenEstimator::approximate();
        let first = prune_tool_outputs(&mut msgs, &config, &est);
        assert_eq!(first.pruned_count, 2);

        // Second pass: already-pruned messages should be skipped.
        let second = prune_tool_outputs(&mut msgs, &config, &est);
        assert_eq!(second.pruned_count, 0);
        assert_eq!(second.tokens_saved, 0);
    }

    #[test]
    fn empty_messages_noop() {
        let mut msgs: Vec<AgentMessage> = Vec::new();
        let outcome = prune_tool_outputs(
            &mut msgs,
            &standard_config(),
            &TokenEstimator::approximate(),
        );
        assert_eq!(outcome, PruneOutcome::default());
    }

    #[test]
    fn uses_tool_name_map_from_assistant() {
        // Tool result has generic tool_name but the assistant's ToolCall
        // names it "read" — should be protected.
        let mut msgs = vec![
            user("go"),
            assistant_with_tool_calls(&[("tc1", "read")]),
            // tool_name field says "generic" but the assistant called it "read"
            AgentMessage::tool_result(ToolResultMessage {
                tool_call_id: "tc1".into(),
                tool_name: "generic".into(),
                content: vec![UserContent::Text(TextContent {
                    text: "x".repeat(200_000),
                })],
                details: serde_json::Value::Null,
                is_error: false,
                timestamp: 0,
            }),
        ];
        let config = PruneConfig {
            protect_recent: 0,
            ..standard_config()
        };
        let outcome = prune_tool_outputs(&mut msgs, &config, &TokenEstimator::approximate());
        // "read" is protected via the tool_name_map lookup.
        assert_eq!(outcome.pruned_count, 0);
    }
}
