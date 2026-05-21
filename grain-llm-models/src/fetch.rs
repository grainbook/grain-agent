//! Runtime fetch + transform from `models.dev`.
//!
//! `models.dev/api.json` returns an object keyed by provider id:
//!
//! ```json
//! {
//!   "anthropic": {
//!     "id": "anthropic",
//!     "env": ["ANTHROPIC_API_KEY"],
//!     "npm": "@ai-sdk/anthropic",
//!     "api": "https://api.anthropic.com",
//!     "name": "Anthropic",
//!     "models": {
//!       "claude-opus-4-1-20250805": {
//!         "id": "claude-opus-4-1-20250805", ...
//!       }
//!     }
//!   },
//!   ...
//! }
//! ```
//!
//! We project each `(provider, model)` pair into a [`ModelDescriptor`] with id
//! `"{provider}/{model}"`. The transform is intentionally conservative —
//! unknown fields are preserved verbatim in [`ModelDescriptor::extra`] when
//! we want to keep them, but for v1 we drop unknown bits and rely on the
//! schema version bump to flag breaking source changes.

use std::collections::HashMap;

use grain_agent_core::{Cost, ThinkingLevel};
use serde::Deserialize;
use thiserror::Error;

use crate::descriptor::{
    ApiKind, Capabilities, ModelDescriptor, ProviderId, ThinkingProfile,
};
use crate::registry::{Registry, RegistryError};
use crate::snapshot::{CURRENT_SNAPSHOT_VERSION, Snapshot};

pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

/// Fetch from the canonical `models.dev/api.json` endpoint.
pub async fn fetch_models_dev() -> Result<Registry, FetchError> {
    fetch_from_url(MODELS_DEV_URL).await
}

/// Fetch and transform from an arbitrary URL (useful for mocking).
pub async fn fetch_from_url(url: &str) -> Result<Registry, FetchError> {
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    parse_models_dev(&bytes)
}

/// Parse a `models.dev/api.json` payload and project into a [`Registry`].
pub fn parse_models_dev(bytes: &[u8]) -> Result<Registry, FetchError> {
    let providers: HashMap<String, RawProvider> = serde_json::from_slice(bytes)?;
    let mut descriptors: Vec<ModelDescriptor> = Vec::new();
    for (provider_key, raw) in providers {
        for (_, model) in raw.models.iter() {
            descriptors.push(transform(&provider_key, &raw, model));
        }
    }
    Ok(Registry::from_descriptors(descriptors)?)
}

/// Project a registry into a serializable [`Snapshot`] (sorted by id so the
/// vendored file diffs cleanly between refreshes).
pub fn registry_to_snapshot(registry: &Registry) -> Snapshot {
    let mut models: Vec<ModelDescriptor> =
        registry.iter().map(|(_, m)| m.clone()).collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Snapshot {
        version: CURRENT_SNAPSHOT_VERSION,
        models,
    }
}

// ---------------------------------------------------------------------------
// Raw deserialization shape — mirrors models.dev exactly.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // `env` / `api` / `name` / `id` are preserved for the upcoming
                    // grain-llm-genai env-var resolver and base-URL routing.
struct RawProvider {
    #[serde(default)]
    id: String,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    npm: String,
    #[serde(default)]
    api: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    models: HashMap<String, RawModel>,
}

#[derive(Debug, Deserialize)]
struct RawModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    attachment: bool,
    #[serde(default)]
    modalities: RawModalities,
    #[serde(default)]
    limit: RawLimit,
    #[serde(default)]
    cost: RawCost,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)] // `output` modalities aren't used yet; kept to document shape.
