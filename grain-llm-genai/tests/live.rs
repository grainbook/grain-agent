//! Live integration tests against real providers. **All `#[ignore]`** by
//! default — run with:
//!
//! ```bash
//! OPENAI_API_KEY=...     cargo test -p grain-llm-genai --test live -- --ignored
//! ANTHROPIC_API_KEY=...  cargo test -p grain-llm-genai --test live -- --ignored
//! MOONSHOT_API_KEY=...   cargo test -p grain-llm-genai --test live -- --ignored
//! ```
//!
//! Each test additionally short-circuits with a printed skip note when its
//! required env var isn't set, so it's fine to pass `--ignored` even when
//! only one key is configured — the others will print "skipped".
//!
//! These tests are intentionally small (one or two turns) and exist to
//! validate the full PR-3 pipeline (outbound → genai → inbound → grain
//! events) against an actual network endpoint. They are not run in CI.

use std::sync::Arc;

use futures::StreamExt;
use grain_agent_core::{
    AgentMessage, AssistantContent, AssistantMessageEvent, AssistantStream, LlmContext, LlmStream,
    Message, Model, StopReason, StreamOptions, TextContent, ToolDefinition, UserContent,
    UserMessage,
};
use grain_llm_genai::{GenaiStream, OpenAiCompatPreset};
use tokio_util::sync::CancellationToken;

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![UserContent::Text(TextContent { text: text.into() })],
        timestamp: 0,
    })
}

fn ctx_with(prompt: &str, system: &str, tools: Vec<ToolDefinition>) -> LlmContext {
    LlmContext {
        system_prompt: system.into(),
        messages: vec![user(prompt)],
        tools,
    }
}

fn model_of(id: &str, api: &str, provider: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: api.into(),
        provider: provider.into(),
        ..Default::default()
    }
}

/// Drive a stream to completion and return the final assistant message
/// from the terminal Done/Error event.
async fn drain(mut stream: AssistantStream) -> AssistantMessageEvent {
    let mut last: Option<AssistantMessageEvent> = None;
    while let Some(ev) = stream.next().await {
        let terminal = matches!(
            &ev,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        );
        last = Some(ev);
        if terminal {
            break;
        }
    }
    last.expect("stream must emit at least one event")
}

fn assert_completed(ev: &AssistantMessageEvent) {
    match ev {
        AssistantMessageEvent::Done { result } => {
            assert!(
                matches!(result.stop_reason, StopReason::Stop | StopReason::ToolUse),
                "unexpected stop_reason: {:?}; error: {:?}",
                result.stop_reason,
                result.error_message
            );
        }
        AssistantMessageEvent::Error { error, result } => {
            panic!(
                "stream failed: error={error}, message={:?}",
                result.error_message
            );
        }
        _ => panic!("expected terminal event, got {ev:?}"),
    }
}

fn assert_text_contains(ev: &AssistantMessageEvent, needle: &str) {
    if let AssistantMessageEvent::Done { result } = ev {
        let joined: String = result
            .content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let hay = joined.to_lowercase();
        assert!(
            hay.contains(needle),
            "expected response to contain {needle:?}; got {joined:?}"
        );
    } else {
        panic!("not a Done event");
    }
}

// ---------------------------------------------------------------------------
// OpenAI — native genai routing
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn live_openai_text_round_trip() {
    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!("[skip] OPENAI_API_KEY not set");
        return;
    }
    let stream_impl = Arc::new(GenaiStream::builder().build());
    let model = model_of("openai/gpt-4o-mini", "openai", "openai");
    let ctx = ctx_with(
        "Reply with exactly the word: pong",
        "You are a terse assistant. Answer with a single word.",
        Vec::new(),
    );
    let stream = stream_impl
        .stream(
            &model,
            &ctx,
            &StreamOptions::default(),
            CancellationToken::new(),
        )
        .await
        .expect("stream initialised");
    let final_event = drain(stream).await;
    assert_completed(&final_event);
    assert_text_contains(&final_event, "pong");
}

