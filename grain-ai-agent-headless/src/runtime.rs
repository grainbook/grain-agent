//! Runtime helpers for wiring tools into `AgentOptions`.
//!
//! Three building blocks (read / write / bash) plus a `coding_full_tools`
//! convenience that joins all three. The CLI defaults to read-only;
//! `--allow-write` enables Write+Edit, `--allow-bash` enables shell. Both
//! flags are off by default.

use std::sync::Arc;

use grain_agent_core::AgentTool;

use crate::tools::{
    BashTool, EditTool, GlobTool, GrepTool, ListTool, ReadTool, SourceInfoTool, WriteTool,
};
use crate::workspace::Workspace;

/// Read-only filesystem tools: Read / List / Glob / Grep / SourceInfo.
pub fn coding_read_tools(workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(ReadTool::new(workspace.clone())),
        Arc::new(ListTool::new(workspace.clone())),
        Arc::new(GlobTool::new(workspace.clone())),
        Arc::new(GrepTool::new(workspace.clone())),
        Arc::new(SourceInfoTool::new(workspace)),
    ]
}

/// Write tools: Write (create / overwrite) + Edit (search-and-replace).
pub fn coding_write_tools(workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(WriteTool::new(workspace.clone())),
        Arc::new(EditTool::new(workspace)),
    ]
}

/// Shell tool: Bash (`/bin/sh -c` with workspace-anchored cwd).
pub fn coding_bash_tools(workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
    vec![Arc::new(BashTool::new(workspace))]
}

/// Read + Write tools combined.
pub fn coding_all_tools(workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
    let mut v = coding_read_tools(workspace.clone());
    v.extend(coding_write_tools(workspace));
    v
}

/// Read + Write + Bash — every tool this crate ships.
pub fn coding_full_tools(workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
    let mut v = coding_all_tools(workspace.clone());
    v.extend(coding_bash_tools(workspace));
    v
}
