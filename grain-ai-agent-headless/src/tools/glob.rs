//! `glob` tool — gitignore-aware glob search.
//!
//! Backed by the `ignore` crate (the `ripgrep` walker). Respects
//! `.gitignore` / `.ignore` / `.git/info/exclude` like rg does.
//!
//! Arguments:
//!
//! ```json
//! {
//!   "pattern": "src/**/*.rs",
//!   "root": ".",                 // optional, defaults to workspace root
//!   "limit": 1000                // optional, defaults to 1000
//! }
//! ```
//!
//! Results are sorted alphabetically (the underlying walk order is unstable).
//! Output is one matching path per line, workspace-relative.

use std::sync::Arc;

use async_trait::async_trait;
use globset::Glob;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use ignore::WalkBuilder;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::workspace::Workspace;

const DEFAULT_LIMIT: usize = 1000;

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct GlobTool {
    def: ToolDefinition,
    workspace: Arc<Workspace>,
}

impl GlobTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        GlobTool {
            def: ToolDefinition {
                name: "glob".into(),
                label: "Glob".into(),
                description:
                    "Find files in the workspace by glob pattern. Honors .gitignore. Returns \
                     workspace-relative paths sorted alphabetically."
                        .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern, e.g. \"src/**/*.rs\" or \"docs/*.md\"."
                        },
                        "root": {
                            "type": "string",
                            "description": "Subdirectory to search under (defaults to workspace root)."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results (default 1000)."
                        }
                    },
                    "required": ["pattern"]
                }),
                execution_mode: None,
            },
            workspace,
        }
    }
}

#[async_trait]
impl AgentTool for GlobTool {
    fn definition(&self) -> &ToolDefinition {
        &self.def
    }

    async fn execute(
        &self,
        _id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let args: GlobArgs =
            serde_json::from_value(args).map_err(|e| AgentToolError::Validation(e.to_string()))?;
        let root_path = self
            .workspace
            .resolve(args.root.as_deref().unwrap_or("."))
            .map_err(|e| AgentToolError::msg(e.to_string()))?;
        let glob = Glob::new(&args.pattern)
            .map_err(|e| AgentToolError::Validation(format!("invalid glob: {e}")))?
            .compile_matcher();
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
        let workspace = self.workspace.clone();

        // `ignore`'s walker is sync; run it on a blocking thread.
        let results = tokio::task::spawn_blocking(move || -> Result<Vec<String>, String> {
            let mut hits: Vec<String> = Vec::new();
            for entry in WalkBuilder::new(&root_path).build() {
                if cancel.is_cancelled() {
                    return Ok(hits);
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if entry.file_type().is_some_and(|t| t.is_dir()) {
                    continue;
                }
                let rel = workspace.display_relative(entry.path());
                if glob.is_match(&rel) {
                    hits.push(rel);
                    if hits.len() >= limit {
                        break;
                    }
                }
            }
            hits.sort();
            Ok(hits)
        })
        .await
        .map_err(|e| AgentToolError::msg(format!("walker task: {e}")))?
        .map_err(AgentToolError::Message)?;

        let total = results.len();
        let body = if results.is_empty() {
            "(no matches)\n".to_string()
        } else {
            let mut s = results.join("\n");
            s.push('\n');
            s
        };

        Ok(AgentToolResult {
            content: vec![UserContent::text(body)],
            details: serde_json::json!({
                "pattern": args.pattern,
                "matches": total,
                "limit": limit,
                "truncated": total >= limit,
            }),
            terminate: None,
        })
    }
}
