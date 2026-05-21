//! Tests for `context_guard`: estimator + policy + transform function.

use std::sync::Arc;

use grain_agent_core::{
    AgentMessage, AssistantContent, AssistantMessage, StopReason, TextContent, ToolCall,
    ToolResultMessage, Usage, UserContent, UserMessage,
};
use grain_agent_harness::{ContextGuard, ContextGuardPolicy, TokenEstimator};
use grain_llm_models::{
    ApiKind, Capabilities, ModelDescriptor, ProviderId, Registry, ThinkingProfile,
};
use tokio_util::sync::CancellationToken;

fn user_text(text: &str) -> AgentMessage {
    AgentMessage::user(UserMessage {
        content: vec![UserContent::Text(TextContent { text: text.into() })],
        timestamp: 0,
    })
}

fn assistant_text(text: &str) -> AgentMessage {
    AgentMessage::assistant(AssistantMessage {
        content: vec![AssistantContent::Text(TextContent { text: text.into() })],
        api: "test".into(),
        provider: "test".into(),
        model: "test".into(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    })
}

fn tool_result(text: &str) -> AgentMessage {
    AgentMessage::tool_result(ToolResultMessage {
        tool_call_id: "c".into(),
        tool_name: "t".into(),
        content: vec![UserContent::Text(TextContent { text: text.into() })],
        details: serde_json::Value::Null,
        is_error: false,
        timestamp: 0,
    })
}

/// Build a registry containing exactly one descriptor with the given
/// context window so tests don't depend on the vendored snapshot.
fn registry_with(model_id: &str, context_window: u64) -> Arc<Registry> {
    let d = ModelDescriptor {
        id: model_id.into(),
        name: model_id.into(),
        provider: ProviderId::Other { id: "test".into() },
        api: ApiKind::OpenAi,
        context_window,
        max_output_tokens: 1024,
        cost: Default::default(),
        capabilities: Capabilities::default(),
        thinking: ThinkingProfile::default(),
        extra: serde_json::Value::Null,
    };
    Arc::new(Registry::from_descriptors([d]).unwrap())
}

#[test]
fn estimator_string_uses_char_count() {
    let est = TokenEstimator::approximate();
    // 8 chars / 4 = 2 tokens
    assert_eq!(est.estimate_string("12345678"), 2);
    // 1 char / 4 ceils to 1
    assert_eq!(est.estimate_string("a"), 1);
    // Empty
    assert_eq!(est.estimate_string(""), 0);
}

#[test]
fn estimator_user_message_counts_text_only() {
    let est = TokenEstimator::approximate();
    let m = user_text("abcd"); // 4 chars / 4 = 1 token
    assert_eq!(est.estimate_message(&m), 1);
}

#[test]
fn estimator_image_content_uses_flat_cost() {
    use grain_agent_core::ImageContent;
    let est = TokenEstimator::approximate();
    let m = AgentMessage::user(UserMessage {
        content: vec![UserContent::Image(ImageContent {
            data: "x".repeat(10_000),
            mime_type: "image/png".into(),
        })],
        timestamp: 0,
    });
    // Image has a flat 100-token cost; the base64 payload is not added.
    assert_eq!(est.estimate_message(&m), 100);
}

#[test]
fn estimator_assistant_thinking_counts_text_plus_signature() {
    use grain_agent_core::ThinkingContent;
    let est = TokenEstimator::approximate();
    let m = AgentMessage::assistant(AssistantMessage {
        content: vec![AssistantContent::Thinking(ThinkingContent {
            thinking: "abcd".into(),       // 1 token
            signature: Some("efgh".into()), // 1 token
            provider_metadata: None,
        })],
        api: "x".into(),
        provider: "x".into(),
        model: "x".into(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    });
    assert_eq!(est.estimate_message(&m), 2);
}

#[test]
fn estimator_tool_call_counts_name_plus_arguments() {
    let est = TokenEstimator::approximate();
    let m = AgentMessage::assistant(AssistantMessage {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "echo".into(),                            // 4 chars / 4 = 1
            arguments: serde_json::json!({ "v": "ab" }),  // {"v":"ab"} = 10 chars / 4 = 3
        })],
        api: "x".into(),
        provider: "x".into(),
        model: "x".into(),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 0,
    });
    assert_eq!(est.estimate_message(&m), 4);
}

#[test]
fn estimator_messages_sums_each_entry() {
    let est = TokenEstimator::approximate();
    let msgs = vec![user_text("aaaa"), assistant_text("bbbb"), tool_result("cccc")];
    assert_eq!(est.estimate_messages(&msgs), 3);
}

#[test]
fn estimator_clamps_invalid_chars_per_token() {
    // Negative and zero values fall back to 1.0 (no divide-by-zero).
    let est = TokenEstimator::with_chars_per_token(-1.0);
    assert_eq!(est.estimate_string("abcd"), 4);
}

