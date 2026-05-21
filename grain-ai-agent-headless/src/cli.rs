//! CLI surface for the `grain-headless` binary.
//!
//! `Args` is the parsed command-line shape; `run(args)` builds a Workspace,
//! Registry, GenaiStream, Agent, registers the read-only tools + context
//! guard, and drives one prompt to completion while streaming events to
//! stdout. Returns once the loop ends.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::{Parser, ValueEnum};
use grain_agent_core::{
    Agent, AgentEvent, AgentMessage, AgentOptions, AssistantMessageEvent, Message,
};
use grain_agent_harness::context_guard::{ContextGuard, ContextGuardPolicy};
use grain_llm_genai::{GenaiStream, OpenAiCompatPreset};
use grain_llm_models::Registry;

use crate::prompt::{
    DEFAULT_CODING_AGENT_SYSTEM_PROMPT, WRITE_ENABLED_CODING_AGENT_SYSTEM_PROMPT,
};
use crate::runtime::{coding_all_tools, coding_read_tools};
use crate::workspace::Workspace;

/// `grain-headless` — single-prompt coding agent over the local workspace.
#[derive(Debug, Parser)]
#[command(name = "grain-headless", version, about, long_about = None)]
pub struct Args {
    /// Workspace root (file tools refuse to read outside this directory).
    #[arg(short = 'C', long, default_value = ".")]
    pub workspace: PathBuf,

    /// Model id from `grain-llm-models` (e.g. "anthropic/claude-sonnet-4-5").
    #[arg(short, long, default_value = "anthropic/claude-sonnet-4-5")]
    pub model: String,

    /// User prompt (omit to read from stdin).
    #[arg(short, long)]
    pub prompt: Option<String>,

    /// Path to a file whose contents replace the default system prompt.
    #[arg(long)]
    pub system_prompt_file: Option<PathBuf>,

    /// Tokens reserved by `context_guard` for system prompt + completion.
    #[arg(long, default_value_t = 4096)]
    pub headroom_tokens: u64,

    /// Which OpenAI-compatible provider preset to register.
    #[arg(long, value_enum, default_value_t = OpenAiCompatChoice::Common)]
    pub openai_compat: OpenAiCompatChoice,

    /// Print thinking-block deltas while streaming (off by default to keep stdout clean).
    #[arg(long, default_value_t = false)]
    pub show_thinking: bool,

    /// Register the write tools (`write` / `edit`). Off by default — the
    /// agent can only inspect the workspace, not mutate it.
    #[arg(long, default_value_t = false)]
    pub allow_write: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OpenAiCompatChoice {
    None,
    Common,
}

impl From<OpenAiCompatChoice> for OpenAiCompatPreset {
    fn from(c: OpenAiCompatChoice) -> Self {
        match c {
            OpenAiCompatChoice::None => OpenAiCompatPreset::None,
            OpenAiCompatChoice::Common => OpenAiCompatPreset::Common,
        }
    }
}

pub type CliError = Box<dyn std::error::Error + Send + Sync>;

/// Build everything from `args` and drive one prompt to completion.
pub async fn run(args: Args) -> Result<(), CliError> {
    // --- Workspace + registry ---------------------------------------------
    let workspace = Arc::new(Workspace::new(&args.workspace)?);
    let registry = Arc::new(Registry::from_embedded_snapshot());

    let model = registry.to_core_model(&args.model).ok_or_else(|| {
        format!(
            "unknown model id '{}': not in the embedded models.dev snapshot",
            args.model
        )
    })?;

    // --- Stream ------------------------------------------------------------
    let stream = Arc::new(
        GenaiStream::builder()
            .with_openai_compat_preset(args.openai_compat.into())
            .with_registry(registry.clone())
            .build(),
    );

    // --- Prompt + system prompt -------------------------------------------
    let prompt_text = resolve_prompt(&args)?;
    let system_prompt = resolve_system_prompt(&args)?;

    // --- Context guard -----------------------------------------------------
    let guard = ContextGuard::new(registry, args.model.clone())
        .with_policy(ContextGuardPolicy::DropOldest)
        .with_headroom_tokens(args.headroom_tokens)
        .into_transform_fn();

    // --- Agent options + agent --------------------------------------------
    let mut opts = AgentOptions::new(model, stream);
    opts.system_prompt = system_prompt;
    opts.tools = if args.allow_write {
        coding_all_tools(workspace)
    } else {
        coding_read_tools(workspace)
    };
    opts.transform_context = Some(guard);

    let agent = Agent::new(opts);

    // --- Subscribe printer ------------------------------------------------
    let printer = Arc::new(EventPrinter::new(args.show_thinking));
    let printer_clone = printer.clone();
    agent
        .subscribe(Arc::new(move |event, _signal| {
            let p = printer_clone.clone();
            Box::pin(async move {
                p.print(&event);
            })
        }))
        .await;

    // --- Run --------------------------------------------------------------
    agent.prompt_text(prompt_text).await?;

    // Surface any error from the synthetic terminal event.
    let state = agent.state().await;
    if let Some(err) = state.error_message {
        return Err(format!("agent ended with error: {err}").into());
    }
    Ok(())
}

fn resolve_prompt(args: &Args) -> Result<String, CliError> {
    if let Some(p) = &args.prompt {
        return Ok(p.clone());
    }
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Err("no prompt: pass --prompt or pipe text on stdin".into());
    }
    Ok(trimmed.to_string())
}

