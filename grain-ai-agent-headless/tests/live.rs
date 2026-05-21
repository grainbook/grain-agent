//! Live integration tests against a real LLM provider. **All `#[ignore]`**
//! by default — they cost money / quota and need network. Run with:
//!
//! ```bash
//! # Put your real keys in `.env.test` (gitignored). See `.env.test.example`.
//! cargo test -p grain-ai-agent-headless --test live -- --ignored
//! ```
//!
//! Default provider: DeepSeek (`deepseek/deepseek-chat`). It's cheap, fast,
//! and supports tool calls — a sensible smoke-test target. Override the
//! model via `GRAIN_LIVE_TEST_MODEL` if you want to point at Anthropic /
//! OpenAI / etc.
//!
//! Each test individually skips with a printed note when its required
//! env var is missing, so passing `--ignored` is always safe even with
//! only one provider configured.

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use grain_agent_core::{
    AgentEvent, AgentMessage, AgentToolResult, AssistantContent, AssistantStream, LlmContext,
    LlmStream, Message, StopReason, StreamOptions, ToolDefinition, UserContent,
};
use grain_ai_agent_headless::{
    Args, EventPrinter, EventSink, JsonEventPrinter, OutputFormat,
};
use grain_llm_genai::GenaiStream;
use grain_llm_models::Registry;
use tokio_util::sync::CancellationToken;

/// Load `.env.test` from the workspace root (if present) into the process
/// environment. We don't fail on missing — each test will skip with a
/// printed note when its key isn't set.
fn load_env_test() {
    // Walk up from CARGO_MANIFEST_DIR looking for .env.test. The crate
    // sits at <root>/grain-ai-agent-headless/, so we go one level up.
    let mut dir: PathBuf = env!("CARGO_MANIFEST_DIR").into();
    for _ in 0..5 {
        let candidate = dir.join(".env.test");
        if candidate.exists() {
            // dotenvy loads `KEY=VALUE` lines into the process env without
            // overwriting existing values — so shell exports still win.
            let _ = dotenvy::from_path(&candidate);
            return;
        }
        if !dir.pop() {
            break;
        }
    }
}

fn require_env(key: &str, test_name: &str) -> Option<String> {
    load_env_test();
    let val = std::env::var(key).ok().filter(|s| !s.is_empty());
    if val.is_none() {
        eprintln!("[skip] {test_name}: {key} not set (put it in .env.test or your shell env)");
    }
    val
}

fn live_model_id() -> String {
    std::env::var("GRAIN_LIVE_TEST_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "deepseek/deepseek-chat".into())
}

/// Resolve which env var key the chosen model needs. Lets the test suite
/// stay agnostic between DeepSeek / Anthropic / OpenAI / Kimi …
fn env_var_for_model(model_id: &str) -> &'static str {
    match model_id.split_once('/').map(|(p, _)| p) {
        Some("anthropic") => "ANTHROPIC_API_KEY",
        Some("openai") => "OPENAI_API_KEY",
        Some("google") | Some("gemini") => "GEMINI_API_KEY",
        Some("kimi") | Some("moonshot") | Some("moonshotai") => "MOONSHOT_API_KEY",
        Some("siliconflow") => "SILICONFLOW_API_KEY",
        // DeepSeek and a sensible default fallback.
        _ => "DEEPSEEK_API_KEY",
    }
}

// ---------------------------------------------------------------------------
// CLI Args parsing — pure, doesn't touch the network. Lives here so the
// rest of the suite can build Args the same way.
// ---------------------------------------------------------------------------

#[test]
fn cli_args_parse_with_session_path() {
    use clap::Parser;
    let args = Args::try_parse_from([
        "grain-headless",
        "--model",
        "deepseek/deepseek-chat",
        "--session",
        "/tmp/sess.jsonl",
        "--prompt",
        "hi",
    ])
    .expect("parse");
    assert_eq!(args.session.as_deref(), Some(std::path::Path::new("/tmp/sess.jsonl")));
    assert_eq!(args.prompt.as_deref(), Some("hi"));
}

#[test]
fn event_sink_dyn_dispatch_works() {
    // Both EventPrinter and JsonEventPrinter implement EventSink — ensure
    // they're trait-object-compatible without ever calling the network.
    let sinks: Vec<Arc<dyn EventSink + Send + Sync>> = vec![
        Arc::new(EventPrinter::new(false)),
        Arc::new(JsonEventPrinter::new()),
    ];
    for s in sinks {
        s.print(&AgentEvent::AgentStart);
    }
    let _format_default = OutputFormat::Text;
}

// ---------------------------------------------------------------------------
// End-to-end live tests
// ---------------------------------------------------------------------------

/// Drive a stream to completion and return the final event.
async fn drain(mut stream: AssistantStream) -> grain_agent_core::AssistantMessageEvent {
    let mut last = None;
    while let Some(ev) = stream.next().await {
        let terminal = matches!(
            &ev,
            grain_agent_core::AssistantMessageEvent::Done { .. }
                | grain_agent_core::AssistantMessageEvent::Error { .. }
        );
        last = Some(ev);
        if terminal {
            break;
        }
    }
    last.expect("stream must terminate")
}

