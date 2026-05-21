//! Boa-owning worker thread + IPC types.
//!
//! `boa_engine::Context` is `!Send`, so we can't share it across
//! tokio tasks. The worker lives on a dedicated `std::thread`, owns
//! one `Context` for the lifetime of the [`crate::BoaExtension`], and
//! talks to the parent via `std::sync::mpsc` for commands. Replies use
//! `std::sync::mpsc::SyncSender` for the synchronous load/list ops
//! and `tokio::sync::oneshot` for the async invoke op (so the agent
//! loop can `await` a tool execution).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use boa_engine::{
    Context, JsError, JsNativeError, JsObject, JsValue, NativeFunction, Source, js_string,
    property::Attribute,
};

/// Metadata captured when a script calls `grain.register_tool({...})`.
#[derive(Debug, Clone)]
pub struct ToolMeta {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

/// Tool invocation result returned by a JS `run` function. JS may
/// return either a bare string (taken as `content`, no error) or an
/// object `{ content: string, is_error?: bool }`.
#[derive(Debug)]
pub struct ToolReply {
    pub content: String,
    pub is_error: bool,
}

/// Commands sent from `BoaExtension` (or `ScriptedTool`) to the worker.
pub(crate) enum WorkerCmd {
    LoadScript {
        code: String,
        label: String,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    ListTools {
        reply: mpsc::SyncSender<Vec<ToolMeta>>,
    },
    InvokeTool {
        name: String,
        args: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<ToolReply, String>>,
    },
    /// Generic named-callback invocation. Used by higher layers
    /// (e.g. `grain-pi-compat`) to dispatch agent events into JS
    /// handlers registered via `grain.register_callback(name, fn)`.
    /// Returns `Ok(())` when the callback ran cleanly; `Err(_)` if
    /// the JS function threw. Unregistered names are NOT an error —
    /// they're a no-op so listeners can fire for every event even
    /// when the script subscribed to a subset.
    InvokeCallback {
        name: String,
        args: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Snapshot every metadata entry the worker has under a given
    /// `kind` (registered via `grain.register_meta`). Used by the pi
    /// compatibility layer to surface command / shortcut /
    /// renderer descriptors at startup.
    ListMetas {
        kind: String,
        reply: mpsc::SyncSender<Vec<(String, serde_json::Value)>>,
    },
    Shutdown,
}

/// Handle the parent holds onto. Dropping it sends `Shutdown` and
/// joins the worker thread cleanly.
pub(crate) struct WorkerHandle {
    pub(crate) tx: mpsc::Sender<WorkerCmd>,
    /// Channel the parent uses to resolve `grain.modal_request`
    /// calls. Each entry is `(request_id, response_value)`. Worker
    /// host-fn blocks on this until the matching id arrives.
    pub(crate) modal_tx: mpsc::Sender<(u64, serde_json::Value)>,
    /// Parent-side receiver for `grain.push_notification(...)` and
    /// `grain.modal_request(...)` payloads. Lives on the parent
    /// side (NOT inside the worker thread's state) so the parent
    /// can drain notifications even while the worker is blocked
    /// inside a synchronous modal host fn.
    pub(crate) notify_rx: Arc<Mutex<mpsc::Receiver<serde_json::Value>>>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerCmd::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Spawn the worker thread + install the `grain` global. Returns a
/// handle the parent can clone the `tx` from.
pub(crate) fn spawn_worker() -> WorkerHandle {
    let (tx, rx) = mpsc::channel::<WorkerCmd>();
    let (modal_tx, modal_rx) = mpsc::channel::<(u64, serde_json::Value)>();
    let (notify_tx, notify_rx) = mpsc::channel::<serde_json::Value>();
    let notify_tx_for_worker = notify_tx;
    let join = thread::Builder::new()
        .name("grain-script-boa".into())
        .spawn(move || worker_loop(rx, modal_rx, notify_tx_for_worker))
        .expect("spawn boa worker thread");
    WorkerHandle {
        tx,
        modal_tx,
        notify_rx: Arc::new(Mutex::new(notify_rx)),
        join: Some(join),
    }
}

struct WorkerState {
    fns: HashMap<String, JsObject>,
    metas: HashMap<String, ToolMeta>,
    /// Named JS callbacks (registered via `grain.register_callback`).
    /// Separate from `fns` so tool / callback namespaces don't collide.
    callbacks: HashMap<String, JsObject>,
    /// Generic kind-scoped metadata bag. Keyed by `(kind, name)`,
    /// value is whatever JSON the script passed. Higher layers
    /// (e.g. grain-pi-compat) use this to attach command / shortcut /
    /// renderer descriptors alongside their callbacks.
    extra_metas: HashMap<(String, String), serde_json::Value>,
    /// Monotonic request id for `grain.modal_request(...)` calls.
    next_modal_id: u64,
}

fn worker_loop(
    rx: mpsc::Receiver<WorkerCmd>,
    modal_rx: mpsc::Receiver<(u64, serde_json::Value)>,
    notify_tx: mpsc::Sender<serde_json::Value>,
) {
    let state: Rc<RefCell<WorkerState>> = Rc::new(RefCell::new(WorkerState {
        fns: HashMap::new(),
        metas: HashMap::new(),
        callbacks: HashMap::new(),
        extra_metas: HashMap::new(),
        next_modal_id: 1,
    }));
    let modal_rx = Rc::new(RefCell::new(modal_rx));
    let mut ctx = Context::default();
    install_grain_global(&mut ctx, state.clone(), modal_rx, notify_tx);

    while let Ok(cmd) = rx.recv() {
        match cmd {
            WorkerCmd::LoadScript { code, label, reply } => {
                let res = ctx
                    .eval(Source::from_bytes(code.as_bytes()))
                    .map(|_| ())
                    .map_err(|e| format!("{label}: {e}"));
                let _ = reply.send(res);
            }
            WorkerCmd::ListTools { reply } => {
                let metas: Vec<ToolMeta> = state.borrow().metas.values().cloned().collect();
                let _ = reply.send(metas);
            }
            WorkerCmd::InvokeTool { name, args, reply } => {
                let res = invoke_tool(&mut ctx, &state, &name, args);
                let _ = reply.send(res);
            }
            WorkerCmd::InvokeCallback { name, args, reply } => {
                let res = invoke_callback(&mut ctx, &state, &name, args);
                let _ = reply.send(res);
            }
            WorkerCmd::ListMetas { kind, reply } => {
                let entries: Vec<(String, serde_json::Value)> = state
                    .borrow()
                    .extra_metas
                    .iter()
                    .filter(|((k, _), _)| k == &kind)
                    .map(|((_, name), v)| (name.clone(), v.clone()))
                    .collect();
                let _ = reply.send(entries);
            }
            WorkerCmd::Shutdown => break,
        }
    }
}

/// Install a `grain` global object with a `register_tool` method.
fn install_grain_global(
    ctx: &mut Context,
    state: Rc<RefCell<WorkerState>>,
    modal_rx: Rc<RefCell<mpsc::Receiver<(u64, serde_json::Value)>>>,
    notify_tx: mpsc::Sender<serde_json::Value>,
) {
    let state_clone = state.clone();
    let register = unsafe {
        NativeFunction::from_closure(move |_this, args, ictx| {
            let opts_val = args.first().cloned().unwrap_or(JsValue::undefined());
            let Some(opts) = opts_val.as_object() else {
                return Err(JsError::from_native(
                    JsNativeError::typ()
                        .with_message("grain.register_tool: expected an object argument"),
                ));
            };
            let name = opts
                .get(js_string!("name"), ictx)?
                .to_string(ictx)?
                .to_std_string()
                .unwrap_or_default();
            if name.is_empty() {
                return Err(JsError::from_native(
                    JsNativeError::typ()
                        .with_message("grain.register_tool: `name` is required"),
                ));
            }
            let description = opts
                .get(js_string!("description"), ictx)?
                .to_string(ictx)?
                .to_std_string()
                .unwrap_or_default();
            let schema_val = opts.get(js_string!("schema"), ictx)?;
            let schema = if schema_val.is_undefined() || schema_val.is_null() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                schema_val
                    .to_json(ictx)?
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()))
            };
            let run_val = opts.get(js_string!("run"), ictx)?;
            let Some(run_obj) = run_val.as_object() else {
                return Err(JsError::from_native(JsNativeError::typ().with_message(
                    "grain.register_tool: `run` must be a function",
                )));
            };
            if !run_obj.is_callable() {
                return Err(JsError::from_native(JsNativeError::typ().with_message(
                    "grain.register_tool: `run` must be callable",
                )));
            }
            state_clone
                .borrow_mut()
                .fns
                .insert(name.clone(), run_obj.clone());
            state_clone.borrow_mut().metas.insert(
                name.clone(),
                ToolMeta {
                    name,
                    description,
                    schema,
                },
            );
            Ok(JsValue::undefined())
        })
    };

