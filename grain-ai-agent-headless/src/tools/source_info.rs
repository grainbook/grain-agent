//! `source_info` tool — exposes the workspace's git state to the LLM.
//!
//! Arguments: none. Returns branch, commit, dirty-file list. Cheaper for the
//! agent than running multiple `bash git ...` calls (and doesn't require
//! `--allow-bash`).

use std::sync::Arc;

use async_trait::async_trait;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use tokio_util::sync::CancellationToken;

use crate::diagnostics::render_source_info_block;
use crate::workspace::Workspace;

pub struct SourceInfoTool {
    def: ToolDefinition,
    workspace: Arc<Workspace>,
}

impl SourceInfoTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        SourceInfoTool {
            def: ToolDefinition {
                name: "source_info".into(),
                label: "Source Info".into(),
                description:
                    "Show the workspace's git source info: branch, short commit, and a list \
                     of modified files. Read-only — no `--allow-bash` required. Returns a \
                     friendly message when the workspace isn't a git repo."
                        .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
                execution_mode: None,
            },
            workspace,
        }
    }
}

#[async_trait]
impl AgentTool for SourceInfoTool {
    fn definition(&self) -> &ToolDefinition {
        &self.def
    }

    async fn execute(
        &self,
        _id: &str,
        _args: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let body = render_source_info_block(self.workspace.root(), 0);
        Ok(AgentToolResult {
            content: vec![UserContent::text(body)],
            details: serde_json::json!({}),
            terminate: None,
        })
    }
}
