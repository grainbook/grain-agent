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
        }
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
            let env = match &p.auth {
                ProviderAuth::ApiKey { env } => env.clone(),
                ProviderAuth::AnthropicOauth => continue,
            };
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
        self
    }

    pub fn build(self) -> GenaiStream {
        let GenaiStreamBuilder {
            chat_options,
            env_resolver,
            provider_router,
            openai_compat,
            registry,
        } = self;

        let chat_options = chat_options.unwrap_or_else(baseline_chat_options);

        // OpenAI-compat lookup by id. Closures capture an Arc<HashMap>.
        let compat_map: HashMap<String, OpenAiCompatEndpoint> = openai_compat
            .into_iter()
            .map(|e| (e.id.clone(), e))
            .collect();
        let compat_map = Arc::new(compat_map);
        let env_resolver = Arc::new(env_resolver);

        // Auth resolver: for every model, look up the adapter's env var
        // (from genai's `default_key_env_name`) — but prefer ours when
        // overridden. Also handle OpenAI-compat models whose namespace is
        // in our compat table.
        let auth_compat = compat_map.clone();
        let auth_env = env_resolver.clone();
        let auth_resolver = move |model_iden: ModelIden|
            -> genai::resolver::Result<Option<AuthData>> {
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
            // 3. Fall through to genai's default lookup by returning None.
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
                let auth = std::env::var(&endpoint.env_var)
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(AuthData::from_single)
                    .unwrap_or(target.auth);
                let new_model = ModelIden::new(AdapterKind::OpenAI, bare.to_string());
                Ok(ServiceTarget {
                    endpoint: Endpoint::from_owned(endpoint.base_url.clone()),
                    auth,
                    model: new_model,
                })
            };

        let client = Client::builder()
            .with_chat_options(chat_options.clone())
            .with_auth_resolver_fn(auth_resolver)
            .with_service_target_resolver_fn(target_resolver)
            .build();

        GenaiStream::with_client_options_and_router(
            client,
            chat_options,
            provider_router,
            registry,
        )
    }
}
