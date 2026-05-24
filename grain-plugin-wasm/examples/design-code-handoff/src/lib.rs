//! Designer -> coder handoff plugin.
//!
//! This is a v2 orchestration plugin. It exports one tool,
//! `handoff_to_coder`, and a `prepare-next-turn` hook. When the designer
//! calls the tool, the hook asks the host to switch to the `coder` role and
//! injects the implementation brief as a new user message.

#![allow(clippy::all)]

wit_bindgen::generate!({
    world: "grain-plugin-v2",
    path: "wit",
});

use exports::grain::plugin::orchestration::{
    Guest as OrchestrationGuest, HookDef, HookPoint, HostAction, RoleDef, UiHeader,
};
use exports::grain::plugin::plugin::{
    Guest as PluginGuest, PluginInfo, ToolDef, ToolResult,
};
use grain::plugin::host;
use serde::{Deserialize, Serialize};

const TOOL_NAME: &str = "handoff_to_coder";

const HANDOFF_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "task": {
      "type": "string",
      "description": "The implementation task the coder should complete."
    },
    "design": {
      "type": "string",
      "description": "Concrete implementation design, including key decisions and constraints."
    },
    "constraints": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Non-negotiable constraints the coder must preserve."
    },
    "acceptance": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Acceptance criteria the coder should verify before finishing."
    },
    "suggestedFiles": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Files or modules likely involved."
    },
    "testCommands": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Commands the coder should run to verify the implementation."
    }
  },
  "required": ["task", "design"]
}"#;

const DESIGNER_PROMPT: &str = r#"You are the designer role.

Your job is to understand the request, inspect the repository, and produce a concrete implementation plan. You should not edit files. When the plan is ready, call `handoff_to_coder` with a structured brief. Include constraints, acceptance criteria, likely files, and test commands. Do not hand off until the plan is specific enough for another model to implement without guessing."#;

const CODER_PROMPT: &str = r#"You are the coder role.

Implement the designer's brief. Use the repository's existing patterns, keep changes scoped, and verify with the requested tests when possible. If the brief is invalid or missing critical information, explain the blocker instead of inventing requirements."#;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct HandoffArgs {
    task: String,
    design: String,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(default)]
    acceptance: Vec<String>,
    #[serde(default)]
    suggested_files: Vec<String>,
    #[serde(default)]
    test_commands: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HandoffEnvelope {
    kind: String,
    to: String,
    payload: HandoffArgs,
    prompt: String,
}

struct DesignCodeHandoff;

impl PluginGuest for DesignCodeHandoff {
    fn init() -> Result<PluginInfo, String> {
        host::log(
            host::LogLevel::Info,
            "design-code-handoff initialized",
        );
        Ok(PluginInfo {
            name: "design-code-handoff".to_string(),
            version: "0.1.0".to_string(),
        })
    }

    fn list_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: TOOL_NAME.to_string(),
            label: "Handoff to coder".to_string(),
            description: "Pass a concrete implementation brief from the designer role to the coder role.".to_string(),
            parameters_json: HANDOFF_SCHEMA.to_string(),
        }]
    }

    fn call_tool(name: String, args_json: String) -> ToolResult {
        if name != TOOL_NAME {
            return error_result(format!("unknown tool: {name}"));
        }

        let args: HandoffArgs = match serde_json::from_str(&args_json) {
            Ok(args) => args,
            Err(e) => return error_result(format!("invalid handoff args: {e}")),
        };
        if args.task.trim().is_empty() || args.design.trim().is_empty() {
            return error_result("`task` and `design` must be non-empty");
        }

        let prompt = format_coder_prompt(&args);
        let envelope = HandoffEnvelope {
            kind: "designCodeHandoff".to_string(),
            to: "coder".to_string(),
            payload: args,
            prompt,
        };

        match serde_json::to_string(&envelope) {
            Ok(content_json) => ToolResult {
                content_json,
                is_error: false,
            },
            Err(e) => error_result(format!("serialize handoff: {e}")),
        }
    }
}

