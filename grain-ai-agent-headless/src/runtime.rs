//! Runtime helpers for wiring the read-only tool set into `AgentOptions`.
//!
//! Future expansion: write tools, shell tool, CLI driver. For v1 we only
//! ship the read-only register-all helper.

use std::sync::Arc;

use grain_agent_core::AgentTool;

use crate::tools::{GlobTool, GrepTool, ListTool, ReadTool};
use crate::workspace::Workspace;

/// Return the four read-only filesystem tools (Read / List / Glob / Grep)
/// constructed against `workspace`. Drop into
/// [`grain_agent_core::AgentOptions::tools`].
pub fn coding_read_tools(workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(ReadTool::new(workspace.clone())),
        Arc::new(ListTool::new(workspace.clone())),
        Arc::new(GlobTool::new(workspace.clone())),
        Arc::new(GrepTool::new(workspace)),
    ]
}
