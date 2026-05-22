//! `grain-script-rhai` — Rhai-powered scripting layer.
//!
//! Sibling to [`grain-script-boa`]. Drop `.rhai` files into a
//! plugin's `scripts/` directory, define a `tools()` manifest plus
//! the handler functions it references, and they show up as agent
//! tools at boot. Rhai is sync-mode + native Rust types, so the
//! integration story is lighter than Boa's worker-thread + JSON
//! bridge: one shared `Engine`, no separate thread, native type
//! conversion via [`Dynamic`].
//!
//! # Convention
//!
//! Each script is expected to define **one top-level function** —
//! `fn tools()` — that returns an array of tool descriptors:
//!
//! ```rhai
//! // scripts/example.rhai
//!
//! fn tools() {
//!     [
//!         #{
//!             name: "echo",
//!             description: "Echo the argument back to the caller",
//!             parameters: #{
//!                 type: "object",
//!                 properties: #{
//!                     text: #{ type: "string" }
//!                 },
//!                 required: ["text"]
//!             },
//!             fn_name: "echo_handler"
//!         }
//!     ]
//! }
//!
//! fn echo_handler(args) {
//!     args.text
//! }
//! ```
//!
//! Host invokes the handler via `engine.call_fn(scope, ast, "echo_handler", (args,))`
//! on every agent tool call. Args are passed as a [`Dynamic`] map
//! built from the incoming JSON.
//!
//! # Why a separate crate from `grain-script-boa`
//!
//! - Rhai and Boa target different audiences (Rust-flavored DSL vs
//!   ECMAScript). Forcing one over the other is hostile to half the
//!   user base.
//! - Boa requires a dedicated worker thread because its `Context` is
//!   `!Send`. Rhai with the `sync` feature is `Send + Sync`, so it
//!   integrates as a plain shared `Arc<Engine>` — fewer moving
//!   parts, no command channel.
//! - Compile cost: Rhai is ~10× smaller than Boa, so opting into
//!   `grain-script-rhai` doesn't bloat builds the way `boa_engine`
//!   does.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback,
};
use rhai::{AST, Dynamic, Engine, Scope};
use tokio_util::sync::CancellationToken;

/// Errors loading a Rhai script tree.
#[derive(Debug, thiserror::Error)]
pub enum RhaiExtensionError {
    #[error("read scripts dir: {0}")]
    Io(#[from] std::io::Error),
    #[error("compile {label}: {reason}")]
    Compile { label: String, reason: String },
    #[error("script {label} has no `tools()` function or it failed: {reason}")]
    Manifest { label: String, reason: String },
    #[error("script {label}: tool entry {index} {reason}")]
    BadEntry {
        label: String,
        index: usize,
        reason: String,
    },
}

/// One discovered Rhai extension. Holds the shared engine + every
/// loaded script's AST so the registered tools' `execute` can call
/// back into them at any time.
pub struct RhaiExtension {
    pub name: &'static str,
    tools: Vec<Arc<dyn AgentTool>>,
    handles: Vec<ScriptHandle>,
}

/// Cloneable handle on one loaded `*.rhai` script. Lets the host
/// invoke any function defined in the script, not just the ones
/// registered via the `tools()` manifest — this is what UI handlers
/// (`OverlayDescriptor`-producing functions) hook into.
///
/// Cheap to clone (two `Arc`s + a label).
#[derive(Clone)]
pub struct ScriptHandle {
    /// Display label for the script, usually its path. Useful in
    /// error messages.
    pub label: String,
    engine: Arc<Engine>,
    ast: Arc<AST>,
}

impl ScriptHandle {
    /// Call `fn_name` and return its result as JSON. **Synchronous**
    /// — wrap in `tokio::task::spawn_blocking` from an async context.
    ///
    /// Adaptive arity: looks up `fn_name` in the script's function
    /// table and dispatches with **zero** args if the script declared
    /// it as `fn foo()`, or with **one** arg (`args` as a JSON map)
    /// if declared as `fn foo(x)`. This lets plugin authors write
    /// `fn ui_install_prompt()` for prompts that need no input *and*
    /// `fn ui_install_submit(args)` for submit handlers that consume
    /// form data, without a separate convention for each kind.
    ///
    /// Returns `Err` for missing functions, arity mismatch (3+
    /// parameters), or Rhai runtime errors. The boundary uses
    /// `serde_json::Value` so callers don't depend on `Dynamic`.
    pub fn call_fn_json(
        &self,
        fn_name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let arity = self
            .ast
            .iter_functions()
            .find(|f| f.name == fn_name)
            .map(|f| f.params.len())
            .ok_or_else(|| format!("function `{fn_name}` not defined in {}", self.label))?;
        let mut scope = Scope::new();
        let result: Dynamic = match arity {
            0 => self
                .engine
                .call_fn(&mut scope, &self.ast, fn_name, ())
                .map_err(|e| e.to_string())?,
            1 => self
                .engine
                .call_fn(&mut scope, &self.ast, fn_name, (json_to_dyn(args),))
                .map_err(|e| e.to_string())?,
            n => {
                return Err(format!(
                    "function `{fn_name}` takes {n} params; UI handlers must take 0 or 1"
                ));
            }
        };
        Ok(dyn_to_json(result))
    }