#[tokio::test]
async fn under_budget_passes_through_unchanged() {
    let registry = registry_with("test/model", 1_000);
    let transform = ContextGuard::new(registry, "test/model")
        .with_headroom_tokens(0)
        .into_transform_fn();
    let msgs = vec![user_text("short"), assistant_text("reply")];
    let out = transform(msgs.clone(), CancellationToken::new()).await;
    assert_eq!(out.len(), msgs.len());
}

#[tokio::test]
async fn drop_oldest_truncates_from_head() {
    // 4 messages each "aaaaaaaaaaaaaaaaaaaa" (20 chars / 4 = 5 tokens) =
    // 20 tokens total. Budget = 11 tokens → should drop until <= 11 (3
    // messages = 15 tokens > 11, 2 messages = 10 tokens ≤ 11).
    let registry = registry_with("test/model", 11);
    let transform = ContextGuard::new(registry, "test/model")
        .with_headroom_tokens(0)
        .with_policy(ContextGuardPolicy::DropOldest)
        .into_transform_fn();
    let msgs = vec![
        user_text(&"a".repeat(20)),
        user_text(&"b".repeat(20)),
        user_text(&"c".repeat(20)),
        user_text(&"d".repeat(20)),
    ];
    let out = transform(msgs, CancellationToken::new()).await;
    assert_eq!(out.len(), 2);
    // The kept messages are the **last** two.
    if let AgentMessage::Standard(grain_agent_core::Message::User(u)) = &out[0]
        && let UserContent::Text(t) = &u.content[0]
    {
        assert!(t.text.starts_with("ccc"));
    }
}

#[tokio::test]
async fn drop_oldest_keeps_at_least_one_message() {
    // Even if the lone message blows the budget, keep it — losing the
    // entire transcript breaks the agent loop.
    let registry = registry_with("test/model", 1);
    let transform = ContextGuard::new(registry, "test/model")
        .with_headroom_tokens(0)
        .into_transform_fn();
    let huge = user_text(&"x".repeat(1_000));
    let out = transform(vec![huge], CancellationToken::new()).await;
    assert_eq!(out.len(), 1);
}

#[tokio::test]
async fn keep_recent_only_kicks_in_when_over_budget() {
    // Under budget → pass through, ignoring the cap.
    let registry = registry_with("test/model", 1_000);
    let transform = ContextGuard::new(registry, "test/model")
        .with_headroom_tokens(0)
        .with_policy(ContextGuardPolicy::KeepRecent(1))
        .into_transform_fn();
    let msgs = vec![user_text("a"), user_text("b"), user_text("c")];
    let out = transform(msgs, CancellationToken::new()).await;
    assert_eq!(out.len(), 3, "under budget — policy doesn't kick in");
}

#[tokio::test]
async fn keep_recent_truncates_when_over_budget() {
    let registry = registry_with("test/model", 3);
    let transform = ContextGuard::new(registry, "test/model")
        .with_headroom_tokens(0)
        .with_policy(ContextGuardPolicy::KeepRecent(1))
        .into_transform_fn();
    // 5 user messages, 5 tokens each → 25 tokens, budget = 3.
    let msgs: Vec<AgentMessage> = (0..5).map(|i| user_text(&format!("{:5}", i))).collect();
    let out = transform(msgs, CancellationToken::new()).await;
    assert_eq!(out.len(), 1);
}

#[tokio::test]
async fn identity_policy_never_truncates() {
    let registry = registry_with("test/model", 1);
    let transform = ContextGuard::new(registry, "test/model")
        .with_headroom_tokens(0)
        .with_policy(ContextGuardPolicy::Identity)
        .into_transform_fn();
    let msgs = vec![user_text(&"a".repeat(100)), user_text(&"b".repeat(100))];
    let out = transform(msgs.clone(), CancellationToken::new()).await;
    assert_eq!(out.len(), msgs.len());
}

#[tokio::test]
async fn unknown_model_id_is_a_noop() {
    let registry = registry_with("known", 1);
    let transform = ContextGuard::new(registry, "unknown/model")
        .with_headroom_tokens(0)
        .with_policy(ContextGuardPolicy::DropOldest)
        .into_transform_fn();
    let huge = vec![user_text(&"x".repeat(10_000))];
    let out = transform(huge.clone(), CancellationToken::new()).await;
    assert_eq!(out.len(), huge.len(), "unknown model → guard does nothing");
}

#[tokio::test]
async fn headroom_is_subtracted_from_budget() {
    // context_window = 100, headroom = 95 → effective budget = 5 tokens.
    let registry = registry_with("test/model", 100);
    let transform = ContextGuard::new(registry, "test/model")
        .with_headroom_tokens(95)
        .with_policy(ContextGuardPolicy::DropOldest)
        .into_transform_fn();
    // 4 messages of 5 tokens each.
    let msgs = vec![
        user_text(&"a".repeat(20)),
        user_text(&"b".repeat(20)),
        user_text(&"c".repeat(20)),
        user_text(&"d".repeat(20)),
    ];
    let out = transform(msgs, CancellationToken::new()).await;
    assert_eq!(out.len(), 1, "only the last message fits within 5-token budget");
}
