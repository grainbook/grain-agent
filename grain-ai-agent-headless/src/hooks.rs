//! Runtime-configurable agent hooks.
//!
//! This module intentionally implements a small declarative surface first:
//! users can define hooks in TOML without recompiling, while host UIs can
//! layer script-backed hooks on top. The exported builders compile rules into
//! the existing `grain-agent-core` hook aliases.

use std::sync::Arc;

use futures::future::BoxFuture;
use globset::Glob;
use grain_agent_core::{
    AfterToolCallFn, AfterToolCallResult, AgentContext, AgentLoopTurnUpdate, AgentMessage,
    BeforeToolCallFn, BeforeToolCallResult, PrepareNextTurnFn, UserContent, UserMessage,
};
use grain_llm_models::Registry;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    BeforeToolCall,
    AfterToolCall,
    PrepareNextTurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookAction {
    Deny,
    Truncate,
    Redact,
    MarkError,
    Terminate,
    InjectUserMessage,
    SwitchModel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookRule {
    pub name: String,
    pub event: HookEvent,
    pub action: HookAction,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub replacement: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookTrace {
    pub name: String,
    pub event: HookEvent,
    pub action: HookAction,
    pub message: String,
}

pub type HookTraceSink = Arc<dyn Fn(HookTrace) + Send + Sync>;

#[derive(Clone)]
pub struct HookRegistry {
    rules: Arc<Vec<HookRule>>,
    trace_sink: Option<HookTraceSink>,
}

impl HookRegistry {
    pub fn new(rules: Vec<HookRule>) -> Self {
        HookRegistry {
            rules: Arc::new(rules),
            trace_sink: None,
        }
    }

    pub fn with_trace_sink(mut self, sink: HookTraceSink) -> Self {
        self.trace_sink = Some(sink);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rules(&self) -> &[HookRule] {
        &self.rules
    }

    fn trace(&self, rule: &HookRule, message: impl Into<String>) {
        if let Some(sink) = &self.trace_sink {
            sink(HookTrace {
                name: rule.name.clone(),
                event: rule.event,
                action: rule.action,
                message: message.into(),
            });
        }
    }
}

pub fn before_tool_call_hook(registry: Arc<HookRegistry>) -> Option<BeforeToolCallFn> {
    if !registry
        .rules()
        .iter()
        .any(|r| r.event == HookEvent::BeforeToolCall)
    {
        return None;
    }
    Some(Arc::new(move |ctx, _cancel| {
        let registry = registry.clone();
        Box::pin(async move {
            let eval = EvalContext {
                event: HookEvent::BeforeToolCall,
                tool_name: Some(ctx.tool_call.name.as_str()),
                args: Some(&ctx.args),
                result_text: None,
                is_error: None,
                message_count: ctx.context.messages.len(),
            };
            for rule in registry
                .rules()
                .iter()
                .filter(|r| r.event == HookEvent::BeforeToolCall)
            {
                if !matches_rule(rule, &eval) {
                    continue;
                }
                if rule.action == HookAction::Deny {
                    let reason = rule
                        .reason
                        .clone()
                        .unwrap_or_else(|| format!("blocked by hook '{}'", rule.name));
                    registry.trace(rule, reason.clone());
                    return Some(BeforeToolCallResult {
                        block: true,
                        reason: Some(reason),
                    });
                }
            }
            None
        })
    }))
}

pub fn after_tool_call_hook(registry: Arc<HookRegistry>) -> Option<AfterToolCallFn> {
    if !registry
        .rules()
        .iter()
        .any(|r| r.event == HookEvent::AfterToolCall)
    {
        return None;
    }
    Some(Arc::new(move |ctx, _cancel| {
        let registry = registry.clone();
        Box::pin(async move {
            let mut out = AfterToolCallResult::default();
            let mut text = tool_result_text(&ctx.result.content);
            let mut changed_text = false;
            for rule in registry
                .rules()
                .iter()
                .filter(|r| r.event == HookEvent::AfterToolCall)
            {
                let eval = EvalContext {
                    event: HookEvent::AfterToolCall,
                    tool_name: Some(ctx.tool_call.name.as_str()),
                    args: Some(&ctx.args),
                    result_text: Some(&text),
                    is_error: Some(ctx.is_error),
                    message_count: ctx.context.messages.len(),
                };
                if !matches_rule(rule, &eval) {
                    continue;
                }
                match rule.action {
                    HookAction::Truncate => {
                        let max = rule.max_bytes.unwrap_or(20_000);
                        if text.len() > max {
                            text.truncate(max);
                            text.push_str("\n[truncated by hook]");
                            changed_text = true;
                            registry.trace(rule, format!("truncated result to {max} bytes"));
                        }
                    }
                    HookAction::Redact => {
                        if let Some(pattern) = &rule.pattern
                            && let Ok(re) = Regex::new(pattern)
                        {
                            let replacement = rule.replacement.as_deref().unwrap_or("[redacted]");
                            let next = re.replace_all(&text, replacement).to_string();
                            if next != text {
                                text = next;
                                changed_text = true;
                                registry.trace(rule, "redacted tool result");
                            }
                        }
                    }
                    HookAction::MarkError => {
                        out.is_error = Some(true);
                        registry.trace(rule, "marked tool result as error");
                    }
                    HookAction::Terminate => {
                        out.terminate = Some(true);
                        registry.trace(rule, "requested loop termination");
                    }
                    _ => {}
                }
            }
            if changed_text {
                out.content = Some(vec![UserContent::text(text)]);
            }
            if out.content.is_some()
                || out.details.is_some()
                || out.is_error.is_some()
                || out.terminate.is_some()
            {
                Some(out)
            } else {
                None
            }
        })
    }))
}

pub fn prepare_next_turn_hook(
    registry: Arc<HookRegistry>,
    model_registry: Arc<Registry>,
) -> Option<PrepareNextTurnFn> {
    if !registry
        .rules()
        .iter()
        .any(|r| r.event == HookEvent::PrepareNextTurn)
    {
        return None;
    }
    Some(Arc::new(move |ctx, _cancel| {
        let registry = registry.clone();
        let model_registry = model_registry.clone();
        Box::pin(async move {
            let mut update = AgentLoopTurnUpdate::default();
            let mut context: Option<AgentContext> = None;
            let eval = EvalContext {
                event: HookEvent::PrepareNextTurn,
                tool_name: None,
                args: None,
                result_text: None,
                is_error: None,
                message_count: ctx.context.messages.len(),
            };
            for rule in registry
                .rules()
                .iter()
                .filter(|r| r.event == HookEvent::PrepareNextTurn)
            {
                if !matches_rule(rule, &eval) {
                    continue;
                }
                match rule.action {
                    HookAction::InjectUserMessage => {
                        let Some(message) = rule.message.clone() else {
                            continue;
                        };
                        let target = context.get_or_insert_with(|| (*ctx.context).clone());
                        target.messages.push(user_text_message(message));
                        registry.trace(rule, "injected user message");
                    }
                    HookAction::SwitchModel => {
                        let Some(model_id) = rule.model.as_deref() else {
                            continue;
                        };
                        let spec = match grain_llm_genai::parse_model_spec(model_id) {
                            Ok(spec) => spec,
                            Err(e) => {
                                registry.trace(rule, e);
                                continue;
                            }
                        };
                        if let Some(mut model) = model_registry.to_core_model(&spec.id) {
                            if let Some(context_window) = spec.context_window {
                                model.context_window = context_window;
                            }
                            update.model = Some(model);
                            registry.trace(rule, format!("switched model to {}", spec.id));
                        }
                    }
                    _ => {}
                }
            }
            if let Some(context) = context {
                update.context = Some(context);
            }
            if update.context.is_some() || update.model.is_some() || update.thinking_level.is_some()
            {
                Some(update)
            } else {
                None
            }
        })
    }))
}

struct EvalContext<'a> {
    event: HookEvent,
    tool_name: Option<&'a str>,
    args: Option<&'a serde_json::Value>,
    result_text: Option<&'a str>,
    is_error: Option<bool>,
    message_count: usize,
}

fn matches_rule(rule: &HookRule, ctx: &EvalContext<'_>) -> bool {
    if rule.event != ctx.event {
        return false;
    }
    if let Some(tool) = &rule.tool
        && ctx.tool_name != Some(tool.as_str())
    {
        return false;
    }
    match rule
        .when
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(expr) => eval_when(expr, ctx),
        None => true,
    }
}

fn eval_when(expr: &str, ctx: &EvalContext<'_>) -> bool {
    if let Some((lhs, rhs)) = split_binary(expr, " contains ") {
        let Some(value) = lookup_string(lhs.trim(), ctx) else {
            return false;
        };
        return value.contains(&unquote(rhs.trim()));
    }
    if let Some((lhs, rhs)) = split_binary(expr, " matches ") {
        let Some(value) = lookup_string(lhs.trim(), ctx) else {
            return false;
        };
        let pattern = unquote(rhs.trim());
        return Glob::new(&pattern)
            .ok()
            .map(|glob| glob.compile_matcher().is_match(value))
            .unwrap_or_else(|| {
                Regex::new(&pattern)
                    .map(|re| re.is_match(value))
                    .unwrap_or(false)
            });
    }
    if let Some((lhs, rhs)) = split_binary(expr, " == ") {
        return lookup_scalar(lhs.trim(), ctx).as_deref() == Some(unquote(rhs.trim()).as_str());
    }
    if let Some((lhs, rhs)) = split_binary(expr, " != ") {
        return lookup_scalar(lhs.trim(), ctx).as_deref() != Some(unquote(rhs.trim()).as_str());
    }
    for op in [">=", "<=", ">", "<"] {
        if let Some((lhs, rhs)) = split_binary(expr, op) {
            let Some(left) = lookup_number(lhs.trim(), ctx) else {
                return false;
            };
            let Ok(right) = rhs.trim().parse::<f64>() else {
                return false;
            };
            return match op {
                ">=" => left >= right,
                "<=" => left <= right,
                ">" => left > right,
                "<" => left < right,
                _ => false,
            };
        }
    }
    false
}

fn split_binary<'a>(expr: &'a str, op: &str) -> Option<(&'a str, &'a str)> {
    expr.split_once(op)
}

