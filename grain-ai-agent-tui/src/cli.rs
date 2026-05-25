//! Command-line surface for the `grain-tui` binary. Mirrors the
//! relevant flags from `grain-headless` so anyone fluent with the
//! headless CLI can drop into the TUI without re-learning options.
//!
//! Behavior-only flags (interactive, prompt, output format, JSON) are
//! omitted — the TUI *is* the interactive surface.

use std::path::PathBuf;

use clap::parser::ValueSource;
use clap::{CommandFactory, Parser, ValueEnum};
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

    /// Output-budget reserve for the assistant response, on top of the
    /// system+tools overhead the worker pre-charges automatically.
    /// Bump for models that produce long answers / heavy reasoning.
    #[arg(long, default_value_t = 8192)]
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
    /// messages are appended as they finalize. Overrides
    /// `--sessions-dir` auto-create.
    #[arg(long)]
    pub session: Option<PathBuf>,

    /// Directory holding session JSONL files. When `--session` isn't
    /// passed, the TUI auto-creates a fresh `<uuidv7>.jsonl` inside
    /// this directory at startup so every run is recoverable later
    /// via `/resume`. Defaults to `<workspace>/.grain/sessions/`.
    #[arg(long)]
    pub sessions_dir: Option<PathBuf>,

    /// Force a fresh session at startup. Without this flag, the TUI
    /// auto-resumes the most-recently-modified session found in
    /// `--sessions-dir`. Pair with `--session <path>` to pick a
    /// specific transcript explicitly; pass this flag to ignore both
    /// auto-resume and any existing transcripts and start clean.
    #[arg(long, default_value_t = false)]
    pub new_session: bool,

    /// Directory scanned for skill files. By default pi-compatible locations
    /// are scanned; passing this flag uses only that path.
    #[arg(long)]
    pub skills_dir: Option<PathBuf>,

    /// Ignore user-global and ancestor skill directories; scan only
    /// workspace-local skill directories unless `--skills-dir` is set.
    #[arg(long, default_value_t = false)]
    pub workspace_skills_only: bool,

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

    /// Directory scanned for `lazy.gagent` plugins. Each subdirectory
    /// is a plugin if it contains a `plugin.toml` manifest; its
    /// `skills/` and `themes/` (Phase B: `scripts/`) folders are
    /// merged into the corresponding catalogs at startup. Defaults to
    /// `<workspace>/.grain/plugins`. Missing directory is fine — no
    /// plugins load then.
    #[arg(long)]
    pub plugins_dir: Option<PathBuf>,

    /// Initial provider profile name. Looked up in profiles loaded
    /// from `--providers-file` / `<workspace>/.grain/providers.toml` /
    /// `~/.config/grain/providers.toml`. When unset, the picker opens
    /// without an active profile and the CLI `--model` flag governs.
    #[arg(long)]
    pub provider: Option<String>,

    /// Override the providers.toml search path. Pass an absolute file
    /// path; takes precedence over workspace + user locations.
    #[arg(long)]
    pub providers_file: Option<PathBuf>,

    /// Directory of `*.js` script files that register additional
    /// tools via `grain.register_tool({...})`. Defaults to
    /// `<workspace>/.grain/scripts/` when the directory exists.
    /// Requires building with `--features scripts-boa`.
    #[arg(long)]
    pub scripts_dir: Option<PathBuf>,

    /// Auto-escalation: when ≥ `--escalate-after` failure signals
    /// (assistant errors + tool errors) accumulate in this session,
    /// swap the active model to this id for the next turn. Pair with
    /// a faster default model (`--model deepseek/deepseek-v4-flash
    /// --escalate-to deepseek/deepseek-v4-pro`) to keep latency / cost
    /// low while still recovering from hard turns automatically.
    /// Unset → escalation hook is not registered.
    #[arg(long)]
    pub escalate_to: Option<String>,

    /// Failure-signal count that triggers `--escalate-to`. Default 3.
    /// Ignored when `--escalate-to` is unset.
    #[arg(long, default_value_t = 3)]
    pub escalate_after: u32,

    /// USD → CNY conversion rate for the cost chip. When set, the
    /// footer renders `¥X.XX` instead of `$X.XX`. Auto-detected from
    /// `$LANG` (any `zh_*` locale) with a default rate of `7.20`
    /// when this flag isn't passed; pass an explicit value here to
    /// override or to opt-in on non-zh locales.
    #[arg(long)]
    pub cny_rate: Option<f64>,

    /// Capture each outbound request body — the projected LLM
    /// messages and tools list — into a ring buffer. Open the `/log`
    /// overlay inside the TUI to view and scroll. Off by default;
    /// when on, every turn pays a `serde_json::to_string_pretty` for
    /// the messages array, which is cheap but non-zero.
    #[arg(long, default_value_t = false)]
    pub debug_log: bool,

    /// Whether to bypass process-wide HTTP proxies (`HTTPS_PROXY` /
    /// `ALL_PROXY` / ...) when calling LLM endpoints.
    ///
    /// - Unset (default): auto — bypass when at least one configured
    ///   OpenAI-compat profile points at a loopback host (LM Studio,
    ///   vLLM, llama.cpp, Ollama). All other traffic still honors the
    ///   proxy env vars.
    /// - `true`: always bypass — useful in environments where a
    ///   transparent proxy on `localhost` mangles requests, even for
    ///   remote endpoints.
    /// - `false`: never bypass — useful when you've set `NO_PROXY`
    ///   yourself and want full proxy control via env vars.
    #[arg(long)]
    pub bypass_proxy: Option<bool>,
}

impl Args {
    /// Return the set of clap argument ids whose values came from the
    /// user (not the clap-built-in default). Used by
    /// [`crate::config_apply::apply_config_to_args`] to avoid overriding
    /// flags the user explicitly passed via the command line.
    ///
    /// Mirrors the helper on `grain_ai_agent_headless::cli::Args` —
    /// kept in lockstep so a config field with the same name behaves
    /// identically across both binaries.
    pub fn explicit_arg_ids(argv: &[String]) -> std::collections::HashSet<String> {
        let cmd = Args::command();
        // Re-parse so we can inspect `value_source` per-arg. The
        // caller has already verified `argv` parses; we discard the
        // result and only keep the ArgMatches.
        let matches = match cmd.try_get_matches_from(argv) {
            Ok(m) => m,
            Err(_) => return std::collections::HashSet::new(),
        };
        matches
            .ids()
            .filter_map(|id| {
                let name = id.as_str();
                match matches.value_source(name) {
                    Some(ValueSource::CommandLine) => Some(name.to_string()),
                    _ => None,
                }
            })
            .collect()
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
