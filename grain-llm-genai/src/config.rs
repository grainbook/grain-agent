//! Configuration types for [`crate::stream::GenaiStream`].
//!
//! - [`EnvKeyResolver`] — provider → environment-variable mapping for API keys.
//! - [`OpenAiCompatEndpoint`] / [`OpenAiCompatPreset`] — providers genai 0.5
//!   does not natively support that speak OpenAI's chat-completions wire format.
//! - [`ProviderRouter`] — provider → genai adapter namespace mapping
//!   (e.g. `google` → `gemini`, `zhipu` → `bigmodel`).
//!
//! All three are `Default`-friendly and `Clone` so they can be captured into
//! the genai resolver closures.

use std::collections::HashMap;

/// Provider name → environment-variable name for API keys.
///
/// Defaults match the conventions used by genai's own adapters (e.g.
/// `ANTHROPIC_API_KEY` for Anthropic). The OpenAI-compat presets supply
/// their own env-var overrides on top.
#[derive(Debug, Clone, Default)]
pub struct EnvKeyResolver {
    map: HashMap<String, String>,
}

impl EnvKeyResolver {
    /// Empty resolver. Use [`Self::default_mapping`] for the conventional set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Conventional `<PROVIDER>_API_KEY` mapping for all genai-native providers
    /// in 0.5 plus the popular OpenAI-compat shops.
    pub fn default_mapping() -> Self {
        let mut map = HashMap::new();
        for (provider, env) in [
            ("anthropic", "ANTHROPIC_API_KEY"),
            ("openai", "OPENAI_API_KEY"),
            ("google", "GEMINI_API_KEY"),
            ("gemini", "GEMINI_API_KEY"),
            ("deepseek", "DEEPSEEK_API_KEY"),
            ("cohere", "COHERE_API_KEY"),
            ("xai", "XAI_API_KEY"),
            ("groq", "GROQ_API_KEY"),
            ("mistral", "MISTRAL_API_KEY"),
            ("fireworks", "FIREWORKS_API_KEY"),
            ("together", "TOGETHER_API_KEY"),
            ("nebius", "NEBIUS_API_KEY"),
            ("zai", "ZAI_API_KEY"),
            ("zhipu", "ZHIPU_API_KEY"),
            ("bigmodel", "ZHIPU_API_KEY"),
            ("mimo", "MIMO_API_KEY"),
            ("kimi", "MOONSHOT_API_KEY"),
            ("moonshot", "MOONSHOT_API_KEY"),
            ("siliconflow", "SILICONFLOW_API_KEY"),
        ] {
            map.insert(provider.into(), env.into());
        }
        EnvKeyResolver { map }
    }

    /// Set or replace one entry. Returns `self` for fluent use.
    pub fn with_override(
        mut self,
        provider: impl Into<String>,
        env_var: impl Into<String>,
    ) -> Self {
        self.map.insert(provider.into(), env_var.into());
        self
    }

    /// Environment variable name registered for `provider`, if any.
    pub fn env_var_for(&self, provider: &str) -> Option<&str> {
        self.map.get(provider).map(String::as_str)
    }

    /// Read the env var for `provider`. Returns `None` if the provider is
    /// unknown or the env var is unset / empty.
    pub fn resolve(&self, provider: &str) -> Option<String> {
        let name = self.env_var_for(provider)?;
        std::env::var(name).ok().filter(|s| !s.is_empty())
    }
}

/// A single OpenAI-compatible provider endpoint genai does not natively know.
///
/// `id` is the provider key used in grain model ids (e.g. `"kimi"` in
/// `"kimi/moonshot-v1-128k"`); `base_url` and `env_var` are the per-provider
/// transport details.
#[derive(Debug, Clone)]
pub struct OpenAiCompatEndpoint {
    pub id: String,
    pub base_url: String,
    pub env_var: String,
}

impl OpenAiCompatEndpoint {
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        env_var: impl Into<String>,
    ) -> Self {
        OpenAiCompatEndpoint {
            id: id.into(),
            base_url: normalize_base_url(base_url.into()),
            env_var: env_var.into(),
        }
    }
}

