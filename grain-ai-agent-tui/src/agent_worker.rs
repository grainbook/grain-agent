//! Tokio task that owns the [`grain_agent_harness::AgentHarness`] and
//! acts as the bridge between UI [`Command`]s and [`TuiEvent`]s.
//!
//! Construction mirrors `grain-ai-agent-headless::cli::run`: build a
//! Workspace, Registry, GenaiStream, tools per CLI flags, install the
//! context guard, subscribe a telemetry/session writer if requested,
//! then loop on commands.
//!
//! This module owns the only `AgentHarness`. The UI thread can only
//! address it through the [`mpsc`] channels returned by [`spawn`].
//!
//! `/resume` swaps the harness in-place by rebuilding it on top of a
//! different session — the [`HarnessBuilder`] captures the long-lived
//! ingredients (model, tools, hooks, etc.) so each rebuild only has
//! to feed in a fresh transcript.

use std::path::PathBuf;
use std::sync::Arc;

use grain_agent_core::{
    AgentEvent, AgentLoopTurnUpdate, AgentMessage, AgentTool, BeforeToolCallFn, ConvertToLlmFn,
    Message, Model, PrepareNextTurnFn, StreamFn, TransformContextFn,
};
use grain_agent_harness::{
    AgentHarness, AgentHarnessOptions, InMemorySessionStorage, Session, SessionMetadata,
    SystemPrompt,
    context_guard::{ActiveModelHandle, ContextGuard, ContextGuardPolicy},
};
use grain_ai_agent_headless::{
    SessionWriter, TelemetrySink, Workspace, coding_agent_system_prompt, coding_bash_tools,
    coding_read_tools, coding_web_tools, coding_write_tools, find_skills, load_messages,
    render_doctor_report, resolve_skills_dir,
};
use grain_llm_genai::GenaiStream;
use grain_llm_models::Registry;
use tokio::sync::mpsc;

use crate::app::Command;
use crate::cli::Args;
use crate::event::TuiEvent;
use grain_llm_genai::{ProviderKind, ProviderProfile};

/// Configuration crystallized out of [`Args`]. Pulled into its own
/// struct so the spawn function isn't argument-soup.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub workspace_root: PathBuf,
    pub model: String,
    pub system_prompt_file: Option<PathBuf>,
    pub headroom_tokens: u64,
    pub openai_compat: grain_llm_genai::OpenAiCompatPreset,
    pub allow_write: bool,
    pub allow_bash: bool,
    pub allow_web: bool,
    pub allow_semantic_search: bool,
    pub skills_dir: Option<PathBuf>,
    pub session: Option<PathBuf>,
    pub telemetry_file: Option<PathBuf>,
    /// Profiles loaded from `providers.toml`. Used both to register
    /// per-profile OpenAI-compat endpoints at startup and to honor
    /// `Command::ApplyProvider(...)` at runtime.
    pub profiles: Vec<ProviderProfile>,
    /// Index into [`Self::profiles`] for the profile to apply at
    /// startup. `None` means use [`Self::model`] verbatim.
    pub initial_profile_idx: Option<usize>,
    /// Directory of JS scripts to load via `grain-script-boa`.
    /// Honored only when the crate is built with the
    /// `scripts-boa` feature.
    pub scripts_dir: Option<PathBuf>,
    /// Auto-escalation target model id (e.g. `"deepseek/deepseek-v4-pro"`).
    /// `None` → no escalation hook installed.
    pub escalate_to: Option<String>,
    /// Failure-signal count that triggers `escalate_to`. Defaults to 3.
    pub escalate_after: u32,
    /// Tristate proxy-bypass override for the genai HTTP client. See
    /// [`crate::cli::Args::bypass_proxy`] for the full truth table.
    /// `None` → auto-detect from registered endpoints (the historical
    /// default; bypasses when any compat endpoint is on loopback).
    pub bypass_proxy: Option<bool>,
    /// Capture outbound request bodies (projected `Message[]`) into a
    /// ring buffer for the in-TUI `/log` overlay. Off → no capture.
    pub debug_log: bool,
    /// Directory for auto-created session files when `session` is
    /// unset. `None` → `<workspace>/.grain/sessions/`.
    pub sessions_dir: Option<PathBuf>,
    /// When `true`, ignore any existing transcripts in `sessions_dir`
    /// and mint a fresh `<uuidv7>.jsonl` — even if `session` is unset.
    /// When `false` (default), the worker auto-resumes the
    /// most-recently-modified session in `sessions_dir` so users
    /// return to where they left off across launches.
    pub new_session: bool,
    /// Root directory for `lazy.gagent` plugins. `None` →
    /// `<workspace>/.grain/plugins`. Phase A merges each plugin's
    /// `skills/` (and, on the TUI side, `themes/`) into the existing
    /// catalogs at startup.
    pub plugins_dir: Option<PathBuf>,
}

impl From<&Args> for WorkerConfig {
    fn from(a: &Args) -> Self {
        WorkerConfig {
            workspace_root: a.workspace.clone(),
            model: a.model.clone(),
            system_prompt_file: a.system_prompt_file.clone(),
            headroom_tokens: a.headroom_tokens,
            openai_compat: a.openai_compat.into(),
            allow_write: a.allow_write,
            allow_bash: a.allow_bash,
            allow_web: a.allow_web,
            allow_semantic_search: a.allow_semantic_search,
            skills_dir: a.skills_dir.clone(),
            session: a.session.clone(),
            telemetry_file: a.telemetry_file.clone(),
            // Profiles/initial_profile_idx are loaded in `run::run_tui`
            // (it has the workspace path on hand). Defaulted here.
            profiles: Vec::new(),
            initial_profile_idx: None,
            scripts_dir: a.scripts_dir.clone(),
            escalate_to: a.escalate_to.clone(),
            escalate_after: a.escalate_after,
            bypass_proxy: a.bypass_proxy,
            debug_log: a.debug_log,
            sessions_dir: a.sessions_dir.clone(),
            new_session: a.new_session,
            plugins_dir: a.plugins_dir.clone(),
        }
    }
}

/// Snapshot of fields the UI needs at construction time. Returned by
/// [`spawn`] so the caller can fill in the [`AppState`].
#[derive(Debug, Clone)]
pub struct WorkerHandles {
    pub model_id: String,
    pub workspace_display: String,
    pub allow_write: bool,
    pub allow_bash: bool,
    pub allow_web: bool,
    pub allow_semantic_search: bool,
    /// Per-million-token pricing for the booted model (read from the
    /// embedded models.dev snapshot). Used by the footer to render a
    /// live cost chip. `Cost::default()` (all zeros) when pricing is
    /// unknown — the footer suppresses the chip in that case.
    pub model_cost: grain_agent_core::Cost,
}

/// Errors that can happen *before* the worker successfully takes over.
/// Once it's running, errors are reported via [`TuiEvent::AgentWorkerError`].
#[derive(Debug, thiserror::Error)]
pub enum WorkerInitError {
    #[error("workspace: {0}")]
    Workspace(String),
    #[error("model '{0}' not found in embedded models.dev snapshot")]
    UnknownModel(String),
    #[error("provider profile '{0}': model '{1}' not in registry")]
    ProfileUnknownModel(String, String),
    #[error("provider profile '{0}': OAuth login is not yet implemented")]
    OauthNotWired(String),
    #[error("read system prompt {path}: {source}")]
    SystemPrompt {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("session load {path}: {source}")]
    Session {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("telemetry open: {0}")]
    Telemetry(String),
    #[error("session writer open: {0}")]
    SessionWriter(String),
    #[error("--allow-semantic-search requires building with `--features rig`")]
    SemanticUnsupported,
}

/// Everything the UI gets back from a successful [`spawn`]: the
/// command sender, the event receiver, the startup-time snapshot, and
/// the worker's join handle (free to drop — the worker exits on
/// `Command::Quit`).
pub struct Worker {
    pub cmd_tx: mpsc::UnboundedSender<Command>,
    pub evt_rx: mpsc::UnboundedReceiver<TuiEvent>,
    pub handles: WorkerHandles,
    pub join: tokio::task::JoinHandle<()>,
}

/// Captures the long-lived ingredients for re-building an
/// [`AgentHarness`] across `/resume` swaps. Hooks / system prompt /
/// tool catalog / streaming endpoint don't change once the worker
/// boots — only the underlying session does.
struct HarnessBuilder {
    model: Model,
    stream: StreamFn,
    system_prompt: String,
    tools: Vec<Arc<dyn AgentTool>>,
    transform_context: TransformContextFn,
    before_tool_call: Option<BeforeToolCallFn>,
    prepare_next_turn: Option<PrepareNextTurnFn>,
    convert_to_llm: Option<ConvertToLlmFn>,
}

