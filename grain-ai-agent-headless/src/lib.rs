//! `grain-ai-agent-headless` — building blocks for a headless coding-agent.
//!
//! Plays the role of `@earendil-works/pi/packages/coding-agent` but with no UI
//! layer and no SDK surface — what we ship here are reusable pieces other
//! binaries can compose.
//!
//! ## v1 surface
//!
//! - [`Workspace`] — root-anchored path validator. Every file tool resolves
//!   user-supplied paths through it and refuses anything that escapes the
//!   workspace root.
//! - [`tools`] — read-only filesystem tools that implement
//!   [`grain_agent_core::AgentTool`]:
//!   - [`ReadTool`] — read a UTF-8 text file with optional line-range trim.
//!   - [`ListTool`] — list a directory's immediate entries.
//!   - [`GlobTool`] — gitignore-aware glob search.
//!   - [`GrepTool`] — gitignore-aware regex search across files.
//! - [`runtime::coding_read_tools`] — convenience that returns all four
//!   tools as `Vec<Arc<dyn AgentTool>>` ready to drop into
//!   [`grain_agent_core::AgentOptions::tools`].
//! - [`prompt::DEFAULT_CODING_AGENT_SYSTEM_PROMPT`] — recommended base
//!   system prompt for coding-agent flows.
//!
//! ## Not yet
//!
//! - Write tools (Write / Edit), shell exec (Bash) — separate PR.
//! - CLI driver (single-prompt / interactive) — separate PR.
//! - rig-backed semantic search (`SemanticSearch` tool) — separate PR.

pub mod cli;
pub mod prompt;
pub mod runtime;
pub mod tools;
pub mod workspace;

pub use cli::{Args, EventPrinter, OpenAiCompatChoice, run};
pub use prompt::DEFAULT_CODING_AGENT_SYSTEM_PROMPT;
pub use runtime::coding_read_tools;
pub use tools::{GlobTool, GrepTool, ListTool, ReadTool};
pub use workspace::{Workspace, WorkspaceError};
