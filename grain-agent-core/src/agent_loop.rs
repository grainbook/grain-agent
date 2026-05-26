//! Low-level agent loop.
//!
//! Ports `packages/agent/src/agent-loop.ts`. Works with [`AgentMessage`]
//! throughout the transcript and converts to [`Message`] only at the LLM
//! boundary.

use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use futures::stream::{FuturesOrdered, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::stream::{StreamFn, StreamOptions};
use crate::types::*;

// ---------------------------------------------------------------------------
// Hook + callback types
// ---------------------------------------------------------------------------

/// Sink invoked once per emitted event, in emission order.
pub type EventSink = Arc<dyn Fn(AgentEvent) -> BoxFuture<'static, ()> + Send + Sync>;

/// Converts an `AgentMessage[]` snapshot into LLM-ready `Message[]`.
pub type ConvertToLlmFn =
    Arc<dyn Fn(Vec<AgentMessage>) -> BoxFuture<'static, Vec<Message>> + Send + Sync>;

/// Optional context transform applied before [`ConvertToLlmFn`].
pub type TransformContextFn = Arc<
    dyn Fn(Vec<AgentMessage>, CancellationToken) -> BoxFuture<'static, Vec<AgentMessage>>
        + Send
        + Sync,
>;

/// Resolves an API key for a provider name (e.g. short-lived OAuth tokens).
pub type GetApiKeyFn = Arc<dyn Fn(String) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// Returns queued steering or follow-up messages.
pub type MessagesProviderFn = Arc<dyn Fn() -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync>;

#[derive(Clone)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub args: serde_json::Value,
    pub context: Arc<AgentContext>,
}

#[derive(Debug, Default, Clone)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
}

pub type BeforeToolCallFn = Arc<
    dyn Fn(
            BeforeToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'static, Option<BeforeToolCallResult>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub args: serde_json::Value,
    pub result: AgentToolResult,
    pub is_error: bool,
    pub context: Arc<AgentContext>,
}

#[derive(Debug, Default, Clone)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<UserContent>>,
    pub details: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}

pub type AfterToolCallFn = Arc<
    dyn Fn(
            AfterToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'static, Option<AfterToolCallResult>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct ShouldStopAfterTurnContext {
    pub message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub context: Arc<AgentContext>,
    pub new_messages: Vec<AgentMessage>,
}

pub type ShouldStopAfterTurnFn =
    Arc<dyn Fn(ShouldStopAfterTurnContext) -> BoxFuture<'static, bool> + Send + Sync>;

pub type PrepareNextTurnContext = ShouldStopAfterTurnContext;

#[derive(Default, Clone)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

pub type PrepareNextTurnFn = Arc<
    dyn Fn(
            PrepareNextTurnContext,
            CancellationToken,
        ) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// Loop configuration
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
    pub tool_execution: ToolExecutionMode,

    pub convert_to_llm: ConvertToLlmFn,
    pub transform_context: Option<TransformContextFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub get_steering_messages: Option<MessagesProviderFn>,
    pub get_follow_up_messages: Option<MessagesProviderFn>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
}

impl AgentLoopConfig {
    /// Create a config with reasonable defaults: off thinking, parallel
    /// tool execution, empty stream options. All hooks default to `None`.
    pub fn new(model: Model, convert_to_llm: ConvertToLlmFn) -> Self {
        AgentLoopConfig {
            model,
            thinking_level: ThinkingLevel::Off,
            stream_options: StreamOptions::default(),
            tool_execution: ToolExecutionMode::Parallel,
            convert_to_llm,
            transform_context: None,
            get_api_key: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
        }
    }
}

/// Errors that abort the loop with no normal event sequence.
#[derive(Debug, thiserror::Error)]
pub enum AgentLoopError {
    #[error("{0}")]
    Other(String),
}

/// Successful agent-loop result.
#[derive(Debug, Clone)]
pub(crate) struct AgentLoopResult {
    /// Messages produced during this loop invocation (the same payload
    /// surfaced in `AgentEnd`).
    pub new_messages: Vec<AgentMessage>,
    /// Final context after any `prepare_next_turn` rewrites.
    pub context: AgentContext,
}

