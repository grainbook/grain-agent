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

pub mod reasoning_scavenge;
pub mod subagent;

pub use reasoning_scavenge::{ScavengedToolCall, scavenge_tool_calls};
pub use subagent::{SubagentDoneEvent, parse_subagent_done};