fn assert_done_with_text(ev: &grain_agent_core::AssistantMessageEvent, needle: &str) {
    match ev {
        grain_agent_core::AssistantMessageEvent::Done { result } => {
            let joined: String = result
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                joined.to_lowercase().contains(needle),
                "expected response to contain {needle:?}; got {joined:?}"
            );
        }
        grain_agent_core::AssistantMessageEvent::Error { error, result } => {
            panic!(
                "stream failed: error={error}, message={:?}",
                result.error_message
            );
        }
        other => panic!("expected terminal, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires .env.test with a real provider key"]
async fn live_simple_prompt_round_trip() {
    let model_id = live_model_id();
    let env_var = env_var_for_model(&model_id);
    let Some(_key) = require_env(env_var, "live_simple_prompt_round_trip") else {
        return;
    };

    let registry = Registry::from_embedded_snapshot();
    let model = registry
        .to_core_model(&model_id)
        .unwrap_or_else(|| panic!("model {model_id} not in embedded snapshot"));

    let stream_impl = Arc::new(GenaiStream::builder().build());
    let ctx = LlmContext {
        system_prompt: "You are a terse assistant. Answer with one word.".into(),
        messages: vec![Message::User(grain_agent_core::UserMessage {
            content: vec![UserContent::Text(grain_agent_core::TextContent {
                text: "Reply with exactly the word: pong".into(),
            })],
            timestamp: 0,
        })],
        tools: Vec::new(),
    };

    let stream = stream_impl
        .stream(&model, &ctx, &StreamOptions::default(), CancellationToken::new())
        .await
        .expect("stream initialised");
    let final_event = drain(stream).await;
    assert_done_with_text(&final_event, "pong");
}

#[tokio::test]
#[ignore = "requires .env.test with a real provider key"]
async fn live_tool_call_round_trip() {
    let model_id = live_model_id();
    let env_var = env_var_for_model(&model_id);
    let Some(_key) = require_env(env_var, "live_tool_call_round_trip") else {
        return;
    };

    let echo = ToolDefinition {
        name: "echo".into(),
        label: "Echo".into(),
        description: "Echo back the value you receive verbatim.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"]
        }),
        execution_mode: None,
    };

    let registry = Registry::from_embedded_snapshot();
    let model = registry
        .to_core_model(&model_id)
        .unwrap_or_else(|| panic!("model {model_id} not in embedded snapshot"));

    let stream_impl = Arc::new(GenaiStream::builder().build());
    let ctx = LlmContext {
        system_prompt: "Use the echo tool with value=\"ping\". Do not answer in text.".into(),
        messages: vec![Message::User(grain_agent_core::UserMessage {
            content: vec![UserContent::Text(grain_agent_core::TextContent {
                text: "Please invoke the echo tool with value=ping.".into(),
            })],
            timestamp: 0,
        })],
        tools: vec![echo],
    };
    let stream = stream_impl
        .stream(&model, &ctx, &StreamOptions::default(), CancellationToken::new())
        .await
        .expect("stream initialised");
    let final_event = drain(stream).await;
    if let grain_agent_core::AssistantMessageEvent::Done { result } = &final_event {
        let made_tool_call = result
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_)));
        assert!(
            made_tool_call,
            "expected an echo tool call from {model_id}; got {:?}",
            result.content
        );
        // tool_use is the canonical stop reason when a model emits a tool call.
        // Some providers map differently — accept Stop as a fallback.
        assert!(
            matches!(result.stop_reason, StopReason::ToolUse | StopReason::Stop),
            "unexpected stop_reason from {model_id}: {:?}",
            result.stop_reason
        );
    } else {
        panic!("expected Done event");
    }
}

#[tokio::test]
#[ignore = "requires .env.test with a real provider key"]
async fn live_agent_with_workspace_tools_round_trip() {
    // End-to-end: build an Agent with the headless read-only tools, ask
    // it about its own workspace, verify the loop completed cleanly.
    use grain_agent_core::{Agent, AgentOptions};
    use grain_ai_agent_headless::{Workspace, coding_read_tools};

    let model_id = live_model_id();
    let env_var = env_var_for_model(&model_id);
    let Some(_key) = require_env(env_var, "live_agent_with_workspace_tools_round_trip") else {
        return;
    };

    // Use a tempdir with a tiny project so the test isn't sensitive to
    // the agent's own source tree.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("README.md"), "# Tiny Demo\n\nDoes nothing.\n")
        .expect("write");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(
        dir.path().join("src").join("main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .expect("write");

    let workspace = Arc::new(Workspace::new(dir.path()).expect("workspace"));
    let registry = Registry::from_embedded_snapshot();
    let model = registry.to_core_model(&model_id).expect("model");
    let stream_impl = Arc::new(GenaiStream::builder().build());

    let mut opts = AgentOptions::new(model, stream_impl);
    opts.system_prompt =
        "You are a terse coding assistant. Use the tools to inspect the workspace, \
         then answer with one short sentence."
            .into();
    opts.tools = coding_read_tools(workspace);

    let agent = Agent::new(opts);
    agent
        .subscribe(Arc::new(|event, _signal| {
            Box::pin(async move {
                if let AgentEvent::ToolExecutionEnd {
                    tool_name,
                    is_error,
                    ..
                } = &event
                {
                    eprintln!("[tool {tool_name} done, is_error={is_error}]");
                }
            })
        }))
        .await;

    agent
        .prompt_text("What language is this project written in? Look at the source files.")
        .await
        .expect("prompt drove the loop");

    let state = agent.state().await;
    assert!(
        state.error_message.is_none(),
        "agent ended with error: {:?}",
        state.error_message
    );
    // Sanity check: at least one assistant message and one tool call landed.
    let assistant_count = state
        .messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::Standard(Message::Assistant(_))))
        .count();
    let tool_result_count = state
        .messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::Standard(Message::ToolResult(_))))
        .count();
    assert!(assistant_count >= 1, "no assistant message in transcript");
    assert!(
        tool_result_count >= 1,
        "no tool result in transcript — model didn't use the tools"
    );
}

// `AgentToolResult` import is used by future fixtures; suppress the dead-code
// warning for the live binary so adding more tests later doesn't need
// touching imports.
#[allow(dead_code)]
fn _ensure_tool_result_visible(_: AgentToolResult) {}