// ---------------------------------------------------------------------------
// Anthropic — native genai routing + tool call
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn live_anthropic_text_round_trip() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("[skip] ANTHROPIC_API_KEY not set");
        return;
    }
    let stream_impl = Arc::new(GenaiStream::builder().build());
    let model = model_of("anthropic/claude-haiku-4-5", "anthropic", "anthropic");
    let ctx = ctx_with(
        "Reply with exactly the word: pong",
        "You are a terse assistant. Answer with a single word.",
        Vec::new(),
    );
    let stream = stream_impl
        .stream(
            &model,
            &ctx,
            &StreamOptions::default(),
            CancellationToken::new(),
        )
        .await
        .expect("stream initialised");
    let final_event = drain(stream).await;
    assert_completed(&final_event);
    assert_text_contains(&final_event, "pong");
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn live_anthropic_tool_call_round_trip() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("[skip] ANTHROPIC_API_KEY not set");
        return;
    }
    let echo = ToolDefinition {
        name: "echo".into(),
        label: "Echo".into(),
        description: "Echo back the value you receive verbatim".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"]
        }),
        execution_mode: None,
    };
    let stream_impl = Arc::new(GenaiStream::builder().build());
    let model = model_of("anthropic/claude-haiku-4-5", "anthropic", "anthropic");
    let ctx = LlmContext {
        system_prompt: "Use the echo tool with value=\"ping\" — do not answer in text.".into(),
        messages: vec![user("Please invoke the echo tool with value=ping.")],
        tools: vec![echo],
    };
    let stream = stream_impl
        .stream(
            &model,
            &ctx,
            &StreamOptions::default(),
            CancellationToken::new(),
        )
        .await
        .expect("stream initialised");
    let final_event = drain(stream).await;
    assert_completed(&final_event);
    if let AssistantMessageEvent::Done { result } = &final_event {
        let made_tool_call = result
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_)));
        assert!(
            made_tool_call,
            "expected an echo tool call; got {:?}",
            result.content
        );
        assert_eq!(result.stop_reason, StopReason::ToolUse);
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compat preset — Kimi
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires MOONSHOT_API_KEY"]
async fn live_kimi_openai_compat_round_trip() {
    if std::env::var("MOONSHOT_API_KEY").is_err() {
        eprintln!("[skip] MOONSHOT_API_KEY not set");
        return;
    }
    let stream_impl = Arc::new(
        GenaiStream::builder()
            .with_openai_compat_preset(OpenAiCompatPreset::Common)
            .build(),
    );
    let model = model_of("kimi/moonshot-v1-8k", "openai", "kimi");
    let ctx = ctx_with(
        "Reply with exactly the word: pong",
        "You are a terse assistant. Answer with a single word.",
        Vec::new(),
    );
    let stream = stream_impl
        .stream(
            &model,
            &ctx,
            &StreamOptions::default(),
            CancellationToken::new(),
        )
        .await
        .expect("stream initialised");
    let final_event = drain(stream).await;
    assert_completed(&final_event);
    assert_text_contains(&final_event, "pong");
}

// ---------------------------------------------------------------------------
// Cancellation — ensure aborted stream emits a terminal Aborted event
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY (any working provider would do)"]
async fn live_cancel_yields_aborted_terminal_event() {
    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!("[skip] OPENAI_API_KEY not set");
        return;
    }
    let stream_impl = Arc::new(GenaiStream::builder().build());
    let model = model_of("openai/gpt-4o-mini", "openai", "openai");
    let ctx = ctx_with(
        "Write a 500-word story about a robot mouse.",
        "",
        Vec::new(),
    );
    let cancel = CancellationToken::new();
    let stream = stream_impl
        .stream(&model, &ctx, &StreamOptions::default(), cancel.clone())
        .await
        .expect("stream initialised");

    // Cancel after ~50ms — should land mid-stream for a long completion.
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        canceller.cancel();
    });

    let final_event = drain(stream).await;
    // Either we managed to cancel before completion (Error+Aborted) or the
    // model finished faster than expected (Done). Both are acceptable as
    // long as the stream terminated cleanly.
    match &final_event {
        AssistantMessageEvent::Error { result, .. } => {
            assert_eq!(result.stop_reason, StopReason::Aborted);
        }
        AssistantMessageEvent::Done { .. } => {
            eprintln!("[note] completion finished before cancel could fire");
        }
        other => panic!("unexpected terminal event: {other:?}"),
    }
}

#[allow(dead_code)]
fn _ensure_agent_message_is_clone(_: AgentMessage) {} // compile-time sanity
