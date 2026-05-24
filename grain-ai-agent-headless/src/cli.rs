//! CLI surface for the `grain-headless` binary.
//!
//! `Args` is the parsed command-line shape; `run(args)` builds a Workspace,
//! Registry, GenaiStream, Agent, registers the read-only tools + context
//! guard, and drives one prompt to completion while streaming events to
//! stdout. Returns once the loop ends.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::{CommandFactory, Parser, ValueEnum, parser::ValueSource};
use grain_agent_core::{
    Agent, AgentEvent, AgentMessage, AgentOptions, AssistantMessageEvent, Message,
};
use grain_agent_harness::context_guard::{ContextGuard, ContextGuardPolicy};
use grain_llm_genai::{
    GenaiStream, OpenAiCompatPreset, ProviderAuth, ProviderKind, ProviderProfile, load_profiles,
    resolve_providers_file,
};
use grain_llm_models::Registry;
use serde::Serialize;
use std::io::BufRead;

use crate::config::{ArgDefaults, ConfigFile};
use crate::diagnostics::{render_doctor_report, render_source_info_block};
use crate::prompt::coding_agent_system_prompt;
use crate::runtime::{coding_bash_tools, coding_read_tools, coding_write_tools};
use crate::session::{SessionWriter, load_messages};
use crate::skills::{find_skills_in_dirs, resolve_skill_dirs};
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

    /// Directory to scan for skill files. By default pi-compatible locations
    /// are scanned (`~/.pi/agent/skills`, `~/.agents/skills`,
    /// `<workspace>/.pi/skills`, `<workspace>/.agents/skills`, and legacy
    /// `<workspace>/.claude/skills`). Passing this flag uses only that path.
    #[arg(long)]
    pub skills_dir: Option<PathBuf>,

    /// Print a workspace + provider diagnostic and exit. Doesn't call any
    /// LLM endpoints; safe to run before configuring keys.
    #[arg(long, default_value_t = false)]
    pub doctor: bool,

    /// Run the OAuth browser login flow for a provider (`anthropic` or
    /// `openai`) and exit.  Tokens are stored under the user data dir
    /// (`~/Library/Application Support/grain/oauth/<provider>.json` on
    /// macOS, `~/.config/grain/oauth/<provider>.json` on Linux) and are
    /// auto-refreshed by the runtime on subsequent runs.
    #[arg(long, value_name = "PROVIDER")]
    pub login: Option<String>,

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

    /// Initial provider profile name (looked up in profiles loaded
    /// from `--providers-file` / `<workspace>/.grain/providers.toml` /
    /// `~/.config/grain/providers.toml`). When set, the profile's
    /// model + auth env var replace the defaults derived from `--model`.
    #[arg(long)]
    pub provider: Option<String>,

    /// Override the providers.toml search path. Absolute file path
    /// takes precedence over workspace + user locations.
    #[arg(long)]
    pub providers_file: Option<PathBuf>,

    /// Directory of JavaScript files (`*.js`) that register
    /// additional tools via `grain.register_tool({...})`. Defaults to
    /// `<workspace>/.grain/scripts/` when the directory exists.
    /// Requires the `scripts-boa` cargo feature; without it, this
    /// flag is accepted but every script load surfaces a build-time
    /// warning instead.
    #[arg(long)]
    pub scripts_dir: Option<PathBuf>,
}

impl Args {
    /// Hard-coded CLI defaults — used by `ConfigFile::apply_to_args` as a
    /// reference, alongside the explicit-flag set computed by
    /// [`Self::explicit_arg_ids`].
    pub fn cli_defaults() -> ArgDefaults {
        ArgDefaults {
            model: "anthropic/claude-sonnet-4-5".into(),
            headroom_tokens: 4096,
        }
    }

