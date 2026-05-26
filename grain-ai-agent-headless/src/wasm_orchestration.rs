//! Host-side mapper for WASM v2 orchestration plugins.
//!
//! WASM plugins can declare roles and lifecycle hooks, but they do not
//! directly mutate agent state. This module validates their requested
//! [`grain_plugin_wasm::HostAction`] values and translates the accepted
//! subset into [`grain_agent_core::AgentLoopTurnUpdate`]s.

use std::collections::HashMap;
use std::sync::Arc;

use grain_agent_core::{
    AgentContext, AgentLoopTurnUpdate, AgentMessage, AgentTool, Model, PrepareNextTurnFn,
    ThinkingLevel, UserContent, UserMessage,
};
use grain_llm_models::Registry;
use grain_plugin_wasm::{HookPoint, HostAction, OrchestrationDef, RoleDef, WasmPluginRuntime};

/// UI-facing request emitted after host-side validation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WasmUiUpdate {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub status: Option<String>,
}

pub type WasmUiSink = Arc<dyn Fn(WasmUiUpdate) + Send + Sync>;

/// Loaded orchestration metadata for one WASM plugin.
#[derive(Clone)]
struct WasmOrchestrationPlugin {
    plugin_id: String,
    runtime: Arc<WasmPluginRuntime>,
    roles: HashMap<String, RoleDef>,
    prepare_next_turn: bool,
}

/// Validates and applies v2 WASM orchestration hook actions.
#[derive(Clone)]
pub struct WasmOrchestrator {
    registry: Arc<Registry>,
    tools_by_name: HashMap<String, Arc<dyn AgentTool>>,
    plugins: Vec<WasmOrchestrationPlugin>,
    ui_sink: Option<WasmUiSink>,
}

impl WasmOrchestrator {
    /// Build an empty mapper with the model registry and full tool catalog
    /// the host is willing to expose to role actions.
    pub fn new(registry: Arc<Registry>, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        let tools_by_name = tools
            .into_iter()
            .map(|tool| (tool.definition().name.clone(), tool))
            .collect();
        WasmOrchestrator {
            registry,
            tools_by_name,
            plugins: Vec::new(),
            ui_sink: None,
        }
    }

    /// Attach a UI sink for accepted UI updates. Hosts without UI can omit
    /// this; orchestration still works.
    pub fn with_ui_sink(mut self, sink: WasmUiSink) -> Self {
        self.ui_sink = Some(sink);
        self
    }