    /// `true` if this script defines `fn_name` at the top level.
    /// Cheap (consults the AST's function table).
    pub fn has_fn(&self, fn_name: &str) -> bool {
        // Rhai's AST exposes `iter_functions()` returning ScriptFnMetadata.
        self.ast.iter_functions().any(|f| f.name == fn_name)
    }

    /// Every top-level function name defined by this script.
    /// Convenient for building "function → script" dispatch tables
    /// without iterating `iter_functions()` directly outside this
    /// crate (Rhai's metadata types aren't re-exported as easily as
    /// strings).
    pub fn ast_function_names(&self) -> Vec<String> {
        self.ast.iter_functions().map(|f| f.name.to_string()).collect()
    }
}

impl RhaiExtension {
    /// Build a fresh [`Engine`] with the same defaults
    /// [`Self::from_scripts_dir`] uses internally. Use when the
    /// caller wants to register host functions (e.g. plugin manager
    /// primitives, file-system helpers, logging) *before* loading
    /// scripts. Hand the configured engine to
    /// [`Self::from_scripts_dirs_with_engine`].
    ///
    /// Defaults applied:
    /// - `set_max_expr_depths(256, 256)` so realistic nested
    ///   JSON-schema literals don't trip Rhai's complexity guard.
    pub fn default_engine() -> Engine {
        let mut engine = Engine::new();
        // Defaults bite on realistic JSON-schema literals: triple-nested
        // `#{ properties: #{ text: #{ type: "string" } } }` inside a
        // function body trips the per-function expression-depth limit
        // (32). Lift the cap to 256 — well below any pathological
        // input but far above any sane manifest.
        engine.set_max_expr_depths(256, 256);
        engine
    }

    /// Single-directory convenience using [`Self::default_engine`].
    pub fn from_scripts_dir(dir: impl AsRef<Path>) -> Result<Self, RhaiExtensionError> {
        Self::from_scripts_dirs(&[dir])
    }

    /// Multi-directory convenience using [`Self::default_engine`].
    pub fn from_scripts_dirs(
        dirs: &[impl AsRef<Path>],
    ) -> Result<Self, RhaiExtensionError> {
        Self::from_scripts_dirs_with_engine(Self::default_engine(), dirs)
    }