fn resolve_system_prompt(args: &Args) -> Result<String, CliError> {
    if let Some(path) = &args.system_prompt_file {
        let s = std::fs::read_to_string(path)
            .map_err(|e| format!("read system prompt {}: {e}", path.display()))?;
        return Ok(s);
    }
    Ok(if args.allow_write {
        WRITE_ENABLED_CODING_AGENT_SYSTEM_PROMPT.to_string()
    } else {
        DEFAULT_CODING_AGENT_SYSTEM_PROMPT.to_string()
    })
}

// ---------------------------------------------------------------------------
// Event printer
// ---------------------------------------------------------------------------

/// Tiny stdout printer with internal lock so streamed text deltas don't
/// interleave with tool-call markers.
pub struct EventPrinter {
    show_thinking: bool,
    lock: Mutex<()>,
}

impl EventPrinter {
    pub fn new(show_thinking: bool) -> Self {
        EventPrinter {
            show_thinking,
            lock: Mutex::new(()),
        }
    }

    /// Render one event to stdout. Returns immediately on lock contention
    /// errors (poisoning is harmless — the writer is just `println!`).
    pub fn print(&self, event: &AgentEvent) {
        let _g = self.lock.lock();
        let mut out = io::stdout().lock();
        match event {
            AgentEvent::AgentStart => {}
            AgentEvent::TurnStart => {}
            AgentEvent::MessageStart { message } => {
                // Print a header for assistant turns (so subsequent TextDeltas
                // are visually grouped). User / tool-result messages don't
                // get a header — they're either already-known or implied.
                if let AgentMessage::Standard(Message::Assistant(_)) = message {
                    writeln!(out, "\n[assistant]").ok();
                }
            }
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => match assistant_message_event {
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    write!(out, "{delta}").ok();
                    out.flush().ok();
                }
                AssistantMessageEvent::ThinkingDelta { delta, .. } if self.show_thinking => {
                    write!(out, "\x1b[2m{delta}\x1b[0m").ok();
                    out.flush().ok();
                }
                _ => {}
            },
            AgentEvent::MessageEnd { message } => {
                if let AgentMessage::Standard(Message::Assistant(a)) = message {
                    writeln!(out).ok();
                    if let Some(err) = &a.error_message {
                        writeln!(out, "[stream error] {err}").ok();
                    }
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } => {
                let short = preview_json(args, 120);
                writeln!(out, "\n→ {tool_name}({short})").ok();
            }
            AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                result,
                ..
            } => {
                let preview = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        grain_agent_core::UserContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .next()
                    .map(|t| truncate(t, 200))
                    .unwrap_or_default();
                writeln!(
                    out,
                    "← {tool_name}{} {}",
                    if *is_error { " [error]" } else { "" },
                    preview
                )
                .ok();
            }
            AgentEvent::ToolExecutionUpdate { .. } => {}
            AgentEvent::TurnEnd { message, .. } => {
                if let Some(err) = &message.error_message {
                    writeln!(out, "\n[turn error] {err}").ok();
                }
            }
            AgentEvent::AgentEnd { messages } => {
                let turns = messages
                    .iter()
                    .filter(|m| matches!(m, AgentMessage::Standard(Message::Assistant(_))))
                    .count();
                writeln!(out, "\n[done] {turns} assistant turn(s)").ok();
            }
        }
    }
}

