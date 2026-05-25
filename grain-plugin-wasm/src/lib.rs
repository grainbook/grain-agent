//! WebAssembly Component Model plugin runtime for grain.
//!
//! Loads `.wasm` files compiled against the `grain:plugin` WIT world,
//! instantiates them via `wasmtime`, and wraps each plugin-declared
//! tool as a [`grain_agent_core::AgentTool`] implementation.
//!
//! # Architecture
//!
//! ```text
//! .grain/plugins/my-tool/
//! +-- plugin.toml          # manifest (extended with [wasm])
//! +-- plugin.wasm          # compiled Component Model module
//! ```
//!
//! The host provides logging, env-var access, and HTTP primitives.
//! Each host function is gated by the plugin's declared capabilities
//! in `plugin.toml` — calls into a denied capability return an error
//! to the guest.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod tool;

pub use tool::WasmTool;

/// Host log callback. Called for every `log` host import the guest
/// invokes (after the `log` capability gate). Arguments: severity tag
/// (`"debug" | "info" | "warn" | "error"`), plugin name, message.
///
/// When unset, the runtime falls back to `eprintln!` — fine for
/// headless / CLI use, but the TUI MUST install a sink that routes
/// to its event channel; otherwise plugin log writes clobber the
/// alt-screen.
pub type LogSink = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

// ---------------------------------------------------------------------------
// Bindgen: generate Rust types + trait from the WIT contract
// ---------------------------------------------------------------------------

wasmtime::component::bindgen!({
    path: "wit/grain-plugin.wit",
    world: "grain-plugin",
});

mod v2_bindings {
    wasmtime::component::bindgen!({
        path: "wit/grain-plugin.wit",
        world: "grain-plugin-v2",
    });
}

// Re-export the generated types used by the tool adapter and callers.
// The bindgen generates modules mirroring the WIT package path:
//   exports::grain::plugin::plugin::{ToolDef, ToolResult, PluginInfo, Guest}
//   grain::plugin::host::{Host, LogLevel, HttpResponse}
pub use exports::grain::plugin::plugin as wit_plugin;
pub use grain::plugin::host as wit_host;

use v2_bindings::exports::grain::plugin::orchestration as wit_orchestration;

const HOST_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Host state carried in the wasmtime Store
// ---------------------------------------------------------------------------

/// Per-plugin capabilities the host enforces.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub log: bool,
    pub env: bool,
    pub http: bool,
    pub role_orchestration: bool,
}

impl Capabilities {
    /// Parse from a list of capability strings (e.g. `["log", "env", "http"]`).
    pub fn from_list(caps: &[String]) -> Self {
        let set: HashSet<&str> = caps.iter().map(|s| s.as_str()).collect();
        Capabilities {
            log: set.contains("log"),
            env: set.contains("env"),
            http: set.contains("http"),
            role_orchestration: set.contains("role-orchestration"),
        }
    }
}

/// State stored in the wasmtime `Store<T>`.
pub struct PluginState {
    wasi_ctx: WasiCtx,
    table: ResourceTable,
    capabilities: Capabilities,
    plugin_name: String,
    /// Tokio runtime handle for running async HTTP inside sync host fns.
    rt_handle: tokio::runtime::Handle,
    /// Optional sink for `log` host imports. When `None`, the host
    /// falls back to `eprintln!` — see [`LogSink`].
    log_sink: Option<LogSink>,
    /// Per-plugin env vars (from plugin.toml + plugin.toml).
    env_map: HashMap<String, String>,
}

impl WasiView for PluginState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.table,
        }
    }
}

// ---------------------------------------------------------------------------
// Host trait implementation
// ---------------------------------------------------------------------------

impl wit_host::Host for PluginState {
    fn log(&mut self, level: wit_host::LogLevel, msg: String) {
        if !self.capabilities.log {
            return;
        }
        let tag = match level {
            wit_host::LogLevel::Debug => "debug",
            wit_host::LogLevel::Info => "info",
            wit_host::LogLevel::Warn => "warn",
            wit_host::LogLevel::Error => "error",
        };
        if let Some(sink) = &self.log_sink {
            sink(tag, &self.plugin_name, &msg);
        } else {
            // No sink installed — fall back to stderr. In TUI mode this
            // would clobber the alt-screen, so the TUI is expected to
            // build the runtime with `WasmPluginRuntime::with_log_sink`.
            eprintln!("[{tag}] wasm plugin '{}': {msg}", self.plugin_name);
        }
    }

