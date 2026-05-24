//! MCP adapter — lazy proxy tool for Model Context Protocol servers.
//!
//! Ported from `pi-mcp-adapter` by nicobailon.
//!
//! ## The Problem
//!
//! MCP tool definitions are verbose. A server with 30 tools can burn 10k+
//! tokens in the system prompt, paid whether or not those tools are used.
//!
//! ## The Solution
//!
//! One proxy tool (~200 tokens) instead of hundreds.  The agent discovers
//! tools on-demand via `search` and `describe`, then calls them via `tool`.
//! Servers connect lazily — only when a tool is first called.
//!
//! ## Configuration
//!
//! Set the `MCP_SERVERS` environment variable to a JSON array of server
//! configs.  Each entry accepts:
//!
//! ```json
//! [
//!   {
//!     "name": "github",
//!     "url": "https://api.githubcopilot.com/mcp/",
//!     "headers": { "Authorization": "Bearer ${GITHUB_TOKEN}" },
//!     "description": "GitHub API tools"
//!   }
//! ]
//! ```
//!
//! - `name` (required) — server identifier.
//! - `url`  (required) — StreamableHTTP endpoint.
//! - `headers` (optional) — extra HTTP headers; supports `${VAR}` interpolation.
//! - `description` (optional) — shown in the status listing.
//!
//! ## Usage
//!
//! | Mode     | Example                                                  |
//! |----------|----------------------------------------------------------|
//! | Status   | `mcp({})`                                                |
//! | Search   | `mcp({ search: "screenshot navigate" })`                 |
//! | Describe | `mcp({ describe: "tool_name" })`                         |
//! | Call     | `mcp({ tool: "tool_name", args: '{\"key\":\"val\"}' })` |
//! | Server   | `mcp({ server: "server_name" })`                         |
//! | Connect  | `mcp({ connect: "server_name" })`                        |

#![allow(clippy::all)]

wit_bindgen::generate!({
    world: "grain-plugin",
    path: "wit",
});

use grain::plugin::host::{self, LogLevel};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// JSON Schema for the single `mcp` proxy tool.  ~200 tokens.
// ---------------------------------------------------------------------------

const MCP_TOOL_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "tool": {
      "type": "string",
      "description": "Tool name to call (e.g., 'github_search_repositories')"
    },
    "args": {
      "type": "string",
      "description": "Arguments as JSON string (e.g., '{\"key\": \"value\"}')"
    },
    "connect": {
      "type": "string",
      "description": "Connect to a specific server by name"
    },
    "describe": {
      "type": "string",
      "description": "Show detailed description and parameter schema for a tool"
    },
    "search": {
      "type": "string",
      "description": "Space-separated search terms — finds matching tools by name/description"
    },
    "server": {
      "type": "string",
      "description": "List tools from a specific server (also disambiguates tool calls)"
    }
  }
}"#;

const MCP_TOOL_DESCRIPTION: &str =
    "MCP gateway — use `search` to find tools by keyword across configured MCP servers, \
`describe` to inspect a tool's parameters (returns JSON Schema), \
`tool` + `args` (JSON string) to invoke. \
Call with no arguments for server status. \
Use `server` to scope operations to one server, `connect` to explicitly connect.";

// ---------------------------------------------------------------------------
// Persistent state (survives across tool calls within a session).
// ---------------------------------------------------------------------------

struct McpState {
    servers: Vec<ServerConfig>,
    tool_cache: HashMap<String, Vec<CachedTool>>,
}

#[derive(Clone, Debug, Deserialize)]
struct ServerConfig {
    name: String,
    url: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedTool {
    name: String,
    description: Option<String>,
    input_schema: serde_json::Value,
}

// Global mutable state — safe under single-threaded WASM.
static STATE: Mutex<Option<McpState>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Plugin entry points.
// ---------------------------------------------------------------------------

struct McpAdapter;

impl exports::grain::plugin::plugin::Guest for McpAdapter {
    fn init() -> Result<exports::grain::plugin::plugin::PluginInfo, String> {
        host::log(LogLevel::Info, "mcp-adapter: init");

        let servers = load_server_configs();
        let count = servers.len();

        let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        *state = Some(McpState {
            servers,
            tool_cache: HashMap::new(),
        });

        host::log(LogLevel::Info, &format!(
            "mcp-adapter: ready with {} server config(s)", count
        ));

        Ok(exports::grain::plugin::plugin::PluginInfo {
            name: "mcp-adapter".to_string(),
            version: "0.1.0".to_string(),
        })
    }