// ---------------------------------------------------------------------------
// Top-level entry points
// ---------------------------------------------------------------------------

fn current_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Start an agent loop with new prompt messages.
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: EventSink,
    cancel: CancellationToken,
    stream_fn: StreamFn,
) -> Result<Vec<AgentMessage>, AgentLoopError> {
    Ok(
        run_agent_loop_with_result(prompts, context, config, emit, cancel, stream_fn)
            .await?
            .new_messages,
    )
}

pub(crate) async fn run_agent_loop_with_result(
    prompts: Vec<AgentMessage>,
    mut context: AgentContext,
    mut config: AgentLoopConfig,
    emit: EventSink,
    cancel: CancellationToken,
    stream_fn: StreamFn,
) -> Result<AgentLoopResult, AgentLoopError> {
    let mut new_messages: Vec<AgentMessage> = prompts.clone();
    context.messages.extend(prompts.iter().cloned());

    emit_now(&emit, AgentEvent::AgentStart).await;
    emit_now(&emit, AgentEvent::TurnStart).await;
    for prompt in &prompts {
        emit_now(
            &emit,
            AgentEvent::MessageStart {
                message: prompt.clone(),
            },
        )
        .await;
        emit_now(
            &emit,
            AgentEvent::MessageEnd {
                message: prompt.clone(),
            },
        )
        .await;
    }

    run_loop(
        &mut context,
        &mut new_messages,
        &mut config,
        cancel,
        emit,
        stream_fn,
        /* first_turn_already_started */ true,
    )
    .await?;

    Ok(AgentLoopResult {
        new_messages,
        context,
    })
}

/// Continue an existing transcript without injecting a new prompt.
///
/// The last message must convert (via `convert_to_llm`) to a `user` or
/// `toolResult` LLM message; otherwise the provider will reject the request.
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: EventSink,
    cancel: CancellationToken,
    stream_fn: StreamFn,
) -> Result<Vec<AgentMessage>, AgentLoopError> {
    Ok(
        run_agent_loop_continue_with_result(context, config, emit, cancel, stream_fn)
            .await?
            .new_messages,
    )
}