    fn env_get(&mut self, key: String) -> Option<String> {
        if !self.capabilities.env {
            return None;
        }
        // Per-plugin env map takes priority over OS env vars.
        if let Some(val) = self.env_map.get(&key) {
            return Some(val.clone());
        }
        std::env::var(&key).ok()
    }

    fn http_get(
        &mut self,
        url: String,
        headers: Vec<(String, String)>,
    ) -> Result<wit_host::HttpResponse, String> {
        if !self.capabilities.http {
            return Err("http capability not granted".into());
        }
        self.rt_handle
            .block_on(async { do_http_request("GET", &url, &headers, None).await })
    }

    fn http_post(
        &mut self,
        url: String,
        headers: Vec<(String, String)>,
        body: String,
    ) -> Result<wit_host::HttpResponse, String> {
        if !self.capabilities.http {
            return Err("http capability not granted".into());
        }
        self.rt_handle
            .block_on(async { do_http_request("POST", &url, &headers, Some(&body)).await })
    }
}

impl v2_bindings::grain::plugin::host::Host for PluginState {
    fn log(&mut self, level: v2_bindings::grain::plugin::host::LogLevel, msg: String) {
        if !self.capabilities.log {
            return;
        }
        let tag = match level {
            v2_bindings::grain::plugin::host::LogLevel::Debug => "debug",
            v2_bindings::grain::plugin::host::LogLevel::Info => "info",
            v2_bindings::grain::plugin::host::LogLevel::Warn => "warn",
            v2_bindings::grain::plugin::host::LogLevel::Error => "error",
        };
        if let Some(sink) = &self.log_sink {
            sink(tag, &self.plugin_name, &msg);
        } else {
            eprintln!("[{tag}] wasm plugin '{}': {msg}", self.plugin_name);
        }
    }

    fn env_get(&mut self, key: String) -> Option<String> {
        if !self.capabilities.env {
            return None;
        }
        if let Some(val) = self.env_map.get(&key) {
            return Some(val.clone());
        }
        std::env::var(&key).ok()
    }

    fn http_get(
        &mut self,
        url: String,
        headers: Vec<(String, String)>,
    ) -> Result<v2_bindings::grain::plugin::host::HttpResponse, String> {
        if !self.capabilities.http {
            return Err("http capability not granted".into());
        }
        let response = self
            .rt_handle
            .block_on(async { do_http_request("GET", &url, &headers, None).await })?;
        Ok(v2_bindings::grain::plugin::host::HttpResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }

    fn http_post(
        &mut self,
        url: String,
        headers: Vec<(String, String)>,
        body: String,
    ) -> Result<v2_bindings::grain::plugin::host::HttpResponse, String> {
        if !self.capabilities.http {
            return Err("http capability not granted".into());
        }
        let response = self
            .rt_handle
            .block_on(async { do_http_request("POST", &url, &headers, Some(&body)).await })?;
        Ok(v2_bindings::grain::plugin::host::HttpResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }
}

async fn do_http_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Result<wit_host::HttpResponse, String> {
    let mut client = reqwest::Client::builder().timeout(HOST_HTTP_TIMEOUT);
    if should_bypass_proxy_for_url(url) {
        client = client.no_proxy();
    }
    let client = client.build().map_err(|e| e.to_string())?;
    let mut builder = match method {
        "POST" => client.post(url),
        _ => client.get(url),
    };
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    if let Some(b) = body {
        builder = builder.body(b.to_string());
    }
    let resp = builder.send().await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let resp_headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let resp_body = resp.text().await.map_err(|e| e.to_string())?;
    Ok(wit_host::HttpResponse {
        status,
        headers: resp_headers,
        body: resp_body,
    })
}

fn should_bypass_proxy_for_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum WasmPluginError {
    #[error("wasmtime: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("plugin init failed: {0}")]
    InitFailed(String),
    #[error("tool call failed: {0}")]
    ToolCallFailed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Loaded plugin
// ---------------------------------------------------------------------------

/// A successfully loaded WASM plugin.
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub info: PluginInfo,
    pub tool_defs: Vec<ToolDef>,
    pub orchestration: Option<OrchestrationDef>,
}

/// Plugin metadata (mirrors the WIT `plugin-info` record but is
/// owned / cloneable for storage outside the store).
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
}

/// Tool definition (mirrors the WIT `tool-def` record, owned).
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters_json: String,
}

/// Optional orchestration metadata exported by v2 plugins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrchestrationDef {
    pub roles: Vec<RoleDef>,
    pub hooks: Vec<HookDef>,
}

/// One model-role slot declared by a v2 plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleDef {
    pub name: String,
    pub model: String,
    pub prompt: String,
    pub tools: Vec<String>,
    pub thinking_level: Option<String>,
}