    fn list_tools() -> Vec<exports::grain::plugin::plugin::ToolDef> {
        vec![exports::grain::plugin::plugin::ToolDef {
            name: "mcp".to_string(),
            label: "MCP".to_string(),
            description: MCP_TOOL_DESCRIPTION.to_string(),
            parameters_json: MCP_TOOL_SCHEMA.to_string(),
        }]
    }

    fn call_tool(name: String, args_json: String) -> exports::grain::plugin::plugin::ToolResult {
        if name != "mcp" {
            return err_result(format!("unknown tool: {name}"));
        }

        let params: McpParams = match serde_json::from_str(&args_json) {
            Ok(p) => p,
            Err(e) => return err_result(format!("parse args: {e}")),
        };

        // Acquire mutable state.
        let mut guard = match STATE.lock() {
            Ok(g) => g,
            Err(e) => return err_result(format!("lock: {e}")),
        };
        let state: &mut McpState = match guard.as_mut() {
            Some(s) => s,
            None => return err_result("MCP adapter not initialized"),
        };

        // Route based on which fields are present.
        if let Some(tool) = params.tool {
            let server = params.server;
            return execute_tool_call(state, &tool, params.args.as_deref(), server.as_deref());
        }
        if let Some(connect) = params.connect {
            return execute_connect(state, &connect);
        }
        if let Some(describe) = params.describe {
            return execute_describe(state, &describe);
        }
        if let Some(search) = params.search {
            return execute_search(state, &search, params.server.as_deref());
        }
        if let Some(server) = params.server {
            return execute_list_server(state, &server);
        }
        execute_status(state)
    }
}

// ---------------------------------------------------------------------------
// Tool argument shape.
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct McpParams {
    tool: Option<String>,
    args: Option<String>,
    connect: Option<String>,
    describe: Option<String>,
    search: Option<String>,
    server: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn err_result(msg: impl Into<String>) -> exports::grain::plugin::plugin::ToolResult {
    let msg = msg.into();
    host::log(LogLevel::Error, &msg);
    exports::grain::plugin::plugin::ToolResult {
        content_json: serde_json::json!({ "error": msg }).to_string(),
        is_error: true,
    }
}

fn ok_result(value: impl Serialize) -> exports::grain::plugin::plugin::ToolResult {
    match serde_json::to_string(&value) {
        Ok(s) => exports::grain::plugin::plugin::ToolResult {
            content_json: s,
            is_error: false,
        },
        Err(e) => err_result(format!("serialize result: {e}")),
    }
}

fn load_server_configs() -> Vec<ServerConfig> {
    let raw = host::env_get("MCP_SERVERS").unwrap_or_default();
    if raw.is_empty() {
        host::log(LogLevel::Warn, "MCP_SERVERS not set — no MCP servers configured");
        return Vec::new();
    }
    match serde_json::from_str::<Vec<ServerConfig>>(&raw) {
        Ok(servers) => {
            host::log(LogLevel::Info, &format!(
                "mcp-adapter: loaded {} MCP server config(s)", servers.len()
            ));
            servers
        }
        Err(e) => {
            host::log(LogLevel::Error, &format!("mcp-adapter: parse MCP_SERVERS: {e}"));
            Vec::new()
        }
    }
}

/// Expand `${VAR}` references in a header value.
fn interpolate_env(s: &str) -> String {
    let mut result = s.to_string();
    let mut pos = 0;
    while let Some(start) = result[pos..].find("${") {
        let abs_start = pos + start;
        if let Some(end) = result[abs_start + 2..].find('}') {
            let var_name = &result[abs_start + 2..abs_start + 2 + end];
            let val = host::env_get(var_name).unwrap_or_default();
            result.replace_range(abs_start..abs_start + 2 + end + 1, &val);
            pos = abs_start + val.len();
        } else {
            break;
        }
    }
    result
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC client for StreamableHTTP MCP.
// ---------------------------------------------------------------------------

static mut NEXT_ID: u32 = 1;

fn next_id() -> u32 {
    unsafe {
        let id = NEXT_ID;
        NEXT_ID = id.wrapping_add(1);
        id
    }
}

fn rpc_call(
    url: &str,
    headers: &HashMap<String, String>,
    method: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = next_id();
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let body = request.to_string();

    let mut hdrs: Vec<(String, String)> = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    for (k, v) in headers {
        hdrs.push((k.clone(), interpolate_env(v)));
    }

    host::log(
        LogLevel::Debug,
        &format!("MCP POST {} {}", url, &body[..body.len().min(400)]),
    );

    let resp = host::http_post(url, &hdrs, &body)
        .map_err(|e| format!("HTTP POST to {url}: {e}"))?;

    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "HTTP {} from {url} — body: {}",
            resp.status,
            truncate_str(&resp.body, 256)
        ));
    }

