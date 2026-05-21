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

use grain_agent_core::{Agent, AgentEvent, AgentOptions};
use grain_agent_harness::context_guard::{ContextGuard, ContextGuardPolicy};
use grain_ai_agent_headless::{
    SessionWriter, TelemetrySink, Workspace, coding_bash_tools, coding_read_tools,
    coding_web_tools, coding_write_tools, coding_agent_system_prompt, find_skills,
    load_messages, render_doctor_report, resolve_skills_dir,
};
use grain_llm_genai::GenaiStream;
use grain_llm_models::Registry;
use tokio::sync::mpsc;

use crate::app::Command;
use crate::cli::Args;
use crate::event::TuiEvent;

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
}

/// Errors that can happen *before* the worker successfully takes over.
/// Once it's running, errors are reported via [`TuiEvent::AgentWorkerError`].
#[derive(Debug, thiserror::Error)]
pub enum WorkerInitError {
    #[error("workspace: {0}")]
    Workspace(String),
    #[error("model '{0}' not found in embedded models.dev snapshot")]
    UnknownModel(String),
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
pub fn spawn(cfg: WorkerConfig) -> Result<Worker, WorkerInitError> {
    // --- Workspace + registry ---------------------------------------------
    let workspace = Arc::new(
        Workspace::new(&cfg.workspace_root).map_err(|e| WorkerInitError::Workspace(e.to_string()))?,
    );
    let registry = Arc::new(Registry::from_embedded_snapshot());
    let model = registry
        .to_core_model(&cfg.model)
        .ok_or_else(|| WorkerInitError::UnknownModel(cfg.model.clone()))?;

    // --- System prompt + skills block -------------------------------------
    let mut system_prompt = match &cfg.system_prompt_file {
        Some(path) => std::fs::read_to_string(path).map_err(|e| WorkerInitError::SystemPrompt {
            path: path.clone(),
            source: e,
        })?,
        None => coding_agent_system_prompt(cfg.allow_write, cfg.allow_bash).to_string(),
    };
    let skills_dir = resolve_skills_dir(workspace.root(), cfg.skills_dir.as_deref());
    if let Ok(skills) = find_skills(&skills_dir) {
        let block = grain_agent_harness::format_skills_for_system_prompt(&skills);
        if !block.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&block);
        }
    }

    // --- Session restore ---------------------------------------------------
    let prior_messages = match &cfg.session {
        Some(path) => load_messages(path).map_err(|e| WorkerInitError::Session {
            path: path.clone(),
            source: Box::new(e),
        })?,
        None => Vec::new(),
    };

    // --- Stream ------------------------------------------------------------
    let stream = Arc::new(
        GenaiStream::builder()
            .with_openai_compat_preset(cfg.openai_compat)
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

    // --- Context guard -----------------------------------------------------
    let guard = ContextGuard::new(registry.clone(), cfg.model.clone())
        .with_policy(ContextGuardPolicy::DropOldest)
        .with_headroom_tokens(cfg.headroom_tokens)
        .into_transform_fn();

    // --- AgentOptions ------------------------------------------------------
    let mut opts = AgentOptions::new(model, stream);
    opts.system_prompt = system_prompt;
    opts.tools = tools;
    opts.messages = prior_messages;
    opts.transform_context = Some(guard);
    let agent = Arc::new(Agent::new(opts));

    // --- Channels ----------------------------------------------------------
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<TuiEvent>();

    let handles = WorkerHandles {
        model_id: cfg.model.clone(),
        workspace_display: workspace.root().display().to_string(),
        allow_write: cfg.allow_write,
        allow_bash: cfg.allow_bash,
        allow_web: cfg.allow_web,
        allow_semantic_search: cfg.allow_semantic_search,
    };

    // --- Per-instance subscriptions: telemetry + session ------------------
    // These have to happen on the worker task because `subscribe` is async.
    let telemetry_path = cfg.telemetry_file.clone();
    let session_path = cfg.session.clone();
    let agent_for_task = agent.clone();
    let workspace_for_task = workspace.clone();
    let registry_for_task = registry.clone();
    let skills_dir_for_task = skills_dir.clone();
    let evt_tx_for_task = evt_tx.clone();

    let join = tokio::spawn(async move {
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

        run_command_loop(
            agent_for_task,
            workspace_for_task,
            registry_for_task,
            skills_dir_for_task,
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

async fn run_command_loop(
    agent: Arc<Agent>,
    workspace: Arc<Workspace>,
    registry: Arc<Registry>,
    skills_dir: PathBuf,
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
                        let _ =
                            evt_tx.send(TuiEvent::AgentWorkerError(format!("prompt: {e}")));
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
                    let _ = evt_tx.send(TuiEvent::AgentWorkerError(format!(
                        "skills scan: {e}"
                    )));
                }
            },
            Command::Quit => {
                // Make sure any in-flight turn gets cancelled before the
                // task exits, so we don't strand a streaming HTTP req.
                agent.abort().await;
                break;
            }
        }
    }
}
