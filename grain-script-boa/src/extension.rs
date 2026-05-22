//! `grain-script-boa`'s public surface: load a directory of `.js`
//! scripts, expose the tools they register as
//! [`grain_ai_agent_headless::Extension`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use async_trait::async_trait;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback,
    UserContent,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::worker::{WorkerCmd, WorkerHandle, spawn_worker};

/// Errors raised while constructing a [`BoaExtension`]. Once
/// construction succeeds, runtime tool-invocation errors are routed
/// back through the regular `AgentToolResult::error` channel.
#[derive(Debug, thiserror::Error)]
pub enum BoaExtensionError {
    #[error("scripts dir: {0}")]
    Io(#[from] std::io::Error),
    #[error("worker disconnected before reply")]
    WorkerGone,
    #[error("script load failed: {0}")]
    ScriptLoad(String),
}

/// One Boa-backed scripting environment. Owns a worker thread (via
/// [`WorkerHandle`]) for the entire lifetime of the extension.
///
/// Callers wrap this in their own `Extension` impl (or just consume
/// `tools()` directly) — we deliberately don't take a dependency on
/// `grain-ai-agent-headless` here to keep the crate one layer below
/// it and avoid a workspace cycle.
pub struct BoaExtension {
    name: &'static str,
    tools: Vec<Arc<dyn AgentTool>>,
    // Owned worker handle. Dropped at end of extension lifetime,
    // which sends Shutdown to the worker thread.
    worker: Arc<WorkerHandle>,
}

impl BoaExtension {
    /// Stable name for logging / `ExtensionRegistry` keys.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The agent tools this extension contributes. Cheap to call —
    /// returns an `Arc::clone` of each pre-built `ScriptedTool`.
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools.clone()
    }

    /// Resolve a pending `grain.modal_request(...)` call.
    /// `request_id` matches the `request_id` field in the
    /// notification payload that initiated the modal. `value` is
    /// what the JS caller will see as the function's return.
    ///
    /// Calling with an unknown id is harmless — the worker will
    /// just drop the message in its filter loop.
    pub fn resolve_modal(
        &self,
        request_id: u64,
        value: serde_json::Value,
    ) -> Result<(), String> {
        self.worker
            .modal_tx
            .send((request_id, value))
            .map_err(|_| "boa worker is no longer running".to_string())
    }

    /// Drain every notification queued via
    /// `grain.push_notification(payload)` or
    /// `grain.modal_request(...)` since the last drain. Reads from
    /// the parent-side channel directly — does NOT round-trip
    /// through the worker thread, so it stays responsive even
    /// while the worker is blocked inside a modal host fn.
    pub fn drain_notifications(&self) -> Vec<serde_json::Value> {
        let rx = match self.worker.notify_rx.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    }

    /// Snapshot every metadata entry registered via
    /// `grain.register_meta(kind, name, attrs)` under the given
    /// `kind`. Returns `(name, attrs)` pairs in unspecified order —
    /// callers should impose stable ordering if needed.
    pub fn list_metas(&self, kind: &str) -> Vec<(String, serde_json::Value)> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .worker
            .tx
            .send(WorkerCmd::ListMetas {
                kind: kind.to_string(),
                reply: reply_tx,
            })
            .is_err()
        {
            return Vec::new();
        }
        reply_rx.recv().unwrap_or_default()
    }

    /// Invoke a JS callback registered via
    /// `grain.register_callback(name, fn)`. Higher layers (e.g.
    /// `grain-pi-compat`'s event bridge) use this to dispatch agent
    /// events into JS handlers. Unregistered names are a silent
    /// no-op — callers can fire every event without checking which
    /// names a particular script subscribed to.
    pub async fn invoke_callback(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .worker
            .tx
            .send(WorkerCmd::InvokeCallback {
                name: name.to_string(),
                args,
                reply: reply_tx,
            })
            .is_err()
        {
            return Err("boa worker is no longer running".to_string());
        }
        reply_rx
            .await
            .map_err(|_| "worker dropped reply".to_string())?
    }
}

impl BoaExtension {
    /// Spawn the worker, evaluate every `*.js` file in `dir` in
    /// directory-listing order, and return an extension exposing all
    /// tools they registered via `grain.register_tool({...})`.
    ///
    /// Missing directory → empty extension (no scripts, no tools).
    /// One bad script → `BoaExtensionError::ScriptLoad` with the
    /// underlying boa diagnostic; the worker is still torn down
    /// cleanly via `Drop`.
    pub fn from_scripts_dir(dir: impl AsRef<Path>) -> Result<Self, BoaExtensionError> {
        Self::from_scripts_dirs(&[dir])
    }

