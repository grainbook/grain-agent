//! Command-line surface for the `grain-tui` binary. Mirrors the
//! relevant flags from `grain-headless` so anyone fluent with the
//! headless CLI can drop into the TUI without re-learning options.
//!
//! Behavior-only flags (interactive, prompt, output format, JSON) are
//! omitted — the TUI *is* the interactive surface.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use grain_llm_genai::OpenAiCompatPreset;

/// `grain-tui` — ratatui terminal UI on top of the headless coding agent.
#[derive(Debug, Clone, Parser)]
#[command(name = "grain-tui", version, about, long_about = None)]
pub struct Args {
    /// Workspace root (file tools refuse paths outside this directory).
    #[arg(short = 'C', long, default_value = ".")]
    pub workspace: PathBuf,

    /// Model id from `grain-llm-models`.
    #[arg(short, long, default_value = "deepseek/deepseek-chat")]
    pub model: String,

    /// Path to a file whose contents replace the default system prompt.
    #[arg(long)]
    pub system_prompt_file: Option<PathBuf>,

    /// Tokens reserved by `context_guard` for system prompt + completion.
    #[arg(long, default_value_t = 4096)]
    pub headroom_tokens: u64,

    /// Which OpenAI-compatible provider preset to register.
    #[arg(long, value_enum, default_value_t = OpenAiCompatChoice::Common)]
    pub openai_compat: OpenAiCompatChoice,

    /// Render thinking deltas inline in the transcript (off by default).
    #[arg(long, default_value_t = false)]
    pub show_thinking: bool,

    /// Allow Write / Edit tools (off → read-only workspace).
    #[arg(long, default_value_t = false)]
    pub allow_write: bool,

    /// Allow the Bash tool (explicit opt-in: shell can do anything).
    #[arg(long, default_value_t = false)]
    pub allow_bash: bool,

    /// Allow the WebFetch tool (explicit opt-in: arbitrary HTTPS GETs).
    #[arg(long, default_value_t = false)]
    pub allow_web: bool,

    /// Allow the SemanticSearch tool. Requires the `rig` cargo feature
    /// on `grain-ai-agent-headless` and `OPENAI_API_KEY` at runtime.
    #[arg(long, default_value_t = false)]
    pub allow_semantic_search: bool,

    /// JSONL session file: prior messages are loaded on start; new
    /// messages are appended as they finalize.
    #[arg(long)]
    pub session: Option<PathBuf>,

    /// Directory scanned for `<name>/SKILL.md` skill files. Defaults to
    /// `<workspace>/.claude/skills`.
    #[arg(long)]
    pub skills_dir: Option<PathBuf>,

    /// Opt-in telemetry log: one JSON-serialized `AgentEvent` per line.
    #[arg(long)]
    pub telemetry_file: Option<PathBuf>,

    /// Tick interval in milliseconds. Lower = smoother spinners, more
    /// CPU. 100ms is a sane default.
    #[arg(long, default_value_t = 100)]
    pub tick_ms: u64,

    /// Initial theme name. Falls back to `default` if not found. Use
    /// `/theme` inside the TUI to switch interactively.
    #[arg(long, default_value = "default")]
    pub theme: String,

    /// Directory scanned for user theme TOML files (`<name>.toml`).
    /// Defaults to `<workspace>/.grain/themes`. Missing directory is
    /// fine — only built-ins are loaded then.
    #[arg(long)]
    pub themes_dir: Option<PathBuf>,
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
