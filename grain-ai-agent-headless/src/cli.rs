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
use serde::Serialize;
use std::io::BufRead;
use grain_agent_harness::context_guard::{ContextGuard, ContextGuardPolicy};
use grain_llm_genai::{GenaiStream, OpenAiCompatPreset};
use grain_llm_models::Registry;

use crate::config::{ArgDefaults, ConfigFile};
use crate::diagnostics::{render_doctor_report, render_source_info_block};
use crate::prompt::coding_agent_system_prompt;
use crate::runtime::{coding_bash_tools, coding_read_tools, coding_write_tools};
use crate::session::{SessionWriter, load_messages};
use crate::skills::{find_skills, resolve_skills_dir};
use crate::slash::{HELP_TEXT, SlashCommand, parse as parse_slash};
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

    /// Register the `bash` tool. Off by default — explicit opt-in because
    /// shell commands can do anything (and they will, given the chance).
    #[arg(long, default_value_t = false)]
    pub allow_bash: bool,

    /// Enter an interactive read-prompt-respond loop after handling the
    /// optional initial `--prompt`. Type `/exit`, `/quit`, or send EOF
    /// (Ctrl-D) to leave.
    #[arg(short, long, default_value_t = false)]
    pub interactive: bool,

    /// JSONL session file (one `AgentMessage` per line). If the file
    /// exists, prior messages are loaded into the transcript on startup;
    /// new messages are appended as they finalize. Missing file is OK —
    /// it's created on first append.
    #[arg(long)]
    pub session: Option<PathBuf>,

    /// Register the `semantic_search` tool. Requires the `rig` cargo
    /// feature to be enabled at build time and `OPENAI_API_KEY` at
    /// runtime. Off by default.
    #[arg(long, default_value_t = false)]
    pub allow_semantic_search: bool,

    /// Directory to scan for `<name>/SKILL.md` skill files. Defaults to
    /// `<workspace>/.claude/skills`. Discovered skills are appended to the
    /// system prompt automatically. Pass an empty / non-existent path to
    /// disable disk-based skills.
    #[arg(long)]
    pub skills_dir: Option<PathBuf>,

    /// Print a workspace + provider diagnostic and exit. Doesn't call any
    /// LLM endpoints; safe to run before configuring keys.
    #[arg(long, default_value_t = false)]
    pub doctor: bool,

    /// Register the `web_fetch` tool. Off by default — opt-in because the
    /// agent can then reach arbitrary HTTP(S) endpoints.
    #[arg(long, default_value_t = false)]
    pub allow_web: bool,

    /// Event output format. `text` (default) prints a human-friendly
    /// stream; `json` emits one `AgentEvent` JSON-serialized per line
    /// for programmatic consumers (`jq`, scripts, …).
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,

    /// Opt-in telemetry log: every `AgentEvent` is appended as one
    /// JSON line to this file. Local-only; nothing is sent over the
    /// network. Off when unset.
    #[arg(long)]
    pub telemetry_file: Option<PathBuf>,
}

impl Args {
    /// Hard-coded CLI defaults — used by `ConfigFile::apply_to_args` to
    /// distinguish "user accepted the default" from "user set this
    /// explicitly". Kept here so both sources of truth move together.
    pub fn cli_defaults() -> ArgDefaults {
        ArgDefaults {
            model: "anthropic/claude-sonnet-4-5".into(),
            headroom_tokens: 4096,
        }
    }
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

/// Output format for streamed agent events. `Text` is the default
/// human-friendly stdout stream; `Json` emits one
/// `AgentEvent`-serialized line per event so callers can pipe into `jq`
/// or another consumer.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
}

pub type CliError = Box<dyn std::error::Error + Send + Sync>;

