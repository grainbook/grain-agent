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
    AgentEvent, AgentMessage, AgentTool, BeforeToolCallFn, ConvertToLlmFn, Message, Model,
    PrepareNextTurnFn, StreamFn, TransformContextFn,
};
use grain_agent_harness::{
    AgentHarness, AgentHarnessOptions, InMemorySessionStorage, Session, SessionMetadata,
    SystemPrompt,
    context_guard::{ContextGuard, ContextGuardPolicy},
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
        let session = Session::new(Arc::new(InMemorySessionStorage::new(
            SessionMetadata::new(),
        )));
        for msg in prior_messages {
            if let Err(e) = session.append_message(msg).await {
                eprintln!("[warn] seed session message failed: {e}");
            }
        }
        let mut opts = AgentHarnessOptions::new(
            session,
            self.model.clone(),
            self.stream.clone(),
        );
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

    // --- System prompt + skills block -------------------------------------
    // Pin the prompt for the lifetime of this session. The harness's
    // `PinnedSystemPrompt` freezes `base + <available_skills>` at
    // session start; never re-render in the hot path so the upstream
    // prefix cache (Anthropic, OpenAI, DeepSeek …) stays warm.
    let base_prompt = match &cfg.system_prompt_file {
        Some(path) => std::fs::read_to_string(path).map_err(|e| WorkerInitError::SystemPrompt {
            path: path.clone(),
            source: e,
        })?,
        None => coding_agent_system_prompt(cfg.allow_write, cfg.allow_bash).to_string(),
    };
    let skills_dir = resolve_skills_dir(workspace.root(), cfg.skills_dir.as_deref());
    let skills = find_skills(&skills_dir).unwrap_or_default();
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
    let scripts_path = cfg
        .scripts_dir
        .clone()
        .unwrap_or_else(|| workspace.root().join(".grain").join("scripts"));
    #[cfg(feature = "scripts-boa")]
    let scripts_extension = match grain_script_boa::BoaExtension::from_scripts_dir(&scripts_path) {
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
    };
    #[cfg(not(feature = "scripts-boa"))]
    {
        if cfg.scripts_dir.is_some() || scripts_path.exists() {
            eprintln!(
                "[warn] --scripts-dir / .grain/scripts/ present at {} but binary was \
                 built without --features scripts-boa; ignoring",
                scripts_path.display()
            );
        }
    }

    // --- Context guard -----------------------------------------------------
    let guard = ContextGuard::new(registry.clone(), active_model_id.clone())
        .with_policy(ContextGuardPolicy::DropOldest)
        .with_headroom_tokens(cfg.headroom_tokens)
        .into_transform_fn();

    // --- Channels ----------------------------------------------------------
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<TuiEvent>();

    // --- Hooks: storm suppressor + optional escalation ---------------------
    let before_tool_call: Option<BeforeToolCallFn> = Some(grain_agent_harness::storm_hook(
        grain_agent_harness::StormConfig::default(),
    ));
    let prepare_next_turn: Option<PrepareNextTurnFn> = match &cfg.escalate_to {
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
        Some(Arc::new(move |messages: Vec<AgentMessage>| {
            let evt_tx_for_log = evt_tx_for_log.clone();
            let log_model_id = log_model_id.clone();
            let log_endpoint = log_endpoint.clone();
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
                let _ = evt_tx_for_log.send(TuiEvent::RequestLogged { body });
                projected
            })
        }))
    } else {
        None
    };

    // --- HarnessBuilder + initial harness ----------------------------------
    let model_cost = model.cost.clone();
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
    let evt_tx_for_task = evt_tx.clone();
    let model_cost_for_task = model_cost.clone();
    // Captured by the worker task closure so the Boa worker stays
    // alive for the whole agent lifetime; dropping at task end sends
    // Shutdown to that worker thread.
    #[cfg(feature = "scripts-boa")]
    let _scripts_keepalive = scripts_extension;

    let join = tokio::spawn(async move {
        // Pin the Boa extension into the task scope so its worker
        // thread lives until the agent task exits.
        #[cfg(feature = "scripts-boa")]
        let _boa_keepalive = _scripts_keepalive;

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
            workspace_for_task,
            registry_for_task,
            skills_dir_for_task,
            sessions_dir_for_task,
            profiles,
            cmd_rx,
            evt_tx_for_task,
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

#[allow(clippy::too_many_arguments)]
async fn run_command_loop(
    mut harness: Arc<AgentHarness>,
    builder: Arc<HarnessBuilder>,
    telemetry_sink: Option<Arc<TelemetrySink>>,
    mut session_writer: Option<Arc<SessionWriter>>,
    workspace: Arc<Workspace>,
    registry: Arc<Registry>,
    skills_dir: PathBuf,
    sessions_dir: PathBuf,
    profiles: Vec<ProviderProfile>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    evt_tx: mpsc::UnboundedSender<TuiEvent>,
) {
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
                let new_harness = builder.build(prior).await;
                install_subscriptions(
                    &new_harness,
                    &evt_tx,
                    telemetry_sink.clone(),
                    new_writer.clone(),
                )
                .await;
                let prior_count = new_harness.agent().state().await.messages.len();
                harness = new_harness;
                session_writer = new_writer;
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
                harness.set_model(model).await;
                let _ = evt_tx.send(TuiEvent::ProviderApplied {
                    profile: profile.name.clone(),
                    model: profile.model.clone(),
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
