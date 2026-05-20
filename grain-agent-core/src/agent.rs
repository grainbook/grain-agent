//! Stateful `Agent` wrapper, ported from `packages/agent/src/agent.ts`.

use std::collections::HashSet;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{
    self, AfterToolCallFn, AgentLoopConfig, BeforeToolCallFn, ConvertToLlmFn, EventSink,
    GetApiKeyFn, MessagesProviderFn, PrepareNextTurnFn, TransformContextFn,
};
use crate::stream::{StreamFn, StreamOptions};
use crate::types::*;

// ---------------------------------------------------------------------------
// Listener type and helpers
// ---------------------------------------------------------------------------

pub type EventListener = Arc<
    dyn Fn(AgentEvent, CancellationToken) -> BoxFuture<'static, ()> + Send + Sync,
>;

#[derive(Default)]
struct PendingMessageQueue {
    messages: Vec<AgentMessage>,
    mode: QueueMode,
}

impl PendingMessageQueue {
    fn new(mode: QueueMode) -> Self {
        PendingMessageQueue {
            messages: Vec::new(),
            mode,
        }
    }
    fn enqueue(&mut self, m: AgentMessage) {
        self.messages.push(m);
    }
    fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }
    fn drain(&mut self) -> Vec<AgentMessage> {
        if self.messages.is_empty() {
            return Vec::new();
        }
        match self.mode {
            QueueMode::All => std::mem::take(&mut self.messages),
            QueueMode::OneAtATime => vec![self.messages.remove(0)],
        }
    }
    fn clear(&mut self) {
        self.messages.clear();
    }
}

// ---------------------------------------------------------------------------
// Inner agent state
// ---------------------------------------------------------------------------

struct Inner {
    system_prompt: String,
    model: Model,
    thinking_level: ThinkingLevel,
    tools: Vec<Arc<dyn AgentTool>>,
    messages: Vec<AgentMessage>,
    is_streaming: bool,
    streaming_message: Option<AgentMessage>,
    pending_tool_calls: HashSet<String>,
    error_message: Option<String>,

    listeners: Vec<EventListener>,
    steering_queue: PendingMessageQueue,
    follow_up_queue: PendingMessageQueue,

    active_run: Option<CancellationToken>,
}

