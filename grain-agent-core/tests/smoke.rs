//! End-to-end smoke test for the core agent loop:
//! - mock `LlmStream` produces a single assistant turn that requests one tool call,
//!   then a follow-up assistant turn that finishes with `stop`
//! - an `echo` tool returns the args verbatim
//! - asserts the full event sequence and tool-result wiring.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures::StreamExt;
use grain_agent_core::{
    Agent, AgentContext, AgentEvent, AgentLoopTurnUpdate, AgentMessage, AgentOptions, AgentTool,
    AgentToolError, AgentToolResult, AssistantContent, AssistantMessage, AssistantMessageEvent,
    LlmContext, LlmStream, Message, Model, StopReason, StreamError, StreamOptions, TextContent,
    ToolCall, ToolDefinition, ToolUpdateCallback, Usage, UserContent,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct MockStream {
    call_count: AtomicUsize,
}

#[async_trait]
impl LlmStream for MockStream {
    async fn stream(
        &self,
        model: &Model,
        _context: &LlmContext,
        _options: &StreamOptions,
        _cancel: CancellationToken,
    ) -> Result<grain_agent_core::AssistantStream, StreamError> {
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);
        let model = model.clone();

        let events: Vec<AssistantMessageEvent> = match n {
            0 => {
                // First turn: ask for one tool call.
                let msg = AssistantMessage {
                    content: vec![AssistantContent::ToolCall(ToolCall {
                        id: "call-1".into(),
                        name: "echo".into(),
                        arguments: serde_json::json!({ "value": "hi" }),
                    })],
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 0,
                };
                vec![
                    AssistantMessageEvent::Start {
                        partial: msg.clone(),
                    },
                    AssistantMessageEvent::Done { result: msg },
                ]
            }
            _ => {
                // Second turn: short text reply, stop.
                let msg = AssistantMessage {
                    content: vec![AssistantContent::Text(TextContent {
                        text: "done".into(),
                    })],
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 0,
                };
                vec![
                    AssistantMessageEvent::Start {
                        partial: msg.clone(),
                    },
                    AssistantMessageEvent::Done { result: msg },
                ]
            }
        };

        let stream = futures::stream::iter(events).boxed();
        Ok(stream)
    }
}

#[derive(Debug)]
struct EchoTool {
    def: ToolDefinition,
}

impl EchoTool {
    fn new() -> Self {
        EchoTool {
            def: ToolDefinition {
                name: "echo".into(),
                label: "Echo".into(),
                description: "Echo back the value".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": ["value"]
                }),
                execution_mode: None,
            },
        }
    }
}

#[async_trait]
impl AgentTool for EchoTool {
    fn definition(&self) -> &ToolDefinition {
        &self.def
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let value = args
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(AgentToolResult {
            content: vec![UserContent::text(value)],
            details: serde_json::json!({}),
            terminate: None,
        })
    }
}

#[tokio::test]
async fn full_round_trip_with_tool_call() {
    let stream = Arc::new(MockStream::default());
    let mut opts = AgentOptions::new(
        Model {
            id: "mock-model".into(),
            name: "mock".into(),
            api: "mock".into(),
            provider: "mock".into(),
            ..Default::default()
        },
        stream.clone(),
    );
    opts.tools = vec![Arc::new(EchoTool::new())];

    let agent = Agent::new(opts);

    let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    agent
        .subscribe(Arc::new(move |event: AgentEvent, _signal| {
            let events = events_clone.clone();
            Box::pin(async move {
                let tag = match event {
                    AgentEvent::AgentStart => "agent_start",
                    AgentEvent::AgentEnd { .. } => "agent_end",
                    AgentEvent::TurnStart => "turn_start",
                    AgentEvent::TurnEnd { .. } => "turn_end",
                    AgentEvent::MessageStart { .. } => "message_start",
                    AgentEvent::MessageUpdate { .. } => "message_update",
                    AgentEvent::MessageEnd { .. } => "message_end",
                    AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
                    AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
                    AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
                };
                events.lock().unwrap().push(tag);
            })
        }))
        .await;

    agent.prompt_text("hello").await.expect("prompt failed");

    let state = agent.state().await;
    assert!(!state.is_streaming);
    assert!(state.error_message.is_none(), "no error expected");
    // user prompt + assistant(tool_use) + tool_result + assistant(stop)
    assert_eq!(
        state.messages.len(),
        4,
        "expected 4 messages, got {:#?}",
        state.messages
    );

    let tags = events.lock().unwrap().clone();
    // Expected high-level order: agent_start, turn_start, prompt msg start/end,
    // assistant msg start/end, tool start/end + result msg start/end, turn_end,
    // turn_start, assistant msg start/end, turn_end, agent_end.
    assert_eq!(tags.first().copied(), Some("agent_start"));
    assert_eq!(tags.last().copied(), Some("agent_end"));
    assert!(tags.contains(&"tool_execution_start"));
    assert!(tags.contains(&"tool_execution_end"));
    assert!(tags.iter().filter(|t| **t == "turn_end").count() >= 2);
    assert_eq!(stream.call_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn prepare_next_turn_context_rewrite_persists_to_agent_state() {
    let stream = Arc::new(MockStream::default());
    let mut opts = AgentOptions::new(
        Model {
            id: "mock-model".into(),
            name: "mock".into(),
            api: "mock".into(),
            provider: "mock".into(),
            ..Default::default()
        },
        stream,
    );
    opts.tools = vec![Arc::new(EchoTool::new())];
    opts.prepare_next_turn = Some(Arc::new(|ctx| {
        Box::pin(async move {
            Some(AgentLoopTurnUpdate {
                context: Some(AgentContext {
                    system_prompt: ctx.context.system_prompt.clone(),
                    messages: vec![AgentMessage::user(grain_agent_core::UserMessage {
                        content: vec![UserContent::text("compacted summary")],
                        timestamp: 0,
                    })],
                    tools: ctx.context.tools.clone(),
                }),
                ..Default::default()
            })
        })
    }));

    let agent = Agent::new(opts);
    agent.prompt_text("hello").await.expect("prompt failed");

    let state = agent.state().await;
    assert_eq!(
        state.messages.len(),
        1,
        "final context rewrite should be persisted"
    );
    match &state.messages[0] {
        AgentMessage::Standard(Message::User(u)) => {
            assert_eq!(
                u.content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.as_str()),
                        UserContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                "compacted summary"
            );
        }
        other => panic!("expected compacted user message, got {other:#?}"),
    }
}