pub(crate) async fn run_agent_loop_continue_with_result(
    context: AgentContext,
    mut config: AgentLoopConfig,
    emit: EventSink,
    cancel: CancellationToken,
    stream_fn: StreamFn,
) -> Result<AgentLoopResult, AgentLoopError> {
    if context.messages.is_empty() {
        return Err(AgentLoopError::Other(
            "Cannot continue: no messages in context".into(),
        ));
    }
    if let Some(last) = context.messages.last()
        && last.role() == "assistant"
    {
        return Err(AgentLoopError::Other(
            "Cannot continue from message role: assistant".into(),
        ));
    }

    let mut new_messages: Vec<AgentMessage> = Vec::new();
    let mut current_context = context;

    emit_now(&emit, AgentEvent::AgentStart).await;
    emit_now(&emit, AgentEvent::TurnStart).await;

    run_loop(
        &mut current_context,
        &mut new_messages,
        &mut config,
        cancel,
        emit,
        stream_fn,
        true,
    )
    .await?;

    Ok(AgentLoopResult {
        new_messages,
        context: current_context,
    })
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

async fn run_loop(
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &mut AgentLoopConfig,
    cancel: CancellationToken,
    emit: EventSink,
    stream_fn: StreamFn,
    mut first_turn_already_started: bool,
) -> Result<(), AgentLoopError> {
    let mut pending_messages: Vec<AgentMessage> =
        if let Some(provider) = &config.get_steering_messages {
            provider().await
        } else {
            Vec::new()
        };

    loop {
        let mut has_more_tool_calls = true;

        while has_more_tool_calls || !pending_messages.is_empty() {
            if !first_turn_already_started {
                emit_now(&emit, AgentEvent::TurnStart).await;
            } else {
                first_turn_already_started = false;
            }

            // Inject pending (steering or follow-up) messages.
            if !pending_messages.is_empty() {
                for message in pending_messages.drain(..) {
                    emit_now(
                        &emit,
                        AgentEvent::MessageStart {
                            message: message.clone(),
                        },
                    )
                    .await;
                    emit_now(
                        &emit,
                        AgentEvent::MessageEnd {
                            message: message.clone(),
                        },
                    )
                    .await;
                    context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            // Stream the next assistant response.
            let assistant = stream_assistant_response(
                context,
                config,
                &emit,
                cancel.clone(),
                stream_fn.clone(),
            )
            .await;
            new_messages.push(AgentMessage::assistant(assistant.clone()));

            if matches!(
                assistant.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                emit_now(
                    &emit,
                    AgentEvent::TurnEnd {
                        message: assistant.clone(),
                        tool_results: Vec::new(),
                    },
                )
                .await;
                emit_now(
                    &emit,
                    AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    },
                )
                .await;
                return Ok(());
            }

            // Collect tool calls.
            let tool_calls: Vec<ToolCall> = assistant
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::ToolCall(tc) => Some(tc.clone()),
                    _ => None,
                })
                .collect();

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;

            if !tool_calls.is_empty() {
                let batch = execute_tool_calls(
                    context,
                    &assistant,
                    tool_calls,
                    config,
                    &emit,
                    cancel.clone(),
                )
                .await;
                tool_results = batch.messages;
                has_more_tool_calls = !batch.terminate;

                for result in &tool_results {
                    context
                        .messages
                        .push(AgentMessage::tool_result(result.clone()));
                    new_messages.push(AgentMessage::tool_result(result.clone()));
                }
            }

            emit_now(
                &emit,
                AgentEvent::TurnEnd {
                    message: assistant.clone(),
                    tool_results: tool_results.clone(),
                },
            )
            .await;

            // prepare_next_turn: allow swapping context/model/thinking before next turn.
            if let Some(hook) = config.prepare_next_turn.clone() {
                let ctx_snapshot = Arc::new(context.clone());
                let snapshot = hook(
                    ShouldStopAfterTurnContext {
                        message: assistant.clone(),
                        tool_results: tool_results.clone(),
                        context: ctx_snapshot,
                        new_messages: new_messages.clone(),
                    },
                    cancel.clone(),
                )
                .await;
                if let Some(update) = snapshot {
                    if let Some(new_ctx) = update.context {
                        *context = new_ctx;
                    }
                    if let Some(m) = update.model {
                        config.model = m;
                    }
                    if let Some(level) = update.thinking_level {
                        config.thinking_level = level;
                        config.stream_options.reasoning = if level == ThinkingLevel::Off {
                            None
                        } else {
                            Some(level)
                        };
                    }
                }
            }

            // should_stop_after_turn: graceful early exit.
            if let Some(hook) = config.should_stop_after_turn.clone() {
                let ctx_snapshot = Arc::new(context.clone());
                let stop = hook(ShouldStopAfterTurnContext {
                    message: assistant.clone(),
                    tool_results: tool_results.clone(),
                    context: ctx_snapshot,
                    new_messages: new_messages.clone(),
                })
                .await;
                if stop {
                    emit_now(
                        &emit,
                        AgentEvent::AgentEnd {
                            messages: new_messages.clone(),
                        },
                    )
                    .await;
                    return Ok(());
                }
            }

            // Pull steering messages for next iteration.
            pending_messages = if let Some(provider) = &config.get_steering_messages {
                provider().await
            } else {
                Vec::new()
            };
        }

        // Agent would stop here. Check for follow-up.
        let follow_up = if let Some(provider) = &config.get_follow_up_messages {
            provider().await
        } else {
            Vec::new()
        };
        if !follow_up.is_empty() {
            pending_messages = follow_up;
            continue;
        }
        break;
    }

    emit_now(
        &emit,
        AgentEvent::AgentEnd {
            messages: new_messages.clone(),
        },
    )
    .await;
    Ok(())
}