fn lookup_scalar(path: &str, ctx: &EvalContext<'_>) -> Option<String> {
    match path {
        "tool" => ctx.tool_name.map(ToOwned::to_owned),
        "is_error" => ctx.is_error.map(|v| v.to_string()),
        "message_count" => Some(ctx.message_count.to_string()),
        "result" | "result.text" => ctx.result_text.map(ToOwned::to_owned),
        _ => lookup_string(path, ctx).map(ToOwned::to_owned),
    }
}

fn lookup_string<'a>(path: &str, ctx: &'a EvalContext<'_>) -> Option<&'a str> {
    let rest = path.strip_prefix("args.")?;
    let mut value = ctx.args?;
    for part in rest.split('.') {
        value = value.get(part)?;
    }
    value.as_str()
}

fn lookup_number(path: &str, ctx: &EvalContext<'_>) -> Option<f64> {
    match path {
        "message_count" => Some(ctx.message_count as f64),
        _ => lookup_scalar(path, ctx)?.parse().ok(),
    }
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn tool_result_text(content: &[UserContent]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            UserContent::Text(t) => Some(t.text.as_str()),
            UserContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
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

pub fn chain_before_hooks(
    a: Option<BeforeToolCallFn>,
    b: Option<BeforeToolCallFn>,
) -> Option<BeforeToolCallFn> {
    match (a, b) {
        (None, None) => None,
        (Some(h), None) | (None, Some(h)) => Some(h),
        (Some(a), Some(b)) => Some(Arc::new(move |ctx, cancel| {
            let a = a.clone();
            let b = b.clone();
            Box::pin(async move {
                if let Some(result) = a(ctx.clone(), cancel.clone()).await
                    && result.block
                {
                    return Some(result);
                }
                b(ctx, cancel).await
            })
        })),
    }
}

pub fn chain_after_hooks(
    a: Option<AfterToolCallFn>,
    b: Option<AfterToolCallFn>,
) -> Option<AfterToolCallFn> {
    match (a, b) {
        (None, None) => None,
        (Some(h), None) | (None, Some(h)) => Some(h),
        (Some(a), Some(b)) => Some(Arc::new(move |ctx, cancel| {
            let a = a.clone();
            let b = b.clone();
            Box::pin(async move {
                let first = a(ctx.clone(), cancel.clone()).await;
                let mut next_ctx = ctx;
                if let Some(result) = &first {
                    if let Some(content) = &result.content {
                        next_ctx.result.content = content.clone();
                    }
                    if let Some(details) = &result.details {
                        next_ctx.result.details = details.clone();
                    }
                    if let Some(terminate) = result.terminate {
                        next_ctx.result.terminate = Some(terminate);
                    }
                    if let Some(is_error) = result.is_error {
                        next_ctx.is_error = is_error;
                    }
                }
                let second = b(next_ctx, cancel).await;
                merge_after_results(first, second)
            })
        })),
    }
}

fn merge_after_results(
    mut a: Option<AfterToolCallResult>,
    b: Option<AfterToolCallResult>,
) -> Option<AfterToolCallResult> {
    let Some(b) = b else {
        return a;
    };
    let out = a.get_or_insert_with(AfterToolCallResult::default);
    if b.content.is_some() {
        out.content = b.content;
    }
    if b.details.is_some() {
        out.details = b.details;
    }
    if b.is_error.is_some() {
        out.is_error = b.is_error;
    }
    if b.terminate.is_some() {
        out.terminate = b.terminate;
    }
    a
}

pub fn boxed_ready<T: Send + 'static>(value: T) -> BoxFuture<'static, T> {
    Box::pin(async move { value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_core::{
        AgentToolResult, AssistantMessage, BeforeToolCallContext, StopReason, ToolCall,
    };
    use tokio_util::sync::CancellationToken;

    fn assistant() -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Default::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        }
    }

    #[test]
    fn config_hook_rule_parses_from_toml() {
        let raw = r#"
            name = "block-dangerous-shell"
            event = "before_tool_call"
            action = "deny"
            tool = "bash"
            when = "args.command contains 'rm -rf'"
            reason = "nope"
        "#;
        let rule: HookRule = toml::from_str(raw).unwrap();
        assert_eq!(rule.event, HookEvent::BeforeToolCall);
        assert_eq!(rule.action, HookAction::Deny);
        assert_eq!(rule.tool.as_deref(), Some("bash"));
    }

    #[tokio::test]
    async fn before_hook_denies_matching_tool_call() {
        let registry = Arc::new(HookRegistry::new(vec![HookRule {
            name: "deny-rm".into(),
            event: HookEvent::BeforeToolCall,
            action: HookAction::Deny,
            tool: Some("bash".into()),
            when: Some("args.command contains 'rm -rf'".into()),
            reason: Some("dangerous".into()),
            max_bytes: None,
            pattern: None,
            replacement: None,
            message: None,
            model: None,
        }]));
        let hook = before_tool_call_hook(registry).unwrap();
        let got = hook(
            BeforeToolCallContext {
                assistant_message: assistant(),
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "rm -rf target"}),
                },
                args: serde_json::json!({"command": "rm -rf target"}),
                context: Arc::new(AgentContext::default()),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(got.block);
        assert_eq!(got.reason.as_deref(), Some("dangerous"));
    }

    #[tokio::test]
    async fn after_hook_truncates_text_result() {
        let registry = Arc::new(HookRegistry::new(vec![HookRule {
            name: "short".into(),
            event: HookEvent::AfterToolCall,
            action: HookAction::Truncate,
            tool: Some("read".into()),
            when: None,
            reason: None,
            max_bytes: Some(4),
            pattern: None,
            replacement: None,
            message: None,
            model: None,
        }]));
        let hook = after_tool_call_hook(registry).unwrap();
        let got = hook(
            grain_agent_core::AfterToolCallContext {
                assistant_message: assistant(),
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
                args: serde_json::json!({}),
                result: AgentToolResult::text("abcdef"),
                is_error: false,
                context: Arc::new(AgentContext::default()),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let text = tool_result_text(&got.content.unwrap());
        assert!(text.starts_with("abcd"));
        assert!(text.contains("truncated"));
    }
}
