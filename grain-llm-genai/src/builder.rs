//! Fluent builder for [`crate::stream::GenaiStream`].
//!
//! Composes the small pieces in [`crate::config`] into a fully-wired
//! `GenaiStream`: a `genai::Client` configured with env-based API-key
//! resolution and OpenAI-compatible endpoint routing.

use std::collections::HashMap;
use std::sync::Arc;

use genai::Client;
use genai::adapter::AdapterKind;
use genai::chat::ChatOptions;
use genai::resolver::{AuthData, Endpoint};
use genai::{ModelIden, ServiceTarget};
use grain_llm_models::Registry;

use crate::config::{EnvKeyResolver, OpenAiCompatEndpoint, OpenAiCompatPreset, ProviderRouter};
use crate::mapping::outbound::baseline_chat_options;
use crate::oauth::OauthConfig;
use crate::oauth::config_for_provider;
use crate::oauth::get_valid_access_token_with_config_sync;
use crate::provider::{ProviderAuth, ProviderKind, ProviderProfile};
use crate::stream::GenaiStream;

/// Fluent builder for [`GenaiStream`].
///
/// Defaults match `GenaiStream::new()`: [`EnvKeyResolver::default_mapping`],
/// empty OpenAI-compat table, no registry. Calling `.build()` is always
/// possible — every field has a working default.
pub struct GenaiStreamBuilder {
    chat_options: Option<ChatOptions>,
    env_resolver: EnvKeyResolver,
    provider_router: ProviderRouter,
    openai_compat: Vec<OpenAiCompatEndpoint>,
    registry: Option<Arc<Registry>>,
    /// Whether the built reqwest client should ignore process-wide
    /// proxy env vars (`HTTPS_PROXY` / `ALL_PROXY` / ...).
    ///
    /// - `None` (default) → auto-detect: bypass when any registered
    ///   OpenAI-compat endpoint resolves to a loopback host. Matches
    ///   pre-config behavior so local LM Studio / vLLM / llama.cpp
    ///   "just works" behind a corporate proxy.
    /// - `Some(true)` → always bypass, regardless of endpoints.
    /// - `Some(false)` → never bypass; let `reqwest` honor the env vars.
    bypass_proxy: Option<bool>,
    /// OAuth profile map: adapter_kind (e.g. `"anthropic"`) → (profile_name, OauthConfig).
    /// Populated by `with_provider_profiles` for auth entries of kind
    /// `anthropic_oauth` / `openai_oauth`. Used by the auth resolver closure
    /// to fetch fresh access tokens at request time.
    oauth_map: HashMap<String, (String, OauthConfig)>,
}

impl Default for GenaiStreamBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl GenaiStreamBuilder {
    pub fn new() -> Self {
        GenaiStreamBuilder {
            chat_options: None,
            env_resolver: EnvKeyResolver::default_mapping(),
            provider_router: ProviderRouter::default(),
            openai_compat: Vec::new(),
            registry: None,
            bypass_proxy: None,
            oauth_map: HashMap::new(),
        }
    }

    /// Override proxy-bypass behavior. See [`Self::bypass_proxy`] field
    /// docs for the truth table.
    pub fn with_bypass_proxy(mut self, choice: Option<bool>) -> Self {
        self.bypass_proxy = choice;
        self
    }

    pub fn with_chat_options(mut self, options: ChatOptions) -> Self {
        self.chat_options = Some(options);
        self
    }

    pub fn with_env_resolver(mut self, resolver: EnvKeyResolver) -> Self {
        self.env_resolver = resolver;
        self
    }

    /// Set or replace a single provider's API-key env var.
    pub fn with_env_override(
        mut self,
        provider: impl Into<String>,
        env_var: impl Into<String>,
    ) -> Self {
        self.env_resolver = self.env_resolver.with_override(provider, env_var);
        self
    }

    pub fn with_provider_router(mut self, router: ProviderRouter) -> Self {
        self.provider_router = router;
        self
    }

    /// Append the endpoints from a built-in preset.
    pub fn with_openai_compat_preset(mut self, preset: OpenAiCompatPreset) -> Self {
        self.openai_compat.extend(preset.endpoints());
        self
    }