impl Inner {
    fn snapshot(&self) -> AgentState {
        AgentState {
            system_prompt: self.system_prompt.clone(),
            model: self.model.clone(),
            thinking_level: self.thinking_level,
            tools: self.tools.clone(),
            messages: self.messages.clone(),
            is_streaming: self.is_streaming,
            streaming_message: self.streaming_message.clone(),
            pending_tool_calls: self.pending_tool_calls.clone(),
            error_message: self.error_message.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// AgentOptions + Agent
// ---------------------------------------------------------------------------

pub struct AgentOptions {
    pub system_prompt: String,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<AgentMessage>,

    pub convert_to_llm: Option<ConvertToLlmFn>,
    pub transform_context: Option<TransformContextFn>,
    pub stream_fn: StreamFn,
    pub get_api_key: Option<GetApiKeyFn>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,

    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub session_id: Option<String>,
    pub transport: Option<String>,
    pub max_retry_delay_ms: Option<u64>,
    pub tool_execution: ToolExecutionMode,
}

impl AgentOptions {
    pub fn new(model: Model, stream_fn: StreamFn) -> Self {
        AgentOptions {
            system_prompt: String::new(),
            model,
            thinking_level: ThinkingLevel::Off,
            tools: Vec::new(),
            messages: Vec::new(),
            convert_to_llm: None,
            transform_context: None,
            stream_fn,
            get_api_key: None,
            before_tool_call: None,
            after_tool_call: None,
            prepare_next_turn: None,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            session_id: None,
            transport: None,
            max_retry_delay_ms: None,
            tool_execution: ToolExecutionMode::Parallel,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent is already processing")]
    AlreadyRunning,
    #[error("no messages to continue from")]
    NoMessagesToContinue,
    #[error("cannot continue from message role: assistant")]
    CannotContinueFromAssistant,
    #[error("{0}")]
    Other(String),
}

/// Default `convert_to_llm`: drop custom messages, keep standard LLM messages.
fn default_convert_to_llm() -> ConvertToLlmFn {
    Arc::new(|messages: Vec<AgentMessage>| {
        Box::pin(async move {
            messages
                .into_iter()
                .filter_map(|m| match m {
                    AgentMessage::Standard(m) => Some(m),
                    AgentMessage::Custom(_) => None,
                })
                .collect()
        })
    })
}

/// Stateful wrapper around the low-level agent loop.
pub struct Agent {
    inner: Arc<Mutex<Inner>>,
    stream_fn: StreamFn,
    convert_to_llm: ConvertToLlmFn,
    transform_context: Option<TransformContextFn>,
    get_api_key: Option<GetApiKeyFn>,
    before_tool_call: Option<BeforeToolCallFn>,
    after_tool_call: Option<AfterToolCallFn>,
    prepare_next_turn: Option<PrepareNextTurnFn>,
    session_id: Option<String>,
    transport: Option<String>,
    max_retry_delay_ms: Option<u64>,
    tool_execution: ToolExecutionMode,
}

impl Agent {
    pub fn new(options: AgentOptions) -> Self {
        let inner = Inner {
            system_prompt: options.system_prompt,
            model: options.model,
            thinking_level: options.thinking_level,
            tools: options.tools,
            messages: options.messages,
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: HashSet::new(),
            error_message: None,
            listeners: Vec::new(),
            steering_queue: PendingMessageQueue::new(options.steering_mode),
            follow_up_queue: PendingMessageQueue::new(options.follow_up_mode),
            active_run: None,
        };
        Agent {
            inner: Arc::new(Mutex::new(inner)),
            stream_fn: options.stream_fn,
            convert_to_llm: options.convert_to_llm.unwrap_or_else(default_convert_to_llm),
            transform_context: options.transform_context,
            get_api_key: options.get_api_key,
            before_tool_call: options.before_tool_call,
            after_tool_call: options.after_tool_call,
            prepare_next_turn: options.prepare_next_turn,
            session_id: options.session_id,
            transport: options.transport,
            max_retry_delay_ms: options.max_retry_delay_ms,
            tool_execution: options.tool_execution,
        }
    }

    // --- listeners -----------------------------------------------------------

    /// Subscribe to lifecycle events. The returned future, when awaited,
    /// unsubscribes the listener.
    pub async fn subscribe(&self, listener: EventListener) -> Unsubscribe {
        let mut guard = self.inner.lock().await;
        let id = guard.listeners.len();
        guard.listeners.push(listener);
        Unsubscribe {
            inner: self.inner.clone(),
            id,
        }
    }

    // --- state ---------------------------------------------------------------

    pub async fn state(&self) -> AgentState {
        self.inner.lock().await.snapshot()
    }

    pub async fn set_system_prompt(&self, prompt: String) {
        self.inner.lock().await.system_prompt = prompt;
    }

    pub async fn set_model(&self, model: Model) {
        self.inner.lock().await.model = model;
    }

    pub async fn set_thinking_level(&self, level: ThinkingLevel) {
        self.inner.lock().await.thinking_level = level;
    }

    pub async fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        self.inner.lock().await.tools = tools;
    }

    pub async fn set_messages(&self, messages: Vec<AgentMessage>) {
        self.inner.lock().await.messages = messages;
    }

    // --- queue management ----------------------------------------------------

    pub async fn steer(&self, message: AgentMessage) {
        self.inner.lock().await.steering_queue.enqueue(message);
    }

    pub async fn follow_up(&self, message: AgentMessage) {
        self.inner.lock().await.follow_up_queue.enqueue(message);
    }

    pub async fn clear_steering_queue(&self) {
        self.inner.lock().await.steering_queue.clear();
    }

    pub async fn clear_follow_up_queue(&self) {
        self.inner.lock().await.follow_up_queue.clear();
    }

    pub async fn clear_all_queues(&self) {
        let mut g = self.inner.lock().await;
        g.steering_queue.clear();
        g.follow_up_queue.clear();
    }

    pub async fn has_queued_messages(&self) -> bool {
        let g = self.inner.lock().await;
        g.steering_queue.has_items() || g.follow_up_queue.has_items()
    }

    pub async fn set_steering_mode(&self, mode: QueueMode) {
        self.inner.lock().await.steering_queue.mode = mode;
    }

    pub async fn set_follow_up_mode(&self, mode: QueueMode) {
        self.inner.lock().await.follow_up_queue.mode = mode;
    }

    // --- run control ---------------------------------------------------------

    pub async fn abort(&self) {
        if let Some(token) = self.inner.lock().await.active_run.clone() {
            token.cancel();
        }
    }

    pub async fn signal(&self) -> Option<CancellationToken> {
        self.inner.lock().await.active_run.clone()
    }

    /// Clear transcript and runtime state.
    pub async fn reset(&self) {
        let mut g = self.inner.lock().await;
        g.messages.clear();
        g.is_streaming = false;
        g.streaming_message = None;
        g.pending_tool_calls.clear();
        g.error_message = None;
        g.steering_queue.clear();
        g.follow_up_queue.clear();
    }

    // --- prompt / continue ---------------------------------------------------

    /// Start a new prompt from a string. Convenience wrapper.
    pub async fn prompt_text(&self, text: impl Into<String>) -> Result<(), AgentError> {
        let msg = AgentMessage::user(UserMessage {
            content: vec![UserContent::text(text)],
            timestamp: current_time_ms(),
        });
        self.prompt(vec![msg]).await
    }

    /// Start a new prompt with a batch of messages.
    pub async fn prompt(&self, messages: Vec<AgentMessage>) -> Result<(), AgentError> {
        self.run_prompt_messages(messages, false).await
    }

    /// Continue from the current transcript.
    pub async fn continue_(&self) -> Result<(), AgentError> {
        {
            let g = self.inner.lock().await;
            if g.active_run.is_some() {
                return Err(AgentError::AlreadyRunning);
            }
            let last = g
                .messages
                .last()
                .cloned()
                .ok_or(AgentError::NoMessagesToContinue)?;
            if last.role() == "assistant" {
                // Try queued steering first, then follow-ups, then fail.
                drop(g);
                let queued_steer = {
                    let mut g = self.inner.lock().await;
                    g.steering_queue.drain()
                };
                if !queued_steer.is_empty() {
                    return self.run_prompt_messages(queued_steer, true).await;
                }
                let queued_follow = {
                    let mut g = self.inner.lock().await;
                    g.follow_up_queue.drain()
                };
                if !queued_follow.is_empty() {
                    return self.run_prompt_messages(queued_follow, false).await;
                }
                return Err(AgentError::CannotContinueFromAssistant);
            }
        }
        self.run_continuation().await
    }

    // --- internal: run lifecycle --------------------------------------------

    async fn run_prompt_messages(
        &self,
        messages: Vec<AgentMessage>,
        skip_initial_steering_poll: bool,
    ) -> Result<(), AgentError> {
        let cancel = self.begin_run().await?;
        let context = self.snapshot_context().await;
        let config = self.build_loop_config(skip_initial_steering_poll).await;
        let emit = self.make_event_sink(cancel.clone());
        let stream_fn = self.stream_fn.clone();
        let inner = self.inner.clone();

        let result = agent_loop::run_agent_loop(
            messages,
            context,
            config,
            emit,
            cancel.clone(),
            stream_fn,
        )
        .await;

        self.finish_run(inner, result.err(), cancel.is_cancelled()).await;
        Ok(())
    }

    async fn run_continuation(&self) -> Result<(), AgentError> {
        let cancel = self.begin_run().await?;
        let context = self.snapshot_context().await;
        let config = self.build_loop_config(false).await;
        let emit = self.make_event_sink(cancel.clone());
        let stream_fn = self.stream_fn.clone();
        let inner = self.inner.clone();

        let result = agent_loop::run_agent_loop_continue(
            context,
            config,
            emit,
            cancel.clone(),
            stream_fn,
        )
        .await;

        self.finish_run(inner, result.err(), cancel.is_cancelled()).await;
        Ok(())
    }

    async fn begin_run(&self) -> Result<CancellationToken, AgentError> {
        let mut g = self.inner.lock().await;
        if g.active_run.is_some() {
            return Err(AgentError::AlreadyRunning);
        }
        let token = CancellationToken::new();
        g.active_run = Some(token.clone());
        g.is_streaming = true;
        g.streaming_message = None;
        g.error_message = None;
        Ok(token)
    }

    async fn finish_run(
        &self,
        inner: Arc<Mutex<Inner>>,
        loop_error: Option<agent_loop::AgentLoopError>,
        aborted: bool,
    ) {
        if let Some(err) = loop_error {
            // Synthesize a terminal failure message akin to the TS handleRunFailure path.
            let failure = AssistantMessage {
                content: vec![AssistantContent::Text(TextContent::default())],
                api: inner.lock().await.model.api.clone(),
                provider: inner.lock().await.model.provider.clone(),
                model: inner.lock().await.model.id.clone(),
                usage: Usage::default(),
                stop_reason: if aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                },
                error_message: Some(err.to_string()),
                timestamp: current_time_ms(),
            };

            // Emit the same three events the TS implementation emits on failure.
            let listeners_clone = inner.lock().await.listeners.clone();
            let token = inner.lock().await.active_run.clone().unwrap_or_default();
            for ev in [
                AgentEvent::MessageStart {
                    message: AgentMessage::assistant(failure.clone()),
                },
                AgentEvent::MessageEnd {
                    message: AgentMessage::assistant(failure.clone()),
                },
                AgentEvent::TurnEnd {
                    message: failure.clone(),
                    tool_results: Vec::new(),
                },
                AgentEvent::AgentEnd {
                    messages: vec![AgentMessage::assistant(failure.clone())],
                },
            ] {
                for listener in &listeners_clone {
                    listener(ev.clone(), token.clone()).await;
                }
                self.process_event(&ev).await;
            }
        }

        let mut g = inner.lock().await;
        g.is_streaming = false;
        g.streaming_message = None;
        g.pending_tool_calls.clear();
        g.active_run = None;
    }

    async fn snapshot_context(&self) -> AgentContext {
        let g = self.inner.lock().await;
        AgentContext {
            system_prompt: g.system_prompt.clone(),
            messages: g.messages.clone(),
            tools: g.tools.clone(),
        }
    }

    async fn build_loop_config(&self, skip_initial_steering_poll: bool) -> AgentLoopConfig {
        let (thinking_level, _model) = {
            let g = self.inner.lock().await;
            (g.thinking_level, g.model.clone())
        };
        let model = self.inner.lock().await.model.clone();
        let mut stream_options = StreamOptions::default();
        stream_options.session_id = self.session_id.clone();
        stream_options.transport = self.transport.clone();
        stream_options.max_retry_delay_ms = self.max_retry_delay_ms;
        stream_options.reasoning = if thinking_level == ThinkingLevel::Off {
            None
        } else {
            Some(thinking_level)
        };

        let mut config = AgentLoopConfig::new(model, self.convert_to_llm.clone());
        config.thinking_level = thinking_level;
        config.stream_options = stream_options;
        config.tool_execution = self.tool_execution;
        config.transform_context = self.transform_context.clone();
        config.get_api_key = self.get_api_key.clone();
        config.before_tool_call = self.before_tool_call.clone();
        config.after_tool_call = self.after_tool_call.clone();
        config.prepare_next_turn = self.prepare_next_turn.clone();

        // Steering queue provider (with optional initial skip).
        let inner = self.inner.clone();
        let skipped = Arc::new(Mutex::new(skip_initial_steering_poll));
        let steering: MessagesProviderFn = Arc::new(move || {
            let inner = inner.clone();
            let skipped = skipped.clone();
            Box::pin(async move {
                {
                    let mut s = skipped.lock().await;
                    if *s {
                        *s = false;
                        return Vec::new();
                    }
                }
                inner.lock().await.steering_queue.drain()
            })
        });
        config.get_steering_messages = Some(steering);

        // Follow-up queue provider.
        let inner2 = self.inner.clone();
        let follow_up: MessagesProviderFn = Arc::new(move || {
            let inner = inner2.clone();
            Box::pin(async move { inner.lock().await.follow_up_queue.drain() })
        });
        config.get_follow_up_messages = Some(follow_up);

        config
    }

    fn make_event_sink(&self, cancel: CancellationToken) -> EventSink {
        let inner = self.inner.clone();
        Arc::new(move |event: AgentEvent| {
            let inner = inner.clone();
            let cancel = cancel.clone();
            Box::pin(async move {
                process_event_impl(&inner, &event).await;
                let listeners = inner.lock().await.listeners.clone();
                for listener in listeners {
                    listener(event.clone(), cancel.clone()).await;
                }
            })
        })
    }

    async fn process_event(&self, event: &AgentEvent) {
        process_event_impl(&self.inner, event).await;
    }
}

async fn process_event_impl(inner: &Arc<Mutex<Inner>>, event: &AgentEvent) {
    let mut g = inner.lock().await;
    match event {
        AgentEvent::MessageStart { message } => {
            g.streaming_message = Some(message.clone());
        }
        AgentEvent::MessageUpdate { message, .. } => {
            g.streaming_message = Some(AgentMessage::assistant(message.clone()));
        }
        AgentEvent::MessageEnd { message } => {
            g.streaming_message = None;
            g.messages.push(message.clone());
        }
        AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
            g.pending_tool_calls.insert(tool_call_id.clone());
        }
        AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
            g.pending_tool_calls.remove(tool_call_id);
        }
        AgentEvent::TurnEnd { message, .. } => {
            if let Some(err) = &message.error_message {
                g.error_message = Some(err.clone());
            }
        }
        AgentEvent::AgentEnd { .. } => {
            g.streaming_message = None;
        }
        _ => {}
    }
}

fn current_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Unsubscribe handle
// ---------------------------------------------------------------------------

pub struct Unsubscribe {
    inner: Arc<Mutex<Inner>>,
    id: usize,
}

impl Unsubscribe {
    pub async fn cancel(self) {
        let mut g = self.inner.lock().await;
        if self.id < g.listeners.len() {
            g.listeners.remove(self.id);
        }
    }
}

// AfterToolCallResult/BeforeToolCallResult re-exported from agent_loop above.
pub use agent_loop::{
    AfterToolCallContext as AfterCtx, AfterToolCallResult as AfterResult,
    AgentLoopConfig as LoopConfig, AgentLoopTurnUpdate as LoopTurnUpdate,
    BeforeToolCallContext as BeforeCtx, BeforeToolCallResult as BeforeResult,
};