impl HarnessBuilder {
    /// Build a fresh harness backed by an in-memory session seeded
    /// with `prior_messages`. The harness mirrors every finalized
    /// message back into the session (used by `compact()` and any
    /// future branch / fork logic); on-disk JSONL persistence stays
    /// with the separate `SessionWriter` subscription installed in
    /// [`install_subscriptions`].
    async fn build(&self, prior_messages: Vec<AgentMessage>) -> Arc<AgentHarness> {
        self.build_with_model(prior_messages, self.model.clone()).await
    }

    /// Same as [`Self::build`] but pins the new harness to the
    /// supplied `model` instead of the captured boot-time one.
    /// Used by `/resume` so the resumed session keeps the
    /// **currently** selected provider/model (the one the user is
    /// actively driving in this run) instead of snapping back to
    /// whatever was active at TUI startup.
    async fn build_with_model(
        &self,
        prior_messages: Vec<AgentMessage>,
        model: Model,
    ) -> Arc<AgentHarness> {
        let session = Session::new(Arc::new(InMemorySessionStorage::new(
            SessionMetadata::new(),
        )));
        for msg in prior_messages {
            if let Err(e) = session.append_message(msg).await {
                eprintln!("[warn] seed session message failed: {e}");
            }
        }
        let mut opts = AgentHarnessOptions::new(session, model, self.stream.clone());
        opts.system_prompt = SystemPrompt::Static(self.system_prompt.clone());
        opts.tools = self.tools.clone();
        opts.transform_context = Some(self.transform_context.clone());
        opts.before_tool_call = self.before_tool_call.clone();
        opts.prepare_next_turn = self.prepare_next_turn.clone();
        opts.convert_to_llm = self.convert_to_llm.clone();
        Arc::new(AgentHarness::new(opts).await)
    }
}

/// Fan the harness's inner-`Agent` events into the TUI's mpsc channel
/// plus (optionally) a telemetry sink and an on-disk session writer.
/// Called once at startup and again on every `/resume` swap.
///
/// The session-writer subscription duplicates what the harness already
/// mirrors into its in-memory session — that's deliberate: harness
/// state powers branch / compaction logic, while the flat-file JSONL
/// on disk is what `/resume`'s discovery scan reads.
async fn install_subscriptions(
    harness: &Arc<AgentHarness>,
    evt_tx: &mpsc::UnboundedSender<TuiEvent>,
    telemetry_sink: Option<Arc<TelemetrySink>>,
    session_writer: Option<Arc<SessionWriter>>,
) {
    let fan_tx = evt_tx.clone();
    harness
        .agent()
        .subscribe(Arc::new(move |event, _signal| {
            let tx = fan_tx.clone();
            Box::pin(async move {
                let _ = tx.send(TuiEvent::Agent(event));
            })
        }))
        .await;

    if let Some(sink) = telemetry_sink {
        harness
            .agent()
            .subscribe(Arc::new(move |event, _signal| {
                let s = sink.clone();
                Box::pin(async move {
                    s.record(&event);
                })
            }))
            .await;
    }

    if let Some(writer) = session_writer {
        harness
            .agent()
            .subscribe(Arc::new(move |event, _signal| {
                let w = writer.clone();
                Box::pin(async move {
                    if let AgentEvent::MessageEnd { message } = event {
                        let _ = w.append(&message);
                    }
                })
            }))
            .await;
    }
}

