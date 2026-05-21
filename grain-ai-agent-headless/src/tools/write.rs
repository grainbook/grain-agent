//! `write` tool — create or overwrite a UTF-8 text file inside the workspace.
//!
//! Arguments:
//!
//! ```json
//! { "path": "src/main.rs", "content": "fn main() {}\n" }
//! ```
//!
//! Refuses to write to a path whose canonical parent isn't inside the
//! workspace root. Reports whether the file was newly created vs overwritten,
//! plus the resulting byte / line count.

use std::sync::Arc;

use async_trait::async_trait;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::workspace::Workspace;

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub struct WriteTool {
    def: ToolDefinition,
    workspace: Arc<Workspace>,
}

impl WriteTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        WriteTool {
            def: ToolDefinition {
                name: "write".into(),
                label: "Write".into(),
                description:
                    "Create or overwrite a UTF-8 text file inside the workspace. The parent \
                     directory must already exist. Use the `edit` tool for in-place changes \
                     to large files."
                        .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative or absolute (inside) path to write."
                        },
                        "content": {
                            "type": "string",
                            "description": "Full file contents. Overwrites the file when it exists."
                        }
                    },
                    "required": ["path", "content"]
                }),
                execution_mode: None,
            },
            workspace,
        }
    }
}

#[async_trait]
impl AgentTool for WriteTool {
    fn definition(&self) -> &ToolDefinition {
        &self.def
    }

    async fn execute(
        &self,
        _id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let args: WriteArgs = serde_json::from_value(args)
            .map_err(|e| AgentToolError::Validation(e.to_string()))?;
        let path = self
            .workspace
            .resolve_for_write(&args.path)
            .map_err(|e| AgentToolError::msg(e.to_string()))?;

        let existed = tokio::fs::try_exists(&path).await.unwrap_or(false);

        tokio::fs::write(&path, args.content.as_bytes())
            .await
            .map_err(|e| AgentToolError::msg(format!("write {}: {e}", path.display())))?;

        let bytes = args.content.len();
        let lines = args.content.lines().count();
        let rel = self.workspace.display_relative(&path);
        let action = if existed { "overwrote" } else { "created" };

        Ok(AgentToolResult {
            content: vec![UserContent::text(format!(
                "{action} {rel} ({lines} lines, {bytes} bytes)"
            ))],
            details: serde_json::json!({
                "path": rel,
                "bytes": bytes,
                "lines": lines,
                "created": !existed,
                "overwrote": existed,
            }),
            terminate: None,
        })
    }
}
