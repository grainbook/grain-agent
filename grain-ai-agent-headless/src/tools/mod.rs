//! Read-only filesystem tools.
//!
//! Each tool implements [`grain_agent_core::AgentTool`] and resolves user
//! paths through a shared [`crate::Workspace`] so the agent never reads
//! outside the workspace root.

pub mod glob;
pub mod grep;
pub mod list;
pub mod read;

pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list::ListTool;
pub use read::ReadTool;
