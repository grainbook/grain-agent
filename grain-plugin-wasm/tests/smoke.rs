//! Smoke tests for `grain-plugin-wasm`.
//!
//! These tests validate the runtime's public API surface without
//! requiring a pre-built `.wasm` component. Full e2e tests (load a
//! real component, call tools, verify output) live behind the
//! `echo-plugin` example and require `cargo-component` + wasm32-wasip2.

use std::collections::HashMap;

use grain_plugin_wasm::{Capabilities, WasmPluginRuntime};

// ---------------------------------------------------------------------------
// Unit: Capabilities parsing
// ---------------------------------------------------------------------------

#[test]
fn capabilities_default_is_all_false() {
    let caps = Capabilities::default();
    assert!(!caps.log);
    assert!(!caps.env);
    assert!(!caps.http);
}

#[test]
fn capabilities_from_list_parses_known_strings() {
    let caps = Capabilities::from_list(&["log".to_string(), "env".to_string(), "http".to_string()]);
    assert!(caps.log);
    assert!(caps.env);
    assert!(caps.http);
}

#[test]
fn capabilities_from_list_ignores_unknown() {
    let caps = Capabilities::from_list(&["log".to_string(), "unknown".to_string()]);
    assert!(caps.log);
    assert!(!caps.env);
    assert!(!caps.http);
}

#[test]
fn capabilities_from_empty_list() {
    let caps = Capabilities::from_list(&[]);
    assert!(!caps.log);
    assert!(!caps.env);
    assert!(!caps.http);
}

// ---------------------------------------------------------------------------
// Unit: Runtime creation
// ---------------------------------------------------------------------------

#[test]
fn runtime_creates_successfully() {
    let rt = WasmPluginRuntime::new();
    assert!(rt.is_ok(), "WasmPluginRuntime::new() should succeed");
}

#[test]
fn runtime_debug_impl_does_not_panic() {
    let rt = WasmPluginRuntime::new().unwrap();
    let s = format!("{rt:?}");
    assert!(s.contains("WasmPluginRuntime"));
}

// ---------------------------------------------------------------------------
// Unit: Load with invalid wasm bytes fails gracefully
// ---------------------------------------------------------------------------

#[tokio::test]
async fn load_invalid_wasm_returns_error() {
    let rt = WasmPluginRuntime::new().unwrap();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"not a valid wasm module").unwrap();
    let result = rt
        .load(
            tmp.path(),
            "bad",
            Capabilities::default(),
            "bad-plugin",
            HashMap::new(),
        )
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("wasmtime"),
        "error should mention wasmtime: {err}"
    );
}

