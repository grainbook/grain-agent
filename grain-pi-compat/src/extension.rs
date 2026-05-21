//! Public surface: discover pi extension files, transform each, load
//! the transformed bundle through [`grain_script_boa::BoaExtension`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grain_agent_core::{AgentEvent, AgentTool, EventListener};
use grain_script_boa::{BoaExtension, BoaExtensionError};
use tempfile::TempDir;

use crate::transform::transform_pi_source;

/// One slash command surfaced by a pi extension. Built from a
/// `pi.registerCommand(name, { description, handler })` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiCommand {
    pub name: String,
    pub description: String,
}

/// One keyboard shortcut surfaced by a pi extension. Built from a
/// `pi.registerShortcut(keys, { description, handler })` call. The
/// `keys` string is verbatim from pi's API (e.g. `"ctrl+x"`,
/// `"shift+alt+a"`); parsing into a `crossterm::KeyEvent` is the
/// TUI's responsibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiShortcut {
    pub keys: String,
    pub description: String,
}

/// One UI event a pi extension can surface to the host. Includes
/// both fire-and-forget toasts (Notify) and synchronous modal
/// round-trips (Confirm / Input / Select).
///
/// For modal variants the host MUST eventually call
/// [`PiExtension::resolve_modal`] with the embedded `request_id` —
/// otherwise the worker thread stays blocked forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiNotification {
    /// Fire-and-forget toast. The host renders the text however it
    /// wants — transcript info line, ratatui toast widget, etc.
    Notify { text: String },
    /// Yes/no modal. Host must resolve with a JSON boolean.
    Confirm { request_id: u64, prompt: String },
    /// Free-text input modal. Host must resolve with a JSON string.
    Input { request_id: u64, prompt: String },
    /// Pick-from-list modal. Host must resolve with one of the
    /// `items` (as a JSON string).
    Select {
        request_id: u64,
        prompt: String,
        items: Vec<String>,
    },
}

/// Errors raised while constructing a [`PiExtension`].
#[derive(Debug, thiserror::Error)]
pub enum PiCompatError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("boa: {0}")]
    Boa(#[from] BoaExtensionError),
}

/// One pi-compat scripting environment. Owns the transformed-script
/// temp directory + a [`BoaExtension`] for the underlying JS runtime.
pub struct PiExtension {
    name: &'static str,
    /// Wrapped in Arc so [`Self::listeners`] can hand out clones that
    /// outlive `&self` borrows — each EventListener captures its own
    /// strong ref to the underlying Boa runtime.
    inner: Arc<BoaExtension>,
    /// Holds the transformed-script dir open so the Boa worker can
    /// read them. Dropped together with the rest of the extension.
    _tempdir: TempDir,
}