/// Build everything from `args` and drive one prompt to completion.
pub async fn run(args: Args) -> Result<(), CliError> {
    // --- Workspace + registry ---------------------------------------------
    let workspace = Arc::new(Workspace::new(&args.workspace)?);
    let registry = Arc::new(Registry::from_embedded_snapshot());

    // --- Config file (TOML) overlay ---------------------------------------
    // CLI flags win; config fills in fields the user accepted the default
    // for. Failures during load are logged but never break the agent.
    let mut args = args;
    match ConfigFile::load(workspace.root()) {
        Ok(cfg) => cfg.apply_to_args(&mut args, &Args::cli_defaults()),
        Err(e) => eprintln!("[warn] config load: {e}"),
    }

    // --- Doctor short-circuit ---------------------------------------------
    // Runs no LLM calls; safe even when no keys are set.
    if args.doctor {
        let report = render_doctor_report(&workspace, &registry);
        print!("{report}");
        return Ok(());
    }

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

    // --- Session restore --------------------------------------------------
    let prior_messages = match &args.session {
        Some(path) => load_messages(path)
            .map_err(|e| -> CliError { format!("session load {}: {e}", path.display()).into() })?,
        None => Vec::new(),
    };

    // --- Prompt + system prompt -------------------------------------------
    // Skip stdin reads when interactive (we read from stdin in the loop
    // ourselves) or when resuming a session with no explicit prompt.
    let initial_prompt = if args.prompt.is_some() {
        Some(resolve_prompt(&args)?)
    } else if args.interactive {
        None
    } else if !prior_messages.is_empty() {
        // Resume-only: continue from where we left off without injecting a new prompt.
        None
    } else {
        Some(resolve_prompt(&args)?)
    };
    let mut system_prompt = resolve_system_prompt(&args)?;
    let skills_block = resolve_skills_block(&args, workspace.root());
    if !skills_block.is_empty() {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&skills_block);
    }

    // --- Context guard -----------------------------------------------------
    let guard = ContextGuard::new(registry.clone(), args.model.clone())
        .with_policy(ContextGuardPolicy::DropOldest)
        .with_headroom_tokens(args.headroom_tokens)
        .into_transform_fn();

    // --- Agent options + agent --------------------------------------------
    let mut opts = AgentOptions::new(model, stream);
    opts.system_prompt = system_prompt;
    let mut tools = coding_read_tools(workspace.clone());
    if args.allow_write {
        tools.extend(coding_write_tools(workspace.clone()));
    }
    if args.allow_bash {
        tools.extend(coding_bash_tools(workspace.clone()));
    }
    if args.allow_web {
        tools.extend(crate::runtime::coding_web_tools());
    }
    if args.allow_semantic_search {
        #[cfg(feature = "rig")]
        {
            let semantic = crate::semantic::SemanticSearchTool::from_env(
                workspace.clone(),
                crate::semantic::SemanticIndexConfig::default(),
            )?;
            tools.push(Arc::new(semantic));
        }
        #[cfg(not(feature = "rig"))]
        {
            return Err(
                "--allow-semantic-search requires the `rig` cargo feature; \
                 rebuild with `cargo build --features rig`"
                    .into(),
            );
        }
    }
    let _ = workspace; // keep alive when no further tool branches consume it
    opts.tools = tools;
    opts.messages = prior_messages;
    opts.transform_context = Some(guard);

    let agent = Agent::new(opts);

    // --- Telemetry subscription -------------------------------------------
    if let Some(path) = &args.telemetry_file {
        let sink = Arc::new(crate::telemetry::TelemetrySink::open(path)?);
        let sink_clone = sink.clone();
        agent
            .subscribe(Arc::new(move |event, _signal| {
                let s = sink_clone.clone();
                Box::pin(async move { s.record(&event) })
            }))
            .await;
    }

    // --- Session writer subscription --------------------------------------
    if let Some(path) = &args.session {
        let writer = Arc::new(SessionWriter::open(path)?);
        let writer_clone = writer.clone();
        agent
            .subscribe(Arc::new(move |event, _signal| {
                let w = writer_clone.clone();
                Box::pin(async move {
                    if let AgentEvent::MessageEnd { message } = event
                        && let Err(e) = w.append(&message)
                    {
                        eprintln!("[warn] session append failed: {e}");
                    }
                })
            }))
            .await;
    }

    // --- Subscribe printer ------------------------------------------------
    let printer: Arc<dyn EventSink + Send + Sync> = match args.output {
        OutputFormat::Text => Arc::new(EventPrinter::new(args.show_thinking)),
        OutputFormat::Json => Arc::new(JsonEventPrinter::new()),
    };
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
    if let Some(text) = initial_prompt {
        agent.prompt_text(text).await?;
        let state = agent.state().await;
        if let Some(err) = state.error_message {
            // In interactive mode we continue past errors; otherwise propagate.
            if !args.interactive {
                return Err(format!("agent ended with error: {err}").into());
            }
            eprintln!("[error] {err}");
        }
    }

    if args.interactive {
        let ctx = InteractiveContext {
            workspace: workspace.clone(),
            registry: registry.clone(),
            skills_dir: resolve_skills_dir(workspace.root(), args.skills_dir.as_deref()),
        };
        run_interactive_loop(&agent, &ctx).await?;
    }

    Ok(())
}

/// Inputs the interactive loop's slash-command dispatch needs (separate from
/// `Args` to keep the run-time helpers reusable from tests).
struct InteractiveContext {
    workspace: Arc<Workspace>,
    registry: Arc<Registry>,
    skills_dir: PathBuf,
}

