//! `grain-llm-genai` — genai-backed [`grain_agent_core::LlmStream`] implementation.
//!
//! Bridges the transport-agnostic agent loop in `grain-agent-core` to the
//! [`genai`] crate's multi-provider chat API. Project layout:
//!
//! - [`mapping::outbound`] — translate [`grain_agent_core::LlmContext`] into a
//!   [`genai::chat::ChatRequest`]; includes thinking / reasoning replay
//!   (PR 3b: `with_reasoning_content` + `thought_signatures` on first tool call).
//! - [`mapping::inbound`] — turn [`genai::chat::ChatStreamEvent`] into
//!   [`grain_agent_core::AssistantMessageEvent`] via the [`InboundState`]
//!   state machine.
//! - [`stream`] — `GenaiStream`, a [`grain_agent_core::LlmStream`] impl.
//! - **`config` / `builder`** *(PR 3c)* — env-var API key resolver, OpenAI-compat
//!   provider presets, [`grain_llm_models::Registry`] wiring.

pub mod mapping;
pub mod stream;

pub use mapping::inbound::InboundState;
pub use mapping::outbound::{baseline_chat_options, to_chat_request};
pub use mapping::usage::map_usage;
pub use stream::GenaiStream;