#[tokio::test]
async fn load_nonexistent_file_returns_error() {
    let rt = WasmPluginRuntime::new().unwrap();
    let result = rt
        .load(
            std::path::Path::new("/tmp/grain-nonexistent-plugin.wasm"),
            "missing",
            Capabilities::default(),
            "missing-plugin",
            HashMap::new(),
        )
        .await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Unit: call_tool on unloaded plugin fails
// ---------------------------------------------------------------------------

#[tokio::test]
async fn call_tool_on_unknown_plugin_returns_error() {
    let rt = WasmPluginRuntime::new().unwrap();
    let result = rt
        .call_tool(
            "nonexistent",
            "echo",
            "{}",
            tokio::runtime::Handle::current(),
        )
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not loaded"), "error: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_web_fetch_plugin_does_not_reenter_tokio_runtime() {
    use grain_agent_core::AgentTool;
    use grain_plugin_wasm::WasmTool;
    use std::io::{ErrorKind, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    let wasm = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples/web-search/target/wasm32-wasip1/release/web_search_plugin.wasm");
    if !wasm.exists() {
        eprintln!("skipping web-search wasm e2e: {} missing", wasm.display());
        return;
    }

    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping web-search wasm e2e: local bind denied by sandbox");
            return;
        }
        Err(e) => panic!("bind local test server: {e}"),
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(pair) => break pair,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept local test connection: {e}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0_u8; 1024];
        let _ = stream.read(&mut buf).unwrap();
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 15\r\n\r\nhello from test",
            )
            .unwrap();
    });

    let rt = Arc::new(WasmPluginRuntime::new().unwrap());
    let loaded = rt
        .load(
            &wasm,
            "web-search",
            Capabilities {
                log: false,
                env: false,
                http: true,
            },
            "web-search",
            HashMap::new(),
        )
        .await
        .unwrap();
    let fetch_def = loaded
        .tool_defs
        .iter()
        .find(|td| td.name == "web_fetch")
        .expect("web_fetch tool")
        .clone();
    let tool = WasmTool::new(rt, "web-search", &fetch_def);

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        tool.execute(
            "fetch",
            serde_json::json!({ "url": format!("http://{addr}/") }),
            CancellationToken::new(),
            Arc::new(|_: grain_agent_core::AgentToolResult| {}),
        ),
    )
    .await
    .expect("web_fetch plugin call timed out")
    .unwrap();

    server.join().unwrap();
    let body = result
        .content
        .iter()
        .filter_map(|c| match c {
            grain_agent_core::UserContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(body.contains("hello from test"), "{body}");
}

// ---------------------------------------------------------------------------
// Unit: ToolDef → WasmTool → ToolDefinition round-trip
// ---------------------------------------------------------------------------

#[test]
fn wasm_tool_definition_from_tool_def() {
    use grain_plugin_wasm::{ToolDef, WasmTool};
    let rt = std::sync::Arc::new(WasmPluginRuntime::new().unwrap());
    let td = ToolDef {
        name: "echo".to_string(),
        label: "Echo".to_string(),
        description: "Returns args verbatim".to_string(),
        parameters_json: r#"{"type":"object"}"#.to_string(),
    };
    let tool = WasmTool::new(rt, "test-plugin", &td);
    let def = grain_agent_core::AgentTool::definition(&tool);
    assert_eq!(def.name, "echo");
    assert_eq!(def.label, "Echo");
    assert_eq!(def.description, "Returns args verbatim");
    assert_eq!(def.parameters, serde_json::json!({"type": "object"}));
    assert!(def.execution_mode.is_none());
}

#[test]
fn wasm_tool_invalid_json_schema_falls_back_to_default() {
    use grain_plugin_wasm::{ToolDef, WasmTool};
    let rt = std::sync::Arc::new(WasmPluginRuntime::new().unwrap());
    let td = ToolDef {
        name: "bad".to_string(),
        label: "Bad".to_string(),
        description: "Invalid JSON schema".to_string(),
        parameters_json: "not json!".to_string(),
    };
    let tool = WasmTool::new(rt, "test-plugin", &td);
    let def = grain_agent_core::AgentTool::definition(&tool);
    assert_eq!(def.parameters, serde_json::Value::Null);
}

// ---------------------------------------------------------------------------
// Integration: WasmConfig manifest parsing (via headless)
// ---------------------------------------------------------------------------

#[test]
fn plugin_manifest_parses_wasm_config() {
    let toml_str = r#"
name = "my-wasm-tool"
version = "1.0.0"

[wasm]
module = "my-tool.wasm"
capabilities = ["log", "http"]
"#;
    let manifest: grain_ai_agent_headless::PluginManifest =
        toml::from_str(toml_str).expect("parse manifest");
    assert_eq!(manifest.name, "my-wasm-tool");
    let wasm = manifest.wasm.expect("wasm config should be present");
    assert_eq!(wasm.module.to_str().unwrap(), "my-tool.wasm");
    assert_eq!(wasm.capabilities, vec!["log", "http"]);
}

#[test]
fn plugin_manifest_without_wasm_section_has_none() {
    let toml_str = r#"name = "plain""#;
    let manifest: grain_ai_agent_headless::PluginManifest =
        toml::from_str(toml_str).expect("parse");
    assert!(manifest.wasm.is_none());
}

#[test]
fn plugin_manifest_wasm_defaults() {
    let toml_str = r#"
name = "defaults"
[wasm]
"#;
    let manifest: grain_ai_agent_headless::PluginManifest =
        toml::from_str(toml_str).expect("parse");
    let wasm = manifest.wasm.unwrap();
    assert_eq!(wasm.module.to_str().unwrap(), "plugin.wasm");
    assert_eq!(wasm.capabilities, vec!["log"]);
}

// ---------------------------------------------------------------------------
// Integration: Plugin::wasm_module() resolves correctly
// ---------------------------------------------------------------------------

#[test]
fn plugin_wasm_module_returns_none_when_no_file() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("my-plugin");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("plugin.toml"), "name = \"my-plugin\"\n").unwrap();
    let plugins = grain_ai_agent_headless::discover_plugins(tmp.path());
    assert_eq!(plugins.len(), 1);
    assert!(plugins[0].wasm_module().is_none());
}

#[test]
fn plugin_wasm_module_returns_path_when_default_file_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("my-plugin");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("plugin.toml"), "name = \"my-plugin\"\n").unwrap();
    // Create a dummy plugin.wasm
    std::fs::write(plugin_dir.join("plugin.wasm"), b"fake wasm").unwrap();
    let plugins = grain_ai_agent_headless::discover_plugins(tmp.path());
    assert_eq!(plugins.len(), 1);
    let path = plugins[0].wasm_module().expect("should find plugin.wasm");
    assert!(path.ends_with("plugin.wasm"));
}

#[test]
fn plugin_wasm_module_uses_manifest_override() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("my-plugin");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        "name = \"my-plugin\"\n[wasm]\nmodule = \"custom.wasm\"\n",
    )
    .unwrap();
    std::fs::write(plugin_dir.join("custom.wasm"), b"fake wasm").unwrap();
    let plugins = grain_ai_agent_headless::discover_plugins(tmp.path());
    let path = plugins[0].wasm_module().expect("should find custom.wasm");
    assert!(path.ends_with("custom.wasm"));
}

#[test]
fn plugin_info_reports_wasm_presence() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("w");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("plugin.toml"), "name = \"w\"\n").unwrap();
    std::fs::write(plugin_dir.join("plugin.wasm"), b"fake").unwrap();
    let plugins = grain_ai_agent_headless::discover_plugins(tmp.path());
    let info = grain_ai_agent_headless::plugin_info(&plugins[0]);
    assert!(info.wasm);
}