/// Read-prompt-respond loop. Reads lines from stdin until EOF or `/exit`.
/// Slash commands are intercepted by the parser; anything else is forwarded
/// to the agent as a new prompt.
async fn run_interactive_loop(agent: &Agent, ctx: &InteractiveContext) -> Result<(), CliError> {
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        eprint!("\n> ");
        io::stderr().flush().ok();
        line.clear();
        let n = stdin.lock().read_line(&mut line)?;
        if n == 0 {
            // EOF (Ctrl-D)
            eprintln!();
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(cmd) = parse_slash(trimmed) {
            match cmd {
                SlashCommand::Exit => break,
                SlashCommand::Help => {
                    print!("{HELP_TEXT}");
                }
                SlashCommand::Clear => {
                    agent.reset().await;
                    eprintln!("(transcript cleared)");
                }
                SlashCommand::Skills => match find_skills(&ctx.skills_dir) {
                    Ok(skills) if skills.is_empty() => {
                        eprintln!("(no skills found in {})", ctx.skills_dir.display());
                    }
                    Ok(skills) => {
                        for s in &skills {
                            let disabled = if s.disable_model_invocation {
                                " [disabled]"
                            } else {
                                ""
                            };
                            println!("- {}{disabled}  — {}", s.name, s.description);
                        }
                    }
                    Err(e) => eprintln!("[error] {e}"),
                },
                SlashCommand::Doctor => {
                    print!("{}", render_doctor_report(&ctx.workspace, &ctx.registry));
                }
                SlashCommand::Source => {
                    print!("{}", render_source_info_block(ctx.workspace.root(), 0));
                }
                SlashCommand::Compact => {
                    eprintln!(
                        "(compaction not yet implemented — see grain-agent-harness::compaction TODO)"
                    );
                }
                SlashCommand::Unknown(raw) => {
                    eprintln!("(unknown command {raw}; try /help)");
                }
            }
            continue;
        }
        if let Err(e) = agent.prompt_text(trimmed).await {
            eprintln!("[error] {e}");
            continue;
        }
        let state = agent.state().await;
        if let Some(err) = &state.error_message {
            eprintln!("[error] {err}");
        }
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
    Ok(coding_agent_system_prompt(args.allow_write, args.allow_bash).to_string())
}

/// Discover skills and render the `<available_skills>` block to append to
/// the system prompt. Errors during discovery degrade to an empty block —
/// missing or malformed skill files shouldn't break the agent.
fn resolve_skills_block(args: &Args, workspace_root: &std::path::Path) -> String {
    let dir = resolve_skills_dir(workspace_root, args.skills_dir.as_deref());
    let skills = match find_skills(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[warn] skills discovery in {}: {e}", dir.display());
            return String::new();
        }
    };
    grain_agent_harness::format_skills_for_system_prompt(&skills)
}

// ---------------------------------------------------------------------------
// Event printer
// ---------------------------------------------------------------------------

/// Common interface for the text / json printers. Lets `run()` swap
/// implementations behind one `Arc<dyn EventSink>`.
pub trait EventSink {
    fn print(&self, event: &AgentEvent);
}

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
    pub fn print_inner(&self, event: &AgentEvent) {
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
                // A single tool requesting batch-level termination isn't enough
                // to halt the loop — `should_terminate` requires consensus from
                // every finalized call. Surface the intent here so it isn't
                // silently ignored from the user's perspective.
                if result.terminate == Some(true) {
                    writeln!(
                        out,
                        "  (↳ {tool_name} requested batch termination; needs all tools in this batch to agree)"
                    )
                    .ok();
                }
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

impl EventSink for EventPrinter {
    fn print(&self, event: &AgentEvent) {
        self.print_inner(event);
    }
}

/// JSON-lines printer for programmatic consumers. Each event is emitted
/// as a single line: `{"event":"<tag>", ...}` derived from
/// `AgentEvent`'s serde representation.
#[derive(Default)]
pub struct JsonEventPrinter {
    lock: Mutex<()>,
}

impl JsonEventPrinter {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Serialize)]
struct JsonEventWrapper<'a> {
    grain_event_version: u32,
    #[serde(flatten)]
    event: &'a AgentEvent,
}

impl EventSink for JsonEventPrinter {
    fn print(&self, event: &AgentEvent) {
        let _g = self.lock.lock();
        let wrapper = JsonEventWrapper {
            grain_event_version: 1,
            event,
        };
        match serde_json::to_string(&wrapper) {
            Ok(s) => {
                let mut out = io::stdout().lock();
                let _ = writeln!(out, "{s}");
                let _ = out.flush();
            }
            Err(e) => {
                eprintln!("[warn] json event serialize failed: {e}");
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
    fn output_format_defaults_to_text() {
        let args = Args::try_parse_from(["grain-headless"]).expect("defaults parse");
        assert!(matches!(args.output, OutputFormat::Text));
    }

    #[test]
    fn output_format_accepts_json() {
        let args = Args::try_parse_from(["grain-headless", "--output", "json"])
            .expect("parse with --output json");
        assert!(matches!(args.output, OutputFormat::Json));
    }

    #[test]
    fn json_event_wrapper_serializes_with_version() {
        let event = AgentEvent::AgentStart;
        let wrapper = JsonEventWrapper {
            grain_event_version: 1,
            event: &event,
        };
        let s = serde_json::to_string(&wrapper).unwrap();
        assert!(s.contains("\"grain_event_version\":1"));
        // AgentEvent tags with `#[serde(tag = "type")]` so the variant
        // name appears as a `type` field.
        assert!(s.contains("\"type\":\"agent_start\""), "got {s}");
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