    let parsed: serde_json::Value = serde_json::from_str(&resp.body).map_err(|e| {
        format!(
            "JSON parse from {url}: {e} — body: {}",
            truncate_str(&resp.body, 200)
        )
    })?;

    if let Some(err) = parsed.get("error") {
        return Err(format!("JSON-RPC error from {}: {}", url, err));
    }

    Ok(parsed.get("result").cloned().unwrap_or(serde_json::Value::Null))
}

fn rpc_notify(
    url: &str,
    headers: &HashMap<String, String>,
    method: &str,
    params: &serde_json::Value,
) -> Result<(), String> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    let body = request.to_string();

    let mut hdrs: Vec<(String, String)> = vec![(
        "Content-Type".to_string(),
        "application/json".to_string(),
    )];
    for (k, v) in headers {
        hdrs.push((k.clone(), interpolate_env(v)));
    }

    let resp = host::http_post(url, &hdrs, &body)
        .map_err(|e| format!("HTTP POST (notify) to {url}: {e}"))?;

    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "HTTP {} from {url} (notify) — body: {}",
            resp.status,
            truncate_str(&resp.body, 256)
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MCP protocol operations.
// ---------------------------------------------------------------------------

fn mcp_initialize(server: &ServerConfig) -> Result<(), String> {
    let params = serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {
            "name": "grain-mcp-adapter",
            "version": "0.1.0"
        }
    });
    let result = rpc_call(&server.url, &server.headers, "initialize", &params)?;
    host::log(LogLevel::Info, &format!(
        "mcp-adapter: initialize {} — capabilities: {}",
        server.name,
        result.get("capabilities").map_or("none", |c| c.as_str().unwrap_or("?"))
    ));
    // Send initialized notification.
    let _ = rpc_notify(
        &server.url,
        &server.headers,
        "notifications/initialized",
        &serde_json::json!({}),
    );
    Ok(())
}

fn mcp_list_tools(server: &ServerConfig) -> Result<Vec<CachedTool>, String> {
    let result = rpc_call(
        &server.url,
        &server.headers,
        "tools/list",
        &serde_json::json!({}),
    )?;
    let tools = result
        .get("tools")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("tools/list response missing 'tools' array: {}", result))?;

    let mut out = Vec::new();
    for t in tools {
        let name = t
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let description = t
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let input_schema = t.get("inputSchema").cloned().unwrap_or(serde_json::json!({
            "type": "object",
            "properties": {}
        }));
        out.push(CachedTool {
            name,
            description,
            input_schema,
        });
    }
    Ok(out)
}

fn mcp_call_tool(
    server: &ServerConfig,
    tool_name: &str,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let params = serde_json::json!({
        "name": tool_name,
        "arguments": arguments,
    });
    rpc_call(&server.url, &server.headers, "tools/call", &params)
}

// ---------------------------------------------------------------------------
// Lazy connect — initialize + fetch tools, cache them.
// ---------------------------------------------------------------------------

