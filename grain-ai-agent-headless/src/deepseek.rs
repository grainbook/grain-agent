//! DeepSeek-specific hooks wired through the headless runtime.
//!
//! Wraps [`grain_deepseek_pack`]'s two passive parsers (reasoning
//! scavenge, subagent.done detection) in a single lightweight handle
//! gated behind [`is_deepseek_model`]. Non-DeepSeek models pay zero
//! overhead — every method on a disabled pack returns an empty `Vec`.
//!
//! # Usage
//!
//! ```ignore
//! use grain_ai_agent_headless::DeepSeekPack;
//!
//! let ds = DeepSeekPack::new(&model);
//!
//! // After assistant message is finalized:
//! for call in ds.scavenge_reasoning(&thinking_text) {
//!     eprintln!("[warn] recovered tool call from reasoning: {}", call.name);
//! }
//! for ev in ds.parse_subagent_done(&assistant_text) {
//!     if let Some(payload) = &ev.payload {
//!         eprintln!("[info] subagent done: {payload}");
//!     }
//! }
//! ```

use grain_agent_core::Model;
use grain_deepseek_pack::{ScavengedToolCall, SubagentDoneEvent, is_deepseek_model};

/// Gated handle for DeepSeek-specific post-processing.
///
/// Created once at boot from the active [`Model`]. When the model is
/// not a DeepSeek endpoint, every method returns an empty `Vec` — the
/// crate's logic is entirely skipped, avoiding both overhead and the
/// risk of false positives on other providers' reasoning text.
#[derive(Debug, Clone)]
pub struct DeepSeekPack {
    enabled: bool,
}

impl DeepSeekPack {
    /// Create a gated pack. Cheap — just stores a boolean.
    pub fn new(model: &Model) -> Self {
        DeepSeekPack {
            enabled: is_deepseek_model(model),
        }
    }

    /// Returns `true` when the pack is active (current model is a
    /// DeepSeek endpoint). Callers can use this to skip related
    /// allocations or logging prefixes.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Scan `reasoning_content` for tool calls the model forgot to
    /// emit structurally. Returns recovered calls in source order,
    /// or an empty vec when disabled.
    ///
    /// Call this after each assistant message with reasoning content
    /// is finalized. Recovered calls still need to be re-issued by
    /// the caller — this is a pure detector.
    pub fn scavenge_reasoning(&self, reasoning: &str) -> Vec<ScavengedToolCall> {
        if !self.enabled {
            return Vec::new();
        }
        grain_deepseek_pack::scavenge_tool_calls(reasoning)
    }

    /// Parse `<deepseek:subagent.done>` markers out of `text`.
    /// Returns events in source order, or an empty vec when disabled.
    ///
    /// Call this on the assistant's text content after streaming
    /// completes. The `span` field on each event helps strip the
    /// marker from the displayed transcript.
    pub fn parse_subagent_done(&self, text: &str) -> Vec<SubagentDoneEvent> {
        if !self.enabled {
            return Vec::new();
        }
        grain_deepseek_pack::parse_subagent_done(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deepseek_model() -> Model {
        Model {
            id: "deepseek/deepseek-chat".into(),
            provider: "deepseek".into(),
            ..Default::default()
        }
    }

    fn anthropic_model() -> Model {
        Model {
            id: "anthropic/claude-sonnet-4-5".into(),
            provider: "anthropic".into(),
            ..Default::default()
        }
    }

    #[test]
    fn disabled_for_non_deepseek() {
        let ds = DeepSeekPack::new(&anthropic_model());
        assert!(!ds.is_enabled());
        assert!(ds.scavenge_reasoning("anything").is_empty());
        assert!(ds.parse_subagent_done("anything").is_empty());
    }

    #[test]
    fn enabled_for_deepseek() {
        let ds = DeepSeekPack::new(&deepseek_model());
        assert!(ds.is_enabled());
    }

    #[test]
    fn scavenge_recovers_tool_call_when_enabled() {
        let ds = DeepSeekPack::new(&deepseek_model());
        let calls = ds.scavenge_reasoning(
            r#"I should call: {"name": "grep", "arguments": {"pattern": "TODO"}}"#,
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "grep");
    }

    #[test]
    fn subagent_done_parses_when_enabled() {
        let ds = DeepSeekPack::new(&deepseek_model());
        let events = ds.parse_subagent_done(r#"done <deepseek:subagent.done>{"summary":"ok"}"#);
        assert_eq!(events.len(), 1);
        assert!(events[0].payload.is_some());
    }
}
