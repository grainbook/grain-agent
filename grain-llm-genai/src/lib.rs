//! `grain-llm-genai` — genai-backed [`grain_agent_core::LlmStream`] implementation.
//!
//! Bridges the transport-agnostic agent loop in `grain-agent-core` to the
//! [`genai`] crate's multi-provider chat API. Project layout:
//!
//! - [`mapping::outbound`] — translate [`grain_agent_core::LlmContext`] into a
//!   [`genai::chat::ChatRequest`].
//! - **`mapping::inbound`** *(PR 3b)* — translate [`genai::chat::ChatStreamEvent`]
//!   into [`grain_agent_core::AssistantMessageEvent`], including thinking /
//!   reasoning block round-tripping via
//!   [`grain_agent_core::ThinkingContent::provider_metadata`].
//! - **`config` / `builder`** *(PR 3c)* — env-var API key resolver, OpenAI-compatible
//!   provider presets, [`grain_llm_models::Registry`] wiring.
//!
//! PR 3a deliberately ships only the outbound mapping function and its smoke
//! tests so the translation surface can be reviewed in isolation before any
//! network-touching code is written.

pub mod mapping;

pub use mapping::outbound::{
    baseline_chat_options, to_chat_request,
};