fn lazy_connect(state: &mut McpState, server_name: &str) -> Result<(), String> {
    if state.tool_cache.contains_key(server_name) {
        return Ok(()); // already connected
    }

    let server = state
        .servers
        .iter()
        .find(|s| s.name == server_name)
        .ok_or_else(|| {
            format!(
                "Server '{server_name}' not found. Available: {}",
                state
                    .servers
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?
        .clone();

    host::log(LogLevel::Info, &format!("mcp-adapter: connecting to {server_name}..."));
    mcp_initialize(&server)?;
    let tools = mcp_list_tools(&server)?;
    host::log(LogLevel::Info, &format!(
        "mcp-adapter: {server_name} — {} tools cached",
        tools.len()
    ));
    state.tool_cache.insert(server_name.to_string(), tools);
    Ok(())
}

// ---------------------------------------------------------------------------
// Execution modes.
// ---------------------------------------------------------------------------

fn execute_status(state: &McpState) -> exports::grain::plugin::plugin::ToolResult {
    let mut servers_info = Vec::new();
    for s in &state.servers {
        let cached = state.tool_cache.get(&s.name);
        let tool_count = cached.map(|v| v.len()).unwrap_or(0);
        let mut info = serde_json::json!({
            "name": s.name,
            "url": s.url,
            "tool_count": tool_count,
            "status": if cached.is_some() { "connected" } else { "unconnected" },
        });
        if let Some(ref desc) = s.description {
            info["description"] = serde_json::Value::String(desc.clone());
        }
        servers_info.push(info);
    }

    if servers_info.is_empty() {
        return ok_result(serde_json::json!({
            "servers": [],
            "total_tools": 0,
            "hint": "No MCP servers configured. Set MCP_SERVERS env var with JSON array of {name, url} objects."
        }));
    }

    let total_tools: usize = servers_info
        .iter()
        .map(|s| s["tool_count"].as_u64().unwrap_or(0) as usize)
        .sum();

    ok_result(serde_json::json!({
        "servers": servers_info,
        "total_tools": total_tools,
        "hint": "Use search to find tools, describe to inspect parameters, tool+args to call. Pass server to scope."
    }))
}

fn execute_connect(state: &mut McpState, server_name: &str) -> exports::grain::plugin::plugin::ToolResult {
    match lazy_connect(state, server_name) {
        Ok(()) => {
            let tool_count = state
                .tool_cache
                .get(server_name)
                .map(|v| v.len())
                .unwrap_or(0);
            ok_result(serde_json::json!({
                "server": server_name,
                "status": "connected",
                "tool_count": tool_count,
            }))
        }
        Err(e) => err_result(e),
    }
}

fn execute_search(
    state: &mut McpState,
    query: &str,
    server_filter: Option<&str>,
) -> exports::grain::plugin::plugin::ToolResult {
    let terms_lower: Vec<String> = query
        .split_whitespace()
        .map(|s| s.to_lowercase())
        .collect();

    let mut results = Vec::new();
    let mut unconnected_checked = Vec::new();

    // Auto-connect if a server filter is specified and it's not connected.
    if let Some(f) = server_filter {
        if !state.tool_cache.contains_key(f) && state.servers.iter().any(|s| s.name == f) {
            if let Err(e) = lazy_connect(state, f) {
                return err_result(format!("auto-connect {f}: {e}"));
            }
        }
    }

    for server in &state.servers.clone() {
        if let Some(f) = server_filter {
            if server.name != f {
                continue;
            }
        }

        // If not cached and no explicit filter, note as unconnected.
        if !state.tool_cache.contains_key(&server.name) {
            unconnected_checked.push(server.name.clone());
            continue;
        }

        let tools = &state.tool_cache[&server.name];
        for t in tools {
            let desc = t.description.as_deref().unwrap_or("");
            let searchable = format!("{} {}", t.name.to_lowercase(), desc.to_lowercase());

            let matches = terms_lower
                .iter()
                .all(|term| searchable.contains(term.as_str()));
            if matches {
                results.push(serde_json::json!({
                    "name": format!("{}_{}", server.name, t.name),
                    "server": server.name,
                    "original_name": t.name,
                    "description": t.description,
                }));
            }
        }
    }

    let mut hint = String::new();
    if results.is_empty() && !unconnected_checked.is_empty() {
        hint = format!(
            "No results found across connected servers. {} unconnected server(s): {}. \
Try connecting: mcp({{connect: \"{}\"}})",
            unconnected_checked.len(),
            unconnected_checked.join(", "),
            unconnected_checked[0],
        );
    } else if results.is_empty() {
        hint = format!("No tools matching '{query}' across connected servers.");
    }

    ok_result(serde_json::json!({
        "query": query,
        "results": results,
        "count": results.len(),
        "unconnected_servers": unconnected_checked,
        "hint": hint,
    }))
}

fn execute_describe(state: &mut McpState, tool_name: &str) -> exports::grain::plugin::plugin::ToolResult {
    // tool_name can be either "server_tool" (prefixed) or just "tool".
    // Auto-connect servers we haven't cached yet.
    let server_names: Vec<String> = state.servers.iter().map(|s| s.name.clone()).collect();
    for sname in &server_names {
        if !state.tool_cache.contains_key(sname) {
            let _ = lazy_connect(state, sname);
        }
    }

    for server in &state.servers {
        let cached = match state.tool_cache.get(&server.name) {
            Some(c) => c,
            None => continue,
        };

        for t in cached {
            let prefixed = format!("{}_{}", server.name, t.name);
            if t.name == tool_name || prefixed == tool_name {
                return ok_result(serde_json::json!({
                    "name": prefixed,
                    "server": server.name,
                    "original_name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                }));
            }
        }
    }

    err_result(format!(
        "Tool '{tool_name}' not found. Try searching first with mcp({{search: \"...\"}}). \
Ensure servers are connected."
    ))
}

fn execute_list_server(state: &mut McpState, server_name: &str) -> exports::grain::plugin::plugin::ToolResult {
    // Auto-connect if not yet cached.
    if !state.tool_cache.contains_key(server_name) {
        if let Err(e) = lazy_connect(state, server_name) {
            return err_result(e);
        }
    }

    let cached = match state.tool_cache.get(server_name) {
        Some(c) => c,
        None => return err_result(format!("Server '{server_name}' not found.")),
    };

    let tools: Vec<serde_json::Value> = cached
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": format!("{}_{}", server_name, t.name),
                "description": t.description,
            })
        })
        .collect();

    ok_result(serde_json::json!({
        "server": server_name,
        "tool_count": tools.len(),
        "tools": tools,
    }))
}

fn execute_tool_call(
    state: &mut McpState,
    tool_name: &str,
    args_json: Option<&str>,
    server_filter: Option<&str>,
) -> exports::grain::plugin::plugin::ToolResult {
    // Parse arguments.
    let arguments: serde_json::Value = match args_json {
        Some(s) if !s.is_empty() => match serde_json::from_str(s) {
            Ok(v) => v,
            Err(e) => return err_result(format!("Invalid args JSON: {e}")),
        },
        _ => serde_json::json!({}),
    };

    // If server_filter given, auto-connect that server.
    if let Some(f) = server_filter {
        if !state.tool_cache.contains_key(f) {
            if let Err(e) = lazy_connect(state, f) {
                return err_result(format!("auto-connect {f}: {e}"));
            }
        }
    }

    // Resolve server and original tool name.
    for server in state.servers.clone() {
        if let Some(f) = server_filter {
            if server.name != f {
                continue;
            }
        }

        // Auto-connect uncached servers.
        if !state.tool_cache.contains_key(&server.name) {
            if let Err(e) = lazy_connect(state, &server.name) {
                host::log(LogLevel::Warn, &format!("auto-connect {}: {e}", server.name));
                continue;
            }
        }

        let cached = match state.tool_cache.get(&server.name) {
            Some(c) => c,
            None => continue,
        };

        // Match: bare name or prefixed "server_tool".
        let found = cached
            .iter()
            .find(|t| {
                t.name == tool_name
                    || format!("{}_{}", server.name, t.name) == tool_name
            });

        if let Some(tool) = found {
            let original_name = tool.name.clone();
            let result = match mcp_call_tool(&server, &original_name, &arguments) {
                Ok(r) => r,
                Err(e) => return err_result(format!("Call {tool_name} on {}: {e}", server.name)),
            };

            return ok_result(serde_json::json!({
                "server": server.name,
                "tool": original_name,
                "result": result,
            }));
        }
    }

    // Not found anywhere.
    let unconnected: Vec<&str> = state
        .servers
        .iter()
        .filter(|s| !state.tool_cache.contains_key(&s.name))
        .map(|s| s.name.as_str())
        .collect();

    let connected: Vec<&str> = state
        .servers
        .iter()
        .filter(|s| state.tool_cache.contains_key(&s.name))
        .map(|s| s.name.as_str())
        .collect();

    if !unconnected.is_empty() {
        return err_result(format!(
            "Tool '{tool_name}' not found. Connected servers: [{}]. Unconnected: [{}]. \
Try connecting: mcp({{connect: \"{}\"}})",
            connected.join(", "),
            unconnected.join(", "),
            unconnected[0],
        ));
    }

    err_result(format!(
        "Tool '{tool_name}' not found on any connected server: [{}]",
        connected.join(", ")
    ))
}

// ---------------------------------------------------------------------------
// Generate guest glue.
// ---------------------------------------------------------------------------
export!(McpAdapter);
