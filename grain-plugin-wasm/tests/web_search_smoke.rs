// Quick smoke: load web-search plugin and verify both tools are registered.
// Also call web_search with a dummy key to check the error path.
use std::collections::HashMap;
use std::path::Path;
use grain_plugin_wasm::{Capabilities, WasmPluginRuntime};

#[tokio::test(flavor = "multi_thread")]
async fn web_search_tools_registered_and_callable() {
    let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../web-search/plugin.wasm");
    // Fallback to the original location
    let wasm = if wasm.exists() {
        wasm
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/web-search/target/wasm32-wasip1/release/web_search_plugin.wasm")
    };
    if !wasm.exists() {
        eprintln!("SKIP: wasm not found at {}", wasm.display());
        return;
    }
    eprintln!("Loading wasm from {}", wasm.display());

    let rt = std::sync::Arc::new(WasmPluginRuntime::new().unwrap());
    let loaded = rt
        .load(
            &wasm,
            "web-search",
            Capabilities {
                log: false,
                env: true,
                http: true,
            },
            "web-search",
            // Set a dummy EXA_API_KEY so the tool won't panic on env lookup
            HashMap::from([
                ("EXA_API_KEY".to_string(), "dummy-key-for-test".to_string()),
            ]),
        )
        .await
        .expect("load plugin");

    eprintln!("Plugin loaded: name={} version={}", loaded.info.name, loaded.info.version);
    eprintln!("Tools ({}):", loaded.tool_defs.len());
    for td in &loaded.tool_defs {
        eprintln!("  {} — {}", td.name, td.label);
    }

    // Verify both tools exist
    let names: Vec<&str> = loaded.tool_defs.iter().map(|td| td.name.as_str()).collect();
    assert!(names.contains(&"web_search"), "web_search tool missing: {names:?}");
    assert!(names.contains(&"web_fetch"), "web_fetch tool missing: {names:?}");
    eprintln!("Both tools registered ✓");

    // Call web_search — should fail with a clear error about auth, not crash
    let search_def = loaded.tool_defs.iter().find(|td| td.name == "web_search").unwrap();
    let tool = grain_plugin_wasm::WasmTool::new(rt.clone(), "web-search", search_def);

    use grain_agent_core::AgentTool;
    use tokio_util::sync::CancellationToken;

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tool.execute(
            "search",
            serde_json::json!({
                "provider": "exa",
                "query": "test query"
            }),
            CancellationToken::new(),
            std::sync::Arc::new(|_: grain_agent_core::AgentToolResult| {}),
        ),
    )
    .await
    .expect("web_search call timed out")
    .unwrap();

    let body = result
        .content
        .iter()
        .filter_map(|c| match c {
            grain_agent_core::UserContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    eprintln!("web_search result: {body}");
    // Should return an error (because the API key is dummy), not crash.
    assert!(body.contains("error") || body.contains("Error") || body.contains("unauthorized") || body.contains("401") || body.contains("403"),
        "Expected error response but got: {body}");
    eprintln!("web_search error handling works ✓");
}

fn main() {
    // allow running via `cargo test --test ...`
    println!("Use #[tokio::test] mode");
}