/// One hook subscription declared by a v2 plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDef {
    pub point: HookPoint,
    pub name: String,
}

/// Lifecycle point names supported by the v2 orchestration surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    BeforeAgentStart,
    AfterToolCall,
    PrepareNextTurn,
    ShouldStopAfterTurn,
}

/// Host action requested by a v2 plugin hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAction {
    SwitchRole(String),
    SwitchModel(String),
    SetSystemPrompt(String),
    SetActiveTools(Vec<String>),
    InjectUserMessage(String),
    StopAfterTurn(bool),
    EmitCustom(String),
    SetUiHeader(UiHeader),
    SetUiStatus(String),
}

/// Optional UI header override requested by a v2 plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiHeader {
    pub provider: Option<String>,
    pub model: Option<String>,
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// Owns the wasmtime engine and can load + call plugins.
pub struct WasmPluginRuntime {
    engine: Engine,
    linker: Linker<PluginState>,
    linker_v2: Linker<PluginState>,
    /// Component entries — kept so we can re-instantiate per call.
    components: Mutex<Vec<PluginEntry>>,
    /// Optional host log sink. Cloned into every `PluginState` we
    /// build, so guest `log` imports route through one shared callback.
    log_sink: Option<LogSink>,
}

struct PluginEntry {
    id: String,
    component: Component,
    capabilities: Capabilities,
    plugin_name: String,
    /// Per-plugin env vars (merged from plugin.toml + plugin.toml).
    env_map: HashMap<String, String>,
}

impl std::fmt::Debug for WasmPluginRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmPluginRuntime").finish_non_exhaustive()
    }
}

