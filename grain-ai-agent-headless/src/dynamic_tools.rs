//! Dynamic tool activation for coding-agent requests.
//!
//! The goal is to avoid uploading every tool schema on every turn. We keep
//! cheap read/navigation tools visible by default, then opt in higher-cost or
//! higher-risk tools when the latest request, recent transcript, or explicit
//! tool names indicate they are needed.

use std::collections::HashSet;
use std::sync::Arc;

use grain_agent_core::{AgentMessage, AgentTool, AssistantContent, Message, UserContent};

pub const BASE_READ_TOOLS: &[&str] = &["read", "list", "glob", "grep", "source_info"];
pub const WRITE_TOOLS: &[&str] = &["write", "edit"];
pub const BASH_TOOLS: &[&str] = &["bash"];
pub const WEB_TOOLS: &[&str] = &["web_fetch"];
pub const SEMANTIC_TOOLS: &[&str] = &["semantic_search"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolActivationDecision {
    pub names: Vec<String>,
    pub reason: String,
}

impl ToolActivationDecision {
    pub fn is_static_all(&self, all_tool_count: usize) -> bool {
        self.names.len() == all_tool_count
    }
}

/// Select tool names for a prompt while preserving the original tool order.
pub fn select_dynamic_tool_names(
    tools: &[Arc<dyn AgentTool>],
    prior_messages: &[AgentMessage],
    prompt: &str,
) -> ToolActivationDecision {
    if tools.is_empty() {
        return ToolActivationDecision {
            names: Vec::new(),
            reason: "no tools registered".into(),
        };
    }

    let mut selected: HashSet<String> = HashSet::new();
    let known: HashSet<&str> = tools.iter().map(|t| t.definition().name.as_str()).collect();

    for name in BASE_READ_TOOLS {
        if known.contains(name) {
            selected.insert((*name).to_string());
        }
    }

    let text = normalize_text(prompt);
    let recent_text = normalize_text(&recent_text(prior_messages, 8));
    let combined = format!("{recent_text}\n{text}");

    if contains_any(&combined, WRITE_HINTS) {
        add_known(&mut selected, &known, WRITE_TOOLS);
        // Most write turns benefit from a cheap validation command if bash
        // is available, but bash still stays off for purely read-only prompts.
        add_known(&mut selected, &known, BASH_TOOLS);
    }
    if contains_any(&combined, BASH_HINTS) {
        add_known(&mut selected, &known, BASH_TOOLS);
    }
    if contains_any(&combined, WEB_HINTS) {
        add_known(&mut selected, &known, WEB_TOOLS);
    }
    if contains_any(&combined, SEMANTIC_HINTS) {
        add_known(&mut selected, &known, SEMANTIC_TOOLS);
    }

    for name in recent_tool_names(prior_messages, 12) {
        if known.contains(name.as_str()) {
            selected.insert(name);
        }
    }

    for tool in tools {
        let def = tool.definition();
        if selected.contains(&def.name) {
            continue;
        }
        if is_builtin_tool(&def.name) {
            continue;
        }
        if tool_is_mentioned(&combined, &def.name, &def.label) {
            selected.insert(def.name.clone());
        }
    }

    // If this catalog has no known built-ins and no heuristic fired, keep the
    // old behavior. A custom-only runtime cannot be safely categorized.
    if selected.is_empty() {
        selected.extend(tools.iter().map(|t| t.definition().name.clone()));
    }

    let names: Vec<String> = tools
        .iter()
        .filter_map(|t| {
            let name = &t.definition().name;
            selected.contains(name).then(|| name.clone())
        })
        .collect();

    let reason = if names.len() == tools.len() {
        "all tools active".into()
    } else {
        format!("{} of {} tools active", names.len(), tools.len())
    };
    ToolActivationDecision { names, reason }
}

pub fn filter_tools_by_names(
    tools: &[Arc<dyn AgentTool>],
    names: &[String],
) -> Vec<Arc<dyn AgentTool>> {
    let selected: HashSet<&str> = names.iter().map(String::as_str).collect();
    tools
        .iter()
        .filter(|t| selected.contains(t.definition().name.as_str()))
        .cloned()
        .collect()
}

fn add_known(selected: &mut HashSet<String>, known: &HashSet<&str>, names: &[&str]) {
    for name in names {
        if known.contains(name) {
            selected.insert((*name).to_string());
        }
    }
}

fn recent_text(messages: &[AgentMessage], limit: usize) -> String {
    let mut out = String::new();
    for msg in messages.iter().rev().take(limit).rev() {
        match msg {
            AgentMessage::Standard(Message::User(u)) => {
                for c in &u.content {
                    if let UserContent::Text(t) = c {
                        out.push_str(&t.text);
                        out.push('\n');
                    }
                }
            }
            AgentMessage::Standard(Message::Assistant(a)) => {
                for c in &a.content {
                    if let AssistantContent::Text(t) = c {
                        out.push_str(&t.text);
                        out.push('\n');
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn recent_tool_names(messages: &[AgentMessage], limit: usize) -> Vec<String> {
    let mut names = Vec::new();
    for msg in messages.iter().rev().take(limit).rev() {
        match msg {
            AgentMessage::Standard(Message::Assistant(a)) => {
                for c in &a.content {
                    if let AssistantContent::ToolCall(tc) = c {
                        names.push(tc.name.clone());
                    }
                }
            }
            AgentMessage::Standard(Message::ToolResult(t)) => names.push(t.tool_name.clone()),
            _ => {}
        }
    }
    names
}

fn contains_any(text: &str, hints: &[&str]) -> bool {
    hints.iter().any(|hint| text.contains(hint))
}

fn tool_is_mentioned(text: &str, name: &str, label: &str) -> bool {
    let name = normalize_text(name);
    let label = normalize_text(label);
    text.contains(&name)
        || (!label.is_empty() && text.contains(&label))
        || name
            .split(['_', '-'])
            .filter(|part| part.len() >= 4)
            .any(|part| text.contains(part))
}

fn normalize_text(s: &str) -> String {
    s.to_lowercase()
}

fn is_builtin_tool(name: &str) -> bool {
    BASE_READ_TOOLS.contains(&name)
        || WRITE_TOOLS.contains(&name)
        || BASH_TOOLS.contains(&name)
        || WEB_TOOLS.contains(&name)
        || SEMANTIC_TOOLS.contains(&name)
}

const WRITE_HINTS: &[&str] = &[
    "edit",
    "write",
    "modify",
    "change",
    "fix",
    "implement",
    "add ",
    "delete",
    "remove",
    "rename",
    "refactor",
    "update",
    "patch",
    "create",
    "apply",
    "修改",
    "写入",
    "编辑",
    "修复",
    "实现",
    "添加",
    "删除",
    "移除",
    "重命名",
    "重构",
    "更新",
    "创建",
    "补丁",
    "改成",
    "开干",
];

const BASH_HINTS: &[&str] = &[
    "test",
    "run",
    "build",
    "cargo",
    "npm",
    "bun",
    "pnpm",
    "yarn",
    "pytest",
    "make",
    "git ",
    "command",
    "shell",
    "execute",
    "install",
    "compile",
    "lint",
    "format",
    "publish",
    "测试",
    "运行",
    "构建",
    "编译",
    "命令",
    "终端",
    "执行",
    "安装",
    "检查",
    "发布",
    "格式化",
];

const WEB_HINTS: &[&str] = &[
    "http://", "https://", "url", "web", "fetch", "browse", "latest", "current", "today", "news",
    "网页", "网站", "链接", "联网", "浏览", "最新", "今天", "当前",
];

const SEMANTIC_HINTS: &[&str] = &[
    "semantic",
    "similar",
    "meaning",
    "concept",
    "related code",
    "语义",
    "相似",
    "相关代码",
    "概念",
];

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use grain_agent_core::{
        AgentToolError, AgentToolResult, AssistantMessage, StopReason, TextContent, ToolCall,
        ToolDefinition, ToolResultMessage, ToolUpdateCallback, Usage,
    };
    use tokio_util::sync::CancellationToken;

    struct StubTool {
        def: ToolDefinition,
    }

    impl StubTool {
        fn new(name: &str, label: &str) -> Arc<Self> {
            Self::new_with_description(name, label, "")
        }

        fn new_with_description(name: &str, label: &str, description: &str) -> Arc<Self> {
            Arc::new(Self {
                def: ToolDefinition {
                    name: name.into(),
                    label: label.into(),
                    description: description.into(),
                    parameters: serde_json::json!({"type": "object"}),
                    execution_mode: None,
                },
            })
        }
    }

    #[async_trait]
    impl AgentTool for StubTool {
        fn definition(&self) -> &ToolDefinition {
            &self.def
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _args: serde_json::Value,
            _cancel: CancellationToken,
            _on_update: ToolUpdateCallback,
        ) -> Result<AgentToolResult, AgentToolError> {
            Ok(AgentToolResult::text("ok"))
        }
    }

    fn catalog() -> Vec<Arc<dyn AgentTool>> {
        vec![
            StubTool::new("read", "Read"),
            StubTool::new("grep", "Grep"),
            StubTool::new("write", "Write"),
            StubTool::new("edit", "Edit"),
            StubTool::new("bash", "Bash"),
            StubTool::new("web_fetch", "Web Fetch"),
            StubTool::new("semantic_search", "Semantic Search"),
            StubTool::new_with_description(
                "semble_rs",
                "Semble",
                "Run Semble code search and retrieval",
            ),
        ]
    }

    #[test]
    fn read_only_prompt_keeps_low_cost_tools() {
        let tools = catalog();
        let decision = select_dynamic_tool_names(&tools, &[], "inspect the parser");
        assert_eq!(decision.names, vec!["read", "grep"]);
    }

    #[test]
    fn write_prompt_enables_write_and_bash() {
        let tools = catalog();
        let decision = select_dynamic_tool_names(&tools, &[], "fix the failing tests");
        assert!(decision.names.contains(&"write".into()));
        assert!(decision.names.contains(&"edit".into()));
        assert!(decision.names.contains(&"bash".into()));
    }

    #[test]
    fn web_prompt_enables_web_fetch() {
        let tools = catalog();
        let decision = select_dynamic_tool_names(&tools, &[], "fetch https://example.com");
        assert!(decision.names.contains(&"web_fetch".into()));
    }

    #[test]
    fn explicit_plugin_request_enables_custom_tool() {
        let tools = catalog();
        let decision = select_dynamic_tool_names(&tools, &[], "使用 semble 的能力");
        assert!(decision.names.contains(&"semble_rs".into()));
    }

    #[test]
    fn generic_plugin_words_do_not_enable_every_custom_tool() {
        let tools = catalog();
        let decision = select_dynamic_tool_names(&tools, &[], "使用工具帮我看看");
        assert!(!decision.names.contains(&"semble_rs".into()));
    }

    #[test]
    fn recent_tool_usage_stays_active() {
        let tools = catalog();
        let prior = vec![AgentMessage::assistant(AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "semble_rs".into(),
                arguments: serde_json::json!({}),
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        })];
        let decision = select_dynamic_tool_names(&tools, &prior, "continue");
        assert!(decision.names.contains(&"semble_rs".into()));
    }

    #[test]
    fn recent_tool_result_stays_active() {
        let tools = catalog();
        let prior = vec![AgentMessage::tool_result(ToolResultMessage {
            tool_call_id: "call-1".into(),
            tool_name: "semble_rs".into(),
            content: vec![UserContent::Text(TextContent { text: "ok".into() })],
            details: serde_json::json!({}),
            is_error: false,
            timestamp: 0,
        })];
        let decision = select_dynamic_tool_names(&tools, &prior, "continue");
        assert!(decision.names.contains(&"semble_rs".into()));
    }

    #[test]
    fn custom_only_catalog_falls_back_to_all() {
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            StubTool::new("alpha_tool", "Alpha"),
            StubTool::new("beta_tool", "Beta"),
        ];
        let decision = select_dynamic_tool_names(&tools, &[], "hello");
        assert_eq!(decision.names, vec!["alpha_tool", "beta_tool"]);
    }
}
