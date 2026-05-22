//! Tokio task that owns the [`grain_agent_core::Agent`] and acts as the
//! bridge between UI [`Command`]s and [`TuiEvent`]s.
//!
//! Construction mirrors `grain-ai-agent-headless::cli::run`: build a
//! Workspace, Registry, GenaiStream, tools per CLI flags, install the
//! context guard, subscribe a telemetry/session writer if requested,
//! then loop on commands.
//!
//! This module owns the only `Agent`. The UI thread can only address it
//! through the [`mpsc`] channels returned by [`spawn`].

use std::path::PathBuf;
use std::sync::Arc;

use grain_agent_core::{Agent, AgentEvent, AgentMessage, AgentOptions, Message};
use grain_agent_harness::context_guard::{ContextGuard, ContextGuardPolicy};
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

/// Spawn the agent worker. Returns a [`Worker`] bundle on success.
pub fn spawn(mut cfg: WorkerConfig) -> Result<Worker, WorkerInitError> {
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
    // Resolve the sessions directory once — used for both
    // auto-create on startup and for `/resume`'s `list_sessions` scan
    // in the command loop.
    let sessions_dir = cfg
        .sessions_dir
        .clone()
        .unwrap_or_else(|| workspace.root().join(".grain").join("sessions"));
    // When `--session` isn't given, mint a fresh `<uuidv7>.jsonl`
    // inside `sessions_dir` so every run leaves a recoverable
    // transcript that `/resume` can later find. Failure to create the
    // dir downgrades to "no session writer" rather than aborting
    // startup.
    if cfg.session.is_none() {
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
    let prior_messages = match &cfg.session {
        Some(path) => load_messages(path).map_err(|e| WorkerInitError::Session {
            path: path.clone(),
            source: Box::new(e),
        })?,
        None => Vec::new(),
    };

    // --- Stream ------------------------------------------------------------
    // Profile endpoint/env routing is now a first-class `grain-llm-genai`
    // capability (`with_provider_profiles`) — runtime
    // `Command::ApplyProvider` only has to swap the active model since
    // the stream already knows how to reach every profile's endpoint.
    let stream = Arc::new(
        GenaiStream::builder()
            .with_openai_compat_preset(cfg.openai_compat)
            .with_provider_profiles(&cfg.profiles)
            .with_bypass_proxy(cfg.bypass_proxy)
            .with_registry(registry.clone())
            .build(),
    );

    // --- Tools -------------------------------------------------------------
    let mut tools = coding_read_tools(workspace.clone());
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
    // Default path: `<workspace>/.grain/scripts/`. Missing directory
    // is fine — `BoaExtension::from_scripts_dir` returns an empty
    // extension. We hold the extension in the closure that gets
    // moved into the worker task; dropping it stops the worker
    // thread cleanly when the agent shuts down.
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
    // Use the resolved active model id (which may have come from a
    // profile) so the token budget matches the model the agent will
    // actually call.
    let guard = ContextGuard::new(registry.clone(), active_model_id.clone())
        .with_policy(ContextGuardPolicy::DropOldest)
        .with_headroom_tokens(cfg.headroom_tokens)
        .into_transform_fn();

    // --- Channels ----------------------------------------------------------
    // Created before AgentOptions so the optional `--debug-log`
    // `convert_to_llm` wrapper can capture `evt_tx` for the `/log`
    // overlay forward.
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<TuiEvent>();

    // --- AgentOptions ------------------------------------------------------
    // Snapshot the pricing table before `model` moves into AgentOptions —
    // the footer renders a live cost chip from this.
    let model_cost = model.cost.clone();
    let mut opts = AgentOptions::new(model, stream);
    opts.system_prompt = system_prompt;
    opts.tools = tools;
    opts.messages = prior_messages;
    opts.transform_context = Some(guard);
    if cfg.debug_log {
        // Wrap the default projection (Standard kept / Custom dropped)
        // so each turn snapshot also lands in the UI's `/log` overlay.
        // Cheap: a serialize + a non-blocking channel send per turn.
        let evt_tx_for_log = evt_tx.clone();
        // Captured at startup — model / provider runtime switches
        // (`/provider`) won't update these. Good enough for the
        // common debug case; a header line prefixes each entry.
        let log_model_id = active_model_id.clone();
        let log_endpoint = match cfg
            .initial_profile_idx
            .and_then(|idx| cfg.profiles.get(idx))
        {
            Some(p) => p.base_url.clone().unwrap_or_else(|| {
                format!("(profile '{}', native adapter)", p.name)
            }),
            None => "(native adapter; genai default endpoint)".to_string(),
        };
        opts.convert_to_llm = Some(Arc::new(move |messages: Vec<AgentMessage>| {
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
                let body_json =
                    serde_json::to_string_pretty(&projected).unwrap_or_else(|e| {
                        format!("(serialize failed: {e})")
                    });
                let body = format!(
                    "POST {log_endpoint}/chat/completions\nmodel: {log_model_id}\n\n{body_json}"
                );
                let _ = evt_tx_for_log.send(TuiEvent::RequestLogged { body });
                projected
            })
        }));
    }
    // Storm suppressor: blocks the 3rd identical (tool, args) call
    // within a 60s window and feeds a reflection note back to the
    // model. Provider-agnostic — defaults are tuned for coding
    // agents that occasionally lock onto a useless grep / search.
    opts.before_tool_call = Some(grain_agent_harness::storm_hook(
        grain_agent_harness::StormConfig::default(),
    ));
    // Auto-escalation: when configured, swap to `escalate_to` after
    // `escalate_after` cumulative failure signals. Missing target
    // model just disables the hook (logged once at startup).
    if let Some(target_id) = &cfg.escalate_to {
        match registry.to_core_model(target_id) {
            Some(target) => {
                eprintln!(
                    "[info] escalation armed: → {} after {} failure(s)",
                    target.id, cfg.escalate_after
                );
                opts.prepare_next_turn = Some(grain_agent_harness::failure_escalation_hook(
                    grain_agent_harness::EscalationConfig::new(cfg.escalate_after, target),
                ));
            }
            None => {
                eprintln!(
                    "[warn] --escalate-to '{target_id}' not in registry; \
                     escalation disabled"
                );
            }
        }
    }
    let agent = Arc::new(Agent::new(opts));

    let handles = WorkerHandles {
        model_id: active_model_id.clone(),
        workspace_display: workspace.root().display().to_string(),
        allow_write: cfg.allow_write,
        allow_bash: cfg.allow_bash,
        allow_web: cfg.allow_web,
        allow_semantic_search: cfg.allow_semantic_search,
        model_cost: model_cost.clone(),
    };

    // --- Per-instance subscriptions: telemetry + session ------------------
    // These have to happen on the worker task because `subscribe` is async.
    let telemetry_path = cfg.telemetry_file.clone();
    let session_path = cfg.session.clone();
    let profiles = cfg.profiles.clone();
    let agent_for_task = agent.clone();
    let workspace_for_task = workspace.clone();
    let registry_for_task = registry.clone();
    let skills_dir_for_task = skills_dir.clone();
    let sessions_dir_for_task = sessions_dir.clone();
    let skills_for_ui = skills_for_ui.clone();
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
        // Fan AgentEvents into TuiEvents.
        let fan_tx = evt_tx_for_task.clone();
        agent_for_task
            .subscribe(Arc::new(move |event, _signal| {
                let tx = fan_tx.clone();
                Box::pin(async move {
                    let _ = tx.send(TuiEvent::Agent(event));
                })
            }))
            .await;

        // Optional telemetry sink.
        if let Some(path) = telemetry_path {
            match TelemetrySink::open(&path) {
                Ok(sink) => {
                    let sink = Arc::new(sink);
                    agent_for_task
                        .subscribe(Arc::new(move |event, _signal| {
                            let s = sink.clone();
                            Box::pin(async move {
                                s.record(&event);
                            })
                        }))
                        .await;
                }
                Err(e) => {
                    let _ = evt_tx_for_task.send(TuiEvent::AgentWorkerError(format!(
                        "telemetry open failed: {e}"
                    )));
                }
            }
        }

        // Optional session writer.
        if let Some(path) = session_path {
            match SessionWriter::open(&path) {
                Ok(writer) => {
                    let writer = Arc::new(writer);
                    agent_for_task
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
                Err(e) => {
                    let _ = evt_tx_for_task.send(TuiEvent::AgentWorkerError(format!(
                        "session writer open failed: {e}"
                    )));
                }
            }
        }

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
            agent_for_task,
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
    agent: Arc<Agent>,
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
                // until completion; AgentEvent forwarding already wired.
                let agent = agent.clone();
                let evt_tx = evt_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = agent.prompt_text(text).await {
                        let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!("prompt: {e}")));
                    }
                });
            }
            Command::AbortCurrentTurn => {
                agent.abort().await;
            }
            Command::Reset => {
                agent.reset().await;
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
                agent.set_model(model).await;
                let _ = evt_tx.send(TuiEvent::ProviderApplied {
                    profile: profile.name.clone(),
                    model: profile.model.clone(),
                    cost,
                });
            }
            Command::Quit => {
                // Make sure any in-flight turn gets cancelled before the
                // task exits, so we don't strand a streaming HTTP req.
                agent.abort().await;
                break;
            }
        }
    }
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
