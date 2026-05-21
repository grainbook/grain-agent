//! Runtime helpers for wiring tools into `AgentOptions`.
//!
//! Two named bundles (read-only / write) plus a combined helper. The CLI
//! defaults to read-only; opting into write requires explicit `--allow-write`.

use std::sync::Arc;

use grain_agent_core::AgentTool;

use crate::tools::{EditTool, GlobTool, GrepTool, ListTool, ReadTool, WriteTool};
use crate::workspace::Workspace;

/// Read-only filesystem tools: Read / List / Glob / Grep.
pub fn coding_read_tools(workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(ReadTool::new(workspace.clone())),
        Arc::new(ListTool::new(workspace.clone())),
        Arc::new(GlobTool::new(workspace.clone())),
        Arc::new(GrepTool::new(workspace)),
    ]
}

/// Write tools: Write (create / overwrite) + Edit (search-and-replace).
pub fn coding_write_tools(workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(WriteTool::new(workspace.clone())),
        Arc::new(EditTool::new(workspace)),
    ]
}

/// Read + write tools combined — drop into `AgentOptions::tools` for a
/// fully-equipped coding agent.
pub fn coding_all_tools(workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
    let mut v = coding_read_tools(workspace.clone());
    v.extend(coding_write_tools(workspace));
    v
}