/// Spawn the agent worker. Returns a [`Worker`] bundle on success.
pub async fn spawn(mut cfg: WorkerConfig) -> Result<Worker, WorkerInitError> {
    // --- Workspace + registry ---------------------------------------------
    let workspace = Arc::new(
        Workspace::new(&cfg.workspace_root)
            .map_err(|e| WorkerInitError::Workspace(e.to_string()))?,
    );
    let registry = Arc::new(Registry::from_embedded_snapshot());

    // Resolve the model that the agent boots with. If a startup profile
    // was named, its `model` (with provider rewritten to the profile
    // name for OpenAI-compat routing) wins; otherwise fall back to the
    // CLI `--model` flag.
    let (model, active_model_id, active_profile_name) = if let Some(idx) = cfg.initial_profile_idx
        && let Some(profile) = cfg.profiles.get(idx)
    {
        if !profile.auth.is_usable() {
            return Err(WorkerInitError::OauthNotWired(profile.name.clone()));
        }
        // Registry-miss for a profile-driven model is **not**
        // fatal: a profile already supplies provider kind +
        // base_url, so we can synthesize a Model with conservative
        // defaults for unknown ids (typical for local LM Studio /
        // vLLM / llama.cpp / Ollama setups whose model names
        // aren't in `models.dev`). The cost chip is suppressed
        // (pricing is zeroed) and `context_window` defaults to
        // 32k — pass `--headroom-tokens` if the real window is
        // smaller.
        let m = match registry.to_core_model(&profile.model) {
            Some(m) => m,
            None => {
                eprintln!(
                    "[info] model '{}' not in registry; synthesizing \
                         from profile '{}' (context 32k, no pricing)",
                    profile.model, profile.name
                );
                synthetic_model_from_profile(profile)
            }
        };
        let m = override_model_provider(m, profile);
        (m, profile.model.clone(), Some(profile.name.clone()))
    } else {
        let m = registry
            .to_core_model(&cfg.model)
            .ok_or_else(|| WorkerInitError::UnknownModel(cfg.model.clone()))?;
        (m, cfg.model.clone(), None)
    };

    // --- Plugins -----------------------------------------------------------
    // Phase C-0: read `<workspace>/.grain/plugin-spec.toml` and
    // sync (git clone / local symlink) any plugins declared there
    // but not yet present under `plugins_dir`. This is the
    // bootstrap path that lets things like `lazy-gagent` (the
    // plugin manager) come along for the ride without a chicken-
    // and-egg problem — engine always exists, so the engine pulls
    // the manager in like any other plugin.
    let plugins_dir = cfg
        .plugins_dir
        .clone()
        .unwrap_or_else(|| grain_ai_agent_headless::default_plugins_dir(workspace.root()));
    // Spec base for relative `src` paths is `<workspace>/.grain/`
    // — the parent of config.toml / plugin-lock.toml / legacy
    // plugin-spec.toml (all three live there). `src = "../lazy-gagent"`
    // resolves to `<workspace>/lazy-gagent`.
    let spec_base = workspace.root().join(".grain");
    // Buffer plugin-install failures so we can mirror them into the
    // TUI transcript once `evt_tx` exists. Without this the user
    // never sees the failure: stderr writes happen before the alt
    // screen takes over and get scrolled out of view.
    let mut deferred_warnings: Vec<String> = Vec::new();
    // Effective spec = config.toml [[plugin]] ∪ plugin-lock.toml ∪
    // legacy plugin-spec.toml (first-source-wins). Reload config
    // here so worker doesn't depend on the earlier apply pass —
    // the file read is cheap and the call site stays self-contained.
    let plugin_spec = {
        let config = grain_ai_agent_headless::ConfigFile::load(workspace.root())
            .unwrap_or_default();
        let (spec, warnings) =
            grain_ai_agent_headless::effective_spec(workspace.root(), &config);
        for w in warnings {
            eprintln!("[warn] {w}");
            deferred_warnings.push(w);
        }
        if !spec.plugins.is_empty() {
            let report =
                grain_ai_agent_headless::sync_plugins(&spec, &plugins_dir, &spec_base);
            report.log_to_stderr();
            for (name, reason) in &report.failed {
                deferred_warnings.push(format!("plugin '{name}' install failed: {reason}"));
            }
        }
        spec
    };
    // Discover both filesystem-installed plugins (`plugins_dir` walk
    // — gets git-cloned + legacy-symlinked + hand-placed dirs) and
    // **local-source** entries from the spec (no filesystem entry
    // under `plugins_dir`; engine reads them straight from `src`).
    let discovered_plugins = grain_ai_agent_headless::discover_plugins_with_spec(
        &plugins_dir,
        &plugin_spec,
        &spec_base,
    );
    for p in &discovered_plugins {
        eprintln!("[info] {}", grain_ai_agent_headless::summarize_plugin(p));
    }
    // Plugin-contributed slash command overrides. Computed here so
    // the boot-time send below uses the same plugin set the rest of
    // the worker sees; recomputed on `Command::ReloadRhaiScripts`.
    let plugin_slashes_at_boot =
        grain_ai_agent_headless::collect_plugin_slash_commands(&discovered_plugins);

    // --- System prompt + skills block -------------------------------------
    // Pin the prompt for the lifetime of this session. The harness's
    // `PinnedSystemPrompt` freezes `base + <available_skills>` at
    // session start; never re-render in the hot path so the upstream
    // prefix cache (Anthropic, OpenAI, DeepSeek …) stays warm.
    //
    // Phase B-3: plugins can ship `prompts/*.md` files that get
    // appended (with a `## Plugin: <name>` banner) onto the base
    // prompt before pinning. This lets a plugin contribute domain
    // rules ("always run clippy") without forking the base prompt.
    let raw_base = match &cfg.system_prompt_file {
        Some(path) => std::fs::read_to_string(path).map_err(|e| WorkerInitError::SystemPrompt {
            path: path.clone(),
            source: e,
        })?,
        None => coding_agent_system_prompt(cfg.allow_write, cfg.allow_bash).to_string(),
    };
    let base_prompt = grain_ai_agent_headless::compose_system_prompt_with_plugins(
        &raw_base,
        &discovered_plugins,
    );
    let skills_dir = resolve_skills_dir(workspace.root(), cfg.skills_dir.as_deref());
    // Phase A/B: `find_skills_with_plugins` walks the primary skills
    // dir, then folds in each plugin's `<plugin>/skills/`.
    let skills = grain_ai_agent_headless::find_skills_with_plugins(&skills_dir, &discovered_plugins)
        .unwrap_or_default();
    // Clone for the UI's slash-palette skill injection — the original
    // moves into `PinnedSystemPrompt::build` below.
    let skills_for_ui = skills.clone();
    let pinned = grain_agent_harness::PinnedSystemPrompt::build(base_prompt, &skills);
    eprintln!(
        "[info] system prompt pinned ({} bytes, digest {:016x})",
        pinned.len(),
        pinned.digest()
    );
    let system_prompt = pinned.to_string_owned();

    // --- Session auto-create + restore -------------------------------------
    let sessions_dir = cfg
        .sessions_dir
        .clone()
        .unwrap_or_else(|| workspace.root().join(".grain").join("sessions"));
    if cfg.session.is_none() {
        // Two-step resolution: auto-resume the most-recently-modified
        // transcript in `sessions_dir` when `--new-session` is off
        // (the default — users expect to return to the conversation
        // they had open). Falls back to minting a fresh `<uuidv7>.jsonl`
        // when no prior session exists, or when the caller forced
        // `--new-session`.
        let resumed = if !cfg.new_session {
            grain_ai_agent_headless::list_sessions(&sessions_dir)
                .into_iter()
                .next()
                .map(|m| m.path)
        } else {
            None
        };
        if let Some(path) = resumed {
            eprintln!("[info] auto-resume: {}", path.display());
            cfg.session = Some(path);
        } else {
            match std::fs::create_dir_all(&sessions_dir) {
                Ok(()) => {
                    let path = grain_ai_agent_headless::new_session_path(&sessions_dir);
                    eprintln!("[info] session: {}", path.display());
                    cfg.session = Some(path);
                }
                Err(e) => {
                    eprintln!(
                        "[warn] could not create sessions dir {}: {e} \
                         (session won't be persisted this run)",
                        sessions_dir.display()
                    );
                }
            }
        }
    }
    let prior_messages = match &cfg.session {
        Some(path) => load_messages(path).map_err(|e| WorkerInitError::Session {
            path: path.clone(),
            source: Box::new(e),
        })?,
        None => Vec::new(),
    };

    // --- Stream ------------------------------------------------------------
    let stream: StreamFn = Arc::new(
        GenaiStream::builder()
            .with_openai_compat_preset(cfg.openai_compat)
            .with_provider_profiles(&cfg.profiles)
            .with_bypass_proxy(cfg.bypass_proxy)
            .with_registry(registry.clone())
            .build(),
    );

    // --- Tools -------------------------------------------------------------
    let mut tools: Vec<Arc<dyn AgentTool>> = coding_read_tools(workspace.clone());
    if cfg.allow_write {
        tools.extend(coding_write_tools(workspace.clone()));
    }
    if cfg.allow_bash {
        tools.extend(coding_bash_tools(workspace.clone()));
    }
    if cfg.allow_web {
        tools.extend(coding_web_tools());
    }
    if cfg.allow_semantic_search {
        return Err(WorkerInitError::SemanticUnsupported);
    }

    // --- JS scripted tools (optional, behind `scripts-boa` feature) ------
    // Phase B-1: plugins contribute their own `<plugin>/scripts/` dirs
    // alongside the workspace's primary scripts dir. All `.js` files
    // get loaded into one shared Boa worker via `from_scripts_dirs`
    // so plugin-registered tools end up exposed to the same agent.
    let scripts_path = cfg
        .scripts_dir
        .clone()
        .unwrap_or_else(|| workspace.root().join(".grain").join("scripts"));
    #[cfg(feature = "scripts-boa")]
    let scripts_extension = {
        let mut all_dirs: Vec<PathBuf> = vec![scripts_path.clone()];
        all_dirs.extend(grain_ai_agent_headless::plugin_script_dirs(
            &discovered_plugins,
        ));
        match grain_script_boa::BoaExtension::from_scripts_dirs(&all_dirs) {
            Ok(ext) => {
                let scripted = ext.tools();
                if !scripted.is_empty() {
                    eprintln!(
                        "[info] loaded {} JS tool(s) from {} dir(s) ({} from plugins)",
                        scripted.len(),
                        all_dirs.len(),
                        all_dirs.len().saturating_sub(1)
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
    {
        let any_plugin_scripts = !grain_ai_agent_headless::plugin_script_dirs(
            &discovered_plugins,
        )
        .is_empty();
        if cfg.scripts_dir.is_some() || scripts_path.exists() || any_plugin_scripts {
            eprintln!(
                "[warn] scripts/ present (workspace or plugin) but binary was \
                 built without --features scripts-boa; ignoring"
            );
        }
    }

    // --- Rhai scripted tools (optional, behind `scripts-rhai` feature) -----
    // Mirrors the Boa pipeline but loads `.rhai` files instead and
    // registers `plugins_install` / `plugins_update` / `plugins_remove`
    // as host native functions. Plugin Rhai scripts can call those
    // to manage other plugins (e.g. lazy-gagent's install.rhai).
    //
    // Same dir set as Boa: workspace primary + each plugin's
    // scripts/. RhaiExtension filters to `*.rhai` so .js files are
    // silently ignored.
    #[cfg(feature = "scripts-rhai")]
    let rhai_dirs: Vec<PathBuf> = {
        let mut v = vec![scripts_path.clone()];
        v.extend(grain_ai_agent_headless::plugin_script_dirs(
            &discovered_plugins,
        ));
        v
    };
    #[cfg(feature = "scripts-rhai")]
    let workspace_for_rhai: PathBuf = workspace.root().to_path_buf();

    // Base tools snapshot **before** Rhai contribution — held by the
    // worker so hot-reload can rebuild the full tool list as
    // `base + fresh_rhai`. Trivially cloneable (each entry is `Arc`).
    #[cfg(feature = "scripts-rhai")]
    let base_tools: Vec<Arc<dyn AgentTool>> = tools.clone();

    #[cfg(feature = "scripts-rhai")]
    let rhai_bundle: RhaiBundle = {
        let bundle = build_rhai_bundle(
            &workspace_for_rhai,
            &plugins_dir,
            &rhai_dirs,
        );
        if !bundle.tools.is_empty() {
            eprintln!(
                "[info] loaded {} Rhai tool(s) from {} dir(s)",
                bundle.tools.len(),
                rhai_dirs.len()
            );
        }
        tools.extend(bundle.tools.clone());
        bundle
    };

    // --- Shared active-model handle -----------------------------------------
    // Both ContextGuard and TokenBudgetPolicy read from the same handle
    // so a mid-session model switch immediately updates context-window
    // enforcement and compaction thresholds.
    let active_model_handle: ActiveModelHandle =
        Arc::new(std::sync::RwLock::new(active_model_id.clone()));

    // --- Context guard -----------------------------------------------------
    // The transform fn only sees `Vec<AgentMessage>` — it never sees
    // the system prompt or tool schemas the provider tacks on for
    // every request. Pre-charge the budget for that fixed overhead so
    // we don't trim to the model's window only to overshoot when the
    // request actually goes out. Computed once at boot from the
    // pinned prompt + serialized tool definitions; 1.3x fudge covers
    // per-message JSON framing and provider-side role tokens.
    let system_overhead_tokens: u64 = {
        let estimator = grain_agent_harness::TokenEstimator::approximate();
        let mut raw = estimator.estimate_string(&system_prompt);
        for t in &tools {
            let def = t.definition();
            raw += estimator.estimate_string(&def.name);
            raw += estimator.estimate_string(&def.description);
            raw += estimator.estimate_string(
                &serde_json::to_string(&def.parameters).unwrap_or_default(),
            );
        }
        (raw as f64 * 1.3).ceil() as u64
    };
    eprintln!(
        "[info] context guard: system+tools overhead ≈ {system_overhead_tokens} tokens \
         ({} tools, system_prompt {} bytes)",
        tools.len(),
        system_prompt.len(),
    );
    let guard = ContextGuard::with_active_model_handle(
        registry.clone(),
        active_model_handle.clone(),
    )
    .with_policy(ContextGuardPolicy::DropOldest)
    .with_headroom_tokens(cfg.headroom_tokens)
    .with_system_overhead_tokens(system_overhead_tokens)
    .into_transform_fn();

    // --- Channels ----------------------------------------------------------
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<TuiEvent>();

    // Replay any plugin-spec sync failures into the TUI transcript so
    // the user actually sees them — the equivalent stderr lines emit
    // *before* `init_terminal()` switches to the alt screen and get
    // hidden behind the UI. AgentWorkerError renders red in the
    // transcript, which is the right visibility for a startup
    // problem the user should act on.
    for msg in deferred_warnings.drain(..) {
        let _ = evt_tx.send(TuiEvent::AgentWorkerError(msg));
    }

    // --- Hooks: storm suppressor + optional escalation ---------------------
    let before_tool_call: Option<BeforeToolCallFn> = Some(grain_agent_harness::storm_hook(
        grain_agent_harness::StormConfig::default(),
    ));
    let escalation_hook: Option<PrepareNextTurnFn> = match &cfg.escalate_to {
        Some(target_id) => match registry.to_core_model(target_id) {
            Some(target) => {
                eprintln!(
                    "[info] escalation armed: → {} after {} failure(s)",
                    target.id, cfg.escalate_after
                );
                Some(grain_agent_harness::failure_escalation_hook(
                    grain_agent_harness::EscalationConfig::new(cfg.escalate_after, target),
                ))
            }
            None => {
                eprintln!(
                    "[warn] --escalate-to '{target_id}' not in registry; \
                     escalation disabled"
                );
                None
            }
        },
        None => None,
    };

    // --- Compaction hook (TokenBudgetPolicy) ------------------------------
    let compaction_policy = Arc::new(grain_agent_harness::TokenBudgetPolicy::new(
        registry.clone(),
        active_model_handle.clone(),
        grain_agent_harness::DEFAULT_COMPACTION_SETTINGS,
        grain_agent_harness::TokenEstimator::approximate(),
    ));
    let compaction_hook: Option<PrepareNextTurnFn> = Some(
        grain_agent_harness::compaction_prepare_next_turn(
            stream.clone(),
            compaction_policy,
            grain_agent_harness::DEFAULT_COMPACTION_PROMPT.to_string(),
        ),
    );

    // Chain compaction (A) with escalation (B). Compaction rewrites the
    // context; escalation may override the model. B's model/thinking_level
    // wins because escalation needs authority to swap models on failure.
    let prepare_next_turn: Option<PrepareNextTurnFn> =
        chain_prepare_next_turn(compaction_hook, escalation_hook);

    // --- Debug-log `convert_to_llm` wrapper --------------------------------
    let convert_to_llm: Option<ConvertToLlmFn> = if cfg.debug_log {
        let evt_tx_for_log = evt_tx.clone();
        let log_model_id = active_model_id.clone();
        let log_endpoint = match cfg
            .initial_profile_idx
            .and_then(|idx| cfg.profiles.get(idx))
        {
            Some(p) => p
                .base_url
                .clone()
                .unwrap_or_else(|| format!("(profile '{}', native adapter)", p.name)),
            None => "(native adapter; genai default endpoint)".to_string(),
        };
        // Persist to `<workspace>/.grain/debug-log` so debug data
        // survives the TUI quitting and the in-memory request_log
        // VecDeque rolling over. Append-mode; opened once at boot
        // and shared via Arc<Mutex<_>>. File-open failures are
        // surfaced as a transcript warning but don't disable the
        // in-memory `/log` overlay (which keeps working).
        use std::sync::Mutex;
        let log_path = workspace.root().join(".grain").join("debug-log");
        let log_file: Option<Arc<Mutex<std::fs::File>>> = match (|| -> std::io::Result<_> {
            if let Some(parent) = log_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
        })() {
            Ok(f) => {
                let _ = evt_tx.send(TuiEvent::Info(format!(
                    "(debug-log persisting to {})",
                    log_path.display()
                )));
                Some(Arc::new(Mutex::new(f)))
            }
            Err(e) => {
                let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                    "debug-log open {} failed: {e} — in-memory /log still works",
                    log_path.display()
                )));
                None
            }
        };
        Some(Arc::new(move |messages: Vec<AgentMessage>| {
            let evt_tx_for_log = evt_tx_for_log.clone();
            let log_model_id = log_model_id.clone();
            let log_endpoint = log_endpoint.clone();
            let log_file = log_file.clone();
            Box::pin(async move {
                let projected: Vec<Message> = messages
                    .into_iter()
                    .filter_map(|m| match m {
                        AgentMessage::Standard(msg) => Some(msg),
                        AgentMessage::Custom(_) => None,
                    })
                    .collect();
                let body_json = serde_json::to_string_pretty(&projected)
                    .unwrap_or_else(|e| format!("(serialize failed: {e})"));
                let body = format!(
                    "POST {log_endpoint}/chat/completions\nmodel: {log_model_id}\n\n{body_json}"
                );
                if let Some(file) = log_file.as_ref()
                    && let Ok(mut f) = file.lock()
                {
                    use std::io::Write;
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let _ = writeln!(
                        f,
                        "\n===== request @ unix {stamp} =====\n{body}\n"
                    );
                    let _ = f.flush();
                }
                let _ = evt_tx_for_log.send(TuiEvent::RequestLogged { body });
                projected
            })
        }))
    } else {
        None
    };

    // --- HarnessBuilder + initial harness ----------------------------------
    let model_cost = model.cost.clone();
    let deepseek = grain_ai_agent_headless::DeepSeekPack::new(&model);
    let builder = Arc::new(HarnessBuilder {
        model,
        stream,
        system_prompt,
        tools,
        transform_context: guard,
        before_tool_call,
        prepare_next_turn,
        convert_to_llm,
    });
    let harness = builder.build(prior_messages).await;

    let handles = WorkerHandles {
        model_id: active_model_id.clone(),
        workspace_display: workspace.root().display().to_string(),
        allow_write: cfg.allow_write,
        allow_bash: cfg.allow_bash,
        allow_web: cfg.allow_web,
        allow_semantic_search: cfg.allow_semantic_search,
        model_cost: model_cost.clone(),
    };

    // --- Telemetry sink (Arc'd; lives across `/resume` swaps) -------------
    let telemetry_sink: Option<Arc<TelemetrySink>> = match cfg.telemetry_file.clone() {
        Some(path) => match TelemetrySink::open(&path) {
            Ok(sink) => Some(Arc::new(sink)),
            Err(e) => {
                let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                    "telemetry open failed: {e}"
                )));
                None
            }
        },
        None => None,
    };

    // --- Session writer (reopened on every `/resume` swap) -----------------
    let session_writer: Option<Arc<SessionWriter>> = match cfg.session.clone() {
        Some(path) => match SessionWriter::open(&path) {
            Ok(w) => Some(Arc::new(w)),
            Err(e) => {
                let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                    "session writer open failed: {e}"
                )));
                None
            }
        },
        None => None,
    };

    let profiles = cfg.profiles.clone();
    let workspace_for_task = workspace.clone();
    let registry_for_task = registry.clone();
    let skills_dir_for_task = skills_dir.clone();
    let sessions_dir_for_task = sessions_dir.clone();
    let plugins_dir_for_task = plugins_dir.clone();
    let evt_tx_for_task = evt_tx.clone();
    let model_cost_for_task = model_cost.clone();
    // Captured by the worker task closure so the Boa worker stays
    // alive for the whole agent lifetime; dropping at task end sends
    // Shutdown to that worker thread.
    #[cfg(feature = "scripts-boa")]
    let _scripts_keepalive = scripts_extension;
    // Rhai tools each own their own `Arc<Engine>` (cloned during
    // `RhaiExtension::tools()`) so we don't need a separate
    // keepalive — dropping the extension wrapper is fine.

    // Capture the ingredients the worker needs to rebuild the Rhai
    // tool list on `Command::ReloadRhaiScripts`. `base_tools` is the
    // pre-Rhai snapshot taken above; on reload we set
    // `agent.tools = base_tools + fresh_rhai_tools`.
    #[cfg(feature = "scripts-rhai")]
    let rhai_ctx_for_task = RhaiReloadCtx {
        workspace_root: workspace_for_rhai.clone(),
        plugins_dir: plugins_dir.clone(),
        script_dirs: rhai_dirs.clone(),
        base_tools,
        ui_handlers: Arc::new(build_ui_handler_map(&rhai_bundle.handles)),
    };

    // Hot-reload: install a notify watcher on every Rhai script dir
    // and forward "something changed" pulses (debounced) into the
    // worker via `Command::ReloadRhaiScripts`. The keepalive tuple
    // (watcher + bridge thread) lives alongside the boa keepalive
    // so it gets torn down at the same time. When the cmd_tx
    // channel closes, the bridge thread sees the send error and
    // exits naturally.
    #[cfg(feature = "hot-reload")]
    let _hot_reload_keepalive: Option<(
        notify::RecommendedWatcher,
        std::thread::JoinHandle<()>,
    )> = {
        use notify::{RecursiveMode, Watcher};

        let (event_tx, event_rx) = std::sync::mpsc::channel::<()>();
        let watcher_result = notify::recommended_watcher(
            move |res: notify::Result<notify::Event>| {
                // Filter: only fire on data changes / file creation /
                // removal. notify can also emit `Access` events on some
                // platforms which would spam the bridge.
                let should_fire = match res {
                    Ok(ev) => matches!(
                        ev.kind,
                        notify::EventKind::Create(_)
                            | notify::EventKind::Modify(_)
                            | notify::EventKind::Remove(_)
                    ),
                    Err(_) => false,
                };
                if should_fire {
                    let _ = event_tx.send(());
                }
            },
        );
        match watcher_result {
            Ok(mut watcher) => {
                let mut watched_any = false;
                for dir in &rhai_dirs {
                    if !dir.exists() {
                        continue;
                    }
                    if let Err(e) = watcher.watch(dir, RecursiveMode::Recursive) {
                        eprintln!(
                            "[warn] notify watch {}: {e}",
                            dir.display()
                        );
                    } else {
                        watched_any = true;
                    }
                }
                if watched_any {
                    let cmd_tx_for_watcher = cmd_tx.clone();
                    let bridge = std::thread::spawn(move || {
                        // First-event blocks; bursts coalesce inside
                        // the DEBOUNCE window so an editor's "atomic
                        // save" (write-temp + rename) only triggers
                        // one reload.
                        const DEBOUNCE: std::time::Duration =
                            std::time::Duration::from_millis(250);
                        while event_rx.recv().is_ok() {
                            while event_rx.recv_timeout(DEBOUNCE).is_ok() {
                                // drain
                            }
                            if cmd_tx_for_watcher
                                .send(Command::ReloadRhaiScripts)
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                    eprintln!(
                        "[info] hot-reload: watching {} Rhai dir(s) for changes",
                        rhai_dirs.len()
                    );
                    Some((watcher, bridge))
                } else {
                    None
                }
            }
            Err(e) => {
                eprintln!("[warn] hot-reload init failed: {e}");
                None
            }
        }
    };

    let join = tokio::spawn(async move {
        // Pin the Boa extension into the task scope so its worker
        // thread lives until the agent task exits.
        #[cfg(feature = "scripts-boa")]
        let _boa_keepalive = _scripts_keepalive;
        // Same lifetime story for the notify watcher + bridge
        // thread: dropping them mid-loop would silently stop hot
        // reload. Pin them into the task scope.
        #[cfg(feature = "hot-reload")]
        let _hot_reload_keepalive_in_task = _hot_reload_keepalive;

        install_subscriptions(
            &harness,
            &evt_tx_for_task,
            telemetry_sink.clone(),
            session_writer.clone(),
        )
        .await;

        // Send loaded skills to the UI so the slash-palette can offer
        // skill prompt injection alongside built-in commands.
        let _ = evt_tx_for_task.send(TuiEvent::SkillsLoaded(skills_for_ui));
        // Ship plugin-contributed slash command overrides so the
        // TUI's dispatch_slash consults them before the built-in
        // table — that's how lazy-gagent claims `/plugins`.
        let _ = evt_tx_for_task.send(TuiEvent::SlashCommandsRegistered(
            plugin_slashes_at_boot,
        ));

        // If we booted with a profile, tell the UI so the status line
        // and `✓` marker land correctly on first frame.
        if let Some(name) = active_profile_name.clone() {
            let _ = evt_tx_for_task.send(TuiEvent::ProviderApplied {
                profile: name,
                model: active_model_id.clone(),
                cost: model_cost_for_task.clone(),
            });
        }

        run_command_loop(
            harness,
            builder,
            telemetry_sink,
            session_writer,
            deepseek,
            workspace_for_task,
            registry_for_task,
            active_model_handle,
            skills_dir_for_task,
            sessions_dir_for_task,
            plugins_dir_for_task,
            profiles,
            cmd_rx,
            evt_tx_for_task,
            #[cfg(feature = "scripts-rhai")]
            rhai_ctx_for_task,
        )
        .await;
    });

    Ok(Worker {
        cmd_tx,
        evt_rx,
        handles,
        join,
    })
}

/// Context the worker needs to rebuild Rhai-contributed tools on
/// `Command::ReloadRhaiScripts`. Captured at boot in `spawn()` and
/// kept alive in the worker for the agent's lifetime.
#[cfg(feature = "scripts-rhai")]
#[derive(Clone)]
pub struct RhaiReloadCtx {
    /// Workspace root — host fn closures derive the lock /
    /// legacy-spec paths from this and re-read `config.toml` per
    /// call so they always see the current plugin declaration set.
    pub workspace_root: PathBuf,
    /// `<plugins_dir>` resolved at boot. Same path the boot-time
    /// `discover_plugins` walked.
    pub plugins_dir: PathBuf,
    /// Every directory we load `*.rhai` from — workspace primary
    /// plus each plugin's `scripts/` subdir (frozen at boot).
    pub script_dirs: Vec<PathBuf>,
    /// Tools the agent already had **before** Rhai contributed:
    /// built-in read/write/bash/web, Boa-scripted tools, etc. On
    /// reload we set `agent.tools = base_tools + fresh_rhai_tools`.
    pub base_tools: Vec<Arc<dyn AgentTool>>,
    /// Handler-name → ScriptHandle map. Built from currently-loaded
    /// scripts + manifest `[[ui_command]]` declarations. Looked up
    /// on `Command::InvokePluginUi`.
    pub ui_handlers: Arc<std::collections::HashMap<String, grain_script_rhai::ScriptHandle>>,
}

/// Build the handler→ScriptHandle map. Registers **every** function
/// defined in any loaded plugin script — UI handlers declared via
/// `[[ui_command]]` and their downstream `on_submit` / `on_yes`
/// callbacks both live in plugin scripts and reference each other
/// by string name, so we can't tell ahead of time which functions
/// are reachable. The cost is negligible (one Arc-clone per
/// function). First match wins on collision; later duplicates emit
/// a warning so the conflict is visible in the startup log.
#[cfg(feature = "scripts-rhai")]
fn build_ui_handler_map(
    handles: &[grain_script_rhai::ScriptHandle],
) -> std::collections::HashMap<String, grain_script_rhai::ScriptHandle> {
    use std::collections::HashMap;
    let mut out: HashMap<String, grain_script_rhai::ScriptHandle> = HashMap::new();
    for handle in handles {
        for fn_name in handle.ast_function_names() {
            if out.contains_key(&fn_name) {
                eprintln!(
                    "[warn] ui handler '{fn_name}' defined by multiple scripts; \
                     keeping first ({})",
                    out[&fn_name].label
                );
                continue;
            }
            out.insert(fn_name, handle.clone());
        }
    }
    out
}

/// Chain two optional [`PrepareNextTurnFn`] hooks: run `a` first, then
/// `b`. If `a` produces a context update, `b` sees it. `b`'s `model`
/// and `thinking_level` override `a`'s (escalation wins over compaction
/// for model swaps). Returns `None` when both inputs are `None`.
fn chain_prepare_next_turn(
    a: Option<PrepareNextTurnFn>,
    b: Option<PrepareNextTurnFn>,
) -> Option<PrepareNextTurnFn> {
    match (a, b) {
        (None, None) => None,
        (Some(f), None) | (None, Some(f)) => Some(f),
        (Some(first), Some(second)) => Some(Arc::new(move |ctx| {
            let first = first.clone();
            let second = second.clone();
            Box::pin(async move {
                let update_a = first(ctx.clone()).await;

                // If A produced a context rewrite, build a new
                // PrepareNextTurnContext with that context so B
                // operates on the compacted transcript.
                let ctx_for_b = if let Some(ref upd) = update_a
                    && let Some(ref new_ctx) = upd.context
                {
                    grain_agent_core::PrepareNextTurnContext {
                        context: Arc::new(new_ctx.clone()),
                        ..ctx
                    }
                } else {
                    ctx
                };

                let update_b = second(ctx_for_b).await;

                // Merge: B's fields override A's where present.
                match (update_a, update_b) {
                    (None, None) => None,
                    (Some(u), None) | (None, Some(u)) => Some(u),
                    (Some(a), Some(b)) => Some(AgentLoopTurnUpdate {
                        context: b.context.or(a.context),
                        model: b.model.or(a.model),
                        thinking_level: b.thinking_level.or(a.thinking_level),
                    }),
                }
            })
        })),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_command_loop(
    mut harness: Arc<AgentHarness>,
    builder: Arc<HarnessBuilder>,
    telemetry_sink: Option<Arc<TelemetrySink>>,
    mut session_writer: Option<Arc<SessionWriter>>,
    deepseek: grain_ai_agent_headless::DeepSeekPack,
    workspace: Arc<Workspace>,
    registry: Arc<Registry>,
    active_model_handle: ActiveModelHandle,
    skills_dir: PathBuf,
    sessions_dir: PathBuf,
    plugins_dir: PathBuf,
    profiles: Vec<ProviderProfile>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    evt_tx: mpsc::UnboundedSender<TuiEvent>,
    #[cfg(feature = "scripts-rhai")] mut rhai_ctx: RhaiReloadCtx,
) {
    if deepseek.is_enabled() {
        eprintln!("[info] DeepSeek pack active — reasoning scavenge + subagent.done detection enabled");
    }

    // Tracks the model the harness is *currently* driving. Starts
    // from the boot-time model captured in HarnessBuilder; updated
    // every time `ApplyProvider` (and any future `/model`-style
    // command) swaps the harness's model. `ResumeSession` reads
    // this to pin the resumed harness to whatever the user is
    // currently using, instead of snapping back to the boot model.
    let mut current_model = builder.model.clone();

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::SendPrompt(text) => {
                // Fire and continue — the prompt task lives on its own
                // until completion; AgentEvent forwarding is wired via
                // `install_subscriptions`.
                let harness = harness.clone();
                let evt_tx = evt_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = harness.prompt_text(text).await {
                        let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!("prompt: {e}")));
                    }
                });
            }
            Command::AbortCurrentTurn => {
                harness.abort().await;
            }
            Command::Reset => {
                // The harness exposes no `reset()` of its own — the
                // underlying agent's reset is enough for the TUI's
                // "blow away in-flight state" intent.
                harness.agent().reset().await;
            }
            Command::ReturnDoctor => {
                let text = render_doctor_report(&workspace, &registry);
                let _ = evt_tx.send(TuiEvent::OverlayDoctor(text));
            }
            Command::ReturnSkills => match find_skills(&skills_dir) {
                Ok(skills) => {
                    let payload: Vec<(String, String, bool)> = skills
                        .into_iter()
                        .map(|s| (s.name, s.description, s.disable_model_invocation))
                        .collect();
                    let _ = evt_tx.send(TuiEvent::OverlaySkills(payload));
                }
                Err(e) => {
                    let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!("skills scan: {e}")));
                }
            },
            Command::ReturnSessions => {
                let list = grain_ai_agent_headless::list_sessions(&sessions_dir);
                let _ = evt_tx.send(TuiEvent::SessionsListed(list));
            }
            Command::ReturnPlugins => {
                // Re-read config + lock + legacy spec so entries
                // the user added since boot show up in the overlay.
                let config = grain_ai_agent_headless::ConfigFile::load(workspace.root())
                    .unwrap_or_default();
                let (spec, _warnings) =
                    grain_ai_agent_headless::effective_spec(workspace.root(), &config);
                let spec_base = workspace.root().join(".grain");
                let discovered = grain_ai_agent_headless::discover_plugins_with_spec(
                    &plugins_dir,
                    &spec,
                    &spec_base,
                );
                let infos: Vec<grain_ai_agent_headless::PluginInfo> = discovered
                    .iter()
                    .map(grain_ai_agent_headless::plugin_info)
                    .collect();
                let ui_commands =
                    grain_ai_agent_headless::collect_ui_commands(&discovered);
                let _ = evt_tx.send(TuiEvent::PluginsListed {
                    plugins: infos,
                    ui_commands,
                });
            }
            Command::InstallPlugin { name, src, rev } => {
                // Block writes that would shadow a config.toml
                // entry — the runtime can't safely modify
                // hand-written declarations.
                let config = grain_ai_agent_headless::ConfigFile::load(workspace.root())
                    .unwrap_or_default();
                if let Some(grain_ai_agent_headless::PluginOrigin::Config) =
                    grain_ai_agent_headless::origin_of(workspace.root(), &config, &name)
                {
                    let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                        "install '{name}': already declared in config.toml — edit that file directly"
                    )));
                    continue;
                }
                let lock_path =
                    grain_ai_agent_headless::default_lock_path(workspace.root());
                match grain_ai_agent_headless::install(
                    &lock_path,
                    &plugins_dir,
                    &name,
                    &src,
                    rev.as_deref(),
                ) {
                    Ok(outcome) => {
                        if outcome.report.failed.iter().any(|(n, _)| n == &name) {
                            let reason = outcome
                                .report
                                .failed
                                .iter()
                                .find(|(n, _)| n == &name)
                                .map(|(_, r)| r.clone())
                                .unwrap_or_default();
                            let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                                "install '{name}' sync failed: {reason}"
                            )));
                        } else {
                            let _ = evt_tx.send(TuiEvent::Info(format!(
                                "(installed '{name}' — restart TUI to pick up its skills / themes / prompts / scripts)"
                            )));
                        }
                    }
                    Err(e) => {
                        let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                            "install '{name}': {e}"
                        )));
                    }
                }
            }
            Command::UpdatePlugin { name } => {
                match grain_ai_agent_headless::update(&plugins_dir, &name) {
                    Ok(grain_ai_agent_headless::UpdateOutcome::Symlink) => {
                        let _ = evt_tx.send(TuiEvent::Info(format!(
                            "(plugin '{name}' is a symlink — source tree is live, nothing to pull)"
                        )));
                    }
                    Ok(grain_ai_agent_headless::UpdateOutcome::Pulled) => {
                        let _ = evt_tx.send(TuiEvent::Info(format!(
                            "(updated '{name}' via git pull — restart TUI to pick up changes)"
                        )));
                    }
                    Err(e) => {
                        let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                            "update '{name}': {e}"
                        )));
                    }
                }
            }
            Command::RemovePlugin {
                name,
                delete_files,
            } => {
                let config = grain_ai_agent_headless::ConfigFile::load(workspace.root())
                    .unwrap_or_default();
                let target_path = match grain_ai_agent_headless::origin_of(
                    workspace.root(),
                    &config,
                    &name,
                ) {
                    Some(grain_ai_agent_headless::PluginOrigin::Config) => {
                        let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                            "remove '{name}': declared in config.toml — edit that file directly"
                        )));
                        continue;
                    }
                    Some(grain_ai_agent_headless::PluginOrigin::Lock) => {
                        grain_ai_agent_headless::default_lock_path(workspace.root())
                    }
                    Some(grain_ai_agent_headless::PluginOrigin::LegacySpec) => {
                        grain_ai_agent_headless::default_spec_path(workspace.root())
                    }
                    None => {
                        let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                            "remove '{name}': not declared in config.toml, plugin-lock.toml, or plugin-spec.toml"
                        )));
                        continue;
                    }
                };
                match grain_ai_agent_headless::remove(
                    &target_path,
                    &plugins_dir,
                    &name,
                    delete_files,
                ) {
                    Ok(outcome) => {
                        let suffix = if outcome.files_removed {
                            " + files"
                        } else {
                            ""
                        };
                        let _ = evt_tx.send(TuiEvent::Info(format!(
                            "(removed '{name}' from {}{suffix} — restart TUI to drop it from the live catalog)",
                            target_path.file_name().and_then(|s| s.to_str()).unwrap_or("spec")
                        )));
                    }
                    Err(e) => {
                        let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                            "remove '{name}': {e}"
                        )));
                    }
                }
            }
            #[cfg(feature = "scripts-rhai")]
            Command::ReloadRhaiScripts => {
                // Rebuild from the captured ingredients. Each tool
                // is `Arc<dyn AgentTool>` so the swap is just a
                // pointer move on the agent side — no in-flight turn
                // gets disturbed. Also refresh the UI handler map so
                // freshly-defined `[[ui_command]]` handlers become
                // dispatchable without restart.
                let fresh = build_rhai_bundle(
                    &rhai_ctx.workspace_root,
                    &rhai_ctx.plugins_dir,
                    &rhai_ctx.script_dirs,
                );
                let mut combined = rhai_ctx.base_tools.clone();
                let count = fresh.tools.len();
                combined.extend(fresh.tools);
                harness.agent().set_tools(combined).await;
                rhai_ctx.ui_handlers = Arc::new(build_ui_handler_map(&fresh.handles));
                let _ = evt_tx.send(TuiEvent::Info(format!(
                    "(reloaded — {count} Rhai tool(s) active)"
                )));
            }
            #[cfg(feature = "scripts-rhai")]
            Command::InvokePluginUi { handler, args } => {
                let handle = match rhai_ctx.ui_handlers.get(&handler) {
                    Some(h) => h.clone(),
                    None => {
                        let _ = evt_tx.send(TuiEvent::UiHandlerError(format!(
                            "ui handler '{handler}' not found in any loaded plugin script"
                        )));
                        continue;
                    }
                };
                let evt_tx_for_call = evt_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let result = handle.call_fn_json(&handler, args);
                    match result {
                        Ok(value) => match serde_json::from_value::<
                            grain_ai_agent_headless::OverlayDescriptor,
                        >(value.clone())
                        {
                            Ok(desc) => {
                                let _ = evt_tx_for_call.send(TuiEvent::UiOverlay(desc));
                            }
                            Err(e) => {
                                let _ = evt_tx_for_call.send(TuiEvent::UiHandlerError(format!(
                                    "ui handler '{handler}' returned invalid descriptor: {e} (got {value})"
                                )));
                            }
                        },
                        Err(e) => {
                            let _ = evt_tx_for_call.send(TuiEvent::UiHandlerError(format!(
                                "ui handler '{handler}' failed: {e}"
                            )));
                        }
                    }
                });
            }
            #[cfg(not(feature = "scripts-rhai"))]
            Command::InvokePluginUi { handler, .. } => {
                let _ = evt_tx.send(TuiEvent::UiHandlerError(format!(
                    "ui handler '{handler}': TUI was built without --features scripts-rhai"
                )));
            }
            #[cfg(not(feature = "scripts-rhai"))]
            Command::ReloadRhaiScripts => {
                let _ = evt_tx.send(TuiEvent::AgentWorkerError(
                    "reload: TUI was built without --features scripts-rhai".into(),
                ));
            }
            Command::ResumeSession(path) => {
                // Cancel any in-flight turn and wait for the old
                // harness to settle so we don't get late events from
                // the abandoned session leaking into the new one.
                harness.abort().await;
                harness.wait_for_idle().await;

                let prior = match load_messages(&path) {
                    Ok(msgs) => msgs,
                    Err(e) => {
                        let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                            "resume load {} failed: {e}",
                            path.display()
                        )));
                        continue;
                    }
                };
                let prior_for_ui = prior.clone();
                let new_writer: Option<Arc<SessionWriter>> = match SessionWriter::open(&path) {
                    Ok(w) => Some(Arc::new(w)),
                    Err(e) => {
                        let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                            "resume session writer open {} failed: {e}",
                            path.display()
                        )));
                        None
                    }
                };
                // Pin the resumed harness to the **currently
                // active** model — not the boot-time one captured
                // in `builder.model`, and not anything saved in
                // the resumed JSONL session's metadata. This
                // matches what the user is actively driving in
                // this run.
                let new_harness = builder
                    .build_with_model(prior, current_model.clone())
                    .await;
                install_subscriptions(
                    &new_harness,
                    &evt_tx,
                    telemetry_sink.clone(),
                    new_writer.clone(),
                )
                .await;
                let prior_count = prior_for_ui.len();
                harness = new_harness;
                session_writer = new_writer;
                let path_display = path.display().to_string();
                let _ = evt_tx.send(TuiEvent::SessionResumed {
                    path: path_display,
                    messages: prior_for_ui,
                });
                let _ = evt_tx.send(TuiEvent::Info(format!(
                    "(resumed: {} — {prior_count} prior message(s))",
                    path.display()
                )));
            }
            Command::Compact { keep_recent } => match harness.compact(keep_recent).await {
                Ok(entry_id) => {
                    let _ = evt_tx
                        .send(TuiEvent::Info(format!("(compacted — entry {entry_id})")));
                }
                Err(e) => {
                    let _ = evt_tx
                        .send(TuiEvent::AgentWorkerError(format!("compact: {e}")));
                }
            },
            Command::ApplyProvider(idx) => {
                let Some(profile) = profiles.get(idx) else {
                    let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                        "ApplyProvider: index {idx} out of range"
                    )));
                    continue;
                };
                if !profile.auth.is_usable() {
                    let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                        "provider '{}' uses OAuth; login flow not yet wired",
                        profile.name
                    )));
                    continue;
                }
                // Same registry-miss-is-fine fallback as the startup
                // path: a profile already carries enough info to call
                // the endpoint; we just need a synthetic descriptor.
                let model = registry
                    .to_core_model(&profile.model)
                    .unwrap_or_else(|| synthetic_model_from_profile(profile));
                let model = override_model_provider(model, profile);
                let cost = model.cost.clone();
                harness.set_model(model.clone()).await;
                // Update the shared handle so ContextGuard and
                // TokenBudgetPolicy see the new model immediately.
                if let Ok(mut w) = active_model_handle.write() {
                    *w = profile.model.clone();
                }
                // Keep `current_model` in sync so a later /resume
                // hands the resumed harness the right model.
                current_model = model;
                let _ = evt_tx.send(TuiEvent::ProviderApplied {
                    profile: profile.name.clone(),
                    model: profile.model.clone(),
                    cost,
                });
            }
            Command::ListModels(provider_name) => {
                // Filter the registry by the provider's vendor
                // family. For native vendors (Anthropic / OpenAI /
                // Gemini) we filter by id prefix; for openai-compat
                // profiles we return the full registry so the user
                // can pick any model — the endpoint decides whether
                // the request is honored.
                let profile = profiles.iter().find(|p| p.name == provider_name);
                let prefix = match profile.map(|p| p.kind) {
                    Some(ProviderKind::Anthropic) => Some("anthropic/"),
                    Some(ProviderKind::OpenAi) => Some("openai/"),
                    Some(ProviderKind::Gemini) => Some("google/"),
                    Some(ProviderKind::OpenAiCompat) | None => None,
                };
                let mut pairs: Vec<(String, String)> = registry
                    .iter()
                    .filter(|(id, _)| prefix.is_none_or(|p| id.starts_with(p)))
                    .map(|(id, desc)| (id.to_string(), desc.name.clone()))
                    .collect();
                pairs.sort_by(|a, b| a.0.cmp(&b.0));
                pairs.dedup_by(|a, b| a.0 == b.0);
                let _ = evt_tx.send(TuiEvent::ModelsListed(pairs));
            }
            Command::SetModel(model_id) => {
                // Resolve via registry; fall back to a clone of the
                // current model with just the id swapped so
                // openai-compat endpoints can drive arbitrary
                // server-side ids (e.g. opencode-zen's "kimi-k2.6"
                // which isn't in models.dev).
                let model = registry.to_core_model(&model_id).unwrap_or_else(|| {
                    let mut m = current_model.clone();
                    m.id = model_id.clone();
                    m.name = model_id.clone();
                    m
                });
                let cost = model.cost.clone();
                harness.set_model(model.clone()).await;
                // Update the shared handle so ContextGuard and
                // TokenBudgetPolicy see the new model immediately.
                if let Ok(mut w) = active_model_handle.write() {
                    *w = model_id.clone();
                }
                current_model = model;
                let _ = evt_tx.send(TuiEvent::ModelApplied {
                    model: model_id,
                    cost,
                });
            }
            Command::Quit => {
                // Make sure any in-flight turn gets cancelled before the
                // task exits, so we don't strand a streaming HTTP req.
                harness.abort().await;
                break;
            }
        }
    }
    // `session_writer` only feeds the subscription closures; we hold a
    // copy here so swaps on `/resume` release the previous file
    // handle when the old Arc count drops to zero.
    let _ = session_writer;
}