    /// Register one loaded v2 plugin. Plugins without orchestration
    /// metadata are ignored.
    pub fn add_plugin(
        &mut self,
        plugin_id: impl Into<String>,
        runtime: Arc<WasmPluginRuntime>,
        orchestration: Option<OrchestrationDef>,
    ) {
        let Some(orchestration) = orchestration else {
            return;
        };
        let prepare_next_turn = orchestration
            .hooks
            .iter()
            .any(|hook| hook.point == HookPoint::PrepareNextTurn);
        if orchestration.roles.is_empty() && !prepare_next_turn {
            return;
        }
        let roles = orchestration
            .roles
            .into_iter()
            .map(|role| (role.name.clone(), role))
            .collect();
        self.plugins.push(WasmOrchestrationPlugin {
            plugin_id: plugin_id.into(),
            runtime,
            roles,
            prepare_next_turn,
        });
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Return a [`PrepareNextTurnFn`] that invokes every plugin hook that
    /// subscribed to `prepare-next-turn` and applies accepted actions in
    /// plugin discovery order.
    pub fn prepare_next_turn_hook(self: Arc<Self>) -> Option<PrepareNextTurnFn> {
        if self.is_empty() || !self.plugins.iter().any(|plugin| plugin.prepare_next_turn) {
            return None;
        }

        Some(Arc::new(move |ctx, _cancel| {
            let orchestrator = self.clone();
            Box::pin(async move {
                let mut draft = DraftUpdate::new((*ctx.context).clone());
                let context_json = prepare_context_json(&ctx);
                let host_rt_handle = tokio::runtime::Handle::current();

                for plugin in orchestrator
                    .plugins
                    .iter()
                    .filter(|plugin| plugin.prepare_next_turn)
                {
                    let actions = match plugin
                        .runtime
                        .call_hook(
                            &plugin.plugin_id,
                            HookPoint::PrepareNextTurn,
                            &context_json,
                            host_rt_handle.clone(),
                        )
                        .await
                    {
                        Ok(actions) => actions,
                        Err(e) => {
                            eprintln!(
                                "[warn] wasm orchestration '{}': prepare-next-turn failed: {e}",
                                plugin.plugin_id
                            );
                            continue;
                        }
                    };
                    for action in actions {
                        orchestrator.apply_action(plugin, action, &mut draft);
                    }
                }

                draft.finish()
            })
        }))
    }

    fn apply_action(
        &self,
        plugin: &WasmOrchestrationPlugin,
        action: HostAction,
        draft: &mut DraftUpdate,
    ) {
        match action {
            HostAction::SwitchRole(role_name) => {
                let Some(role) = plugin.roles.get(&role_name) else {
                    warn(&plugin.plugin_id, format!("unknown role '{role_name}'"));
                    return;
                };
                self.apply_role(&plugin.plugin_id, role, draft);
            }
            HostAction::SwitchModel(model_id) => {
                self.apply_model(&plugin.plugin_id, &model_id, draft);
            }
            HostAction::SetSystemPrompt(prompt) => {
                draft.context.system_prompt = prompt;
                draft.context_changed = true;
            }
            HostAction::SetActiveTools(names) => {
                if let Some(tools) = self.resolve_tools(&plugin.plugin_id, &names) {
                    draft.context.tools = tools;
                    draft.context_changed = true;
                }
            }
            HostAction::InjectUserMessage(text) => {
                draft.context.messages.push(user_text_message(text));
                draft.context_changed = true;
            }
            HostAction::EmitCustom(json) => match serde_json::from_str(&json) {
                Ok(value) => {
                    draft.context.messages.push(AgentMessage::Custom(value));
                    draft.context_changed = true;
                }
                Err(e) => warn(
                    &plugin.plugin_id,
                    format!("emit-custom ignored; invalid JSON: {e}"),
                ),
            },
            HostAction::StopAfterTurn(_) => {
                warn(
                    &plugin.plugin_id,
                    "stop-after-turn is not supported from prepare-next-turn",
                );
            }
            HostAction::SetUiHeader(header) => {
                self.emit_ui(WasmUiUpdate {
                    provider: header.provider,
                    model: header.model,
                    status: None,
                });
            }
            HostAction::SetUiStatus(status) => {
                self.emit_ui(WasmUiUpdate {
                    provider: None,
                    model: None,
                    status: Some(status),
                });
            }
        }
    }

    fn apply_role(&self, plugin_id: &str, role: &RoleDef, draft: &mut DraftUpdate) {
        let Some(model) = self.registry.to_core_model(&role.model) else {
            warn(plugin_id, format!("unknown model '{}'", role.model));
            return;
        };
        let Some(tools) = self.resolve_tools(plugin_id, &role.tools) else {
            return;
        };
        let thinking_level = match &role.thinking_level {
            Some(level) => match parse_thinking_level(level) {
                Some(parsed) => Some(parsed),
                None => {
                    warn(plugin_id, format!("unknown thinking level '{level}'"));
                    return;
                }
            },
            None => None,
        };

        self.apply_model_value(model, draft);
        draft.context.system_prompt = role.prompt.clone();
        draft.context.tools = tools;
        draft.context_changed = true;
        draft.thinking_level = thinking_level;
    }

    fn apply_model(&self, plugin_id: &str, model_id: &str, draft: &mut DraftUpdate) {
        match self.registry.to_core_model(model_id) {
            Some(model) => self.apply_model_value(model, draft),
            None => warn(plugin_id, format!("unknown model '{model_id}'")),
        }
    }

    fn apply_model_value(&self, model: Model, draft: &mut DraftUpdate) {
        self.emit_ui(WasmUiUpdate {
            provider: Some(model.provider.clone()),
            model: Some(model.id.clone()),
            status: None,
        });
        draft.model = Some(model);
    }

    fn resolve_tools(&self, plugin_id: &str, names: &[String]) -> Option<Vec<Arc<dyn AgentTool>>> {
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let Some(tool) = self.tools_by_name.get(name) else {
                warn(plugin_id, format!("unknown tool '{name}'"));
                return None;
            };
            out.push(tool.clone());
        }
        Some(out)
    }
}

impl WasmOrchestrator {
    fn emit_ui(&self, update: WasmUiUpdate) {
        if let Some(sink) = &self.ui_sink {
            sink(update);
        }
    }
}

struct DraftUpdate {
    context: AgentContext,
    context_changed: bool,
    model: Option<grain_agent_core::Model>,
    thinking_level: Option<ThinkingLevel>,
}

impl DraftUpdate {
    fn new(context: AgentContext) -> Self {
        DraftUpdate {
            context,
            context_changed: false,
            model: None,
            thinking_level: None,
        }
    }

    fn finish(self) -> Option<AgentLoopTurnUpdate> {
        if !self.context_changed && self.model.is_none() && self.thinking_level.is_none() {
            return None;
        }
        Some(AgentLoopTurnUpdate {
            context: self.context_changed.then_some(self.context),
            model: self.model,
            thinking_level: self.thinking_level,
        })
    }
}

fn prepare_context_json(ctx: &grain_agent_core::PrepareNextTurnContext) -> String {
    let active_tools: Vec<String> = ctx
        .context
        .tools
        .iter()
        .map(|tool| tool.definition().name.clone())
        .collect();
    serde_json::to_string(&serde_json::json!({
        "hook": "prepareNextTurn",
        "message": ctx.message,
        "toolResults": ctx.tool_results,
        "newMessages": ctx.new_messages,
        "systemPrompt": ctx.context.system_prompt,
        "activeTools": active_tools,
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

fn user_text_message(text: String) -> AgentMessage {
    AgentMessage::user(UserMessage {
        content: vec![UserContent::text(text)],
        timestamp: current_time_ms(),
    })
}

fn current_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn parse_thinking_level(value: &str) -> Option<ThinkingLevel> {
    match value {
        "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        _ => None,
    }
}

fn warn(plugin_id: &str, message: impl AsRef<str>) {
    eprintln!(
        "[warn] wasm orchestration '{}': {}",
        plugin_id,
        message.as_ref()
    );
}