    /// Load every `*.rhai` file under each of `dirs` (sorted within
    /// each dir for determinism) into the supplied [`Engine`]. Each
    /// script's `tools()` manifest is invoked once; the returned
    /// descriptors become `Arc<dyn AgentTool>` entries on
    /// [`Self::tools`]. Missing directories are silently skipped so
    /// the call site can pass plugin script dirs that may or may not
    /// exist.
    ///
    /// Pass-an-engine variant: register host functions on `engine`
    /// before calling this — those functions remain available to
    /// every loaded script.
    pub fn from_scripts_dirs_with_engine(
        engine: Engine,
        dirs: &[impl AsRef<Path>],
    ) -> Result<Self, RhaiExtensionError> {
        let engine = Arc::new(engine);

        // Collect script paths from every dir.
        let mut script_files: Vec<PathBuf> = Vec::new();
        for dir in dirs {
            let dir = dir.as_ref();
            let entries = match std::fs::read_dir(dir) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let mut here: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("rhai"))
                .collect();
            here.sort();
            script_files.extend(here);
        }

        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
        let mut handles: Vec<ScriptHandle> = Vec::new();
        for path in &script_files {
            let label = path.display().to_string();
            let ast = engine.compile_file(path.clone()).map_err(|e| {
                RhaiExtensionError::Compile {
                    label: label.clone(),
                    reason: e.to_string(),
                }
            })?;
            let ast = Arc::new(ast);
            handles.push(ScriptHandle {
                label: label.clone(),
                engine: engine.clone(),
                ast: ast.clone(),
            });

            // Module-level statements run; functions get declared.
            // Some scripts may not have top-level statements at all,
            // which is fine — `run_ast` only runs what's there.
            let _: Result<Dynamic, _> = engine.eval_ast(&ast);

            // Call the script's `tools()` manifest.
            let mut scope = Scope::new();
            let manifest: Dynamic = engine
                .call_fn(&mut scope, &ast, "tools", ())
                .map_err(|e| RhaiExtensionError::Manifest {
                    label: label.clone(),
                    reason: e.to_string(),
                })?;
            let arr = manifest
                .into_array()
                .map_err(|t| RhaiExtensionError::Manifest {
                    label: label.clone(),
                    reason: format!("`tools()` returned {t} — expected an array"),
                })?;

            for (idx, item) in arr.into_iter().enumerate() {
                let map = item.try_cast::<rhai::Map>().ok_or_else(|| {
                    RhaiExtensionError::BadEntry {
                        label: label.clone(),
                        index: idx,
                        reason: "entry is not a map".into(),
                    }
                })?;
                let entry = parse_tool_entry(&map, &label, idx)?;
                tools.push(Arc::new(ScriptedRhaiTool {
                    def: ToolDefinition {
                        name: entry.name.clone(),
                        label: entry.name.clone(),
                        description: entry.description,
                        parameters: entry.parameters,
                        execution_mode: None,
                    },
                    engine: engine.clone(),
                    ast: ast.clone(),
                    fn_name: entry.fn_name,
                }));
            }
        }
        Ok(RhaiExtension {
            name: "grain-script-rhai",
            tools,
            handles,
        })
    }

    /// Cloneable handle on the loaded tool list. Caller merges these
    /// into the agent's tool catalog.
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools.clone()
    }

    /// One [`ScriptHandle`] per loaded `*.rhai` file, in load order
    /// (alphabetical within each scripts dir). Used by hosts that
    /// need to call functions outside the `tools()` manifest — UI
    /// handlers, custom hooks, etc.
    pub fn script_handles(&self) -> Vec<ScriptHandle> {
        self.handles.clone()
    }
}

// ----- Internals --------------------------------------------------------

struct ToolEntry {
    name: String,
    description: String,
    parameters: serde_json::Value,
    fn_name: String,
}

fn parse_tool_entry(
    map: &rhai::Map,
    label: &str,
    idx: usize,
) -> Result<ToolEntry, RhaiExtensionError> {
    let bad = |reason: String| RhaiExtensionError::BadEntry {
        label: label.into(),
        index: idx,
        reason,
    };
    let name = map
        .get("name")
        .and_then(|v| v.clone().into_string().ok())
        .ok_or_else(|| bad("missing `name` (string)".into()))?;
    let description = map
        .get("description")
        .and_then(|v| v.clone().into_string().ok())
        .unwrap_or_default();
    let parameters = map
        .get("parameters")
        .map(|v| dyn_to_json(v.clone()))
        .unwrap_or_else(|| {
            serde_json::json!({
                "type": "object",
                "properties": {},
            })
        });
    let fn_name = map
        .get("fn_name")
        .and_then(|v| v.clone().into_string().ok())
        .unwrap_or_else(|| name.clone());
    Ok(ToolEntry {
        name,
        description,
        parameters,
        fn_name,
    })
}