async fn emit_now(emit: &EventSink, event: AgentEvent) {
    (emit)(event).await;
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: CancellationToken,
    stream_fn: StreamFn,
) -> AssistantMessage {
    // 1) Optional context transform (AgentMessage[] -> AgentMessage[]).
    let transformed = if let Some(transform) = &config.transform_context {
        transform(context.messages.clone(), cancel.clone()).await
    } else {
        context.messages.clone()
    };

    // 2) Convert to LLM messages.
    let llm_messages = (config.convert_to_llm)(transformed).await;

    // 3) Build LLM context.
    let tool_defs: Vec<ToolDefinition> = context
        .tools
        .iter()
        .map(|t| t.definition().clone())
        .collect();
    let llm_context = LlmContext {
        system_prompt: context.system_prompt.clone(),
        messages: llm_messages,
        tools: tool_defs,
    };

    // 4) Resolve API key.
    let mut options = config.stream_options.clone();
    if let Some(get_key) = &config.get_api_key
        && let Some(key) = get_key(config.model.provider.clone()).await
    {
        options.api_key = Some(key);
    }

    // 5) Stream events.
    let stream_result = stream_fn
        .stream(&config.model, &llm_context, &options, cancel.clone())
        .await;

    let mut event_stream = match stream_result {
        Ok(s) => s,
        Err(err) => {
            // Loop contract: implementations should not return Err; degrade gracefully.
            let final_message = AssistantMessage {
                content: vec![AssistantContent::Text(TextContent::default())],
                api: config.model.api.clone(),
                provider: config.model.provider.clone(),
                model: config.model.id.clone(),
                usage: Usage::default(),
                stop_reason: StopReason::Error,
                error_message: Some(err.to_string()),
                timestamp: current_time_ms(),
            };
            context
                .messages
                .push(AgentMessage::assistant(final_message.clone()));
            emit_now(
                emit,
                AgentEvent::MessageStart {
                    message: AgentMessage::assistant(final_message.clone()),
                },
            )
            .await;
            emit_now(
                emit,
                AgentEvent::MessageEnd {
                    message: AgentMessage::assistant(final_message.clone()),
                },
            )
            .await;
            return final_message;
        }
    };

    let mut added_partial = false;
    while let Some(event) = event_stream.next().await {
        match &event {
            AssistantMessageEvent::Start { partial } => {
                added_partial = true;
                context
                    .messages
                    .push(AgentMessage::assistant(partial.clone()));
                emit_now(
                    emit,
                    AgentEvent::MessageStart {
                        message: AgentMessage::assistant(partial.clone()),
                    },
                )
                .await;
            }
            AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolcallStart { partial, .. }
            | AssistantMessageEvent::ToolcallDelta { partial, .. }
            | AssistantMessageEvent::ToolcallEnd { partial, .. } => {
                if added_partial {
                    if let Some(last) = context.messages.last_mut() {
                        *last = AgentMessage::assistant(partial.clone());
                    }
                    emit_now(
                        emit,
                        AgentEvent::MessageUpdate {
                            message: partial.clone(),
                            assistant_message_event: event.clone(),
                        },
                    )
                    .await;
                }
            }
            AssistantMessageEvent::Done { result }
            | AssistantMessageEvent::Error { result, .. } => {
                let final_message = result.clone();
                if added_partial {
                    if let Some(last) = context.messages.last_mut() {
                        *last = AgentMessage::assistant(final_message.clone());
                    }
                } else {
                    context
                        .messages
                        .push(AgentMessage::assistant(final_message.clone()));
                    emit_now(
                        emit,
                        AgentEvent::MessageStart {
                            message: AgentMessage::assistant(final_message.clone()),
                        },
                    )
                    .await;
                }
                emit_now(
                    emit,
                    AgentEvent::MessageEnd {
                        message: AgentMessage::assistant(final_message.clone()),
                    },
                )
                .await;
                return final_message;
            }
        }
    }

    // Stream ended without a terminal event: synthesize a placeholder error.
    let final_message = AssistantMessage {
        content: vec![AssistantContent::Text(TextContent::default())],
        api: config.model.api.clone(),
        provider: config.model.provider.clone(),
        model: config.model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some("stream ended without terminal event".into()),
        timestamp: current_time_ms(),
    };
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = AgentMessage::assistant(final_message.clone());
        }
    } else {
        context
            .messages
            .push(AgentMessage::assistant(final_message.clone()));
        emit_now(
            emit,
            AgentEvent::MessageStart {
                message: AgentMessage::assistant(final_message.clone()),
            },
        )
        .await;
    }
    emit_now(
        emit,
        AgentEvent::MessageEnd {
            message: AgentMessage::assistant(final_message.clone()),
        },
    )
    .await;
    final_message
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

