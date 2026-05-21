//! Boa-backed scripting layer.
//!
//! Users drop `.js` files into a scripts directory; the crate spawns a
//! dedicated worker thread that owns a `boa_engine::Context` and
//! evaluates the scripts at startup. Scripts call
//! `grain.register_tool({ name, description, schema, run })` to expose
//! JS functions to the agent as [`grain_agent_core::AgentTool`]s.
//!
//! ## Design constraints
//!
//! `boa_engine::Context` is `!Send` — its realm, GC, and shape cache
//! all live on a single thread. So we can't share the context across
//! tokio tasks. Instead the worker is a `std::thread` that owns the
//! context, and the parent talks to it via a `std::sync::mpsc` channel.
//! Each `ScriptedTool::execute` posts an `InvokeTool` command and
//! `await`s the reply through a tokio oneshot — so the worker thread
//! stays single-threaded while the agent loop keeps its async shape.
//!
//! ## What's in v1
//!
//! - Load every `*.js` file from a directory at startup.
//! - `grain.register_tool({...})` to register a synchronous tool.
//! - Tool args arrive as a JS object (from the agent's `tool_call.args`
//!   JSON); the JS `run` function returns a string OR an object with
//!   `{ content, is_error? }`.
//! - End-to-end test exercises registration + execution.
//!
//! ## Deferred (next patch)
//!
//! - File-watcher hot reload via `notify`.
//! - Slash command registration (`grain.register_slash(...)`).
//! - `BeforeToolCallFn` / `AfterToolCallFn` hooks.
//! - Async JS host functions.
//! - Richer `grain` host API (file read, http, log).

pub mod extension;
pub mod worker;

pub use extension::{BoaExtension, BoaExtensionError};
