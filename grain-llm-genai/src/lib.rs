//! `grain-llm-genai` — genai-backed [`grain_agent_core::LlmStream`] implementation.
//!
//! Bridges the transport-agnostic agent loop in `grain-agent-core` to the
//! [`genai`] crate's multi-provider chat API.
//!
//! - [`mapping::outbound`] — `LlmContext` → `genai::chat::ChatRequest`,
//!   including Anthropic-style signed-thinking replay.
//! - [`mapping::inbound`] — `genai::chat::ChatStreamEvent` →
//!   `AssistantMessageEvent` via the [`InboundState`] state machine.
//! - [`stream`] — `GenaiStream`, the [`LlmStream`] implementation.
//! - [`builder`] — `GenaiStreamBuilder`: env-var API-key resolver, OpenAI-compat
//!   endpoint routing, optional [`grain_llm_models::Registry`] wiring.
//! - [`config`] — small config types ([`EnvKeyResolver`],
//!   [`OpenAiCompatEndpoint`], [`OpenAiCompatPreset`], [`ProviderRouter`]).

pub mod builder;
pub mod config;
pub mod mapping;
pub mod oauth;
pub mod provider;
pub mod stream;

pub use builder::GenaiStreamBuilder;
pub use config::{EnvKeyResolver, OpenAiCompatEndpoint, OpenAiCompatPreset, ProviderRouter};
pub use mapping::inbound::InboundState;
pub use mapping::outbound::{baseline_chat_options, to_chat_request};
pub use mapping::usage::map_usage;
pub use provider::{
    AuthEntry, ModelSpec, ProfileEntry, ProviderAuth, ProviderKind, ProviderProfile, load_profiles,
    parse_model_spec, profile_from_entry, resolve_providers_file,
};
pub use stream::GenaiStream;