struct ScriptedRhaiTool {
    def: ToolDefinition,
    engine: Arc<Engine>,
    ast: Arc<AST>,
    fn_name: String,
}

#[async_trait]
impl AgentTool for ScriptedRhaiTool {
    fn definition(&self) -> &ToolDefinition {
        &self.def
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let engine = self.engine.clone();
        let ast = self.ast.clone();
        let fn_name = self.fn_name.clone();
        let args_dyn = json_to_dyn(args);

        // Rhai is synchronous; bounce into `spawn_blocking` so the
        // tokio runtime doesn't stall on a long-running script.
        let outcome = tokio::task::spawn_blocking(move || -> Result<Dynamic, String> {
            let mut scope = Scope::new();
            engine
                .call_fn::<Dynamic>(&mut scope, &ast, &fn_name, (args_dyn,))
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| AgentToolError::Message(format!("rhai task join: {e}")))?;

        match outcome {
            Ok(value) => Ok(AgentToolResult::text(dyn_to_text(value))),
            Err(reason) => Err(AgentToolError::Message(format!(
                "rhai handler `{fn}`: {reason}",
                fn = self.fn_name
            ))),
        }
    }
}

// ----- Dynamic ↔ JSON ---------------------------------------------------

fn json_to_dyn(v: serde_json::Value) -> Dynamic {
    match v {
        serde_json::Value::Null => Dynamic::UNIT,
        serde_json::Value::Bool(b) => Dynamic::from(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else if let Some(f) = n.as_f64() {
                Dynamic::from(f)
            } else {
                Dynamic::UNIT
            }
        }
        serde_json::Value::String(s) => Dynamic::from(s),
        serde_json::Value::Array(arr) => {
            let v: Vec<Dynamic> = arr.into_iter().map(json_to_dyn).collect();
            Dynamic::from(v)
        }
        serde_json::Value::Object(obj) => {
            let mut m = rhai::Map::new();
            for (k, vv) in obj {
                m.insert(k.into(), json_to_dyn(vv));
            }
            Dynamic::from(m)
        }
    }
}

fn dyn_to_json(v: Dynamic) -> serde_json::Value {
    if v.is_unit() {
        return serde_json::Value::Null;
    }
    if v.is::<bool>() {
        return serde_json::json!(v.as_bool().unwrap_or(false));
    }
    if v.is::<i64>() {
        return serde_json::json!(v.as_int().unwrap_or(0));
    }
    if v.is::<f64>() {
        return serde_json::json!(v.as_float().unwrap_or(0.0));
    }
    if v.is_string() {
        return serde_json::Value::String(v.into_string().unwrap_or_default());
    }
    if v.is_array() {
        let arr = v.into_array().unwrap_or_default();
        return serde_json::Value::Array(arr.into_iter().map(dyn_to_json).collect());
    }
    if v.is::<rhai::Map>() {
        let m: rhai::Map = v.cast();
        let mut obj = serde_json::Map::new();
        for (k, vv) in m {
            obj.insert(k.into(), dyn_to_json(vv));
        }
        return serde_json::Value::Object(obj);
    }
    serde_json::Value::String(format!("{v:?}"))
}

fn dyn_to_text(v: Dynamic) -> String {
    if v.is_string() {
        v.into_string().unwrap_or_default()
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(format!("{name}.rhai"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn missing_dir_returns_empty_extension() {
        let ext =
            RhaiExtension::from_scripts_dir("/tmp/grain-nonexistent-rhai-dir-xyz-12345").unwrap();
        assert!(ext.tools().is_empty());
        assert!(ext.script_handles().is_empty());
    }

    #[test]
    fn script_handle_calls_arbitrary_function_outside_tools_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "ui",
            r#"
                fn tools() { [] }

                fn ui_install_prompt() {
                    #{
                        kind: "form",
                        title: "Install plugin",
                        fields: [
                            #{ name: "name", label: "Name", placeholder: "", initial: "" },
                            #{ name: "src",  label: "Source", placeholder: "", initial: "" }
                        ],
                        on_submit: "ui_install_submit"
                    }
                }
            "#,
        );
        let ext = RhaiExtension::from_scripts_dir(tmp.path()).unwrap();
        let handles = ext.script_handles();
        assert_eq!(handles.len(), 1);
        assert!(handles[0].has_fn("ui_install_prompt"));
        assert!(!handles[0].has_fn("nonexistent"));
        let out = handles[0]
            .call_fn_json("ui_install_prompt", serde_json::Value::Null)
            .unwrap();
        assert_eq!(out["kind"], "form");
        assert_eq!(out["title"], "Install plugin");
        assert_eq!(out["on_submit"], "ui_install_submit");
        assert_eq!(out["fields"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn script_handle_call_fn_json_propagates_rhai_error() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "ui2",
            r#"
                fn tools() { [] }
                fn always_panics() { throw "boom"; }
            "#,
        );
        let ext = RhaiExtension::from_scripts_dir(tmp.path()).unwrap();
        let handles = ext.script_handles();
        let err = handles[0]
            .call_fn_json("always_panics", serde_json::Value::Null)
            .err()
            .expect("expected error");
        assert!(err.contains("boom"), "{err}");
    }

    #[test]
    fn loads_tool_via_manifest_function() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "ok",
            r#"
                fn tools() {
                    [
                        #{
                            name: "echo",
                            description: "Echo argument back",
                            parameters: #{
                                type: "object",
                                properties: #{
                                    text: #{ type: "string" }
                                }
                            },
                            fn_name: "echo_handler"
                        }
                    ]
                }
                fn echo_handler(args) {
                    args.text
                }
            "#,
        );
        let ext = RhaiExtension::from_scripts_dir(tmp.path()).unwrap();
        let tools = ext.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].definition().name, "echo");
        assert!(!tools[0].definition().description.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_dispatches_back_into_rhai_handler() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "ok",
            r#"
                fn tools() {
                    [
                        #{ name: "shout", description: "uppercase", parameters: #{}, fn_name: "shout" }
                    ]
                }
                fn shout(args) {
                    args.text.to_upper()
                }
            "#,
        );
        let ext = RhaiExtension::from_scripts_dir(tmp.path()).unwrap();
        let tool = ext.tools().into_iter().next().unwrap();
        let result = tool
            .execute(
                "call-1",
                serde_json::json!({ "text": "hello" }),
                CancellationToken::new(),
                Arc::new(|_: grain_agent_core::AgentToolResult| {}),
            )
            .await
            .unwrap();
        let body = result
            .content
            .into_iter()
            .filter_map(|c| match c {
                grain_agent_core::UserContent::Text(t) => Some(t.text),
                _ => None,
            })
            .collect::<String>();
        assert!(body.contains("HELLO"), "got {body:?}");
    }

    #[test]
    fn malformed_manifest_returns_error_pointing_at_the_script() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(tmp.path(), "bad", r#"fn tools() { 42 }"#);
        let err = RhaiExtension::from_scripts_dir(tmp.path()).err().expect("expected error");
        match err {
            RhaiExtensionError::Manifest { label, .. } => {
                assert!(label.ends_with("bad.rhai"), "{label}");
            }
            other => panic!("expected Manifest error, got {other:?}"),
        }
    }

    #[test]
    fn entry_missing_name_is_reported_with_index() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "partial",
            r#"
                fn tools() {
                    [
                        #{ name: "ok", description: "x", parameters: #{}, fn_name: "ok" },
                        #{ description: "no name" }
                    ]
                }
                fn ok(args) { "ok" }
            "#,
        );
        let err = RhaiExtension::from_scripts_dir(tmp.path()).err().expect("expected error");
        match err {
            RhaiExtensionError::BadEntry { index, reason, .. } => {
                assert_eq!(index, 1);
                assert!(reason.contains("name"), "{reason}");
            }
            other => panic!("expected BadEntry, got {other:?}"),
        }
    }

    #[test]
    fn compile_error_surfaces_with_script_label() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(tmp.path(), "syntax", r#"fn tools() { #! not valid"#);
        let err = RhaiExtension::from_scripts_dir(tmp.path()).err().expect("expected error");
        assert!(matches!(err, RhaiExtensionError::Compile { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn host_registered_fn_is_callable_from_script() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "uses_host",
            r#"
                fn tools() {
                    [#{ name: "ping", parameters: #{}, fn_name: "ping" }]
                }
                fn ping(args) {
                    // Host function `host_echo(s)` is registered by the caller
                    // before scripts load; here we just call it like any native.
                    host_echo(args.text)
                }
            "#,
        );
        let mut engine = RhaiExtension::default_engine();
        engine.register_fn("host_echo", |s: String| -> String { format!("host:{s}") });
        let ext = RhaiExtension::from_scripts_dirs_with_engine(engine, &[tmp.path()]).unwrap();
        let tool = ext.tools().into_iter().next().unwrap();
        let result = tool
            .execute(
                "call-1",
                serde_json::json!({ "text": "hello" }),
                CancellationToken::new(),
                Arc::new(|_: grain_agent_core::AgentToolResult| {}),
            )
            .await
            .unwrap();
        let body = result
            .content
            .into_iter()
            .filter_map(|c| match c {
                grain_agent_core::UserContent::Text(t) => Some(t.text),
                _ => None,
            })
            .collect::<String>();
        assert!(body.contains("host:hello"), "got {body:?}");
    }

    #[test]
    fn lazy_gagent_install_rhai_loads_with_host_primitives_registered() {
        // Smoke-test that the shipping `lazy-gagent/scripts/install.rhai`
        // parses and its `tools()` manifest returns the three expected
        // entries when the host primitives are registered. Catches drift
        // between the TUI's host-fn names and the script's calls.
        let script_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("lazy-gagent")
            .join("scripts");
        if !script_dir.is_dir() {
            // Older checkouts may not have the lazy-gagent scripts/
            // directory; nothing to smoke-test in that case.
            return;
        }
        let mut engine = RhaiExtension::default_engine();
        engine.register_fn(
            "plugins_install",
            |_a: String, _b: String| -> String { "ok".into() },
        );
        engine.register_fn("plugins_update", |_a: String| -> String { "ok".into() });
        engine.register_fn(
            "plugins_remove",
            |_a: String, _b: bool| -> String { "ok".into() },
        );
        let ext = RhaiExtension::from_scripts_dirs_with_engine(engine, &[script_dir]).unwrap();
        let names: Vec<_> = ext
            .tools()
            .into_iter()
            .map(|t| t.definition().name.clone())
            .collect();
        assert!(
            names.contains(&"lazy_install".to_string()),
            "tools: {names:?}"
        );
        assert!(
            names.contains(&"lazy_update".to_string()),
            "tools: {names:?}"
        );
        assert!(
            names.contains(&"lazy_remove".to_string()),
            "tools: {names:?}"
        );
    }

    #[test]
    fn from_scripts_dirs_walks_each_dir_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_script(
            &a,
            "x",
            r#"
                fn tools() { [#{ name: "from_a", parameters: #{}, fn_name: "h" }] }
                fn h(args) { "a" }
            "#,
        );
        write_script(
            &b,
            "y",
            r#"
                fn tools() { [#{ name: "from_b", parameters: #{}, fn_name: "h" }] }
                fn h(args) { "b" }
            "#,
        );
        let ext = RhaiExtension::from_scripts_dirs(&[a, b]).unwrap();
        let names: Vec<_> = ext
            .tools()
            .into_iter()
            .map(|t| t.definition().name.clone())
            .collect();
        assert_eq!(names, vec!["from_a", "from_b"]);
    }
}
