//! Echo plugin — minimal example of a grain WASM plugin.
//!
//! Exports one tool ("echo") that returns its arguments verbatim.
//! Build with `cargo component build --release` (requires
//! `cargo-component` and the `wasm32-wasip2` target).

// Generate guest bindings from the WIT contract.
wit_bindgen::generate!({
    world: "grain-plugin",
    path: "wit",
});

struct EchoPlugin;

impl Guest for EchoPlugin {
    fn init() -> Result<PluginInfo, String> {
        Ok(PluginInfo {
            name: "echo".to_string(),
            version: "0.1.0".to_string(),
        })
    }

    fn list_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "echo".to_string(),
            label: "Echo".to_string(),
            description: "Returns its arguments verbatim. Useful for testing.".to_string(),
            parameters_json: r#"{"type":"object","properties":{"text":{"type":"string","description":"Text to echo back"}}}"#.to_string(),
        }]
    }

    fn call_tool(name: String, args_json: String) -> ToolResult {
        if name != "echo" {
            return ToolResult {
                content_json: format!(r#"{{"error":"unknown tool: {name}"}}"#),
                is_error: true,
            };
        }
        // Echo the args back verbatim.
        ToolResult {
            content_json: args_json,
            is_error: false,
        }
    }
}

export!(EchoPlugin);
