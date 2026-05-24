//! `bash` tool — run a shell command in the workspace.
//!
//! Arguments:
//!
//! ```json
//! {
//!   "command": "cargo test -p grain-agent-core",
//!   "cwd": ".",                  // optional, defaults to workspace root
//!   "timeout_ms": 30000          // optional, default 30s, capped at 5min
//! }
//! ```
//!
//! Safety + ergonomics:
//! - `cwd` is resolved through the workspace so the agent can't run a
//!   command outside the workspace tree (even though `command` itself can
//!   reference absolute paths — the cwd is the controllable surface).
//! - Timeout caps the wall-clock; child process is killed on timeout
//!   (`kill_on_drop = true` on the underlying tokio `Command`).
//! - CancellationToken short-circuits the same way as timeout.
//! - Combined stdout + stderr is captured and truncated to fit a reasonable
//!   transcript budget (default 50 KiB tail) via
//!   `grain_agent_harness::truncate_tail`.
//! - Exit status drives `is_error`: any non-zero / killed-by-signal status is
//!   surfaced as a tool error.
//!
//! **Security caveat (M-5)**: the `command` string is passed verbatim to
//! `/bin/sh -c`, so anything a shell can do — `curl`, `ssh`, `chmod`,
//! `rm -rf`, reading or writing files outside the workspace — is fair
//! game. `cwd` containment is a speed bump, not a sandbox.
//!
//! This is intentional (the tool is opt-in behind `--allow-bash`), but
//! callers should understand that enabling Bash grants the LLM effectively
//! unrestricted command-execution capability. The system prompt asks the
//! model to avoid destructive operations; that is a prompt-level defense,
//! not a code-level one. Run in a container, ephemeral VM, or restricted
//! user account if you need stronger isolation.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use grain_agent_harness::{TruncationOptions, truncate_tail};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::workspace::Workspace;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 5 * 60 * 1000;
const MAX_OUTPUT_BYTES: usize = 50 * 1024;

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub struct BashTool {
    def: ToolDefinition,
    workspace: Arc<Workspace>,
}

impl BashTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        BashTool {
            def: ToolDefinition {
                name: "bash".into(),
                label: "Bash".into(),
                description: format!(
                    "Run a shell command inside the workspace via `/bin/sh -c`. Default \
                     timeout {DEFAULT_TIMEOUT_MS}ms (capped at {MAX_TIMEOUT_MS}ms). Combined \
                     stdout+stderr is captured (tail-truncated at {MAX_OUTPUT_BYTES} bytes)."
                ),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to run. Passed to `/bin/sh -c`."
                        },
                        "cwd": {
                            "type": "string",
                            "description": "Working directory, workspace-relative (defaults to root)."
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "description": "Wall-clock timeout in milliseconds (capped at 5 minutes)."
                        }
                    },
                    "required": ["command"]
                }),
                execution_mode: None,
            },
            workspace,
        }
    }
}

#[async_trait]
impl AgentTool for BashTool {
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
        let args: BashArgs =
            serde_json::from_value(args).map_err(|e| AgentToolError::Validation(e.to_string()))?;
        if args.command.trim().is_empty() {
            return Err(AgentToolError::Validation(
                "command must be non-empty".into(),
            ));
        }

        let cwd = match &args.cwd {
            Some(c) => self
                .workspace
                .resolve(c)
                .map_err(|e| AgentToolError::msg(e.to_string()))?,
            None => self.workspace.root().to_path_buf(),
        };

        let timeout = Duration::from_millis(
            args.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );

        let started = Instant::now();
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(&args.command)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = cmd
            .spawn()
            .map_err(|e| AgentToolError::msg(format!("spawn /bin/sh: {e}")))?;

        // Race the child's wait_with_output() against timeout + cancel.
        // kill_on_drop ensures the child dies when the future is dropped on
        // either of the racing branches.
        let outcome = tokio::select! {
            _ = tokio::time::sleep(timeout) => Outcome::Timeout,
            _ = cancel.cancelled() => Outcome::Aborted,
            res = child.wait_with_output() => match res {
                Ok(output) => Outcome::Finished(output),
                Err(e) => Outcome::Io(e.to_string()),
            },
        };

        let duration = started.elapsed();
        let dur_ms = duration.as_millis() as u64;
        let rel_cwd = self.workspace.display_relative(&cwd);

        match outcome {
            Outcome::Finished(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let combined = if stderr.is_empty() {
                    stdout.into_owned()
                } else if stdout.is_empty() {
                    format!("--- stderr ---\n{stderr}")
                } else {
                    format!("{stdout}\n--- stderr ---\n{stderr}")
                };
                let trunc = truncate_tail(
                    &combined,
                    TruncationOptions {
                        max_lines: None,
                        max_bytes: Some(MAX_OUTPUT_BYTES),
                    },
                );

                let exit_code = output.status.code();
                let success = output.status.success();

                let mut body = trunc.content;
                if trunc.truncated {
                    body.push_str(&format!(
                        "\n[Truncated: kept tail {} of {} bytes]",
                        trunc.output_bytes, trunc.total_bytes
                    ));
                }

                Ok(AgentToolResult {
                    content: vec![UserContent::text(body)],
                    details: serde_json::json!({
                        "command": args.command,
                        "cwd": rel_cwd,
                        "exitCode": exit_code,
                        "success": success,
                        "stdoutBytes": output.stdout.len(),
                        "stderrBytes": output.stderr.len(),
                        "durationMs": dur_ms,
                        "truncated": trunc.truncated,
                        "outputBytes": trunc.output_bytes,
                        "totalOutputBytes": trunc.total_bytes,
                    }),
                    terminate: None,
                })
            }
            Outcome::Timeout => Err(AgentToolError::msg(format!(
                "command timed out after {} ms: {}",
                timeout.as_millis(),
                args.command
            ))),
            Outcome::Aborted => Err(AgentToolError::Aborted),
            Outcome::Io(e) => Err(AgentToolError::msg(format!("bash io error: {e}"))),
        }
    }
}

enum Outcome {
    Finished(std::process::Output),
    Timeout,
    Aborted,
    Io(String),
}