    /// Like [`Self::from_scripts_dir`] but loads scripts from multiple
    /// directories into the **same** worker (one shared JS realm).
    /// Used by `lazy.gagent` to fold each plugin's `scripts/` folder
    /// into the same Boa instance as the workspace's primary scripts
    /// dir, so all registered tools end up exposed to one Agent.
    ///
    /// Iteration order: directories are walked in the order supplied;
    /// `*.js` files within each directory are sorted alphabetically.
    /// Tool registration order matters when two scripts register the
    /// same name (last one wins) — keep this in mind when stacking
    /// plugin scripts over a base set.
    pub fn from_scripts_dirs(
        dirs: &[impl AsRef<Path>],
    ) -> Result<Self, BoaExtensionError> {
        let worker = Arc::new(spawn_worker());

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
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("js"))
                .collect();
            here.sort();
            script_files.extend(here);
        }

        for path in &script_files {
            let code = std::fs::read_to_string(path)?;
            let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<(), String>>(1);
            worker
                .tx
                .send(WorkerCmd::LoadScript {
                    code,
                    label: path.display().to_string(),
                    reply: reply_tx,
                })
                .map_err(|_| BoaExtensionError::WorkerGone)?;
            match reply_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(BoaExtensionError::ScriptLoad(e)),
                Err(_) => return Err(BoaExtensionError::WorkerGone),
            }
        }

        // Snapshot the registered tools.
        let (list_tx, list_rx) = mpsc::sync_channel(1);
        worker
            .tx
            .send(WorkerCmd::ListTools { reply: list_tx })
            .map_err(|_| BoaExtensionError::WorkerGone)?;
        let metas = list_rx.recv().map_err(|_| BoaExtensionError::WorkerGone)?;

        let tools: Vec<Arc<dyn AgentTool>> = metas
            .into_iter()
            .map(|meta| -> Arc<dyn AgentTool> {
                Arc::new(ScriptedTool {
                    definition: ToolDefinition {
                        name: meta.name.clone(),
                        label: meta.name.clone(),
                        description: meta.description,
                        parameters: meta.schema,
                        execution_mode: None,
                    },
                    cmd_tx: worker.tx.clone(),
                })
            })
            .collect();

        Ok(BoaExtension {
            name: "grain-script-boa",
            tools,
            worker,
        })
    }
}

/// One tool registered by a JS script. `execute` ships the arguments
/// to the worker and awaits the reply on a tokio oneshot — the agent
/// loop never blocks the worker's single-threaded Context.
struct ScriptedTool {
    definition: ToolDefinition,
    cmd_tx: mpsc::Sender<WorkerCmd>,
}

#[async_trait]
impl AgentTool for ScriptedTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(WorkerCmd::InvokeTool {
                name: self.definition.name.clone(),
                args,
                reply: reply_tx,
            })
            .is_err()
        {
            return Err(AgentToolError::msg("boa worker is no longer running"));
        }
        match reply_rx.await {
            Ok(Ok(reply)) => {
                if reply.is_error {
                    Err(AgentToolError::msg(reply.content))
                } else {
                    Ok(AgentToolResult {
                        content: vec![UserContent::text(reply.content)],
                        details: serde_json::Value::Object(Default::default()),
                        terminate: None,
                    })
                }
            }
            Ok(Err(msg)) => Err(AgentToolError::msg(format!(
                "{}: {msg}",
                self.definition.name
            ))),
            Err(_) => Err(AgentToolError::msg(format!(
                "{}: worker dropped reply",
                self.definition.name
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_js(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[tokio::test]
    async fn registers_and_invokes_a_simple_tool() {
        let tmp = tempfile::tempdir().unwrap();
        write_js(
            tmp.path(),
            "reverse.js",
            r#"
            grain.register_tool({
                name: "reverse",
                description: "Reverses the input string",
                schema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
                run: (args) => args.text.split("").reverse().join("")
            });
            "#,
        );

        let ext = BoaExtension::from_scripts_dir(tmp.path()).unwrap();
        let tools = ext.tools();
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.definition().name, "reverse");

        let on_update: ToolUpdateCallback = Arc::new(|_| {});
        let result = tool
            .execute(
                "tc-1",
                serde_json::json!({ "text": "hello" }),
                CancellationToken::new(),
                on_update,
            )
            .await
            .expect("tool execution succeeded");
        assert_eq!(result.content.len(), 1);
        let UserContent::Text(t) = &result.content[0] else {
            panic!("expected text content");
        };
        assert_eq!(t.text, "olleh");
    }

    #[tokio::test]
    async fn missing_scripts_dir_is_an_empty_extension() {
        let ext = BoaExtension::from_scripts_dir("/tmp/grain-no-such-dir-2026-05-22").unwrap();
        assert!(ext.tools().is_empty());
    }

    #[tokio::test]
    async fn script_with_syntax_error_surfaces_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        write_js(tmp.path(), "bad.js", "this is not js !!!");
        let result = BoaExtension::from_scripts_dir(tmp.path());
        let Err(err) = result else {
            panic!("expected ScriptLoad error, got an Ok extension");
        };
        match err {
            BoaExtensionError::ScriptLoad(msg) => {
                assert!(msg.contains("bad.js"), "{msg}");
            }
            other => panic!("expected ScriptLoad error, got: {other:?}"),
        }
    }
}
