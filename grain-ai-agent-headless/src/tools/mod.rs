//! Read-only filesystem tools.
//!
//! Each tool implements [`grain_agent_core::AgentTool`] and resolves user
//! paths through a shared [`crate::Workspace`] so the agent never reads
//! outside the workspace root.

pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod list;
pub mod read;
pub mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list::ListTool;
pub use read::ReadTool;
pub use write::WriteTool;