struct ExecutedBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

#[derive(Clone)]
struct FinalizedCall {
    tool_call: ToolCall,
    result: AgentToolResult,
    is_error: bool,
}

async fn execute_tool_calls(
    context: &AgentContext,
    assistant: &AssistantMessage,
    tool_calls: Vec<ToolCall>,
    config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: CancellationToken,
) -> ExecutedBatch {
    let has_sequential = tool_calls.iter().any(|tc| {
        context
            .tools
            .iter()
            .find(|t| t.definition().name == tc.name)
            .and_then(|t| t.execution_mode())
            == Some(ToolExecutionMode::Sequential)
    });

    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential {
        execute_sequential(context, assistant, tool_calls, config, emit, cancel).await
    } else {
        execute_parallel(context, assistant, tool_calls, config, emit, cancel).await
    }
}

async fn execute_sequential(
    context: &AgentContext,
    assistant: &AssistantMessage,
    tool_calls: Vec<ToolCall>,
    config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: CancellationToken,
) -> ExecutedBatch {
    let mut finalized_calls: Vec<FinalizedCall> = Vec::new();
    let mut messages: Vec<ToolResultMessage> = Vec::new();

    for tool_call in tool_calls {
        emit_now(
            emit,
            AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            },
        )
        .await;

        let preparation = prepare_tool_call(
            context,
            assistant,
            tool_call.clone(),
            config,
            cancel.clone(),
        )
        .await;

        let finalized = match preparation {
            Preparation::Immediate(result, is_error) => FinalizedCall {
                tool_call: tool_call.clone(),
                result,
                is_error,
            },
            Preparation::Prepared(prepared) => {
                let executed = execute_prepared(&prepared, cancel.clone(), emit).await;
                finalize_executed(
                    context,
                    assistant,
                    &prepared,
                    executed,
                    config,
                    cancel.clone(),
                )
                .await
            }
        };

        emit_tool_execution_end(&finalized, emit).await;
        let trm = make_tool_result_message(&finalized);
        emit_tool_result_message(&trm, emit).await;
        finalized_calls.push(finalized);
        messages.push(trm);

        if cancel.is_cancelled() {
            break;
        }
    }

    ExecutedBatch {
        terminate: should_terminate(&finalized_calls),
        messages,
    }
}