    // `grain.register_callback(name, fn)` — generic named-callback
    // registry. Higher layers (e.g. grain-pi-compat) translate their
    // domain-specific event names into this single host call.
    let cb_state = state.clone();
    let register_callback = unsafe {
        NativeFunction::from_closure(move |_this, args, ictx| {
            let name = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(ictx)?
                .to_std_string()
                .unwrap_or_default();
            if name.is_empty() {
                return Err(JsError::from_native(JsNativeError::typ().with_message(
                    "grain.register_callback: callback name (string) is required",
                )));
            }
            let fn_val = args.get(1).cloned().unwrap_or(JsValue::undefined());
            let Some(fn_obj) = fn_val.as_object() else {
                return Err(JsError::from_native(JsNativeError::typ().with_message(
                    "grain.register_callback: second argument must be a function",
                )));
            };
            if !fn_obj.is_callable() {
                return Err(JsError::from_native(JsNativeError::typ().with_message(
                    "grain.register_callback: second argument must be callable",
                )));
            }
            cb_state
                .borrow_mut()
                .callbacks
                .insert(name, fn_obj.clone());
            Ok(JsValue::undefined())
        })
    };

    // `grain.register_meta(kind, name, attrs)` — generic kv slot for
    // descriptors that higher layers want to surface at startup
    // (commands, shortcuts, renderers, …).
    let meta_state = state.clone();
    let register_meta = unsafe {
        NativeFunction::from_closure(move |_this, args, ictx| {
            let kind = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(ictx)?
                .to_std_string()
                .unwrap_or_default();
            let name = args
                .get(1)
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(ictx)?
                .to_std_string()
                .unwrap_or_default();
            if kind.is_empty() || name.is_empty() {
                return Err(JsError::from_native(JsNativeError::typ().with_message(
                    "grain.register_meta: kind and name must be non-empty strings",
                )));
            }
            let attrs_val = args.get(2).cloned().unwrap_or(JsValue::undefined());
            let attrs = if attrs_val.is_undefined() || attrs_val.is_null() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                attrs_val
                    .to_json(ictx)?
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()))
            };
            meta_state
                .borrow_mut()
                .extra_metas
                .insert((kind, name), attrs);
            Ok(JsValue::undefined())
        })
    };

    let grain_obj = JsObject::with_null_proto();
    grain_obj
        .set(js_string!("register_tool"), register.to_js_function(ctx.realm()), false, ctx)
        .expect("set register_tool on grain");
    grain_obj
        .set(
            js_string!("register_callback"),
            register_callback.to_js_function(ctx.realm()),
            false,
            ctx,
        )
        .expect("set register_callback on grain");
    grain_obj
        .set(
            js_string!("register_meta"),
            register_meta.to_js_function(ctx.realm()),
            false,
            ctx,
        )
        .expect("set register_meta on grain");

    // `grain.push_notification(payload)` — append a JSON value to
    // the parent's notification queue. Fire-and-forget; the host
    // drains it on its own cadence. The tx is shared with
    // `modal_request` below so both go through the same channel —
    // important because the parent must be able to read
    // notifications even while a host fn is blocking on a modal.
    let notify_tx_push = notify_tx.clone();
    let push_notification = unsafe {
        NativeFunction::from_closure(move |_this, args, ictx| {
            let payload_val = args.first().cloned().unwrap_or(JsValue::undefined());
            let payload = if payload_val.is_undefined() || payload_val.is_null() {
                serde_json::Value::Object(Default::default())
            } else {
                payload_val.to_json(ictx)?.unwrap_or_else(|| {
                    serde_json::Value::Object(Default::default())
                })
            };
            let _ = notify_tx_push.send(payload);
            Ok(JsValue::undefined())
        })
    };
    grain_obj
        .set(
            js_string!("push_notification"),
            push_notification.to_js_function(ctx.realm()),
            false,
            ctx,
        )
        .expect("set push_notification on grain");

    // `grain.modal_request(kind, payload)` — synchronous modal
    // round-trip. Pushes `{ kind, request_id, ...payload }` to the
    // notification queue, **blocks the worker thread** on
    // `modal_rx` until the host calls `resolve_modal(request_id,
    // response)`, then returns the response value to JS.
    //
    // Blocking is intentional: it lets pi extensions write
    // `const ok = pi.ui.confirm("?")` and read the answer
    // synchronously, mirroring pi's documented semantics. The cost:
    // while a modal is open the worker can't process other
    // commands — fine inside a command handler (user is already
    // waiting) but a deadlock risk inside an event listener (the
    // agent's progress depends on the listener returning).
    let modal_state = state.clone();
    let modal_rx_for_fn = modal_rx.clone();
    let notify_tx_modal = notify_tx;
    let modal_request = unsafe {
        NativeFunction::from_closure(move |_this, args, ictx| {
            let kind = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(ictx)?
                .to_std_string()
                .unwrap_or_default();
            if kind.is_empty() {
                return Err(JsError::from_native(JsNativeError::typ().with_message(
                    "grain.modal_request: kind (string) is required",
                )));
            }
            let payload_val = args.get(1).cloned().unwrap_or(JsValue::undefined());
            let mut payload = if payload_val.is_undefined() || payload_val.is_null() {
                serde_json::Map::new()
            } else {
                let v = payload_val.to_json(ictx)?.unwrap_or_else(|| {
                    serde_json::Value::Object(serde_json::Map::new())
                });
                match v {
                    serde_json::Value::Object(m) => m,
                    _ => serde_json::Map::new(),
                }
            };
            let request_id = {
                let mut s = modal_state.borrow_mut();
                let id = s.next_modal_id;
                s.next_modal_id += 1;
                id
            };
            payload.insert("kind".into(), serde_json::Value::String(kind));
            payload.insert(
                "request_id".into(),
                serde_json::Value::Number(request_id.into()),
            );
            let _ = notify_tx_modal.send(serde_json::Value::Object(payload));

            // Drain modal_rx until we see our id. Any response with
            // a different id is dropped — out-of-order or duplicate
            // resolves shouldn't happen in practice (one modal at a
            // time per worker), but the loop keeps us robust if they
            // do.
            loop {
                let recv_result = modal_rx_for_fn.borrow_mut().recv();
                let Ok((resp_id, resp_value)) = recv_result else {
                    return Err(JsError::from_native(
                        JsNativeError::error()
                            .with_message("grain.modal_request: modal channel closed"),
                    ));
                };
                if resp_id == request_id {
                    return JsValue::from_json(&resp_value, ictx);
                }
                // else: not our response, drop it.
            }
        })
    };
    grain_obj
        .set(
            js_string!("modal_request"),
            modal_request.to_js_function(ctx.realm()),
            false,
            ctx,
        )
        .expect("set modal_request on grain");

    ctx.register_global_property(js_string!("grain"), grain_obj, Attribute::all())
        .expect("register global `grain`");
}

