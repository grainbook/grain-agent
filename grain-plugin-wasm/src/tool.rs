//! `WasmTool` — adapter that wraps a WASM plugin's tool as a
//! [`grain_agent_core::AgentTool`].

use std::sync::Arc;

use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback,
};
use tokio_util::sync::CancellationToken;

use crate::{ToolDef, WasmPluginRuntime};

/// One tool exported by a WASM plugin, presented as an [`AgentTool`].
pub struct WasmTool {
    runtime: Arc<WasmPluginRuntime>,
    plugin_id: String,
    definition: ToolDefinition,
}

impl std::fmt::Debug for WasmTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmTool")
            .field("plugin_id", &self.plugin_id)
            .field("definition", &self.definition)
            .finish()
    }
}

impl WasmTool {
    /// Create a `WasmTool` from a plugin's [`ToolDef`].
    pub fn new(runtime: Arc<WasmPluginRuntime>, plugin_id: &str, tool_def: &ToolDef) -> Self {
        let parameters: serde_json::Value =
            serde_json::from_str(&tool_def.parameters_json).unwrap_or_default();

        WasmTool {
            runtime,
            plugin_id: plugin_id.to_string(),
            definition: ToolDefinition {
                name: tool_def.name.clone(),
                label: tool_def.label.clone(),
                description: tool_def.description.clone(),
                parameters,
                execution_mode: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for WasmTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let args_json = serde_json::to_string(&args)
            .map_err(|e| AgentToolError::msg(format!("serialize args: {e}")))?;

        let runtime = self.runtime.clone();
        let plugin_id = self.plugin_id.clone();
        let tool_name = self.definition.name.clone();

        // Run wasmtime in a blocking task so we don't block the
        // async agent loop. Select against cancellation.
        let handle = tokio::task::spawn_blocking(move || {
            // Build a current-thread runtime for the blocking context
            // so host HTTP calls can use async reqwest internally.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| AgentToolError::msg(format!("spawn runtime: {e}")))?;
            rt.block_on(runtime.call_tool(&plugin_id, &tool_name, &args_json))
                .map_err(|e| AgentToolError::msg(e.to_string()))
        });

        tokio::select! {
            result = handle => {
                let result = result
                    .map_err(|e| AgentToolError::msg(format!("task join: {e}")))?
                    ?;
                if result.is_error {
                    Ok(AgentToolResult::error(result.content_json))
                } else {
                    Ok(AgentToolResult::text(result.content_json))
                }
            }
            _ = cancel.cancelled() => {
                Err(AgentToolError::Aborted)
            }
        }
    }
}