async fn execute_parallel(
    context: &AgentContext,
    assistant: &AssistantMessage,
    tool_calls: Vec<ToolCall>,
    config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: CancellationToken,
) -> ExecutedBatch {
    // Pre-flight: emit `tool_execution_start` in source order, prepare each call
    // synchronously, and capture either an immediate failure or the executable
    // closure for the parallel phase.
    enum Slot {
        Immediate(FinalizedCall),
        Pending(PreparedToolCall),
    }
    let mut slots: Vec<Slot> = Vec::with_capacity(tool_calls.len());

    for tool_call in tool_calls {
        emit_now(
            emit,
            AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            },
        )
        .await;

        let preparation = prepare_tool_call(
            context,
            assistant,
            tool_call.clone(),
            config,
            cancel.clone(),
        )
        .await;
        match preparation {
            Preparation::Immediate(result, is_error) => {
                let finalized = FinalizedCall {
                    tool_call,
                    result,
                    is_error,
                };
                emit_tool_execution_end(&finalized, emit).await;
                slots.push(Slot::Immediate(finalized));
            }
            Preparation::Prepared(prepared) => slots.push(Slot::Pending(prepared)),
        }

        if cancel.is_cancelled() {
            break;
        }
    }

    // Parallel phase: execute pending tools concurrently while preserving
    // source order in the resulting Vec via FuturesOrdered.
    let mut ordered = FuturesOrdered::new();
    for slot in slots {
        match slot {
            Slot::Immediate(finalized) => {
                ordered.push_back(Box::pin(async move { finalized })
                    as futures::future::BoxFuture<'static, FinalizedCall>);
            }
            Slot::Pending(prepared) => {
                let emit_cloned = emit.clone();
                let cancel_cloned = cancel.clone();
                let assistant_cloned = assistant.clone();
                let context_for_after = Arc::new(context.clone());
                let after = config.after_tool_call.clone();
                ordered.push_back(Box::pin(async move {
                    let executed =
                        execute_prepared(&prepared, cancel_cloned.clone(), &emit_cloned).await;
                    let finalized = finalize_executed_owned(
                        context_for_after,
                        assistant_cloned,
                        &prepared,
                        executed,
                        after,
                        cancel_cloned,
                    )
                    .await;
                    emit_tool_execution_end(&finalized, &emit_cloned).await;
                    finalized
                })
                    as futures::future::BoxFuture<'static, FinalizedCall>);
            }
        }
    }

    let mut ordered_finalized: Vec<FinalizedCall> = Vec::new();
    while let Some(finalized) = ordered.next().await {
        ordered_finalized.push(finalized);
    }

    // Emit tool-result message artifacts in source (assistant) order.
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for finalized in &ordered_finalized {
        let trm = make_tool_result_message(finalized);
        emit_tool_result_message(&trm, emit).await;
        messages.push(trm);
    }

    ExecutedBatch {
        terminate: should_terminate(&ordered_finalized),
        messages,
    }
}

// --- Preparation / execution helpers ----------------------------------------

#[derive(Clone)]
struct PreparedToolCall {
    tool_call: ToolCall,
    tool: Arc<dyn AgentTool>,
    args: serde_json::Value,
}

enum Preparation {
    Immediate(AgentToolResult, bool),
    Prepared(PreparedToolCall),
}

async fn prepare_tool_call(
    context: &AgentContext,
    assistant: &AssistantMessage,
    tool_call: ToolCall,
    config: &AgentLoopConfig,
    cancel: CancellationToken,
) -> Preparation {
    let tool = match context
        .tools
        .iter()
        .find(|t| t.definition().name == tool_call.name)
    {
        Some(t) => t.clone(),
        None => {
            return Preparation::Immediate(
                AgentToolResult::error(format!("Tool {} not found", tool_call.name)),
                true,
            );
        }
    };

    let prepared_args = match tool.prepare_arguments(tool_call.arguments.clone()) {
        Ok(v) => v,
        Err(e) => return Preparation::Immediate(AgentToolResult::error(e.to_string()), true),
    };
    if let Err(e) = tool.validate_arguments(&prepared_args) {
        return Preparation::Immediate(AgentToolResult::error(e.to_string()), true);
    }

    if let Some(hook) = config.before_tool_call.clone() {
        let ctx_snapshot = Arc::new(context.clone());
        let before_ctx = BeforeToolCallContext {
            assistant_message: assistant.clone(),
            tool_call: tool_call.clone(),
            args: prepared_args.clone(),
            context: ctx_snapshot,
        };
        let outcome = hook(before_ctx, cancel.clone()).await;
        if cancel.is_cancelled() {
            return Preparation::Immediate(AgentToolResult::error("Operation aborted"), true);
        }
        if let Some(result) = outcome
            && result.block
        {
            let reason = result
                .reason
                .unwrap_or_else(|| "Tool execution was blocked".into());
            return Preparation::Immediate(AgentToolResult::error(reason), true);
        }
    }

    if cancel.is_cancelled() {
        return Preparation::Immediate(AgentToolResult::error("Operation aborted"), true);
    }

    Preparation::Prepared(PreparedToolCall {
        tool_call,
        tool,
        args: prepared_args,
    })
}

struct ExecutedOutcome {
    result: AgentToolResult,
    is_error: bool,
}

async fn execute_prepared(
    prepared: &PreparedToolCall,
    cancel: CancellationToken,
    emit: &EventSink,
) -> ExecutedOutcome {
    let emit_cloned = emit.clone();
    let id = prepared.tool_call.id.clone();
    let name = prepared.tool_call.name.clone();
    let args = prepared.tool_call.arguments.clone();

    let on_update: ToolUpdateCallback = Arc::new(move |partial: AgentToolResult| {
        let event = AgentEvent::ToolExecutionUpdate {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            args: args.clone(),
            partial_result: partial,
        };
        // Fire-and-forget: emit asynchronously without awaiting from the tool
        // body. tokio's spawn keeps semantics close to the TS implementation
        // which accumulates promises and awaits them after execute resolves.
        //
        // Wrap in `catch_unwind` so a panicking subscriber surfaces as a
        // stderr log instead of silently disappearing into the spawn — the
        // JoinHandle is dropped, so without this the panic is fully eaten.
        let fut = std::panic::AssertUnwindSafe((emit_cloned)(event)).catch_unwind();
        tokio::spawn(async move {
            if let Err(panic) = fut.await {
                let msg = panic
                    .downcast_ref::<&'static str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic>".into());
                eprintln!("[warn] ToolExecutionUpdate listener panicked: {msg}");
            }
        });
    });

    let exec = prepared
        .tool
        .execute(
            &prepared.tool_call.id,
            prepared.args.clone(),
            cancel.clone(),
            on_update,
        )
        .await;

    match exec {
        Ok(result) => ExecutedOutcome {
            result,
            is_error: false,
        },
        Err(err) => ExecutedOutcome {
            result: AgentToolResult::error(err.to_string()),
            is_error: true,
        },
    }
}

async fn finalize_executed(
    context: &AgentContext,
    assistant: &AssistantMessage,
    prepared: &PreparedToolCall,
    executed: ExecutedOutcome,
    config: &AgentLoopConfig,
    cancel: CancellationToken,
) -> FinalizedCall {
    finalize_executed_owned(
        Arc::new(context.clone()),
        assistant.clone(),
        prepared,
        executed,
        config.after_tool_call.clone(),
        cancel,
    )
    .await
}

async fn finalize_executed_owned(
    context: Arc<AgentContext>,
    assistant: AssistantMessage,
    prepared: &PreparedToolCall,
    executed: ExecutedOutcome,
    after_tool_call: Option<AfterToolCallFn>,
    cancel: CancellationToken,
) -> FinalizedCall {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(hook) = after_tool_call {
        let after_ctx = AfterToolCallContext {
            assistant_message: assistant.clone(),
            tool_call: prepared.tool_call.clone(),
            args: prepared.args.clone(),
            result: result.clone(),
            is_error,
            context,
        };
        let outcome = hook(after_ctx, cancel.clone()).await;
        if let Some(after) = outcome {
            if let Some(content) = after.content {
                result.content = content;
            }
            if let Some(details) = after.details {
                result.details = details;
            }
            if let Some(t) = after.terminate {
                result.terminate = Some(t);
            }
            if let Some(err_flag) = after.is_error {
                is_error = err_flag;
            }
        }
    }

    FinalizedCall {
        tool_call: prepared.tool_call.clone(),
        result,
        is_error,
    }
}

/// The batch terminates the loop only when **every** finalized tool call sets
/// `result.terminate == Some(true)`. A single tool saying "I'm done" is not
/// enough to halt the loop — the loop will continue with the remaining
/// tool-result messages so other tools can finish their work.
///
/// Per-tool intent is still visible to callers via `ToolExecutionEnd.result.terminate`;
/// UI / logging layers can flag single-tool termination requests without
/// changing loop semantics.
fn should_terminate(finalized: &[FinalizedCall]) -> bool {
    !finalized.is_empty() && finalized.iter().all(|f| f.result.terminate == Some(true))
}

fn make_tool_result_message(finalized: &FinalizedCall) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized.result.content.clone(),
        details: finalized.result.details.clone(),
        is_error: finalized.is_error,
        timestamp: current_time_ms(),
    }
}

async fn emit_tool_execution_end(finalized: &FinalizedCall, emit: &EventSink) {
    emit_now(
        emit,
        AgentEvent::ToolExecutionEnd {
            tool_call_id: finalized.tool_call.id.clone(),
            tool_name: finalized.tool_call.name.clone(),
            result: finalized.result.clone(),
            is_error: finalized.is_error,
        },
    )
    .await;
}

async fn emit_tool_result_message(trm: &ToolResultMessage, emit: &EventSink) {
    emit_now(
        emit,
        AgentEvent::MessageStart {
            message: AgentMessage::tool_result(trm.clone()),
        },
    )
    .await;
    emit_now(
        emit,
        AgentEvent::MessageEnd {
            message: AgentMessage::tool_result(trm.clone()),
        },
    )
    .await;
}