    /// Append a single endpoint. Duplicates by id are kept; the last wins on
    /// lookup.
    pub fn with_openai_compat(mut self, endpoint: OpenAiCompatEndpoint) -> Self {
        self.openai_compat.push(endpoint);
        self
    }

    /// Attach a model registry. PR 3c stores it for later use by adapters
    /// and harness hooks (e.g. context-window guard); routing decisions
    /// don't depend on the registry today.
    pub fn with_registry(mut self, registry: Arc<Registry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Wire a set of [`ProviderProfile`]s into the routing tables.
    ///
    /// For `kind = openai-compat` profiles with API-key auth, the
    /// `(name, base_url, env_var)` triple is registered as an
    /// [`OpenAiCompatEndpoint`] — models addressed as
    /// `<profile_name>/<model>` will route through that endpoint with
    /// that env var. Multiple profiles per vendor (e.g. work +
    /// personal OpenAI keys) work this way: declare each as a
    /// distinct compat profile with its own name.
    ///
    /// For `kind = anthropic | openai | gemini` profiles with a
    /// non-default env var, the env resolver gets an override so the
    /// native adapter uses the profile's key. Note: when multiple
    /// profiles share a native kind, the *last* one wins — use
    /// `openai-compat` profiles if you need multiple-account semantics
    /// against the same vendor.
    ///
    /// OAuth profiles ([`ProviderAuth::AnthropicOauth`]) are accepted
    /// but skipped here — Phase 2 of provider work will plug them in
    /// via a refresh-aware auth path. Callers can list them in their
    /// UIs today; trying to actually use one returns a clear error.
    pub fn with_provider_profiles(mut self, profiles: &[ProviderProfile]) -> Self {
        for p in profiles {
            match &p.auth {
                ProviderAuth::ApiKey { env } => {
                    let env = env.clone();
                    match p.kind {
                        ProviderKind::OpenAiCompat => {
                            if let Some(base_url) = p.base_url.clone() {
                                self.openai_compat.push(OpenAiCompatEndpoint::new(
                                    p.name.clone(),
                                    base_url,
                                    env,
                                ));
                            }
                        }
                        ProviderKind::Anthropic => {
                            self.env_resolver = self.env_resolver.with_override("anthropic", env);
                        }
                        ProviderKind::OpenAi => {
                            self.env_resolver = self.env_resolver.with_override("openai", env);
                        }
                        ProviderKind::Gemini => {
                            self.env_resolver = self.env_resolver.with_override("gemini", env);
                        }
                    }
                }
                ProviderAuth::AnthropicOauth => {
                    if let Some(config) = config_for_provider("anthropic") {
                        self.oauth_map
                            .insert("anthropic".to_string(), (p.name.clone(), config));
                    }
                }
                ProviderAuth::OpenAiOauth => {
                    if let Some(config) = config_for_provider("openai") {
                        self.oauth_map
                            .insert("openai".to_string(), (p.name.clone(), config));
                    }
                }
            }
        }
        self
    }

    pub fn build(self) -> GenaiStream {
        let GenaiStreamBuilder {
            chat_options,
            env_resolver,
            provider_router,
            openai_compat,
            registry,
            bypass_proxy,
            oauth_map,
        } = self;

        let chat_options = chat_options.unwrap_or_else(baseline_chat_options);

        // OpenAI-compat lookup by id. Closures capture an Arc<HashMap>.
        let compat_map: HashMap<String, OpenAiCompatEndpoint> = openai_compat
            .into_iter()
            .map(|e| (e.id.clone(), e))
            .collect();
        let has_loopback_compat_endpoint = compat_map
            .values()
            .any(|endpoint| is_loopback_url(&endpoint.base_url));
        let compat_map = Arc::new(compat_map);
        let env_resolver = Arc::new(env_resolver);

        // Auth resolver: for every model, look up the adapter's env var
        // (from genai's `default_key_env_name`) — but prefer ours when
        // overridden. Also handle OpenAI-compat models whose namespace is
        // in our compat table.
        let auth_compat = compat_map.clone();
        let auth_env = env_resolver.clone();
        let auth_oauth = oauth_map.clone();
        let auth_resolver =
            move |model_iden: ModelIden| -> genai::resolver::Result<Option<AuthData>> {
                // 1. OpenAI-compat namespace? Use its env var.
                let (namespace, _) = model_iden.model_name.namespace_and_name();
                if let Some(ns) = namespace
                    && let Some(ep) = auth_compat.get(ns)
                    && let Ok(v) = std::env::var(&ep.env_var)
                    && !v.is_empty()
                {
                    return Ok(Some(AuthData::from_single(v)));
                }
                // 2. Custom override for the adapter? Use it.
                let adapter_name = model_iden.adapter_kind.as_lower_str();
                if let Some(env) = auth_env.env_var_for(adapter_name)
                    && let Ok(v) = std::env::var(env)
                    && !v.is_empty()
                {
                    return Ok(Some(AuthData::from_single(v)));
                }
                // 3. OAuth profile for this adapter kind?
                if let Some((profile_name, config)) = auth_oauth.get(adapter_name) {
                    if let Some(token) =
                        get_valid_access_token_with_config_sync(config, profile_name)
                            .ok()
                            .flatten()
                    {
                        return Ok(Some(AuthData::from_single(token)));
                    }
                }
                // 4. Fall through to genai's default lookup by returning None.
                Ok(None)
            };

        // Service-target resolver: only intervenes for OpenAI-compat namespaces.
        // Overrides endpoint + adapter_kind to OpenAI, strips the namespace
        // from the model name so the OpenAI adapter doesn't get confused.
        let target_compat = compat_map.clone();
        let target_resolver =
            move |target: ServiceTarget| -> genai::resolver::Result<ServiceTarget> {
                let (namespace, bare) = target.model.model_name.namespace_and_name();
                let Some(ns) = namespace else {
                    return Ok(target);
                };
                let Some(endpoint) = target_compat.get(ns) else {
                    return Ok(target);
                };
                // Local OpenAI-compatible servers (LM Studio, llama.cpp,
                // vLLM) usually ignore the bearer token, but genai's OpenAI
                // adapter still needs a single auth value to build headers.
                let auth = std::env::var(&endpoint.env_var)
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(AuthData::from_single)
                    .or_else(|| {
                        is_loopback_url(&endpoint.base_url)
                            .then(|| AuthData::from_single("grain-local"))
                    })
                    .unwrap_or(target.auth);
                let new_model = ModelIden::new(AdapterKind::OpenAI, bare.to_string());
                Ok(ServiceTarget {
                    endpoint: Endpoint::from_owned(endpoint.base_url.clone()),
                    auth,
                    model: new_model,
                })
            };

        let mut client_builder = Client::builder()
            .with_chat_options(chat_options.clone())
            .with_auth_resolver_fn(auth_resolver)
            .with_service_target_resolver_fn(target_resolver);
        // Tristate: explicit `Some(_)` overrides the auto-detect.
        // `None` → fall back to auto-detect (bypass if any registered
        // compat endpoint is a loopback URL — the typical
        // local-model setup).
        let should_bypass = bypass_proxy.unwrap_or(has_loopback_compat_endpoint);
        if should_bypass {
            let reqwest_client = reqwest13::Client::builder()
                .no_proxy()
                .build()
                .expect("build reqwest client with local proxy bypass");
            client_builder = client_builder.with_reqwest(reqwest_client);
        }
        let client = client_builder.build();

        GenaiStream::with_client_options_and_router(client, chat_options, provider_router, registry)
    }
}

fn is_loopback_url(raw: &str) -> bool {
    let Ok(url) = reqwest13::Url::parse(raw) else {
        return false;
    };
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|addr| addr.is_loopback()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_loopback_url;

    #[test]
    fn loopback_url_detection_covers_local_provider_hosts() {
        assert!(is_loopback_url("http://127.0.0.1:1234/v1/"));
        assert!(is_loopback_url("http://localhost:1234/v1/"));
        assert!(is_loopback_url("http://[::1]:1234/v1/"));
        assert!(!is_loopback_url("https://api.example.com/v1/"));
    }
}