/// For OpenAI-compat profiles, replace `Model.provider` with the
/// profile name so genai routes through the per-profile endpoint
/// registered by `with_provider_profiles`. Native-kind profiles pass
/// through unchanged.
/// Build the Rhai engine, register the plugin-manager host
/// primitives, and load every `*.rhai` script under each of
/// `script_dirs`. Returns the agent tools registered by those
/// scripts — each tool owns an `Arc<Engine>` so the caller doesn't
/// have to keep the engine alive separately.
///
/// Called both from `spawn()` at boot and from
/// `Command::ReloadRhaiScripts` so the same code path produces the
/// initial tool list and the hot-reload tool list — the agent never
/// sees inconsistency between "fresh boot" and "after reload".
///
/// Failures emit a `[warn]` and return an empty bundle — one bad
/// script never breaks the rest of the agent.
#[cfg(feature = "scripts-rhai")]
pub struct RhaiBundle {
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub handles: Vec<grain_script_rhai::ScriptHandle>,
}

#[cfg(feature = "scripts-rhai")]
fn build_rhai_bundle(
    workspace_root: &std::path::Path,
    plugins_dir: &std::path::Path,
    script_dirs: &[PathBuf],
) -> RhaiBundle {
    use grain_script_rhai::RhaiExtension;
    let mut engine = RhaiExtension::default_engine();

    // plugins_install(name, src) → status string. Writes to
    // <workspace>/.grain/plugin-lock.toml; refuses to shadow a
    // config.toml [[plugin]] declaration.
    let ws_install = workspace_root.to_path_buf();
    let pdir_install = plugins_dir.to_path_buf();
    engine.register_fn(
        "plugins_install",
        move |name: String, src: String| -> String {
            let config = grain_ai_agent_headless::ConfigFile::load(&ws_install)
                .unwrap_or_default();
            if let Some(grain_ai_agent_headless::PluginOrigin::Config) =
                grain_ai_agent_headless::origin_of(&ws_install, &config, &name)
            {
                return format!(
                    "install '{name}' refused: declared in config.toml — edit that file directly"
                );
            }
            let lock_path = grain_ai_agent_headless::default_lock_path(&ws_install);
            match grain_ai_agent_headless::install(
                &lock_path,
                &pdir_install,
                &name,
                &src,
                None,
            ) {
                Ok(outcome) => {
                    if let Some((_, reason)) =
                        outcome.report.failed.iter().find(|(n, _)| n == &name)
                    {
                        format!("install '{name}' sync failed: {reason}")
                    } else {
                        format!("installed '{name}'")
                    }
                }
                Err(e) => format!("install '{name}' error: {e}"),
            }
        },
    );

    // plugins_list() → Array<Map> of {name, version, description,
    // author, src, rev, origin, root, skills, themes, scripts,
    // prompts}. Called by plugin Rhai scripts that build their
    // own /plugins overlay (e.g. lazy-gagent's ui_plugins_panel).
    let ws_list = workspace_root.to_path_buf();
    let pdir_list = plugins_dir.to_path_buf();
    engine.register_fn("plugins_list", move || -> rhai::Dynamic {
        let config = grain_ai_agent_headless::ConfigFile::load(&ws_list)
            .unwrap_or_default();
        let (spec, _warnings) =
            grain_ai_agent_headless::effective_spec(&ws_list, &config);
        let spec_base = ws_list.join(".grain");
        let discovered = grain_ai_agent_headless::discover_plugins_with_spec(
            &pdir_list,
            &spec,
            &spec_base,
        );
        let mut arr: Vec<rhai::Dynamic> = Vec::with_capacity(discovered.len());
        for p in discovered {
            let info = grain_ai_agent_headless::plugin_info(&p);
            let origin = match grain_ai_agent_headless::origin_of(&ws_list, &config, &info.name) {
                Some(grain_ai_agent_headless::PluginOrigin::Config) => "config",
                Some(grain_ai_agent_headless::PluginOrigin::Lock) => "lock",
                Some(grain_ai_agent_headless::PluginOrigin::LegacySpec) => "legacy",
                None => "manual",
            };
            let entry = spec.plugins.iter().find(|e| e.name == info.name);
            let mut m = rhai::Map::new();
            m.insert("name".into(), rhai::Dynamic::from(info.name.clone()));
            m.insert("version".into(), rhai::Dynamic::from(info.version));
            m.insert("description".into(), rhai::Dynamic::from(info.description));
            m.insert("author".into(), rhai::Dynamic::from(info.author));
            m.insert(
                "src".into(),
                rhai::Dynamic::from(entry.map(|e| e.src.clone()).unwrap_or_default()),
            );
            m.insert(
                "rev".into(),
                rhai::Dynamic::from(
                    entry.and_then(|e| e.rev.clone()).unwrap_or_default(),
                ),
            );
            m.insert("origin".into(), rhai::Dynamic::from(origin.to_string()));
            m.insert(
                "root".into(),
                rhai::Dynamic::from(info.root.display().to_string()),
            );
            m.insert("skills".into(), rhai::Dynamic::from(info.skills as i64));
            m.insert("themes".into(), rhai::Dynamic::from(info.themes as i64));
            m.insert("scripts".into(), rhai::Dynamic::from(info.scripts as i64));
            m.insert("prompts".into(), rhai::Dynamic::from(info.prompts as i64));
            arr.push(rhai::Dynamic::from(m));
        }
        rhai::Dynamic::from(arr)
    });

    // plugins_update(name) → status string. Doesn't touch any
    // spec file — just `git pull`s in <plugins_dir>/<name>/.
    let pdir_update = plugins_dir.to_path_buf();
    engine.register_fn("plugins_update", move |name: String| -> String {
        match grain_ai_agent_headless::update(&pdir_update, &name) {
            Ok(grain_ai_agent_headless::UpdateOutcome::Symlink) => {
                format!("'{name}' is a symlink (live, no pull needed)")
            }
            Ok(grain_ai_agent_headless::UpdateOutcome::Pulled) => {
                format!("updated '{name}' via git pull")
            }
            Err(e) => format!("update '{name}' error: {e}"),
        }
    });

    // plugins_remove(name, delete_files) → status string. Targets
    // the file the entry actually lives in (lock or legacy spec).
    // Refuses to mutate config.toml-declared entries.
    let ws_remove = workspace_root.to_path_buf();
    let pdir_remove = plugins_dir.to_path_buf();
    engine.register_fn(
        "plugins_remove",
        move |name: String, delete_files: bool| -> String {
            let config = grain_ai_agent_headless::ConfigFile::load(&ws_remove)
                .unwrap_or_default();
            let target_path =
                match grain_ai_agent_headless::origin_of(&ws_remove, &config, &name) {
                    Some(grain_ai_agent_headless::PluginOrigin::Config) => {
                        return format!(
                            "remove '{name}' refused: declared in config.toml — edit that file directly"
                        );
                    }
                    Some(grain_ai_agent_headless::PluginOrigin::Lock) => {
                        grain_ai_agent_headless::default_lock_path(&ws_remove)
                    }
                    Some(grain_ai_agent_headless::PluginOrigin::LegacySpec) => {
                        grain_ai_agent_headless::default_spec_path(&ws_remove)
                    }
                    None => {
                        return format!(
                            "remove '{name}' error: not declared in config.toml, plugin-lock.toml, or plugin-spec.toml"
                        );
                    }
                };
            match grain_ai_agent_headless::remove(
                &target_path,
                &pdir_remove,
                &name,
                delete_files,
            ) {
                Ok(outcome) => {
                    let suffix = if outcome.files_removed { " + files" } else { "" };
                    format!("removed '{name}'{suffix}")
                }
                Err(e) => format!("remove '{name}' error: {e}"),
            }
        },
    );

    match RhaiExtension::from_scripts_dirs_with_engine(engine, script_dirs) {
        Ok(ext) => RhaiBundle {
            tools: ext.tools(),
            handles: ext.script_handles(),
        },
        Err(e) => {
            eprintln!("[warn] rhai scripts: {e}");
            RhaiBundle {
                tools: Vec::new(),
                handles: Vec::new(),
            }
        }
    }
}

