//! `grain-agent-core` — Rust port of `@earendil-works/pi-agent-core`.
//!
//! Stateful agent framework that manages tool execution and event streaming
//! around a pluggable LLM provider.
//!
//! - [`types`]: messages, tools, events, state primitives.
//! - [`stream`]: [`LlmStream`](stream::LlmStream) trait — the injection point
//!   for any LLM provider adapter (counterpart to the TypeScript `streamFn`).
//! - [`agent_loop`]: low-level [`run_agent_loop`](agent_loop::run_agent_loop)
//!   and [`run_agent_loop_continue`](agent_loop::run_agent_loop_continue),
//!   parallel/sequential tool execution, before/after-tool hooks, steering
//!   and follow-up queue drainage, prepare-next-turn and should-stop hooks.
//! - [`agent`]: high-level [`Agent`](agent::Agent) wrapping the loop with
//!   subscribe/abort/steer/follow-up APIs.
//!
//! See `packages/agent` in <https://github.com/earendil-works/pi> for the
//! reference TypeScript implementation this crate mirrors.

pub mod agent;
pub mod agent_loop;
pub mod stream;
pub mod types;

pub use agent::{Agent, AgentError, AgentOptions, EventListener, Unsubscribe};
pub use agent_loop::{
    AfterToolCallContext, AfterToolCallFn, AfterToolCallResult, AgentLoopConfig,
    AgentLoopError, AgentLoopTurnUpdate, BeforeToolCallContext, BeforeToolCallFn,
    BeforeToolCallResult, ConvertToLlmFn, EventSink, GetApiKeyFn, MessagesProviderFn,
    PrepareNextTurnContext, PrepareNextTurnFn, ShouldStopAfterTurnContext,
    ShouldStopAfterTurnFn, TransformContextFn, run_agent_loop, run_agent_loop_continue,
};
pub use stream::{AssistantStream, LlmStream, StreamError, StreamFn, StreamOptions};
pub use types::{
    AgentContext, AgentEvent, AgentMessage, AgentState, AgentTool, AgentToolError,
    AgentToolResult, AssistantContent, AssistantMessage, AssistantMessageEvent, Cost,
    ImageContent, LlmContext, Message, Model, QueueMode, StopReason, TextContent,
    ThinkingContent, ThinkingLevel, ToolCall, ToolDefinition, ToolExecutionMode,
    ToolResultMessage, ToolUpdateCallback, Usage, UserContent, UserMessage,
};
