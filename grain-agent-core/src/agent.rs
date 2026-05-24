//! Stateful `Agent` wrapper, ported from `packages/agent/src/agent.ts`.

use std::collections::{BTreeMap, HashSet, VecDeque};
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

pub type EventListener =
    Arc<dyn Fn(AgentEvent, CancellationToken) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Default)]
struct PendingMessageQueue {
    // Front-popping queue: `Vec::remove(0)` is O(n), `VecDeque::pop_front` is O(1).
    messages: VecDeque<AgentMessage>,
    mode: QueueMode,
}

impl PendingMessageQueue {
    fn new(mode: QueueMode) -> Self {
        PendingMessageQueue {
            messages: VecDeque::new(),
            mode,
        }
    }
    fn enqueue(&mut self, m: AgentMessage) {
        self.messages.push_back(m);
    }
    fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }
    fn drain(&mut self) -> Vec<AgentMessage> {
        if self.messages.is_empty() {
            return Vec::new();
        }
        match self.mode {
            QueueMode::All => self.messages.drain(..).collect(),
            QueueMode::OneAtATime => self
                .messages
                .pop_front()
                .map(|m| vec![m])
                .unwrap_or_default(),
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

    /// Listeners keyed by a monotonic id. `BTreeMap` preserves subscription
    /// order on iteration (id is monotonic, BTreeMap iterates sorted), which
    /// keeps event delivery deterministic. `HashMap<usize, _>` would also
    /// work but iteration order would be unpredictable.
    listeners: BTreeMap<u64, EventListener>,
    next_listener_id: u64,

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
    /// System prompt prepended to every LLM request.
    pub system_prompt: String,
    /// Default model used when starting a run.
    pub model: Model,
    /// Starting thinking level.
    pub thinking_level: ThinkingLevel,
    /// Tool set available to the agent.
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Seed messages loaded into the transcript on construction.
    pub messages: Vec<AgentMessage>,

    /// Custom projection from `AgentMessage[]` → `Message[]` (default: drop custom messages).
    pub convert_to_llm: Option<ConvertToLlmFn>,
    /// Optional context transform applied before each LLM request.
    pub transform_context: Option<TransformContextFn>,
    /// LLM streaming adapter.
    pub stream_fn: StreamFn,
    /// Dynamic API-key provider.
    pub get_api_key: Option<GetApiKeyFn>,
    /// Pre-tool-execution hook (e.g. storm suppression).
    pub before_tool_call: Option<BeforeToolCallFn>,
    /// Post-tool-execution hook (e.g. result truncation).
    pub after_tool_call: Option<AfterToolCallFn>,
    /// Between-turn hook (e.g. failure-signal escalation).
    pub prepare_next_turn: Option<PrepareNextTurnFn>,

    /// Steering-queue drain policy.
    pub steering_mode: QueueMode,
    /// Follow-up queue drain policy.
    pub follow_up_mode: QueueMode,
    /// Opaque session id forwarded in stream requests.
    pub session_id: Option<String>,
    /// Transport identifier forwarded in stream requests.
    pub transport: Option<String>,
    /// Max retry backoff forwarded in stream requests.
    pub max_retry_delay_ms: Option<u64>,
    /// Tool execution parallelism mode.
    pub tool_execution: ToolExecutionMode,
}

impl AgentOptions {
    /// Minimal options: model + stream function. Everything else
    /// defaults to empty / off.
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
    /// Build an agent from [`AgentOptions`].
    ///
    /// If `convert_to_llm` is not set, installs a default that drops
    /// [`AgentMessage::Custom`] entries and keeps standard LLM messages.
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
            listeners: BTreeMap::new(),
            next_listener_id: 0,
            steering_queue: PendingMessageQueue::new(options.steering_mode),
            follow_up_queue: PendingMessageQueue::new(options.follow_up_mode),
            active_run: None,
        };
        Agent {
            inner: Arc::new(Mutex::new(inner)),
            stream_fn: options.stream_fn,
            convert_to_llm: options
                .convert_to_llm
                .unwrap_or_else(default_convert_to_llm),
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

    /// Subscribe to lifecycle events. The returned handle removes the
    /// listener when `cancel().await`ed. Removal is keyed by a monotonic
    /// id, so concurrent / out-of-order unsubscription doesn't shift other
    /// listeners' identities (the previous `Vec::remove(idx)` approach
    /// silently removed the wrong listener after the first cancellation).
    pub async fn subscribe(&self, listener: EventListener) -> Unsubscribe {
        let mut guard = self.inner.lock().await;
        let id = guard.next_listener_id;
        guard.next_listener_id += 1;
        guard.listeners.insert(id, listener);
        Unsubscribe {
            inner: self.inner.clone(),
            id,
        }
    }

    // --- state ---------------------------------------------------------------

    /// Return a snapshot of the current agent state (model, messages, tools,
    /// streaming status, …). Cheap clone of internally `Arc`-shared fields.
    pub async fn state(&self) -> AgentState {
        self.inner.lock().await.snapshot()
    }

    /// Replace the system prompt for subsequent turns.
    pub async fn set_system_prompt(&self, prompt: String) {
        self.inner.lock().await.system_prompt = prompt;
    }

    /// Switch the model for subsequent turns.
    pub async fn set_model(&self, model: Model) {
        self.inner.lock().await.model = model;
    }

    /// Change the thinking level for subsequent turns.
    pub async fn set_thinking_level(&self, level: ThinkingLevel) {
        self.inner.lock().await.thinking_level = level;
    }

    /// Replace the tool set for subsequent turns.
    pub async fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        self.inner.lock().await.tools = tools;
    }

    /// Replace the full transcript (useful after compaction or branch
    /// switching).
    pub async fn set_messages(&self, messages: Vec<AgentMessage>) {
        self.inner.lock().await.messages = messages;
    }

    // --- queue management ----------------------------------------------------

    /// Enqueue a steering message. When the active turn finishes, the
    /// agent loop will drain the steering queue first and run a follow-up
    /// turn with the queued messages.
    pub async fn steer(&self, message: AgentMessage) {
        self.inner.lock().await.steering_queue.enqueue(message);
    }

    /// Enqueue a follow-up message. Unlike steering messages, follow-up
    /// messages run after the steering queue is drained and only when
    /// the transcript ends on a non-assistant role.
    pub async fn follow_up(&self, message: AgentMessage) {
        self.inner.lock().await.follow_up_queue.enqueue(message);
    }

    /// Discard all pending steering messages.
    pub async fn clear_steering_queue(&self) {
        self.inner.lock().await.steering_queue.clear();
    }

    /// Discard all pending follow-up messages.
    pub async fn clear_follow_up_queue(&self) {
        self.inner.lock().await.follow_up_queue.clear();
    }

    /// Discard both steering and follow-up queues.
    pub async fn clear_all_queues(&self) {
        let mut g = self.inner.lock().await;
        g.steering_queue.clear();
        g.follow_up_queue.clear();
    }

    /// Returns `true` when either queue has at least one pending message.
    pub async fn has_queued_messages(&self) -> bool {
        let g = self.inner.lock().await;
        g.steering_queue.has_items() || g.follow_up_queue.has_items()
    }

    /// Set the steering-queue drain mode.
    pub async fn set_steering_mode(&self, mode: QueueMode) {
        self.inner.lock().await.steering_queue.mode = mode;
    }

    /// Set the follow-up queue drain mode.
    pub async fn set_follow_up_mode(&self, mode: QueueMode) {
        self.inner.lock().await.follow_up_queue.mode = mode;
    }

    // --- run control ---------------------------------------------------------

    /// Cancel the currently active run (if any). Issuing `abort()` causes
    /// an in-progress LLM stream to be cancelled via the shared
    /// [`CancellationToken`].
    pub async fn abort(&self) {
        if let Some(token) = self.inner.lock().await.active_run.clone() {
            token.cancel();
        }
    }

    /// Return a clone of the active run's cancellation token, or `None`
    /// when the agent is idle. Can be used by external code to tie their
    /// own cleanup to the agent lifecycle.
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

    /// Start a new prompt from a plain-text string. Convenience wrapper that
    /// builds a single-turn [`UserMessage`] and calls [`Self::prompt`].
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::AlreadyRunning`] when a run is already in
    /// progress.
    pub async fn prompt_text(&self, text: impl Into<String>) -> Result<(), AgentError> {
        let msg = AgentMessage::user(UserMessage {
            content: vec![UserContent::text(text)],
            timestamp: current_time_ms(),
        });
        self.prompt(vec![msg]).await
    }

    /// Start a new prompt with a batch of [`AgentMessage`]s.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::AlreadyRunning`] when a run is already in
    /// progress.
    pub async fn prompt(&self, messages: Vec<AgentMessage>) -> Result<(), AgentError> {
        self.run_prompt_messages(messages, false).await
    }

    /// Continue from the current transcript. Behaves like the TS reference
    /// implementation:
    ///
    /// - If the last message is `assistant`: drain steering queue first,
    ///   then follow-up queue, else return
    ///   [`AgentError::CannotContinueFromAssistant`].
    /// - Otherwise resume from the transcript tail.
    ///
    /// # Errors
    ///
    /// - [`AgentError::AlreadyRunning`] — a run is active.
    /// - [`AgentError::NoMessagesToContinue`] — transcript is empty.
    /// - [`AgentError::CannotContinueFromAssistant`] — last message is
    ///   assistant and both queues are empty.
    pub async fn continue_(&self) -> Result<(), AgentError> {
        // Decide what to do atomically under one lock so concurrent callers
        // can't sneak between the active-run guard and the queue drain.
        enum ContinueAction {
            ResumeFromSteering(Vec<AgentMessage>),
            ResumeFromFollowUp(Vec<AgentMessage>),
            FromTranscript,
        }
        let action = {
            let mut g = self.inner.lock().await;
            if g.active_run.is_some() {
                return Err(AgentError::AlreadyRunning);
            }
            let last = g
                .messages
                .last()
                .cloned()
                .ok_or(AgentError::NoMessagesToContinue)?;
            if last.role() == "assistant" {
                let queued_steer = g.steering_queue.drain();
                if !queued_steer.is_empty() {
                    ContinueAction::ResumeFromSteering(queued_steer)
                } else {
                    let queued_follow = g.follow_up_queue.drain();
                    if !queued_follow.is_empty() {
                        ContinueAction::ResumeFromFollowUp(queued_follow)
                    } else {
                        return Err(AgentError::CannotContinueFromAssistant);
                    }
                }
            } else {
                ContinueAction::FromTranscript
            }
        };
        match action {
            ContinueAction::ResumeFromSteering(msgs) => self.run_prompt_messages(msgs, true).await,
            ContinueAction::ResumeFromFollowUp(msgs) => self.run_prompt_messages(msgs, false).await,
            ContinueAction::FromTranscript => self.run_continuation().await,
        }
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

        let result = agent_loop::run_agent_loop_with_result(
            messages,
            context,
            config,
            emit,
            cancel.clone(),
            stream_fn,
        )
        .await;

        let (loop_error, final_context) = match result {
            Ok(result) => (None, Some(result.context)),
            Err(err) => (Some(err), None),
        };
        self.finish_run(inner, loop_error, final_context, cancel.is_cancelled())
            .await;
        Ok(())
    }

    async fn run_continuation(&self) -> Result<(), AgentError> {
        let cancel = self.begin_run().await?;
        let context = self.snapshot_context().await;
        let config = self.build_loop_config(false).await;
        let emit = self.make_event_sink(cancel.clone());
        let stream_fn = self.stream_fn.clone();
        let inner = self.inner.clone();

        let result = agent_loop::run_agent_loop_continue_with_result(
            context,
            config,
            emit,
            cancel.clone(),
            stream_fn,
        )
        .await;

        let (loop_error, final_context) = match result {
            Ok(result) => (None, Some(result.context)),
            Err(err) => (Some(err), None),
        };
        self.finish_run(inner, loop_error, final_context, cancel.is_cancelled())
            .await;
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
        final_context: Option<AgentContext>,
        aborted: bool,
    ) {
        if let Some(err) = loop_error {
            // Snapshot every field we need under a single lock so concurrent
            // setters (e.g. `set_model`, `subscribe`, `abort`) can't splice
            // mismatched parts into the synthetic failure message or pick up
            // a different listener set partway through emission.
            let (failure, listeners_clone, token) = {
                let g = inner.lock().await;
                let failure = AssistantMessage {
                    content: vec![AssistantContent::Text(TextContent::default())],
                    api: g.model.api.clone(),
                    provider: g.model.provider.clone(),
                    model: g.model.id.clone(),
                    usage: Usage::default(),
                    stop_reason: if aborted {
                        StopReason::Aborted
                    } else {
                        StopReason::Error
                    },
                    error_message: Some(err.to_string()),
                    timestamp: current_time_ms(),
                };
                let listeners_clone: Vec<EventListener> = g.listeners.values().cloned().collect();
                let token = g.active_run.clone().unwrap_or_default();
                (failure, listeners_clone, token)
            };

            // Emit the same three events the TS implementation emits on failure.
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
        if let Some(context) = final_context {
            g.messages = context.messages;
        }
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
        let (thinking_level, model) = {
            let g = self.inner.lock().await;
            (g.thinking_level, g.model.clone())
        };
        let stream_options = StreamOptions {
            session_id: self.session_id.clone(),
            transport: self.transport.clone(),
            max_retry_delay_ms: self.max_retry_delay_ms,
            reasoning: if thinking_level == ThinkingLevel::Off {
                None
            } else {
                Some(thinking_level)
            },
            ..StreamOptions::default()
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
                // BTreeMap iterates in key order, so listener delivery follows
                // subscription order regardless of intervening unsubscribes.
                let listeners: Vec<EventListener> =
                    inner.lock().await.listeners.values().cloned().collect();
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
    id: u64,
}

impl Unsubscribe {
    /// Remove the listener identified by this handle. Safe to call
    /// concurrently or after other listeners have been added/removed;
    /// removal is keyed by a monotonic id so the wrong listener is never
    /// accidentally dropped.
    pub async fn cancel(self) {
        self.inner.lock().await.listeners.remove(&self.id);
    }
}

// AfterToolCallResult/BeforeToolCallResult re-exported from agent_loop above.
pub use agent_loop::{
    AfterToolCallContext as AfterCtx, AfterToolCallResult as AfterResult,
    AgentLoopConfig as LoopConfig, AgentLoopTurnUpdate as LoopTurnUpdate,
    BeforeToolCallContext as BeforeCtx, BeforeToolCallResult as BeforeResult,
};
