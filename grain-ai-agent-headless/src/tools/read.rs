//! `read` tool — read a UTF-8 text file with optional line-range trim.
//!
//! Mirrors the pi coding-agent's Read tool. Arguments:
//!
//! ```json
//! {
//!   "path": "src/main.rs",            // workspace-relative or absolute (inside)
//!   "offset": 0,                       // optional: lines to skip from start
//!   "limit": 200                       // optional: max lines to return
//! }
//! ```
//!
//! Default limit is [`grain_agent_harness::DEFAULT_MAX_LINES`] (2000) /
//! [`grain_agent_harness::DEFAULT_MAX_BYTES`] (50 KiB). Output is suffixed
//! with `[Truncated: kept N of M lines (X); Y remaining]` when the file
//! exceeds the budget.

use std::sync::Arc;

use async_trait::async_trait;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use grain_agent_harness::{TruncationOptions, format_size, truncate_head};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::workspace::Workspace;

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct ReadTool {
    def: ToolDefinition,
    workspace: Arc<Workspace>,
}

impl ReadTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        ReadTool {
            def: ToolDefinition {
                name: "read".into(),
                label: "Read".into(),
                description:
                    "Read a UTF-8 text file from the workspace. Supports optional line-offset \
                     and line-limit for large files."
                        .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path relative to the workspace root, or absolute inside the workspace."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Number of leading lines to skip (default 0)."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum lines to return (default 2000)."
                        }
                    },
                    "required": ["path"]
                }),
                execution_mode: None,
            },
            workspace,
        }
    }
}

#[async_trait]
impl AgentTool for ReadTool {
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
        let args: ReadArgs =
            serde_json::from_value(args).map_err(|e| AgentToolError::Validation(e.to_string()))?;
        let path = self
            .workspace
            .resolve(&args.path)
            .map_err(|e| AgentToolError::msg(e.to_string()))?;

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| AgentToolError::msg(format!("read {}: {e}", path.display())))?;

        let offset = args.offset.unwrap_or(0);
        let trimmed: String = if offset == 0 {
            content
        } else {
            content.lines().skip(offset).collect::<Vec<_>>().join("\n")
        };

        let opts = TruncationOptions {
            max_lines: args.limit,
            max_bytes: None,
        };
        let result = truncate_head(&trimmed, opts);

        let mut text = result.content;
        if result.truncated {
            let remaining_bytes = result.total_bytes.saturating_sub(result.output_bytes);
            text.push_str(&format!(
                "\n\n[Truncated: kept {} of {} lines ({}); {} remaining]",
                result.output_lines,
                result.total_lines,
                format_size(result.output_bytes),
                format_size(remaining_bytes),
            ));
        }

        Ok(AgentToolResult {
            content: vec![UserContent::text(text)],
            details: serde_json::json!({
                "path": self.workspace.display_relative(&path),
                "lines": result.output_lines,
                "totalLines": result.total_lines,
                "bytes": result.output_bytes,
                "totalBytes": result.total_bytes,
                "truncated": result.truncated,
                "offset": offset,
            }),
            terminate: None,
        })
    }
}
