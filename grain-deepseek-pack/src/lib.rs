//! `grain-deepseek-pack` — DeepSeek-specific quirks on top of
//! `grain-agent-core` and `grain-llm-genai`.
//!
//! Everything provider-agnostic lives in `grain-agent-harness` (prefix
//! pin, storm suppression, escalation, …). This crate is for the
//! pieces that only make sense when you're talking to a DeepSeek
//! endpoint:
//!
//! - [`reasoning_scavenge`] — DeepSeek-R1 occasionally writes tool
//!   calls into the `reasoning_content` blob instead of emitting them
//!   in the structured `tool_calls` slot. The scavenger sweeps the
//!   reasoning text with a JSON-aware regex and yields any
//!   recoverable calls so callers can re-issue them.
//!
//! - [`subagent`] — DeepSeek's subagent protocol marks completion with
//!   a literal `<deepseek:subagent.done>` tag in the assistant's
//!   stream. The parser extracts the optional JSON payload that
//!   follows it.
//!
//! Both modules are passive parsers — no `LlmStream` wrapping yet, so
//! adoption can stay incremental. A future revision may bundle them
//! into a decorator (see crate-level discussion).
//!
//! # Model gating
//!
//! Every public entry point that should only run against DeepSeek
//! models is gated behind [`is_deepseek_model`]. Callers should
//! check the active model before invoking any scavenge / subagent
//! logic — non-DeepSeek models never trigger these code paths, so
//! scanning their output is pure overhead (and risks false positives).

use grain_agent_core::Model;

pub mod reasoning_scavenge;
pub mod subagent;

pub use reasoning_scavenge::{ScavengedToolCall, scavenge_tool_calls};
pub use subagent::{SubagentDoneEvent, parse_subagent_done};

/// Returns `true` when `model` is a DeepSeek endpoint.
///
/// Checks both the `provider` field and the `id` prefix so it works
/// with both native-model lookups (`provider == "deepseek"`) and
/// OpenAI-compat profiles whose model id starts with `deepseek/`
/// (e.g. `deepseek/deepseek-chat` routed through a custom endpoint
/// that still identifies as DeepSeek).
///
/// # Examples
///
/// ```
/// use grain_agent_core::Model;
/// use grain_deepseek_pack::is_deepseek_model;
///
/// let m = Model { provider: "deepseek".into(), ..Default::default() };
/// assert!(is_deepseek_model(&m));
///
/// let m = Model { id: "deepseek/deepseek-chat".into(), ..Default::default() };
/// assert!(is_deepseek_model(&m));
///
/// let m = Model { provider: "anthropic".into(), ..Default::default() };
/// assert!(!is_deepseek_model(&m));
/// ```
pub fn is_deepseek_model(model: &Model) -> bool {
    model.provider == "deepseek" || model.id.starts_with("deepseek/")
}