impl WasmPluginRuntime {
    /// Create a new runtime with a fresh wasmtime engine.
    pub fn new() -> Result<Self, WasmPluginError> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;
        let mut linker = Linker::<PluginState>::new(&engine);
        // Link WASI + our host interface.
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        GrainPlugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)?;
        let mut linker_v2 = Linker::<PluginState>::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker_v2)?;
        v2_bindings::GrainPluginV2::add_to_linker::<_, HasSelf<_>>(&mut linker_v2, |s| s)?;
        Ok(WasmPluginRuntime {
            engine,
            linker,
            linker_v2,
            components: Mutex::new(Vec::new()),
            log_sink: None,
        })
    }

    /// Install a [`LogSink`]. Builder form so callers can chain on
    /// the result of `new()`. The sink is cloned into every plugin
    /// instance the runtime builds — set it before loading plugins.
    pub fn with_log_sink(mut self, sink: LogSink) -> Self {
        self.log_sink = Some(sink);
        self
    }

    /// Load a `.wasm` component file. Calls the plugin's `init`
    /// export and returns metadata + tool definitions.
    pub async fn load(
        &self,
        path: &Path,
        plugin_id: &str,
        capabilities: Capabilities,
        plugin_name: &str,
        env_map: HashMap<String, String>,
    ) -> Result<LoadedPlugin, WasmPluginError> {
        let wasm_bytes = tokio::fs::read(path).await?;
        let component = Component::new(&self.engine, &wasm_bytes)?;

        // Create a store for the init + list-tools calls.
        let wasi = WasiCtxBuilder::new().build();
        let state = PluginState {
            wasi_ctx: wasi,
            table: ResourceTable::new(),
            capabilities: capabilities.clone(),
            plugin_name: plugin_name.to_string(),
            rt_handle: tokio::runtime::Handle::current(),
            log_sink: self.log_sink.clone(),
            env_map: env_map.clone(),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings = GrainPlugin::instantiate(&mut store, &component, &self.linker)?;

        // Call init via the exported `plugin` interface.
        let guest = bindings.grain_plugin_plugin();
        let info_raw = guest
            .call_init(&mut store)?
            .map_err(WasmPluginError::InitFailed)?;
        let info = PluginInfo {
            name: info_raw.name,
            version: info_raw.version,
        };

        // Call list-tools.
        let tools_raw = guest.call_list_tools(&mut store)?;
        let tool_defs: Vec<ToolDef> = tools_raw
            .into_iter()
            .map(|t| ToolDef {
                name: t.name,
                label: t.label,
                description: t.description,
                parameters_json: t.parameters_json,
            })
            .collect();

        let orchestration = self
            .load_orchestration_metadata(&component, &capabilities, plugin_name, &env_map)
            .await?;

        // Stash the component for later call-tool / hook invocations.
        self.components.lock().await.push(PluginEntry {
            id: plugin_id.to_string(),
            component,
            capabilities,
            plugin_name: plugin_name.to_string(),
            env_map,
        });

        Ok(LoadedPlugin {
            info,
            tool_defs,
            orchestration,
        })
    }

    async fn load_orchestration_metadata(
        &self,
        component: &Component,
        capabilities: &Capabilities,
        plugin_name: &str,
        env_map: &HashMap<String, String>,
    ) -> Result<Option<OrchestrationDef>, WasmPluginError> {
        if !capabilities.role_orchestration {
            return Ok(None);
        }
        let wasi = WasiCtxBuilder::new().build();
        let state = PluginState {
            wasi_ctx: wasi,
            table: ResourceTable::new(),
            capabilities: capabilities.clone(),
            plugin_name: plugin_name.to_string(),
            rt_handle: tokio::runtime::Handle::current(),
            log_sink: self.log_sink.clone(),
            env_map: env_map.clone(),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings =
            v2_bindings::GrainPluginV2::instantiate(&mut store, component, &self.linker_v2)?;
        let guest = bindings.grain_plugin_orchestration();
        let roles = guest
            .call_list_roles(&mut store)?
            .into_iter()
            .map(role_from_wit)
            .collect();
        let hooks = guest
            .call_list_hooks(&mut store)?
            .into_iter()
            .map(hook_from_wit)
            .collect();
        Ok(Some(OrchestrationDef { roles, hooks }))
    }

    /// Call a tool on a loaded plugin from async code. Creates a fresh
    /// Store per call (isolation — no shared mutable state between
    /// invocations).
    ///
    /// This wrapper is intended for lightweight direct callers. The
    /// [`WasmTool`] adapter uses [`Self::call_tool_blocking`] inside
    /// `tokio::task::spawn_blocking` so wasmtime execution never
    /// blocks the async agent loop.
    ///
    /// `host_rt_handle` is the runtime handle that the host imports
    /// (http-get/http-post) will use to drive async work.
    pub async fn call_tool(
        &self,
        plugin_id: &str,
        tool_name: &str,
        args_json: &str,
        host_rt_handle: tokio::runtime::Handle,
    ) -> Result<CallToolResult, WasmPluginError> {
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    self.call_tool_blocking(plugin_id, tool_name, args_json, host_rt_handle)
                })
                .join()
                .map_err(|_| WasmPluginError::ToolCallFailed("tool call panicked".into()))?
        })
    }

    /// Synchronous tool call used from a plain blocking thread.
    ///
    /// This is the safe path for sync wasmtime guest execution plus
    /// sync host imports that need to run async HTTP. The caller must
    /// invoke it outside a Tokio runtime context; then `http-get` /
    /// `http-post` can use `host_rt_handle.block_on(...)` without
    /// tripping Tokio's nested-runtime guard.
    pub fn call_tool_blocking(
        &self,
        plugin_id: &str,
        tool_name: &str,
        args_json: &str,
        host_rt_handle: tokio::runtime::Handle,
    ) -> Result<CallToolResult, WasmPluginError> {
        let entries = self.components.blocking_lock();
        let entry = entries.iter().find(|e| e.id == plugin_id).ok_or_else(|| {
            WasmPluginError::ToolCallFailed(format!("plugin '{plugin_id}' not loaded"))
        })?;

        let wasi = WasiCtxBuilder::new().build();
        let state = PluginState {
            wasi_ctx: wasi,
            table: ResourceTable::new(),
            capabilities: entry.capabilities.clone(),
            plugin_name: entry.plugin_name.clone(),
            rt_handle: host_rt_handle,
            log_sink: self.log_sink.clone(),
            env_map: entry.env_map.clone(),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings = GrainPlugin::instantiate(&mut store, &entry.component, &self.linker)?;

        let guest = bindings.grain_plugin_plugin();

        // Must call init before call-tool (component starts fresh).
        let _ = guest.call_init(&mut store)?;

        let result = guest.call_call_tool(&mut store, tool_name, args_json)?;
        Ok(CallToolResult {
            content_json: result.content_json,
            is_error: result.is_error,
        })
    }

    /// Invoke a v2 orchestration hook on a loaded plugin.
    ///
    /// The returned actions are untrusted intent. Callers must validate
    /// model ids, role names, tools, and lifecycle timing before applying
    /// them to an agent or harness.
    pub async fn call_hook(
        &self,
        plugin_id: &str,
        point: HookPoint,
        context_json: &str,
        host_rt_handle: tokio::runtime::Handle,
    ) -> Result<Vec<HostAction>, WasmPluginError> {
        let point_raw = point_to_wit(point);
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    self.call_hook_blocking(plugin_id, point_raw, context_json, host_rt_handle)
                })
                .join()
                .map_err(|_| WasmPluginError::ToolCallFailed("hook call panicked".into()))?
        })
    }

    fn call_hook_blocking(
        &self,
        plugin_id: &str,
        point: wit_orchestration::HookPoint,
        context_json: &str,
        host_rt_handle: tokio::runtime::Handle,
    ) -> Result<Vec<HostAction>, WasmPluginError> {
        let entries = self.components.blocking_lock();
        let entry = entries.iter().find(|e| e.id == plugin_id).ok_or_else(|| {
            WasmPluginError::ToolCallFailed(format!("plugin '{plugin_id}' not loaded"))
        })?;
        if !entry.capabilities.role_orchestration {
            return Err(WasmPluginError::ToolCallFailed(format!(
                "plugin '{plugin_id}' does not have role-orchestration capability"
            )));
        }

        let wasi = WasiCtxBuilder::new().build();
        let state = PluginState {
            wasi_ctx: wasi,
            table: ResourceTable::new(),
            capabilities: entry.capabilities.clone(),
            plugin_name: entry.plugin_name.clone(),
            rt_handle: host_rt_handle,
            log_sink: self.log_sink.clone(),
            env_map: entry.env_map.clone(),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings =
            v2_bindings::GrainPluginV2::instantiate(&mut store, &entry.component, &self.linker_v2)?;
        let guest = bindings.grain_plugin_orchestration();
        let actions = guest
            .call_call_hook(&mut store, point, context_json)?
            .map_err(WasmPluginError::ToolCallFailed)?
            .into_iter()
            .map(action_from_wit)
            .collect();
        Ok(actions)
    }
}

