//! Serde compatibility tests for `ThinkingContent`.
//!
//! `provider_metadata` was added after `signature` and must not change the
//! serialized form of existing values. Both fields are `Option` + `skip_serializing_if`.

use grain_agent_core::ThinkingContent;
use serde_json::json;

#[test]
fn legacy_json_without_new_field_deserializes() {
    let legacy = json!({
        "thinking": "let me think",
        "signature": "sig-1"
    });
    let parsed: ThinkingContent = serde_json::from_value(legacy).unwrap();
    assert_eq!(parsed.thinking, "let me think");
    assert_eq!(parsed.signature.as_deref(), Some("sig-1"));
    assert!(parsed.provider_metadata.is_none());
}

#[test]
fn legacy_json_with_only_thinking_deserializes() {
    let legacy = json!({ "thinking": "..." });
    let parsed: ThinkingContent = serde_json::from_value(legacy).unwrap();
    assert!(parsed.signature.is_none());
    assert!(parsed.provider_metadata.is_none());
}

#[test]
fn empty_optionals_are_omitted_from_serialized_form() {
    let value = ThinkingContent {
        thinking: "step 1".into(),
        signature: None,
        provider_metadata: None,
    };
    let json = serde_json::to_value(&value).unwrap();
    assert_eq!(json, json!({ "thinking": "step 1" }));
}

#[test]
fn provider_metadata_round_trips() {
    let payload = json!({
        "reasoning_content": "model self-talk",
        "openai_response_id": "resp_abc"
    });
    let value = ThinkingContent {
        thinking: "user-visible thinking".into(),
        signature: None,
        provider_metadata: Some(payload.clone()),
    };
    let serialized = serde_json::to_value(&value).unwrap();
    assert_eq!(
        serialized,
        json!({
            "thinking": "user-visible thinking",
            "providerMetadata": payload,
        })
    );
    let back: ThinkingContent = serde_json::from_value(serialized).unwrap();
    assert_eq!(back, value);
}

#[test]
fn both_signature_and_provider_metadata_round_trip() {
    let value = ThinkingContent {
        thinking: "anthropic chain".into(),
        signature: Some("anthropic-sig".into()),
        provider_metadata: Some(json!({ "extra": 1 })),
    };
    let json = serde_json::to_value(&value).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "thinking": "anthropic chain",
            "signature": "anthropic-sig",
            "providerMetadata": { "extra": 1 }
        })
    );
    let back: ThinkingContent = serde_json::from_value(json).unwrap();
    assert_eq!(back, value);
}