fn normalize_base_url(raw: String) -> String {
    let Ok(mut url) = reqwest13::Url::parse(&raw) else {
        return ensure_trailing_slash(raw);
    };
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url.to_string()
}

fn ensure_trailing_slash(raw: String) -> String {
    if raw.ends_with('/') {
        raw
    } else {
        format!("{raw}/")
    }
}

/// Curated bundles of [`OpenAiCompatEndpoint`].
///
/// `genai 0.5` already supports Anthropic, OpenAI, Gemini, DeepSeek, BigModel
/// (Zhipu), Groq, Mimo, Nebius, xAI, Zai, Fireworks, Together, Cohere, and
/// Ollama natively — they are *not* part of any preset here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiCompatPreset {
    /// No endpoints.
    None,
    /// Moonshot AI (Kimi) and SiliconFlow — the common Chinese hosts that
    /// don't have native genai adapters.
    Common,
}

impl OpenAiCompatPreset {
    pub fn endpoints(self) -> Vec<OpenAiCompatEndpoint> {
        match self {
            OpenAiCompatPreset::None => Vec::new(),
            OpenAiCompatPreset::Common => vec![
                OpenAiCompatEndpoint::new("kimi", "https://api.moonshot.cn/v1", "MOONSHOT_API_KEY"),
                OpenAiCompatEndpoint::new(
                    "siliconflow",
                    "https://api.siliconflow.cn/v1",
                    "SILICONFLOW_API_KEY",
                ),
            ],
        }
    }
}

/// Provider name → genai adapter namespace.
///
/// Grain ids look like `"<provider>/<model>"`; genai dispatches on the lower-
/// case namespace from `<namespace>::<model>`. Most providers pass through
/// unchanged; a few need renaming:
/// - `google` → `gemini`
/// - `zhipu` → `bigmodel`
/// - `moonshot` → `kimi` (so the OpenAI-compat resolver picks it up)
#[derive(Debug, Clone)]
pub struct ProviderRouter {
    map: HashMap<String, String>,
}

impl Default for ProviderRouter {
    fn default() -> Self {
        Self::default_mapping()
    }
}

impl ProviderRouter {
    /// Empty router. All provider names pass through verbatim.
    pub fn new() -> Self {
        ProviderRouter {
            map: HashMap::new(),
        }
    }

    /// Conventional renames for grain provider ids that don't match genai's
    /// lowercase adapter namespaces 1:1.
    pub fn default_mapping() -> Self {
        let mut map = HashMap::new();
        for (grain_provider, genai_namespace) in [
            ("google", "gemini"),
            ("zhipu", "bigmodel"),
            ("moonshot", "kimi"),
        ] {
            map.insert(grain_provider.into(), genai_namespace.into());
        }
        ProviderRouter { map }
    }

    pub fn with_override(
        mut self,
        grain_provider: impl Into<String>,
        genai_namespace: impl Into<String>,
    ) -> Self {
        self.map
            .insert(grain_provider.into(), genai_namespace.into());
        self
    }

    /// Translate a grain provider id into a genai adapter namespace.
    pub fn namespace_for(&self, grain_provider: &str) -> String {
        self.map
            .get(grain_provider)
            .cloned()
            .unwrap_or_else(|| grain_provider.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::OpenAiCompatEndpoint;

    #[test]
    fn openai_compat_endpoint_normalizes_base_url_for_url_join() {
        let endpoint = OpenAiCompatEndpoint::new("local", "http://127.0.0.1:1234/v1", "KEY");
        assert_eq!(endpoint.base_url, "http://127.0.0.1:1234/v1/");
    }

    #[test]
    fn openai_compat_endpoint_preserves_query_while_normalizing_path() {
        let endpoint =
            OpenAiCompatEndpoint::new("local", "http://127.0.0.1:1234/v1?tenant=a", "KEY");
        assert_eq!(endpoint.base_url, "http://127.0.0.1:1234/v1/?tenant=a");
    }
}