    /// Return the set of clap argument ids whose values came from the
    /// user (not the clap-built-in default). Used by
    /// `ConfigFile::apply_to_args` to avoid overriding flags the user
    /// explicitly passed.
    pub fn explicit_arg_ids(argv: &[String]) -> std::collections::HashSet<String> {
        // Re-parse so we can ask `value_source` per-arg. The original
        // `Args::parse()` already verified the input; we discard the
        // result and only keep the ArgMatches.
        let cmd = Args::command();
        let matches = match cmd.try_get_matches_from(argv) {
            Ok(m) => m,
            // Shouldn't fail since we already parsed once, but fall back
            // to "everything looks default" rather than panic.
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
    let argv: Vec<String> = std::env::args().collect();
    let explicit = Args::explicit_arg_ids(&argv);
    match ConfigFile::load(workspace.root()) {
        Ok(cfg) => cfg.apply_to_args(&mut args, &explicit, &Args::cli_defaults()),
        Err(e) => eprintln!("[warn] config load: {e}"),
    }

    // --- Doctor short-circuit ---------------------------------------------
    // Runs no LLM calls; safe even when no keys are set.
    if args.doctor {
        let report = render_doctor_report(&workspace, &registry);
        print!("{report}");
        return Ok(());
    }

    // --- Login short-circuit ----------------------------------------------
    // Opens the browser, runs the PKCE flow, persists tokens, then exits.
    // No LLM calls; safe to invoke before any provider profile is set up.
    if let Some(provider) = args.login.as_deref() {
        let config = grain_llm_genai::oauth::config_for_provider(provider).ok_or_else(|| {
            format!("unknown OAuth provider '{provider}' (known: anthropic, openai)")
        })?;
        grain_llm_genai::oauth::start_login_flow(&config, |msg| println!("{msg}")).await?;
        println!(
            "Login successful — tokens saved for '{}'. You can now use a \
             provider profile with `auth = {{ kind = \"{}_oauth\" }}`.",
            config.provider, config.provider
        );
        return Ok(());
    }

    // --- Provider profiles (optional) -------------------------------------
    // Profiles add OpenAI-compat endpoints + env-var overrides to the
    // genai builder. When `--provider <name>` is set, the named
    // profile's model + auth replace what `--model` / env-vars would
    // otherwise pick.
    //
    // Two sources, in order:
    //   1. config.toml [[provider]] — authoritative.
    //   2. legacy providers.toml — fills in any names config didn't
    //      already cover. Migration warning emitted when it
    //      contributes.
    let mut profiles: Vec<ProviderProfile> = Vec::new();
    if let Ok(cfg) = crate::config::ConfigFile::load(workspace.root()) {
        for entry in cfg.providers {
            match grain_llm_genai::profile_from_entry(entry) {
                Ok(p) => profiles.push(p),
                Err(e) => eprintln!("[warn] config.toml provider: {e}"),
            }
        }
    }
    if let Some(p) = resolve_providers_file(args.providers_file.as_deref(), workspace.root()) {
        let (legacy_profiles, profile_warnings) = load_profiles(&p);
        for w in profile_warnings {
            eprintln!("[warn] {w}");
        }
        let mut migrated = 0usize;
        for legacy in legacy_profiles {
            if profiles.iter().any(|e| e.name == legacy.name) {
                continue;
            }
            migrated += 1;
            profiles.push(legacy);
        }
        if migrated > 0 {
            eprintln!(
                "[warn] {migrated} entries in legacy {}; consider migrating to config.toml [[provider]] blocks",
                p.display()
            );
        }
    }
    let active_profile: Option<&ProviderProfile> = match &args.provider {
        None => None,
        Some(name) => match profiles.iter().find(|p| &p.name == name) {
            Some(p) => Some(p),
            None => {
                return Err(format!(
                    "provider '{name}' not in config.toml or providers.toml ({} loaded)",
                    profiles.len()
                )
                .into());
            }
        },
    };
    if let Some(p) = active_profile
        && !profile_has_credentials(p)
    {
        let hint = match &p.auth {
            ProviderAuth::AnthropicOauth => {
                "run `grain-headless --login anthropic` first".to_string()
            }
            ProviderAuth::OpenAiOauth => {
                "run `grain-headless --login openai` first".to_string()
            }
            ProviderAuth::ApiKey { env } => {
                format!("env var `{env}` is empty or unset")
            }
        };
        return Err(format!("provider '{}': {hint}", p.name).into());
    }

    // Resolve the model id we'll actually drive: profile overrides
    // `--model` when active.
    let resolved_model_id = active_profile
        .map(|p| p.model.clone())
        .unwrap_or_else(|| args.model.clone());
    let mut model = registry.to_core_model(&resolved_model_id).ok_or_else(|| {
        format!("unknown model id '{resolved_model_id}': not in the embedded models.dev snapshot")
    })?;
    if let Some(p) = active_profile
        && matches!(p.kind, ProviderKind::OpenAiCompat)
    {
        // For OpenAI-compat profiles the genai router dispatches on
        // `Model.provider`; rewrite to the profile name so the
        // (name, base_url, env_var) endpoint we register below matches.
        model.provider = p.name.clone();
    }

    // --- Stream ------------------------------------------------------------
    let stream = Arc::new(
        GenaiStream::builder()
            .with_openai_compat_preset(args.openai_compat.into())
            .with_provider_profiles(&profiles)
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
    let deepseek = crate::deepseek::DeepSeekPack::new(&model);
    if deepseek.is_enabled() {
        eprintln!(
            "[info] DeepSeek pack active — reasoning scavenge + subagent.done detection enabled"
        );
    }
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
            return Err("--allow-semantic-search requires the `rig` cargo feature; \
                 rebuild with `cargo build --features rig`"
                .into());
        }
    }
    // --- JS scripted tools (optional) -------------------------------------
    // Resolves CLI flag → `<workspace>/.grain/scripts/` default. With
    // the `scripts-boa` feature, loads every `*.js`, surfaces any
    // tools the scripts registered, and keeps the extension alive for
    // the agent's lifetime (the worker thread shuts down on drop).
    let scripts_path = args
        .scripts_dir
        .clone()
        .unwrap_or_else(|| workspace.root().join(".grain").join("scripts"));
    #[cfg(feature = "scripts-boa")]
    let _scripts_keepalive = {
        match grain_script_boa::BoaExtension::from_scripts_dir(&scripts_path) {
            Ok(ext) => {
                let scripted = ext.tools();
                if !scripted.is_empty() {
                    eprintln!(
                        "[info] loaded {} JS tool(s) from {}",
                        scripted.len(),
                        scripts_path.display()
                    );
                }
                tools.extend(scripted);
                Some(ext)
            }
            Err(e) => {
                eprintln!("[warn] boa scripts: {e}");
                None
            }
        }
    };
    #[cfg(not(feature = "scripts-boa"))]
    if args.scripts_dir.is_some() || scripts_path.exists() {
        eprintln!(
            "[warn] --scripts-dir / .grain/scripts/ present at {} but binary was \
             built without --features scripts-boa; ignoring",
            scripts_path.display()
        );
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
            skill_dirs: resolve_skill_dirs(workspace.root(), args.skills_dir.as_deref()),
            session_path: args.session.clone(),
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
    skill_dirs: Vec<PathBuf>,
    /// Active JSONL session file (if any). `/clear` truncates it so the
    /// next load doesn't show stale messages alongside the new transcript.
    session_path: Option<PathBuf>,
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
                    // Also truncate the session file (if any) so the next
                    // load doesn't see a mix of pre-clear and post-clear
                    // messages. Failures here are non-fatal — log and
                    // continue with the in-memory clear.
                    if let Some(path) = &ctx.session_path {
                        match std::fs::OpenOptions::new()
                            .write(true)
                            .truncate(true)
                            .open(path)
                        {
                            Ok(_) => eprintln!(
                                "(transcript cleared; session file {} truncated)",
                                path.display()
                            ),
                            Err(e) => eprintln!(
                                "[warn] cleared in-memory transcript but failed to truncate {}: {e}",
                                path.display()
                            ),
                        }
                    } else {
                        eprintln!("(transcript cleared)");
                    }
                }
                SlashCommand::Skills => match find_skills_in_dirs(&ctx.skill_dirs) {
                    Ok(skills) if skills.is_empty() => {
                        eprintln!("(no skills found)");
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

/// Whether a provider profile is ready to drive a request:
/// - `ApiKey { env }` → the env var must be set + non-empty
/// - OAuth variants → tokens must already exist on disk for the matching
///   provider name (`anthropic` / `openai`); the user obtains them via
///   `--login <provider>`.  We don't try to refresh here — that happens
///   lazily at request time inside the auth resolver.
fn profile_has_credentials(p: &ProviderProfile) -> bool {
    match &p.auth {
        ProviderAuth::ApiKey { env } => std::env::var(env)
            .ok()
            .filter(|v| !v.is_empty())
            .is_some(),
        ProviderAuth::AnthropicOauth => grain_llm_genai::oauth::load_tokens("anthropic")
            .ok()
            .flatten()
            .is_some(),
        ProviderAuth::OpenAiOauth => grain_llm_genai::oauth::load_tokens("openai")
            .ok()
            .flatten()
            .is_some(),
    }
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
    let dirs = resolve_skill_dirs(workspace_root, args.skills_dir.as_deref());
    let mut skills = match find_skills_in_dirs(&dirs) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[warn] skills discovery: {e}");
            return String::new();
        }
    };
    // AGENTS.md standard (<https://agents.md/>) — treat it as an auto-
    // discovered skill when present in the workspace root.
    if let Some(s) = crate::skills::maybe_load_agents_md(workspace_root) {
        skills.push(s);
    }
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
            AssistantContent, AssistantMessage, StopReason, TextContent, ToolResultMessage, Usage,
            UserContent, UserMessage,
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
            content: vec![UserContent::Text(TextContent {
                text: "echoed".into(),
            })],
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
