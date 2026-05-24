//! Extensions: a small registry of optional tool / event-listener
//! providers callers can bolt into `AgentOptions` at startup.
//!
//! Mirrors pi's `core/extensions/` directory shape at the minimum
//! viable level — the trait gives third-party crates a single entry
//! point to contribute (a) tools and (b) event subscribers without
//! forking `cli::run`. Dynamic loading (`.so` / `.dll`) is **not**
//! supported; extensions are crates compiled in alongside grain-
//! ai-agent-headless or registered via the public Rust API.
//!
//! ## Building an extension
//!
//! ```ignore
//! use std::sync::Arc;
//! use grain_agent_core::{AgentTool, EventListener};
//! use grain_ai_agent_headless::extensions::Extension;
//! use grain_ai_agent_headless::Workspace;
//!
//! pub struct MyExtension;
//!
//! impl Extension for MyExtension {
//!     fn name(&self) -> &'static str { "my-extension" }
//!     fn tools(&self, _workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> { vec![] }
//!     fn listeners(&self) -> Vec<EventListener> { vec![] }
//! }
//! ```

use std::sync::Arc;

use grain_agent_core::{AgentTool, EventListener};

use crate::workspace::Workspace;

/// One unit of opt-in functionality. Anything an extension contributes is
/// taken at registration time and folded into the agent — there's no
/// per-call dispatch overhead.
pub trait Extension: Send + Sync {
    /// Stable identifier used in diagnostics / telemetry. Should match the
    /// crate name to avoid collisions.
    fn name(&self) -> &'static str;

    /// Tools to merge into `AgentOptions::tools`. Default: none.
    fn tools(&self, _workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
        Vec::new()
    }

    /// Listeners to subscribe to the agent's event bus. Default: none.
    fn listeners(&self) -> Vec<EventListener> {
        Vec::new()
    }
}

/// Aggregate `Extension`s into one collection that `cli::run` can iterate.
/// Order is registration order, which is also the order extensions see
/// events.
#[derive(Default)]
pub struct ExtensionRegistry {
    items: Vec<Arc<dyn Extension>>,
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, ext: Arc<dyn Extension>) -> &mut Self {
        self.items.push(ext);
        self
    }

    /// Collect all extensions' tools, in registration order.
    pub fn collect_tools(&self, workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
        let mut out = Vec::new();
        for ext in &self.items {
            out.extend(ext.tools(workspace.clone()));
        }
        out
    }

    /// Collect all extensions' listeners, in registration order.
    pub fn collect_listeners(&self) -> Vec<EventListener> {
        let mut out = Vec::new();
        for ext in &self.items {
            out.extend(ext.listeners());
        }
        out
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.items.iter().map(|e| e.name()).collect()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use grain_agent_core::{AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback};
    use tokio_util::sync::CancellationToken;

    struct NamedNoop {
        name: &'static str,
        tool_name: &'static str,
    }

    struct StubTool {
        def: ToolDefinition,
    }

    #[async_trait]
    impl AgentTool for StubTool {
        fn definition(&self) -> &ToolDefinition {
            &self.def
        }
        async fn execute(
            &self,
            _id: &str,
            _args: serde_json::Value,
            _cancel: CancellationToken,
            _on_update: ToolUpdateCallback,
        ) -> Result<AgentToolResult, AgentToolError> {
            Ok(AgentToolResult::text("stub"))
        }
    }

    impl Extension for NamedNoop {
        fn name(&self) -> &'static str {
            self.name
        }
        fn tools(&self, _workspace: Arc<Workspace>) -> Vec<Arc<dyn AgentTool>> {
            let def = ToolDefinition {
                name: self.tool_name.into(),
                label: self.tool_name.into(),
                description: "stub".into(),
                parameters: serde_json::json!({ "type": "object" }),
                execution_mode: None,
            };
            vec![Arc::new(StubTool { def })]
        }
    }

    fn workspace() -> Arc<Workspace> {
        let dir = tempfile::tempdir().unwrap();
        let ws = Arc::new(Workspace::new(dir.path()).unwrap());
        // Leak the tempdir for the test's lifetime so canonicalization
        // doesn't race against drop.
        std::mem::forget(dir);
        ws
    }

    #[test]
    fn empty_registry_is_empty() {
        let r = ExtensionRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn collect_preserves_registration_order() {
        let mut r = ExtensionRegistry::new();
        r.register(Arc::new(NamedNoop {
            name: "a",
            tool_name: "tool_a",
        }));
        r.register(Arc::new(NamedNoop {
            name: "b",
            tool_name: "tool_b",
        }));
        assert_eq!(r.names(), vec!["a", "b"]);
        let ws = workspace();
        let tools = r.collect_tools(ws);
        let names: Vec<&str> = tools.iter().map(|t| t.definition().name.as_str()).collect();
        assert_eq!(names, vec!["tool_a", "tool_b"]);
    }
}