impl OrchestrationGuest for DesignCodeHandoff {
    fn list_roles() -> Vec<RoleDef> {
        vec![
            RoleDef {
                name: "designer".to_string(),
                model: env_or("DESIGNER_MODEL", "openai/gpt-5.1-codex-mini"),
                prompt: env_or("DESIGNER_PROMPT", DESIGNER_PROMPT),
                tools: env_list_or(
                    "DESIGNER_TOOLS",
                    &["read", "list", "glob", "grep", "source_info", TOOL_NAME],
                ),
                thinking_level: Some(env_or("DESIGNER_THINKING", "high")),
            },
            RoleDef {
                name: "coder".to_string(),
                model: env_or("CODER_MODEL", "deepseek/deepseek-v4-pro"),
                prompt: env_or("CODER_PROMPT", CODER_PROMPT),
                tools: env_list_or(
                    "CODER_TOOLS",
                    &["read", "list", "glob", "grep", "source_info", "write", "edit", "bash"],
                ),
                thinking_level: Some(env_or("CODER_THINKING", "medium")),
            },
        ]
    }

    fn list_hooks() -> Vec<HookDef> {
        vec![HookDef {
            point: HookPoint::PrepareNextTurn,
            name: "design-code-handoff".to_string(),
        }]
    }

    fn call_hook(point: HookPoint, context_json: String) -> Result<Vec<HostAction>, String> {
        if point != HookPoint::PrepareNextTurn {
            return Ok(Vec::new());
        }
        let Some(envelope) = find_latest_handoff(&context_json)? else {
            return Ok(Vec::new());
        };
        if envelope.to != "coder" {
            return Ok(Vec::new());
        }
        Ok(vec![
            HostAction::SwitchRole("coder".to_string()),
            HostAction::SetUiHeader(UiHeader {
                provider: Some("deepseek".to_string()),
                model: Some(env_or("CODER_MODEL", "deepseek/deepseek-v4-pro")),
            }),
            HostAction::InjectUserMessage(envelope.prompt),
        ])
    }
}

fn find_latest_handoff(context_json: &str) -> Result<Option<HandoffEnvelope>, String> {
    let root: serde_json::Value =
        serde_json::from_str(context_json).map_err(|e| format!("parse hook context: {e}"))?;
    let Some(results) = root.get("toolResults").and_then(|v| v.as_array()) else {
        return Ok(None);
    };

    for result in results.iter().rev() {
        if result.get("toolName").and_then(|v| v.as_str()) != Some(TOOL_NAME) {
            continue;
        }
        if result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let Some(content) = result.get("content").and_then(|v| v.as_array()) else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(|v| v.as_str()) != Some("text") {
                continue;
            }
            let Some(text) = block.get("text").and_then(|v| v.as_str()) else {
                continue;
            };
            let Ok(envelope) = serde_json::from_str::<HandoffEnvelope>(text) else {
                continue;
            };
            if envelope.kind == "designCodeHandoff" {
                return Ok(Some(envelope));
            }
        }
    }
    Ok(None)
}

fn format_coder_prompt(args: &HandoffArgs) -> String {
    let mut out = String::new();
    out.push_str("Implement the following designer handoff.\n\n");
    out.push_str("Task:\n");
    out.push_str(args.task.trim());
    out.push_str("\n\nDesign:\n");
    out.push_str(args.design.trim());
    push_list(&mut out, "Constraints", &args.constraints);
    push_list(&mut out, "Acceptance criteria", &args.acceptance);
    push_list(&mut out, "Suggested files", &args.suggested_files);
    push_list(&mut out, "Test commands", &args.test_commands);
    out
}

fn push_list(out: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str("\n\n");
    out.push_str(title);
    out.push_str(":\n");
    for item in items {
        let trimmed = item.trim();
        if !trimmed.is_empty() {
            out.push_str("- ");
            out.push_str(trimmed);
            out.push('\n');
        }
    }
}

fn env_or(key: &str, fallback: &str) -> String {
    host::env_get(key).unwrap_or_else(|| fallback.to_string())
}

fn env_list_or(key: &str, fallback: &[&str]) -> Vec<String> {
    match host::env_get(key) {
        Some(value) => value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        None => fallback.iter().map(|s| s.to_string()).collect(),
    }
}

fn error_result(message: impl Into<String>) -> ToolResult {
    let message = message.into();
    ToolResult {
        content_json: serde_json::json!({ "error": message }).to_string(),
        is_error: true,
    }
}

export!(DesignCodeHandoff);