/// Owned result from a tool call (mirrors the WIT `tool-result`).
#[derive(Debug, Clone)]
pub struct CallToolResult {
    pub content_json: String,
    pub is_error: bool,
}

fn role_from_wit(role: wit_orchestration::RoleDef) -> RoleDef {
    RoleDef {
        name: role.name,
        model: role.model,
        prompt: role.prompt,
        tools: role.tools,
        thinking_level: role.thinking_level,
    }
}

fn hook_from_wit(hook: wit_orchestration::HookDef) -> HookDef {
    HookDef {
        point: hook_point_from_wit(hook.point),
        name: hook.name,
    }
}

fn hook_point_from_wit(point: wit_orchestration::HookPoint) -> HookPoint {
    match point {
        wit_orchestration::HookPoint::BeforeAgentStart => HookPoint::BeforeAgentStart,
        wit_orchestration::HookPoint::AfterToolCall => HookPoint::AfterToolCall,
        wit_orchestration::HookPoint::PrepareNextTurn => HookPoint::PrepareNextTurn,
        wit_orchestration::HookPoint::ShouldStopAfterTurn => HookPoint::ShouldStopAfterTurn,
    }
}

fn point_to_wit(point: HookPoint) -> wit_orchestration::HookPoint {
    match point {
        HookPoint::BeforeAgentStart => wit_orchestration::HookPoint::BeforeAgentStart,
        HookPoint::AfterToolCall => wit_orchestration::HookPoint::AfterToolCall,
        HookPoint::PrepareNextTurn => wit_orchestration::HookPoint::PrepareNextTurn,
        HookPoint::ShouldStopAfterTurn => wit_orchestration::HookPoint::ShouldStopAfterTurn,
    }
}

fn action_from_wit(action: wit_orchestration::HostAction) -> HostAction {
    match action {
        wit_orchestration::HostAction::SwitchRole(role) => HostAction::SwitchRole(role),
        wit_orchestration::HostAction::SwitchModel(model) => HostAction::SwitchModel(model),
        wit_orchestration::HostAction::SetSystemPrompt(prompt) => {
            HostAction::SetSystemPrompt(prompt)
        }
        wit_orchestration::HostAction::SetActiveTools(tools) => HostAction::SetActiveTools(tools),
        wit_orchestration::HostAction::InjectUserMessage(message) => {
            HostAction::InjectUserMessage(message)
        }
        wit_orchestration::HostAction::StopAfterTurn(stop) => HostAction::StopAfterTurn(stop),
        wit_orchestration::HostAction::EmitCustom(value) => HostAction::EmitCustom(value),
        wit_orchestration::HostAction::SetUiHeader(header) => HostAction::SetUiHeader(UiHeader {
            provider: header.provider,
            model: header.model,
        }),
        wit_orchestration::HostAction::SetUiStatus(status) => HostAction::SetUiStatus(status),
    }
}