impl PiExtension {
    /// Stable name for logging.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Tools registered by all loaded pi extension files.
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.inner.tools()
    }

    /// Slash commands registered via `pi.registerCommand(name, {...})`.
    /// Sorted by name for deterministic display in pickers.
    pub fn commands(&self) -> Vec<PiCommand> {
        let mut entries: Vec<PiCommand> = self
            .inner
            .list_metas("command")
            .into_iter()
            .map(|(name, attrs)| {
                let description = attrs
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                PiCommand { name, description }
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }

    /// Dispatch a slash command registered via `pi.registerCommand`.
    /// `args` is forwarded as the first argument to the JS handler.
    /// JS-side throws come back as `Err(msg)`.
    pub async fn invoke_command(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(), String> {
        self.inner
            .invoke_callback(&format!("cmd:{name}"), args)
            .await
    }

    /// Keyboard shortcuts registered via
    /// `pi.registerShortcut(keys, {...})`. Sorted by `keys` for
    /// deterministic display in the TUI's help / cheatsheet.
    pub fn shortcuts(&self) -> Vec<PiShortcut> {
        let mut entries: Vec<PiShortcut> = self
            .inner
            .list_metas("shortcut")
            .into_iter()
            .map(|(keys, attrs)| {
                let description = attrs
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                PiShortcut { keys, description }
            })
            .collect();
        entries.sort_by(|a, b| a.keys.cmp(&b.keys));
        entries
    }

    /// Dispatch a shortcut registered via `pi.registerShortcut`. The
    /// TUI matches key events against `shortcuts()` and calls this
    /// with the matched spec. No args are forwarded — shortcuts are
    /// nullary in pi's API.
    pub async fn invoke_shortcut(&self, keys: &str) -> Result<(), String> {
        self.inner
            .invoke_callback(
                &format!("shortcut:{keys}"),
                serde_json::Value::Object(Default::default()),
            )
            .await
    }

    /// Drain every `pi.ui.notify(text)` payload that the scripts
    /// have pushed since the last call. The TUI polls this each
    /// tick / event loop iteration and renders entries however it
    /// wants. Unknown payload shapes are silently dropped.
    pub fn drain_notifications(&self) -> Vec<PiNotification> {
        self.inner
            .drain_notifications()
            .into_iter()
            .filter_map(decode_notification)
            .collect()
    }

    /// Resolve a pending modal initiated via
    /// `pi.ui.confirm/input/select`. The JS caller blocks until
    /// this is called; `request_id` matches the field in the
    /// `PiNotification` payload. `response` must be the right shape
    /// for the modal kind: bool for Confirm, string for Input or
    /// Select. Mismatched shapes will simply land in JS as whatever
    /// type they decode to — the JS code is responsible for
    /// checking.
    pub fn resolve_modal(
        &self,
        request_id: u64,
        response: serde_json::Value,
    ) -> Result<(), String> {
        self.inner.resolve_modal(request_id, response)
    }

    /// One [`EventListener`] that translates supported `AgentEvent`
    /// variants into the pi event schema and dispatches them into
    /// JS handlers registered via `pi.on(event_name, fn)`.
    ///
    /// Subscribe this to an [`grain_agent_core::Agent`] via
    /// `agent.subscribe(listener).await` and pi scripts will start
    /// receiving events.
    pub fn listeners(&self) -> Vec<EventListener> {
        let inner = self.inner.clone();
        let dispatch: EventListener = Arc::new(move |event, _signal| {
            let inner = inner.clone();
            Box::pin(async move {
                let Some((pi_name, payload)) = map_agent_event_to_pi(&event) else {
                    return;
                };
                let key = format!("on:{pi_name}");
                // Swallow errors — listeners can't return diagnostics
                // to the agent. JS-side throws are stringified at the
                // worker boundary and stay there; they don't break
                // the agent's run.
                let _ = inner.invoke_callback(&key, payload).await;
            })
        });
        vec![dispatch]
    }

    /// Scan pi's conventional locations and load every `*.js` file:
    ///
    /// - `<workspace>/.pi/extensions/` (per-project)
    /// - `~/.pi/agent/extensions/` (global)
    ///
    /// Missing locations are not an error — they're simply skipped.
    pub fn from_pi_dirs(workspace_root: &Path) -> Result<Self, PiCompatError> {
        let dirs = pi_search_paths(workspace_root);
        Self::from_dirs(&dirs)
    }

    /// Explicit-paths variant. Useful for tests and for callers who
    /// want to override pi's default search behavior.
    pub fn from_dirs(dirs: &[PathBuf]) -> Result<Self, PiCompatError> {
        let tempdir = tempfile::tempdir()?;
        let mut count = 0usize;
        for dir in dirs {
            if !dir.exists() {
                continue;
            }
            let entries = match fs::read_dir(dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                    continue;
                };
                // Phase 1: JS only. TypeScript lands in Phase 3 via
                // an swc transpile step.
                if ext != "js" {
                    continue;
                }
                let source = fs::read_to_string(&path)?;
                let transformed = transform_pi_source(&source);
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("anonymous");
                // Numeric prefix preserves a stable load order even
                // across directories.
                let out_name = format!("{count:03}_{stem}.js");
                fs::write(tempdir.path().join(&out_name), transformed)?;
                count += 1;
            }
        }
        let inner = Arc::new(BoaExtension::from_scripts_dir(tempdir.path())?);
        Ok(PiExtension {
            name: "grain-pi-compat",
            inner,
            _tempdir: tempdir,
        })
    }
}

/// Map our [`AgentEvent`] variants to pi's documented event schema.
/// Returns `None` for events pi doesn't declare today (e.g. our
/// turn-level lifecycle is pi's per-message lifecycle plus tool
/// hooks, so `TurnStart` / `TurnEnd` have no direct pi equivalent).
fn map_agent_event_to_pi(event: &AgentEvent) -> Option<(&'static str, serde_json::Value)> {
    match event {
        AgentEvent::AgentStart => Some(("agent_start", serde_json::json!({}))),
        AgentEvent::AgentEnd { messages } => Some((
            "agent_end",
            serde_json::json!({ "message_count": messages.len() }),
        )),
        AgentEvent::MessageStart { message } => Some((
            "message_start",
            serde_json::json!({ "role": message.role() }),
        )),
        AgentEvent::MessageEnd { message } => Some((
            "message_end",
            serde_json::json!({ "role": message.role() }),
        )),
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => Some((
            "tool_call",
            serde_json::json!({
                "tool_call_id": tool_call_id,
                "tool_name": tool_name,
                "args": args,
            }),
        )),
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => Some((
            "tool_result",
            serde_json::json!({
                "tool_call_id": tool_call_id,
                "tool_name": tool_name,
                "is_error": is_error,
                // AgentToolResult is `Serialize`; project just the
                // text content list to keep the JS payload simple.
                "content": result.content,
            }),
        )),
        _ => None,
    }
}

/// Map one raw queue payload from the Boa worker into a typed
/// [`PiNotification`]. Returns `None` for unknown shapes so the
/// queue stays forward-compatible.
fn decode_notification(v: serde_json::Value) -> Option<PiNotification> {
    let kind = v.get("kind")?.as_str()?;
    match kind {
        "notify" => {
            let text = v.get("text")?.as_str()?.to_string();
            Some(PiNotification::Notify { text })
        }
        "confirm" => {
            let request_id = v.get("request_id")?.as_u64()?;
            let prompt = v.get("prompt")?.as_str()?.to_string();
            Some(PiNotification::Confirm { request_id, prompt })
        }
        "input" => {
            let request_id = v.get("request_id")?.as_u64()?;
            let prompt = v.get("prompt")?.as_str()?.to_string();
            Some(PiNotification::Input { request_id, prompt })
        }
        "select" => {
            let request_id = v.get("request_id")?.as_u64()?;
            let prompt = v.get("prompt")?.as_str()?.to_string();
            let items = v
                .get("items")?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            Some(PiNotification::Select {
                request_id,
                prompt,
                items,
            })
        }
        _ => None,
    }
}

/// pi's conventional discovery paths, in load order.
fn pi_search_paths(workspace_root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![workspace_root.join(".pi").join("extensions")];
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".pi").join("agent").join("extensions"));
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_core::{AgentEvent, AgentToolError, ToolUpdateCallback, UserContent};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn write_script(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    async fn run_tool(
        tool: &Arc<dyn AgentTool>,
        args: serde_json::Value,
    ) -> Result<String, AgentToolError> {
        let cb: ToolUpdateCallback = Arc::new(|_| {});
        let result = tool
            .execute("tc-1", args, CancellationToken::new(), cb)
            .await?;
        let text = result
            .content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .next()
            .unwrap_or_default();
        Ok(text)
    }

    #[tokio::test]
    async fn factory_style_pi_extension_works() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "shout.js",
            r#"
            export default (pi) => {
                pi.registerTool({
                    name: "shout",
                    description: "Uppercases the input",
                    parameters: { type: "object", properties: { text: { type: "string" }}},
                    execute: (args) => args.text.toUpperCase(),
                });
            };
            "#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();
        let tools = ext.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].definition().name, "shout");
        let out = run_tool(&tools[0], serde_json::json!({ "text": "hi" }))
            .await
            .unwrap();
        assert_eq!(out, "HI");
    }

    #[tokio::test]
    async fn top_level_pi_call_also_works_without_factory() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "reverse.js",
            r#"
            pi.registerTool({
                name: "reverse",
                description: "Reverses text",
                parameters: { type: "object" },
                execute: (args) => args.text.split("").reverse().join(""),
            });
            "#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();
        let tools = ext.tools();
        assert_eq!(tools.len(), 1);
        let out = run_tool(&tools[0], serde_json::json!({ "text": "hello" }))
            .await
            .unwrap();
        assert_eq!(out, "olleh");
    }

    #[tokio::test]
    async fn ignores_non_js_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(tmp.path(), "should-be-ignored.ts", "throw 'this is TS';");
        write_script(
            tmp.path(),
            "ok.js",
            r#"pi.registerTool({ name: "ok", description: "", parameters: {}, execute: () => "" });"#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();
        let tools = ext.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].definition().name, "ok");
    }

    #[tokio::test]
    async fn missing_dirs_are_skipped_silently() {
        let nonexistent = PathBuf::from("/tmp/grain-pi-no-such-dir-2026-05");
        let ext = PiExtension::from_dirs(&[nonexistent]).unwrap();
        assert!(ext.tools().is_empty());
    }

    #[tokio::test]
    async fn pi_on_routes_through_invoke_callback() {
        // The pi.on shim should land the JS handler in
        // grain.register_callback, keyed `on:<event>`. We can prove
        // it by directly invoking via BoaExtension::invoke_callback
        // and asserting the JS throws (or doesn't) based on payload.
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "listener.js",
            r#"
            pi.on("tool_call", (event) => {
                if (event.tool_name !== "expected") {
                    throw new Error("got tool_name=" + event.tool_name);
                }
            });
            "#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();

        // Happy path — payload matches what the JS expects.
        let ok = ext
            .inner
            .invoke_callback(
                "on:tool_call",
                serde_json::json!({ "tool_name": "expected" }),
            )
            .await;
        assert!(ok.is_ok(), "expected Ok, got {ok:?}");

        // Sad path — JS throws, error string surfaces back.
        let err = ext
            .inner
            .invoke_callback(
                "on:tool_call",
                serde_json::json!({ "tool_name": "wrong" }),
            )
            .await;
        let Err(msg) = err else {
            panic!("expected JS throw to surface as Err");
        };
        assert!(msg.contains("got tool_name=wrong"), "{msg}");
    }

    #[tokio::test]
    async fn unregistered_callback_name_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "x.js",
            r#"pi.on("tool_call", () => {});"#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();
        // No handler subscribed to "agent_end" — must NOT error.
        let res = ext
            .inner
            .invoke_callback("on:agent_end", serde_json::json!({}))
            .await;
        assert!(res.is_ok(), "unregistered event must be silent: {res:?}");
    }

    #[tokio::test]
    async fn listeners_dispatches_supported_agent_events() {
        // Same idea as `pi_on_routes_through_invoke_callback`, but
        // exercising the public `listeners()` path (the one we'd
        // subscribe to a real Agent).
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "tap.js",
            r#"
            pi.on("agent_end", (event) => {
                if (event.message_count < 0) {
                    throw new Error("negative message_count?!");
                }
            });
            "#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();
        let listeners = ext.listeners();
        assert_eq!(listeners.len(), 1, "single dispatching listener");

        let signal = CancellationToken::new();
        let evt = AgentEvent::AgentEnd { messages: vec![] };
        // Listener returns BoxFuture<()>; awaiting it succeeds since
        // the JS handler doesn't throw.
        listeners[0](evt, signal).await;
    }

    #[tokio::test]
    async fn register_command_surfaces_in_commands_list() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "cmds.js",
            r#"
            export default (pi) => {
                pi.registerCommand("audit", {
                    description: "Print an audit log",
                    handler: () => {},
                });
                pi.registerCommand("aaa-first", {
                    description: "Comes first alphabetically",
                    handler: () => {},
                });
            };
            "#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();
        let cmds = ext.commands();
        assert_eq!(cmds.len(), 2);
        // Sorted by name.
        assert_eq!(cmds[0].name, "aaa-first");
        assert_eq!(cmds[1].name, "audit");
        assert_eq!(cmds[1].description, "Print an audit log");
    }

    #[tokio::test]
    async fn invoke_command_dispatches_to_js_handler() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "ck.js",
            r#"
            pi.registerCommand("check", {
                description: "Throws if the magic number is wrong",
                handler: (args) => {
                    if (args.magic !== 42) {
                        throw new Error("magic was " + args.magic);
                    }
                },
            });
            "#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();
        // Happy path.
        let ok = ext
            .invoke_command("check", serde_json::json!({ "magic": 42 }))
            .await;
        assert!(ok.is_ok(), "expected Ok, got {ok:?}");
        // Sad path — JS throws.
        let err = ext
            .invoke_command("check", serde_json::json!({ "magic": 7 }))
            .await;
        let Err(msg) = err else {
            panic!("expected JS throw to surface as Err");
        };
        assert!(msg.contains("magic was 7"), "{msg}");
    }

    #[tokio::test]
    async fn commands_is_empty_when_no_script_registers_any() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "just_tool.js",
            r#"pi.registerTool({ name: "t", description: "", parameters: {}, execute: () => "" });"#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();
        assert!(ext.commands().is_empty());
    }

    #[tokio::test]
    async fn register_shortcut_surfaces_in_shortcuts_list_and_dispatches() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "sc.js",
            r#"
            export default (pi) => {
                pi.registerShortcut("ctrl+x", {
                    description: "Cut",
                    handler: () => { /* nothing */ },
                });
                pi.registerShortcut("ctrl+s", {
                    description: "Save — throws if 'saving' state mismatched",
                    handler: () => { throw new Error("not saving!"); },
                });
            };
            "#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();
        let scs = ext.shortcuts();
        // Sorted by `keys`.
        assert_eq!(scs.len(), 2);
        assert_eq!(scs[0].keys, "ctrl+s");
        assert_eq!(scs[0].description, "Save — throws if 'saving' state mismatched");
        assert_eq!(scs[1].keys, "ctrl+x");

        // Dispatch the no-op shortcut.
        let ok = ext.invoke_shortcut("ctrl+x").await;
        assert!(ok.is_ok(), "expected Ok, got {ok:?}");
        // Dispatch the throwing shortcut.
        let err = ext.invoke_shortcut("ctrl+s").await;
        let Err(msg) = err else {
            panic!("expected JS throw to surface as Err");
        };
        assert!(msg.contains("not saving!"), "{msg}");
    }

    #[tokio::test]
    async fn pi_ui_notify_pushes_into_the_queue_and_drain_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "noisy.js",
            r#"
            // Top-level notifications fire at load time; handlers
            // can also use pi.ui.notify after registration.
            pi.ui.notify("hello from script");
            pi.ui.notify("second line");
            "#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();
        let drained = ext.drain_notifications();
        assert_eq!(drained.len(), 2);
        assert_eq!(
            drained[0],
            PiNotification::Notify {
                text: "hello from script".into()
            }
        );
        assert_eq!(
            drained[1],
            PiNotification::Notify {
                text: "second line".into()
            }
        );
        // Second drain returns empty — queue was cleared.
        assert!(ext.drain_notifications().is_empty());
    }

    #[tokio::test]
    async fn pi_ui_notify_inside_command_handler_routes_through_queue() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "cmd.js",
            r#"
            pi.registerCommand("say", {
                description: "Push a notification",
                handler: (args) => { pi.ui.notify("said: " + args.what); },
            });
            "#,
        );
        let ext = PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap();
        // No notifications yet — registration alone shouldn't fire any.
        assert!(ext.drain_notifications().is_empty());
        ext.invoke_command("say", serde_json::json!({ "what": "hi" }))
            .await
            .unwrap();
        let drained = ext.drain_notifications();
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0],
            PiNotification::Notify {
                text: "said: hi".into()
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pi_ui_confirm_blocks_until_host_resolves() {
        // Multi-thread tokio so we can run the JS handler on one
        // worker and resolve the modal from another.
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "ask.js",
            r#"
            pi.registerCommand("ask", {
                description: "Ask a yes/no question",
                handler: () => {
                    const ok = pi.ui.confirm("really?");
                    pi.ui.notify("answer was " + ok);
                },
            });
            "#,
        );
        let ext = Arc::new(PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap());
        // Spawn the command invocation — it'll block inside the
        // Boa worker until we resolve the modal.
        let ext_for_invoke = ext.clone();
        let invoke_task = tokio::spawn(async move {
            ext_for_invoke
                .invoke_command("ask", serde_json::json!({}))
                .await
        });
        // Wait for the confirm modal to appear in the queue. Poll
        // because the JS handler ran on the worker thread and we
        // can't precisely await that.
        let mut confirm_id = None;
        for _ in 0..200 {
            for note in ext.drain_notifications() {
                if let PiNotification::Confirm { request_id, prompt } = note {
                    assert_eq!(prompt, "really?");
                    confirm_id = Some(request_id);
                    break;
                }
            }
            if confirm_id.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let confirm_id = confirm_id.expect("confirm modal never appeared");
        // Resolve with `true`.
        ext.resolve_modal(confirm_id, serde_json::json!(true)).unwrap();
        // The handler should now finish and post the answer.
        invoke_task.await.unwrap().unwrap();
        // Drain any remaining notifications — should include the
        // post-confirm notify.
        let leftover = ext.drain_notifications();
        assert!(
            leftover.iter().any(|n| matches!(n,
                PiNotification::Notify { text } if text == "answer was true"
            )),
            "expected post-confirm notify, got {leftover:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pi_ui_input_returns_resolved_string() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "name.js",
            r#"
            pi.registerCommand("name", {
                description: "Ask for a name",
                handler: () => {
                    const who = pi.ui.input("who are you?");
                    pi.ui.notify("hello " + who);
                },
            });
            "#,
        );
        let ext = Arc::new(PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap());
        let ext_for_invoke = ext.clone();
        let invoke_task = tokio::spawn(async move {
            ext_for_invoke
                .invoke_command("name", serde_json::json!({}))
                .await
        });
        let mut input_id = None;
        for _ in 0..200 {
            for note in ext.drain_notifications() {
                if let PiNotification::Input { request_id, .. } = note {
                    input_id = Some(request_id);
                    break;
                }
            }
            if input_id.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let input_id = input_id.expect("input modal never appeared");
        ext.resolve_modal(input_id, serde_json::json!("Yoda")).unwrap();
        invoke_task.await.unwrap().unwrap();
        let leftover = ext.drain_notifications();
        assert!(
            leftover.iter().any(|n| matches!(n,
                PiNotification::Notify { text } if text == "hello Yoda"
            )),
            "expected greeting notify, got {leftover:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pi_ui_select_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(
            tmp.path(),
            "pick.js",
            r#"
            pi.registerCommand("pick", {
                description: "Pick a fruit",
                handler: () => {
                    const fruit = pi.ui.select("which?", ["apple", "banana", "cherry"]);
                    pi.ui.notify("picked " + fruit);
                },
            });
            "#,
        );
        let ext = Arc::new(PiExtension::from_dirs(&[tmp.path().to_path_buf()]).unwrap());
        let ext_for_invoke = ext.clone();
        let invoke_task = tokio::spawn(async move {
            ext_for_invoke
                .invoke_command("pick", serde_json::json!({}))
                .await
        });
        let mut select_id = None;
        let mut received_items = vec![];
        for _ in 0..200 {
            for note in ext.drain_notifications() {
                if let PiNotification::Select {
                    request_id, items, ..
                } = note
                {
                    select_id = Some(request_id);
                    received_items = items;
                    break;
                }
            }
            if select_id.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let select_id = select_id.expect("select modal never appeared");
        assert_eq!(received_items, vec!["apple", "banana", "cherry"]);
        ext.resolve_modal(select_id, serde_json::json!("banana"))
            .unwrap();
        invoke_task.await.unwrap().unwrap();
        let leftover = ext.drain_notifications();
        assert!(
            leftover.iter().any(|n| matches!(n,
                PiNotification::Notify { text } if text == "picked banana"
            )),
            "got {leftover:?}"
        );
    }

    #[tokio::test]
    async fn from_pi_dirs_resolves_workspace_dot_pi() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_dir = tmp.path().join(".pi").join("extensions");
        std::fs::create_dir_all(&ext_dir).unwrap();
        write_script(
            &ext_dir,
            "demo.js",
            r#"
            export default (pi) => {
                pi.registerTool({
                    name: "demo",
                    description: "",
                    parameters: {},
                    execute: () => "ok",
                });
            };
            "#,
        );
        let ext = PiExtension::from_pi_dirs(tmp.path()).unwrap();
        assert_eq!(ext.tools().len(), 1);
        assert_eq!(ext.tools()[0].definition().name, "demo");
    }
}
