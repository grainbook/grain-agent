//! Smoke tests for the builder + config types.
//!
//! No HTTP: we exercise the data structures and the model-id translation
//! that runs before any genai call.

use grain_llm_genai::{
    EnvKeyResolver, GenaiStream, GenaiStreamBuilder, OpenAiCompatEndpoint, OpenAiCompatPreset,
    ProviderRouter,
};

#[test]
fn env_resolver_default_mapping_covers_native_providers() {
    let r = EnvKeyResolver::default_mapping();
    assert_eq!(r.env_var_for("anthropic"), Some("ANTHROPIC_API_KEY"));
    assert_eq!(r.env_var_for("openai"), Some("OPENAI_API_KEY"));
    assert_eq!(r.env_var_for("gemini"), Some("GEMINI_API_KEY"));
    assert_eq!(r.env_var_for("deepseek"), Some("DEEPSEEK_API_KEY"));
    assert_eq!(r.env_var_for("xai"), Some("XAI_API_KEY"));
    assert_eq!(r.env_var_for("kimi"), Some("MOONSHOT_API_KEY"));
    assert_eq!(r.env_var_for("siliconflow"), Some("SILICONFLOW_API_KEY"));
}

#[test]
fn env_resolver_unknown_provider_returns_none() {
    let r = EnvKeyResolver::default_mapping();
    assert_eq!(r.env_var_for("totally-unknown"), None);
}

#[test]
fn env_resolver_override_replaces_existing_entry() {
    let r = EnvKeyResolver::default_mapping()
        .with_override("openai", "MY_OPENAI_KEY")
        .with_override("custom", "MY_CUSTOM_KEY");
    assert_eq!(r.env_var_for("openai"), Some("MY_OPENAI_KEY"));
    assert_eq!(r.env_var_for("custom"), Some("MY_CUSTOM_KEY"));
    // Other defaults survive.
    assert_eq!(r.env_var_for("anthropic"), Some("ANTHROPIC_API_KEY"));
}

#[test]
fn env_resolver_resolve_returns_none_for_unset_env() {
    let r = EnvKeyResolver::default_mapping()
        .with_override("test-provider", "GRAIN_TEST_NOT_SET_KEY_XYZ");
    // Don't rely on any global env state — just assert None on a key we set.
    unsafe {
        std::env::remove_var("GRAIN_TEST_NOT_SET_KEY_XYZ");
    }
    assert_eq!(r.resolve("test-provider"), None);
}

#[test]
fn env_resolver_resolve_reads_env() {
    unsafe {
        std::env::set_var("GRAIN_TEST_SET_KEY_ABC", "sk-test-123");
    }
    let r =
        EnvKeyResolver::default_mapping().with_override("test-provider", "GRAIN_TEST_SET_KEY_ABC");
    assert_eq!(r.resolve("test-provider").as_deref(), Some("sk-test-123"));
    unsafe {
        std::env::remove_var("GRAIN_TEST_SET_KEY_ABC");
    }
}

#[test]
fn openai_compat_preset_common_includes_kimi_and_siliconflow() {
    let endpoints = OpenAiCompatPreset::Common.endpoints();
    let ids: Vec<&str> = endpoints.iter().map(|e| e.id.as_str()).collect();
    assert!(ids.contains(&"kimi"), "preset includes kimi: {ids:?}");
    assert!(
        ids.contains(&"siliconflow"),
        "preset includes siliconflow: {ids:?}"
    );

    // Native-supported providers should NOT be in the OpenAI-compat preset
    // (otherwise the resolver would override native routing).
    assert!(
        !ids.contains(&"deepseek"),
        "deepseek is native in genai 0.5"
    );
    assert!(
        !ids.contains(&"zhipu"),
        "zhipu is native (BigModel) in genai 0.5"
    );

    let kimi = endpoints.iter().find(|e| e.id == "kimi").unwrap();
    assert_eq!(kimi.base_url, "https://api.moonshot.cn/v1/");
    assert_eq!(kimi.env_var, "MOONSHOT_API_KEY");
}

#[test]
fn openai_compat_preset_none_is_empty() {
    assert!(OpenAiCompatPreset::None.endpoints().is_empty());
}

#[test]
fn openai_compat_endpoint_constructor() {
    let e = OpenAiCompatEndpoint::new("foo", "https://foo.example/v1", "FOO_KEY");
    assert_eq!(e.id, "foo");
    assert_eq!(e.base_url, "https://foo.example/v1/");
    assert_eq!(e.env_var, "FOO_KEY");
}

#[test]
fn provider_router_default_renames() {
    let r = ProviderRouter::default();
    assert_eq!(r.namespace_for("google"), "gemini");
    assert_eq!(r.namespace_for("zhipu"), "bigmodel");
    assert_eq!(r.namespace_for("moonshot"), "kimi");
    // Identity for everything else.
    assert_eq!(r.namespace_for("anthropic"), "anthropic");
    assert_eq!(r.namespace_for("openai"), "openai");
    assert_eq!(r.namespace_for("totally-novel"), "totally-novel");
}

#[test]
fn provider_router_override_wins() {
    let r = ProviderRouter::default()
        .with_override("google", "vertex")
        .with_override("custom", "openai");
    assert_eq!(r.namespace_for("google"), "vertex");
    assert_eq!(r.namespace_for("custom"), "openai");
}

#[test]
fn provider_router_empty_passes_through() {
    let r = ProviderRouter::new();
    assert_eq!(r.namespace_for("anything"), "anything");
}

#[test]
fn translate_model_id_uses_router() {
    let stream = GenaiStream::builder().build();
    assert_eq!(
        stream.translate_model_id("anthropic/claude-sonnet-4-5"),
        "anthropic::claude-sonnet-4-5"
    );
    assert_eq!(
        stream.translate_model_id("google/gemini-2.0-flash"),
        "gemini::gemini-2.0-flash"
    );
    assert_eq!(
        stream.translate_model_id("zhipu/glm-4-plus"),
        "bigmodel::glm-4-plus"
    );
    assert_eq!(
        stream.translate_model_id("kimi/moonshot-v1-128k"),
        "kimi::moonshot-v1-128k"
    );
}

#[test]
fn translate_model_id_passes_unprefixed_through() {
    let stream = GenaiStream::builder().build();
    assert_eq!(stream.translate_model_id("gpt-4o"), "gpt-4o");
}

#[test]
fn builder_with_custom_provider_router_overrides_defaults() {
    let router = ProviderRouter::new().with_override("google", "vertex");
    let stream = GenaiStream::builder().with_provider_router(router).build();
    assert_eq!(
        stream.translate_model_id("google/gemini-2.0-flash"),
        "vertex::gemini-2.0-flash"
    );
}

#[test]
fn builder_with_openai_compat_preset_succeeds() {
    // Just verify the builder accepts the preset and produces a working stream.
    let _ = GenaiStreamBuilder::new()
        .with_openai_compat_preset(OpenAiCompatPreset::Common)
        .with_openai_compat(OpenAiCompatEndpoint::new(
            "custom-host",
            "https://custom.example/v1",
            "CUSTOM_KEY",
        ))
        .build();
}
