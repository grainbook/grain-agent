//! Embedded-snapshot smoke tests.
//!
//! Guarantees the vendored `data/models-dev.json` is well-formed and that the
//! schema mapping behaves the way adapters and harness hooks will expect.

use grain_agent_core::{Cost, ThinkingLevel};
use grain_llm_models::{
    ApiKind, ModelDescriptor, ProviderId, Registry, Snapshot, ThinkingProfile,
};

#[test]
fn embedded_snapshot_parses_and_is_nonempty() {
    let snapshot = Snapshot::from_embedded().expect("embedded snapshot parses");
    assert_eq!(snapshot.version, 1);
    assert!(!snapshot.models.is_empty(), "snapshot ships at least one model");
}

#[test]
fn embedded_registry_lookups_known_models() {
    let registry = Registry::from_embedded_snapshot();
    assert!(registry.len() >= 5);

    let claude = registry
        .lookup("anthropic/claude-sonnet-4-5")
        .expect("anthropic/claude-sonnet-4-5 present");
    assert_eq!(claude.provider, ProviderId::Anthropic);
    assert_eq!(claude.api, ApiKind::Anthropic);
    assert_eq!(claude.context_window, 200_000);
    assert!(claude.capabilities.tool_use);
    assert!(claude.thinking.supported);
    assert_eq!(
        claude.thinking.reasoning_field_name.as_deref(),
        Some("thinking")
    );

    let gpt4o = registry.lookup("openai/gpt-4o").expect("openai/gpt-4o present");
    assert_eq!(gpt4o.provider, ProviderId::OpenAi);
    assert_eq!(gpt4o.api, ApiKind::OpenAi);
    assert!(!gpt4o.thinking.supported);
}

#[test]
fn openai_compatible_providers_preserve_origin_id() {
    let registry = Registry::from_embedded_snapshot();
    let zhipu = registry.lookup("zhipu/glm-4-plus").expect("zhipu/glm-4-plus present");
    match &zhipu.provider {
        ProviderId::OpenAiCompatible { id } => assert_eq!(id, "zhipu"),
        other => panic!("expected OpenAiCompatible, got {other:?}"),
    }
    // OpenAI-compatible providers still speak the OpenAI wire protocol.
    assert_eq!(zhipu.api, ApiKind::OpenAi);
}

#[test]
fn to_core_model_carries_canonical_fields() {
    let registry = Registry::from_embedded_snapshot();
    let core = registry
        .to_core_model("anthropic/claude-haiku-4-5")
        .expect("haiku resolvable");
    assert_eq!(core.id, "anthropic/claude-haiku-4-5");
    assert_eq!(core.api, "anthropic");
    assert_eq!(core.provider, "anthropic");
    assert!(core.reasoning);
    assert_eq!(core.context_window, 200_000);
    assert_eq!(core.cost, Cost::default()); // starter snapshot leaves pricing unset
}

#[test]
fn duplicate_descriptor_ids_are_rejected() {
    let dup = vec![
        ModelDescriptor {
            id: "x/y".into(),
            name: String::new(),
            provider: ProviderId::Other { id: "x".into() },
            api: ApiKind::OpenAi,
            context_window: 100,
            max_output_tokens: 100,
            cost: Cost::default(),
            capabilities: Default::default(),
            thinking: ThinkingProfile::default(),
            extra: serde_json::Value::Null,
        },
        ModelDescriptor {
            id: "x/y".into(),
            name: String::new(),
            provider: ProviderId::Other { id: "x".into() },
            api: ApiKind::OpenAi,
            context_window: 200,
            max_output_tokens: 200,
            cost: Cost::default(),
            capabilities: Default::default(),
            thinking: ThinkingProfile::default(),
            extra: serde_json::Value::Null,
        },
    ];
    let err = Registry::from_descriptors(dup).unwrap_err();
    assert!(matches!(err, grain_llm_models::RegistryError::DuplicateId(id) if id == "x/y"));
}

#[test]
fn merged_with_overlays_other_on_top() {
    let base = Registry::from_embedded_snapshot();
    let override_haiku = ModelDescriptor {
        id: "anthropic/claude-haiku-4-5".into(),
        name: "Overridden Haiku".into(),
        provider: ProviderId::Anthropic,
        api: ApiKind::Anthropic,
        context_window: 1,
        max_output_tokens: 1,
        cost: Cost::default(),
        capabilities: Default::default(),
        thinking: ThinkingProfile {
            supported: false,
            default_level: ThinkingLevel::Off,
            supported_levels: vec![],
            reasoning_field_name: None,
        },
        extra: serde_json::Value::Null,
    };
    let overlay = Registry::from_descriptors([override_haiku]).unwrap();
    let merged = base.merged_with(&overlay);
    let haiku = merged.lookup("anthropic/claude-haiku-4-5").unwrap();
    assert_eq!(haiku.name, "Overridden Haiku");
    assert_eq!(haiku.context_window, 1);
    // Other entries survive.
    assert!(merged.lookup("openai/gpt-4o").is_some());
}

#[test]
fn unsupported_snapshot_version_is_rejected() {
    let json = r#"{ "version": 999, "models": [] }"#;
    let err = Snapshot::from_json_str(json).unwrap_err();
    assert!(matches!(
        err,
        grain_llm_models::SnapshotError::UnsupportedVersion { found: 999, expected: 1 }
    ));
}