fn preview_json(v: &serde_json::Value, max_chars: usize) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    truncate(&s, max_chars)
}

fn truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}… [+{} chars]", count - max_chars)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parse_with_defaults() {
        let args = Args::try_parse_from(["grain-headless"]).expect("defaults parse");
        assert_eq!(args.model, "anthropic/claude-sonnet-4-5");
        assert_eq!(args.workspace, PathBuf::from("."));
        assert_eq!(args.headroom_tokens, 4096);
        assert!(matches!(args.openai_compat, OpenAiCompatChoice::Common));
        assert!(!args.show_thinking);
    }

    #[test]
    fn args_parse_with_overrides() {
        let args = Args::try_parse_from([
            "grain-headless",
            "-C",
            "/tmp/work",
            "--model",
            "openai/gpt-4o",
            "--prompt",
            "say hi",
            "--openai-compat",
            "none",
            "--show-thinking",
        ])
        .expect("overrides parse");
        assert_eq!(args.workspace, PathBuf::from("/tmp/work"));
        assert_eq!(args.model, "openai/gpt-4o");
        assert_eq!(args.prompt.as_deref(), Some("say hi"));
        assert!(matches!(args.openai_compat, OpenAiCompatChoice::None));
        assert!(args.show_thinking);
    }

    #[test]
    fn args_rejects_unknown_openai_compat() {
        let err = Args::try_parse_from(["grain-headless", "--openai-compat", "bogus"]).unwrap_err();
        // clap surfaces this as a UnknownArgumentValueParse error.
        let s = err.to_string();
        assert!(s.contains("bogus"), "expected 'bogus' in error: {s}");
    }

    #[test]
    fn truncate_clips_long_strings() {
        let s: String = "x".repeat(300);
        let out = truncate(&s, 100);
        assert!(out.starts_with(&"x".repeat(100)));
        assert!(out.contains("[+200 chars]"));
    }

    #[test]
    fn preview_json_serializes_and_truncates() {
        let v = serde_json::json!({ "k": "v".repeat(500) });
        let p = preview_json(&v, 50);
        assert!(p.chars().count() <= 50 + 30); // 30 chars slop for "[+N chars]" suffix
    }

    #[test]
    fn event_printer_does_not_panic_on_any_variant() {
        use grain_agent_core::{
            AssistantContent, AssistantMessage, StopReason, TextContent, ToolResultMessage,
            Usage, UserContent, UserMessage,
        };

        let printer = EventPrinter::new(false);

        let user_msg = AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: "hi".into() })],
            timestamp: 0,
        });
        let asst_msg = AssistantMessage {
            content: vec![AssistantContent::Text(TextContent { text: "ok".into() })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        let asst_agent_msg = AgentMessage::assistant(asst_msg.clone());

        let trm = ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "echo".into(),
            content: vec![UserContent::Text(TextContent { text: "echoed".into() })],
            details: serde_json::Value::Null,
            is_error: false,
            timestamp: 0,
        };

        printer.print(&AgentEvent::AgentStart);
        printer.print(&AgentEvent::TurnStart);
        printer.print(&AgentEvent::MessageStart {
            message: user_msg.clone(),
        });
        printer.print(&AgentEvent::MessageStart {
            message: asst_agent_msg.clone(),
        });
        printer.print(&AgentEvent::MessageUpdate {
            message: asst_msg.clone(),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                partial: asst_msg.clone(),
                content_index: 0,
                delta: "ok".into(),
            },
        });
        printer.print(&AgentEvent::MessageEnd {
            message: asst_agent_msg,
        });
        printer.print(&AgentEvent::ToolExecutionStart {
            tool_call_id: "c1".into(),
            tool_name: "echo".into(),
            args: serde_json::json!({ "v": 1 }),
        });
        let tool_result = grain_agent_core::AgentToolResult {
            content: vec![UserContent::Text(TextContent {
                text: "echoed".into(),
            })],
            details: serde_json::Value::Null,
            terminate: None,
        };
        printer.print(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "c1".into(),
            tool_name: "echo".into(),
            result: tool_result,
            is_error: false,
        });
        printer.print(&AgentEvent::TurnEnd {
            message: asst_msg,
            tool_results: vec![trm],
        });
        printer.print(&AgentEvent::AgentEnd {
            messages: vec![user_msg],
        });
    }
}
