//! `edit` tool — in-place search-and-replace on an existing file.
//!
//! Arguments:
//!
//! ```json
//! {
//!   "path": "src/lib.rs",
//!   "old": "fn foo()",
//!   "new": "fn bar()",
//!   "expected_occurrences": 1
//! }
//! ```
//!
//! Behavior:
//! - File must already exist (use `write` to create new files).
//! - `old` must appear exactly `expected_occurrences` times (default 1).
//!   The whole edit fails if the count doesn't match — saves you from
//!   "thought I changed one, changed three".
//! - Plain string replace, not regex. Use multi-line `old` / `new` for
//!   structural edits.
//! - Refuses no-op edits (`old == new`).

use std::sync::Arc;

use async_trait::async_trait;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::workspace::Workspace;

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    old: String,
    new: String,
    #[serde(default)]
    expected_occurrences: Option<usize>,
}

pub struct EditTool {
    def: ToolDefinition,
    workspace: Arc<Workspace>,
}

impl EditTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        EditTool {
            def: ToolDefinition {
                name: "edit".into(),
                label: "Edit".into(),
                description:
                    "In-place search-and-replace on an existing UTF-8 file. Plain string match \
                     (not regex). Fails loudly if `old` doesn't appear `expected_occurrences` \
                     times (default 1) — prevents silent over-edits."
                        .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative or absolute (inside) path to an existing file."
                        },
                        "old": {
                            "type": "string",
                            "description": "Exact substring to replace. May span multiple lines."
                        },
                        "new": {
                            "type": "string",
                            "description": "Replacement text. Must differ from `old`."
                        },
                        "expected_occurrences": {
                            "type": "integer",
                            "description": "Required number of occurrences of `old`. Defaults to 1."
                        }
                    },
                    "required": ["path", "old", "new"]
                }),
                execution_mode: None,
            },
            workspace,
        }
    }
}

#[async_trait]
impl AgentTool for EditTool {
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
        let args: EditArgs = serde_json::from_value(args)
            .map_err(|e| AgentToolError::Validation(e.to_string()))?;

        if args.old == args.new {
            return Err(AgentToolError::Validation(
                "old == new: refusing no-op edit".into(),
            ));
        }
        if args.old.is_empty() {
            return Err(AgentToolError::Validation(
                "old must be non-empty".into(),
            ));
        }

        let path = self
            .workspace
            .resolve(&args.path)
            .map_err(|e| AgentToolError::msg(e.to_string()))?;

        let original = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| AgentToolError::msg(format!("read {}: {e}", path.display())))?;

        let expected = args.expected_occurrences.unwrap_or(1);
        let actual = original.matches(&args.old).count();
        if actual != expected {
            return Err(AgentToolError::msg(format!(
                "expected {expected} occurrence(s) of `old` in {}, found {actual}",
                self.workspace.display_relative(&path)
            )));
        }

        let updated = original.replace(&args.old, &args.new);

        tokio::fs::write(&path, updated.as_bytes())
            .await
            .map_err(|e| AgentToolError::msg(format!("write {}: {e}", path.display())))?;

        let rel = self.workspace.display_relative(&path);
        let before_bytes = original.len();
        let after_bytes = updated.len();
        let bytes_delta = after_bytes as i64 - before_bytes as i64;

        Ok(AgentToolResult {
            content: vec![UserContent::text(format!(
                "edited {rel}: {expected} replacement(s), {bytes_delta:+} bytes"
            ))],
            details: serde_json::json!({
                "path": rel,
                "replacements": expected,
                "bytesBefore": before_bytes,
                "bytesAfter": after_bytes,
                "bytesDelta": bytes_delta,
            }),
            terminate: None,
        })
    }
}
