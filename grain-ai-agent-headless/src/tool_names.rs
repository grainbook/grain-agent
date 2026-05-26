//! Provider-safe tool-name adaptation.
//!
//! Some OpenAI-compatible providers reject tool names containing
//! characters outside `[A-Za-z0-9_]`. This module wraps tools with a
//! sanitized `ToolDefinition::name` while forwarding execution to the
//! original implementation.

use std::collections::HashSet;
use std::sync::Arc;

use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct ProviderSafeTool {
    inner: Arc<dyn AgentTool>,
    definition: ToolDefinition,
}

#[async_trait::async_trait]
impl AgentTool for ProviderSafeTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn prepare_arguments(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, AgentToolError> {
        self.inner.prepare_arguments(args)
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        self.inner
            .execute(tool_call_id, args, cancel, on_update)
            .await
    }
}

pub fn provider_safe_tool_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().max(4));
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        "tool".to_string()
    } else {
        out
    }
}

pub fn make_unique_tool_name(base: String, used: &mut HashSet<String>) -> String {
    if used.insert(base.clone()) {
        return base;
    }

    let mut idx = 2usize;
    loop {
        let candidate = format!("{base}_{idx}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        idx += 1;
    }
}

pub fn normalize_tool_names_for_provider(
    tools: Vec<Arc<dyn AgentTool>>,
) -> Vec<Arc<dyn AgentTool>> {
    let mut used = HashSet::new();
    tools
        .into_iter()
        .map(|tool| {
            let original = tool.definition().name.clone();
            let safe = make_unique_tool_name(provider_safe_tool_name(&original), &mut used);
            if safe == original {
                return tool;
            }

            let mut definition = tool.definition().clone();
            definition.name = safe.clone();
            eprintln!(
                "[info] provider-safe tool alias: '{}' -> '{}'",
                original, safe
            );
            Arc::new(ProviderSafeTool {
                inner: tool,
                definition,
            }) as Arc<dyn AgentTool>
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_safe_tool_name_replaces_provider_hostile_chars() {
        assert_eq!(provider_safe_tool_name("web-search"), "web_search");
        assert_eq!(
            provider_safe_tool_name("web.search/fetch"),
            "web_search_fetch"
        );
        assert_eq!(provider_safe_tool_name("  "), "tool");
    }

    #[test]
    fn make_unique_tool_name_appends_suffix_for_collisions() {
        let mut used = HashSet::new();
        assert_eq!(
            make_unique_tool_name("web_fetch".to_string(), &mut used),
            "web_fetch"
        );
        assert_eq!(
            make_unique_tool_name("web_fetch".to_string(), &mut used),
            "web_fetch_2"
        );
        assert_eq!(
            make_unique_tool_name("web_fetch".to_string(), &mut used),
            "web_fetch_3"
        );
    }
}
