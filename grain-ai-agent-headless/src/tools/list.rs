//! `list` tool — list a directory's immediate entries.
//!
//! Arguments:
//!
//! ```json
//! { "path": "src" }
//! ```
//!
//! Output: one entry per line, sorted with directories first (each suffixed
//! with `/`). Hidden entries (`.`-prefixed) are included so the model can
//! see `.gitignore`, `.github`, etc. — coding agents rely on those.

use std::sync::Arc;

use async_trait::async_trait;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::workspace::Workspace;

#[derive(Debug, Deserialize)]
struct ListArgs {
    #[serde(default = "default_path")]
    path: String,
}

fn default_path() -> String {
    ".".into()
}

pub struct ListTool {
    def: ToolDefinition,
    workspace: Arc<Workspace>,
}

impl ListTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        ListTool {
            def: ToolDefinition {
                name: "list".into(),
                label: "List".into(),
                description:
                    "List the immediate entries of a directory inside the workspace. \
                     Directories are suffixed with `/` and sorted ahead of files."
                        .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path relative to the workspace root (defaults to the root)."
                        }
                    }
                }),
                execution_mode: None,
            },
            workspace,
        }
    }
}

#[async_trait]
impl AgentTool for ListTool {
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
        let args: ListArgs = serde_json::from_value(args)
            .map_err(|e| AgentToolError::Validation(e.to_string()))?;
        let path = self
            .workspace
            .resolve(&args.path)
            .map_err(|e| AgentToolError::msg(e.to_string()))?;

        if !path.is_dir() {
            return Err(AgentToolError::msg(format!(
                "not a directory: {}",
                self.workspace.display_relative(&path)
            )));
        }

        let mut entries = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| AgentToolError::msg(format!("read_dir {}: {e}", path.display())))?;

        let mut dirs: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| AgentToolError::msg(e.to_string()))?
        {
            let file_type = entry.file_type().await.map_err(|e| {
                AgentToolError::msg(format!("file_type {}: {e}", entry.path().display()))
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if file_type.is_dir() {
                dirs.push(format!("{name}/"));
            } else {
                files.push(name);
            }
        }
        dirs.sort();
        files.sort();

        let mut rendered = String::new();
        for name in dirs.iter().chain(files.iter()) {
            rendered.push_str(name);
            rendered.push('\n');
        }
        if rendered.is_empty() {
            rendered.push_str("(empty)\n");
        }

        Ok(AgentToolResult {
            content: vec![UserContent::text(rendered)],
            details: serde_json::json!({
                "path": self.workspace.display_relative(&path),
                "directories": dirs.len(),
                "files": files.len(),
            }),
            terminate: None,
        })
    }
}