struct RawModalities {
    #[serde(default)]
    input: Vec<String>,
    #[serde(default)]
    output: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLimit {
    #[serde(default)]
    context: u64,
    #[serde(default)]
    output: u64,
}

#[derive(Debug, Default, Deserialize)]
struct RawCost {
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    cache_read: f64,
    #[serde(default)]
    cache_write: f64,
}

// ---------------------------------------------------------------------------
// Transform models.dev raw → ModelDescriptor.
// ---------------------------------------------------------------------------

fn transform(provider_key: &str, provider: &RawProvider, model: &RawModel) -> ModelDescriptor {
    let provider_id = classify_provider(provider_key, &provider.npm);
    let api = classify_api(&provider.npm, &provider_id);
    let reasoning_field = if model.reasoning {
        default_reasoning_field(&api)
    } else {
        None
    };

    let vision = model
        .modalities
        .input
        .iter()
        .any(|m| m == "image" || m == "video")
        || model.attachment;

    ModelDescriptor {
        id: format!("{provider_key}/{}", model.id),
        name: if model.name.is_empty() {
            model.id.clone()
        } else {
            model.name.clone()
        },
        provider: provider_id,
        api,
        context_window: model.limit.context,
        max_output_tokens: model.limit.output,
        cost: Cost {
            input: model.cost.input,
            output: model.cost.output,
            cache_read: model.cost.cache_read,
            cache_write: model.cost.cache_write,
            total: 0.0,
        },
        capabilities: Capabilities {
            streaming: true,
            tool_use: model.tool_call,
            vision,
            json_mode: false,
            structured_output: false,
        },
        thinking: ThinkingProfile {
            supported: model.reasoning,
            default_level: ThinkingLevel::Off,
            supported_levels: Vec::new(),
            reasoning_field_name: reasoning_field,
        },
        extra: serde_json::Value::Null,
    }
}

fn classify_provider(key: &str, npm: &str) -> ProviderId {
    match key {
        "anthropic" => ProviderId::Anthropic,
        "openai" => ProviderId::OpenAi,
        "google" | "google-ai-studio" | "google-vertex" | "vertex-anthropic" => {
            ProviderId::Google
        }
        "deepseek" => ProviderId::DeepSeek,
        "mistral" => ProviderId::Mistral,
        "meta" => ProviderId::Meta,
        "cohere" => ProviderId::Cohere,
        "xai" => ProviderId::Xai,
        _ if npm.contains("openai-compatible") => {
            ProviderId::OpenAiCompatible { id: key.to_string() }
        }
        _ => ProviderId::Other { id: key.to_string() },
    }
}

fn classify_api(npm: &str, provider: &ProviderId) -> ApiKind {
    if npm.contains("@ai-sdk/anthropic") || matches!(provider, ProviderId::Anthropic) {
        return ApiKind::Anthropic;
    }
    if npm.contains("@ai-sdk/google") || matches!(provider, ProviderId::Google) {
        return ApiKind::Gemini;
    }
    if npm.contains("@ai-sdk/mistral") || matches!(provider, ProviderId::Mistral) {
        return ApiKind::Mistral;
    }
    if npm.contains("@ai-sdk/cohere") || matches!(provider, ProviderId::Cohere) {
        return ApiKind::Cohere;
    }
    // Everything else (OpenAI, DeepSeek, xAI, Meta via Together, OpenAI-compat) speaks OpenAI.
    ApiKind::OpenAi
}

fn default_reasoning_field(api: &ApiKind) -> Option<String> {
    match api {
        ApiKind::Anthropic => Some("thinking".into()),
        ApiKind::OpenAi => Some("reasoning_content".into()),
        ApiKind::Gemini => Some("reasoning".into()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Unit tests (no network).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Vec<u8> {
        json!({
            "anthropic": {
                "id": "anthropic",
                "env": ["ANTHROPIC_API_KEY"],
                "npm": "@ai-sdk/anthropic",
                "api": "https://api.anthropic.com",
                "name": "Anthropic",
                "models": {
                    "claude-opus-4-1": {
                        "id": "claude-opus-4-1",
                        "name": "Claude Opus 4.1",
                        "reasoning": true,
                        "tool_call": true,
                        "attachment": false,
                        "modalities": { "input": ["text", "image"], "output": ["text"] },
                        "limit": { "context": 200000, "output": 32000 },
                        "cost": { "input": 15.0, "output": 75.0, "cache_read": 1.5, "cache_write": 18.75 }
                    }
                }
            },
            "zhipu": {
                "id": "zhipu",
                "env": ["ZHIPU_API_KEY"],
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://open.bigmodel.cn/api/paas/v4",
                "name": "Zhipu",
                "models": {
                    "glm-4-plus": {
                        "id": "glm-4-plus",
                        "name": "GLM-4 Plus",
                        "tool_call": true,
                        "modalities": { "input": ["text"], "output": ["text"] },
                        "limit": { "context": 128000, "output": 4096 },
                        "cost": { "input": 0.5, "output": 0.5 }
                    }
                }
            }
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn anthropic_model_round_trips() {
        let registry = parse_models_dev(&fixture()).unwrap();
        let m = registry.lookup("anthropic/claude-opus-4-1").expect("present");
        assert_eq!(m.provider, ProviderId::Anthropic);
        assert_eq!(m.api, ApiKind::Anthropic);
        assert_eq!(m.context_window, 200_000);
        assert_eq!(m.max_output_tokens, 32_000);
        assert!(m.capabilities.tool_use);
        assert!(m.capabilities.vision); // modalities.input has "image"
        assert!(m.thinking.supported);
        assert_eq!(m.thinking.reasoning_field_name.as_deref(), Some("thinking"));
        assert_eq!(m.cost.input, 15.0);
        assert_eq!(m.cost.cache_write, 18.75);
    }

    #[test]
    fn zhipu_classifies_as_openai_compatible() {
        let registry = parse_models_dev(&fixture()).unwrap();
        let m = registry.lookup("zhipu/glm-4-plus").expect("present");
        match &m.provider {
            ProviderId::OpenAiCompatible { id } => assert_eq!(id, "zhipu"),
            other => panic!("expected OpenAiCompatible, got {other:?}"),
        }
        assert_eq!(m.api, ApiKind::OpenAi);
        assert!(!m.thinking.supported);
        assert!(m.thinking.reasoning_field_name.is_none());
    }

    #[test]
    fn snapshot_round_trip_is_deterministic() {
        let registry = parse_models_dev(&fixture()).unwrap();
        let snap = registry_to_snapshot(&registry);
        // Sorted by id.
        let ids: Vec<&str> = snap.models.iter().map(|m| m.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        // Serializes + round-trips through Snapshot::from_json_str.
        let json = serde_json::to_string(&snap).unwrap();
        let back = Snapshot::from_json_str(&json).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn malformed_input_is_a_parse_error() {
        let err = parse_models_dev(b"not json").unwrap_err();
        assert!(matches!(err, FetchError::Parse(_)));
    }
}
