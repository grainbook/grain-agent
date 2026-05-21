//! Normalized model metadata types.
//!
//! These are deliberately small and stable: anything provider-specific that we
//! don't yet know how to surface goes into [`ModelDescriptor::extra`] as raw
//! JSON instead of forcing a schema change.

use grain_agent_core::{Cost, Model, ThinkingLevel};
use serde::{Deserialize, Serialize};

/// One model entry in the registry.
///
/// `id` is the canonical lookup key (`"<provider>/<model>"`, e.g.
/// `"anthropic/claude-sonnet-4-5"`). Callers should never construct this
/// manually for runtime use — load from [`crate::Registry`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelDescriptor {
    pub id: String,
    /// Display name, falls back to `id` when absent.
    #[serde(default)]
    pub name: String,
    /// Higher-level provider grouping (Anthropic / OpenAI / Google / …).
    pub provider: ProviderId,
    /// Wire protocol the model speaks — drives which adapter is used.
    pub api: ApiKind,
    /// Total context window in tokens.
    pub context_window: u64,
    /// Maximum tokens in a single response.
    pub max_output_tokens: u64,
    /// Per-million-token pricing in USD. `Default` (all zeros) means unknown.
    #[serde(default)]
    pub cost: Cost,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub thinking: ThinkingProfile,
    /// Provider-specific extras preserved for forward compatibility.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub extra: serde_json::Value,
}

impl ModelDescriptor {
    /// Project to a [`grain_agent_core::Model`] for use with `AgentOptions`.
    ///
    /// `name` is populated from `descriptor.name` (or `descriptor.id` when
    /// blank). `base_url` is left empty — adapters resolve it from their own
    /// provider configuration.
    pub fn to_core_model(&self) -> Model {
        Model {
            id: self.id.clone(),
            name: if self.name.is_empty() {
                self.id.clone()
            } else {
                self.name.clone()
            },
            api: self.api.wire_name().to_string(),
            provider: self.provider.canonical_name().to_string(),
            base_url: String::new(),
            reasoning: self.thinking.supported,
            context_window: self.context_window,
            max_tokens: self.max_output_tokens,
            cost: self.cost.clone(),
        }
    }
}

/// Coarse-grained provider taxonomy.
///
/// `OpenAiCompatible` carries the originating provider id (`"zhipu"`,
/// `"kimi"`, `"deepseek"`, …) — `api` is still [`ApiKind::OpenAi`] for these,
/// but the registry preserves origin so adapters can pick the right base URL
/// and env-var pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderId {
    Anthropic,
    OpenAi,
    Google,
    DeepSeek,
    Mistral,
    Meta,
    Cohere,
    Xai,
    OpenAiCompatible { id: String },
    Other { id: String },
}

impl ProviderId {
    /// Stable name used as `Model::provider` in [`grain_agent_core::Model`].
    pub fn canonical_name(&self) -> &str {
        match self {
            ProviderId::Anthropic => "anthropic",
            ProviderId::OpenAi => "openai",
            ProviderId::Google => "google",
            ProviderId::DeepSeek => "deepseek",
            ProviderId::Mistral => "mistral",
            ProviderId::Meta => "meta",
            ProviderId::Cohere => "cohere",
            ProviderId::Xai => "xai",
            ProviderId::OpenAiCompatible { id } | ProviderId::Other { id } => id.as_str(),
        }
    }
}

/// Wire protocol — what request/response shape the model speaks.
///
/// `api` for OpenAI-compatible domestic providers (Kimi/智谱/DeepSeek) is
/// [`ApiKind::OpenAi`]; origin lives on [`ProviderId::OpenAiCompatible`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiKind {
    OpenAi,
    Anthropic,
    Gemini,
    Mistral,
    Cohere,
}

impl ApiKind {
    /// String form used in [`grain_agent_core::Model::api`].
    pub fn wire_name(&self) -> &'static str {
        match self {
            ApiKind::OpenAi => "openai",
            ApiKind::Anthropic => "anthropic",
            ApiKind::Gemini => "gemini",
            ApiKind::Mistral => "mistral",
            ApiKind::Cohere => "cohere",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    #[serde(default = "yes")]
    pub streaming: bool,
    #[serde(default)]
    pub tool_use: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub json_mode: bool,
    #[serde(default)]
    pub structured_output: bool,
}

fn yes() -> bool {
    true
}

/// Per-model thinking / reasoning profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingProfile {
    /// Whether the model exposes any form of reasoning output.
    #[serde(default)]
    pub supported: bool,
    /// Default level when [`ThinkingLevel`] is not specified.
    #[serde(default)]
    pub default_level: ThinkingLevel,
    /// Allowed levels. Empty means "any".
    #[serde(default)]
    pub supported_levels: Vec<ThinkingLevel>,
    /// Wire field carrying the reasoning blob in this provider's protocol
    /// (e.g. `"reasoning_content"` for OpenAI o-series / DeepSeek-R1,
    /// `"thinking"` for Anthropic's signed block, `"reasoning"` for Gemini).
    ///
    /// Adapters use this to round-trip reasoning blocks across turns; see
    /// the `provider_metadata` slot on `grain_agent_core::ThinkingContent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_field_name: Option<String>,
}
