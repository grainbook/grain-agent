//! `grep` tool — gitignore-aware regex search across files.
//!
//! Arguments:
//!
//! ```json
//! {
//!   "pattern": "TODO",                    // regex (RE2-ish via the `regex` crate)
//!   "root": "src",                          // optional sub-tree (defaults to workspace root)
//!   "file_glob": "*.rs",                    // optional file-name filter
//!   "max_matches": 200,                     // optional per-file cap (default 200)
//!   "max_total": 1000                       // optional overall cap (default 1000)
//! }
//! ```
//!
//! Output format: one match per line, `path:line:column: text` with text
//! truncated at [`grain_agent_harness::GREP_MAX_LINE_LENGTH`] characters.

use std::io::{BufRead, BufReader};
use std::sync::Arc;

use async_trait::async_trait;
use globset::Glob;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use grain_agent_harness::truncate_line;
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::workspace::Workspace;

const DEFAULT_PER_FILE: usize = 200;
const DEFAULT_TOTAL: usize = 1000;

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    file_glob: Option<String>,
    #[serde(default)]
    max_matches: Option<usize>,
    #[serde(default)]
    max_total: Option<usize>,
}

pub struct GrepTool {
    def: ToolDefinition,
    workspace: Arc<Workspace>,
}

impl GrepTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        GrepTool {
            def: ToolDefinition {
                name: "grep".into(),
                label: "Grep".into(),
                description:
                    "Regex search across files in the workspace. Honors .gitignore. \
                     Supports an optional file-name glob filter."
                        .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regular expression (Rust `regex` crate syntax)."
                        },
                        "root": {
                            "type": "string",
                            "description": "Sub-directory to search (defaults to workspace root)."
                        },
                        "file_glob": {
                            "type": "string",
                            "description": "Glob filter for filenames, e.g. \"*.rs\"."
                        },
                        "max_matches": {
                            "type": "integer",
                            "description": "Max matches per file (default 200)."
                        },
                        "max_total": {
                            "type": "integer",
                            "description": "Max total matches across all files (default 1000)."
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
impl AgentTool for GrepTool {
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
        let args: GrepArgs = serde_json::from_value(args)
            .map_err(|e| AgentToolError::Validation(e.to_string()))?;
        let root_path = self
            .workspace
            .resolve(args.root.as_deref().unwrap_or("."))
            .map_err(|e| AgentToolError::msg(e.to_string()))?;
        let regex = Regex::new(&args.pattern)
            .map_err(|e| AgentToolError::Validation(format!("invalid regex: {e}")))?;
        let file_matcher = args
            .file_glob
            .as_deref()
            .map(|p| {
                Glob::new(p)
                    .map(|g| g.compile_matcher())
                    .map_err(|e| AgentToolError::Validation(format!("invalid file_glob: {e}")))
            })
            .transpose()?;
        let per_file = args.max_matches.unwrap_or(DEFAULT_PER_FILE);
        let total_cap = args.max_total.unwrap_or(DEFAULT_TOTAL);

        let workspace = self.workspace.clone();
        let pattern_for_details = args.pattern.clone();

        // ignore::Walk + regex::Regex are sync; offload.
        let (lines, hit_count, files_with_hits, truncated) =
            tokio::task::spawn_blocking(move || -> Result<(Vec<String>, usize, usize, bool), String> {
                let mut lines: Vec<String> = Vec::new();
                let mut total = 0usize;
                let mut files_with_hits = 0usize;
                let mut truncated = false;

                for entry in WalkBuilder::new(&root_path).build() {
                    if cancel.is_cancelled() {
                        truncated = true;
                        break;
                    }
                    let entry = match entry {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    if entry.file_type().is_some_and(|t| t.is_dir()) {
                        continue;
                    }
                    let rel_path = workspace.display_relative(entry.path());
                    if let Some(m) = &file_matcher
                        && !m.is_match(&rel_path)
                    {
                        continue;
                    }
                    let file = match std::fs::File::open(entry.path()) {
                        Ok(f) => f,
                        Err(_) => continue,
                    };
                    let reader = BufReader::new(file);
                    let mut per_file_hits = 0usize;
                    let mut file_has_hits = false;
                    for (idx, line) in reader.lines().enumerate() {
                        let line = match line {
                            Ok(l) => l,
                            Err(_) => break, // binary file or invalid UTF-8 — skip rest
                        };
                        if let Some(m) = regex.find(&line) {
                            let (text, _was) = truncate_line(&line, None);
                            lines.push(format!(
                                "{}:{}:{}: {}",
                                rel_path,
                                idx + 1,
                                m.start() + 1,
                                text
                            ));
                            file_has_hits = true;
                            per_file_hits += 1;
                            total += 1;
                            if per_file_hits >= per_file || total >= total_cap {
                                break;
                            }
                        }
                    }
                    if file_has_hits {
                        files_with_hits += 1;
                    }
                    if total >= total_cap {
                        truncated = true;
                        break;
                    }
                }
                Ok((lines, total, files_with_hits, truncated))
            })
            .await
            .map_err(|e| AgentToolError::msg(format!("grep task: {e}")))?
            .map_err(AgentToolError::Message)?;

        let body = if lines.is_empty() {
            "(no matches)\n".to_string()
        } else {
            let mut s = lines.join("\n");
            s.push('\n');
            if truncated {
                s.push_str(&format!(
                    "\n[Truncated at {total_cap} total matches]\n"
                ));
            }
            s
        };

        Ok(AgentToolResult {
            content: vec![UserContent::text(body)],
            details: serde_json::json!({
                "pattern": pattern_for_details,
                "matches": hit_count,
                "files": files_with_hits,
                "truncated": truncated,
                "maxPerFile": per_file,
                "maxTotal": total_cap,
            }),
            terminate: None,
        })
    }
}