fn invoke_callback(
    ctx: &mut Context,
    state: &Rc<RefCell<WorkerState>>,
    name: &str,
    args: serde_json::Value,
) -> Result<(), String> {
    let func = state.borrow().callbacks.get(name).cloned();
    let Some(func) = func else {
        // Unregistered callback — silent no-op so callers can fire
        // every event without checking each script's subscriptions.
        return Ok(());
    };
    let args_js = JsValue::from_json(&args, ctx).map_err(|e| e.to_string())?;
    func.call(&JsValue::undefined(), &[args_js], ctx)
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn invoke_tool(
    ctx: &mut Context,
    state: &Rc<RefCell<WorkerState>>,
    name: &str,
    args: serde_json::Value,
) -> Result<ToolReply, String> {
    let func = state
        .borrow()
        .fns
        .get(name)
        .cloned()
        .ok_or_else(|| format!("tool '{name}' not registered"))?;
    let args_js = JsValue::from_json(&args, ctx)
        .map_err(|e| e.to_string())?;
    // `JsValue::from_json` returns JsValue directly in 0.21 — no Option wrap.
    let result = func
        .call(&JsValue::undefined(), &[args_js], ctx)
        .map_err(|e| e.to_string())?;
    coerce_tool_reply(ctx, result)
}

fn coerce_tool_reply(ctx: &mut Context, result: JsValue) -> Result<ToolReply, String> {
    if let Some(obj) = result.as_object()
        && obj.has_property(js_string!("content"), ctx).unwrap_or(false)
    {
        let content = obj
            .get(js_string!("content"), ctx)
            .map_err(|e| e.to_string())?
            .to_string(ctx)
            .map_err(|e| e.to_string())?
            .to_std_string()
            .unwrap_or_default();
        let is_error = obj
            .get(js_string!("is_error"), ctx)
            .map_err(|e| e.to_string())?
            .to_boolean();
        return Ok(ToolReply { content, is_error });
    }
    let s = result
        .to_string(ctx)
        .map_err(|e| e.to_string())?
        .to_std_string()
        .unwrap_or_default();
    Ok(ToolReply {
        content: s,
        is_error: false,
    })
}