fn override_model_provider(
    mut model: grain_agent_core::Model,
    profile: &ProviderProfile,
) -> grain_agent_core::Model {
    if matches!(profile.kind, ProviderKind::OpenAiCompat) {
        model.provider = profile.name.clone();
    }
    model
}

/// Build a synthetic [`Model`] for a profile whose `model` id isn't in
/// the embedded `models.dev` registry. The profile alone carries enough
/// info (provider kind + base_url + model id) to drive `grain-llm-genai`;
/// the synthetic descriptor fills in the registry-supplied fields with
/// conservative defaults:
/// - `context_window = 32_768` (most local models advertise ≥ 8k; 32k
///   is a sweet spot — pass `--headroom-tokens` for smaller windows).
/// - `max_tokens = 4_096`.
/// - `cost = zero` (suppresses the footer cost chip — providers without
///   public pricing shouldn't lie).
/// - `reasoning = false` (model-specific; the agent will still send
///   reasoning hints if the user passes `--show-thinking`).
fn synthetic_model_from_profile(profile: &ProviderProfile) -> grain_agent_core::Model {
    let api = match profile.kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenAi | ProviderKind::OpenAiCompat => "openai",
        ProviderKind::Gemini => "gemini",
    }
    .to_string();
    // For OpenAI-compat profiles, prefix the model id with the
    // profile name. Genai's service-target resolver routes
    // `<profile_name>::<rest>` through the matching
    // `OpenAiCompatEndpoint` (the one we registered from the profile's
    // base_url). Without this prefix, genai falls back to its own
    // namespace heuristic and picks the wrong adapter — often Ollama —
    // which causes the obvious "502 Bad Gateway" against the local
    // LM Studio / vLLM server.
    //
    // Native kinds (Anthropic / OpenAI / Gemini) leave the id alone;
    // there auth flows via the env-var override on the resolver, and
    // the native adapter takes the model name verbatim.
    let id = match profile.kind {
        ProviderKind::OpenAiCompat => {
            // Avoid double-namespacing if the user already wrote
            // `model = "lmstudio/..."` in providers.toml.
            let stripped = profile
                .model
                .strip_prefix(&format!("{}/", profile.name))
                .unwrap_or(&profile.model);
            format!("{}/{}", profile.name, stripped)
        }
        _ => profile.model.clone(),
    };
    grain_agent_core::Model {
        id,
        name: profile.model.clone(),
        api,
        provider: profile.name.clone(),
        base_url: profile.base_url.clone().unwrap_or_default(),
        reasoning: false,
        context_window: 32_768,
        max_tokens: 4_096,
        cost: grain_agent_core::Cost::default(),
    }
}
