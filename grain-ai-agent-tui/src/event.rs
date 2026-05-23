//! Cross-thread event envelope. Everything the UI reacts to — a key
//! press, a tick, a terminal resize, an `AgentEvent` from the worker —
//! lands here so the main loop has a single point of dispatch into
//! [`crate::AppState::on_event`].

use crossterm::event::KeyEvent;
use grain_agent_core::{AgentEvent, Cost};

/// Tagged union of every event the TUI main loop knows how to consume.
///
/// `AgentEvent`'s `MessageUpdate` carries a full streaming partial
/// (~470 bytes). Boxing it keeps `TuiEvent` compact for the common
/// case (a `Key` event at 16 bytes) while still avoiding a separate
/// allocation per agent event — each `AgentEvent` already involves
/// heap allocations for message content.
#[derive(Debug, Clone)]
pub enum TuiEvent {
    /// One key press from the terminal (release events are filtered out
    /// in `on_key`).
    Key(KeyEvent),
    /// Periodic timer fired by the run loop. Drives any time-based
    /// rendering (currently a no-op; kept so future spinners are cheap
    /// to add).
    Tick,
    /// Terminal resized — passed through so anyone caching layout state
    /// can invalidate it.
    Resize(u16, u16),
    /// One [`AgentEvent`] received from the running agent worker.
    Agent(Box<AgentEvent>),
    /// Worker computed a doctor report on a background thread. Carries
    /// the already-rendered string so the UI doesn't need access to the
    /// `Workspace` / `Registry`.
    OverlayDoctor(String),
    /// Worker resolved the skills list. Tuples are
    /// `(name, description, disable_model_invocation)`.
    OverlaySkills(Vec<(String, String, bool)>),
    /// Worker loaded skills at startup (includes body content for
    /// slash-palette injection).
    SkillsLoaded(Vec<grain_agent_harness::Skill>),
    /// Worker hit a fatal-ish error (e.g. agent ended with `error_message`,
    /// or a slash command sub-call failed). Already user-facing.
    AgentWorkerError(String),
    /// Worker successfully switched to a new provider profile. Carries
    /// the profile name + resolved model id so the UI can log a status
    /// line ("(provider: openai-work · openai/gpt-4o)") plus the new
    /// model's pricing table so the cost chip refreshes too.
    ProviderApplied {
        profile: String,
        model: String,
        cost: Cost,
    },
    /// Worker successfully switched to a new model via `/model`. Carries
    /// the model id + pricing so the UI can refresh the status line
    /// ("(model: deepseek/deepseek-v4-pro)") and cost chip.
    ModelApplied {
        model: String,
        cost: Cost,
    },
    /// Mouse wheel rolled up — translated into transcript scroll-up by
    /// `amount` rows. Same follow-bottom semantics as PgUp.
    ScrollUp { amount: u16 },
    /// Mouse wheel rolled down — translated into transcript scroll-down
    /// by `amount` rows. Same catch-up-to-tail semantics as PgDn.
    ScrollDown { amount: u16 },
    /// Left mouse button pressed at absolute terminal cell `(row, col)`.
    /// AppState translates into the transcript area's rendered-row
    /// coordinate space and starts a selection.
    MouseDown { row: u16, col: u16 },
    /// Left mouse button dragged to `(row, col)` — extend the in-flight
    /// selection.
    MouseDrag { row: u16, col: u16 },
    /// Left mouse button released — finalize the selection: extract
    /// text from the rendered rows under the selection rectangle and
    /// write it to the OS clipboard.
    MouseUp,
    /// Captured "request body" (pretty-printed JSON of the projected
    /// LLM messages) emitted on every turn when `--debug-log` is on.
    /// Pushed into [`crate::AppState::request_log`] and viewable via
    /// the `/log` overlay.
    RequestLogged { body: String },
    /// Worker scanned `sessions_dir` and returns the discovered
    /// session list (newest first). Fills the `/resume` picker.
    SessionsListed(Vec<grain_ai_agent_headless::SessionMeta>),
    /// Worker completed a `/resume` in-place session swap. Carries the
    /// full set of prior messages so the UI can clear the current
    /// transcript and repopulate with the loaded history.
    SessionResumed { path: String, messages: Vec<grain_agent_core::AgentMessage> },
    /// Worker returned the list of models for the current provider
    /// (id + display name pairs). Fills the `/model` picker.
    ModelsListed(Vec<(String, String)>),
    /// Worker pushed an informational status line. Rendered as a
    /// `TranscriptKind::Info` row. Used for `/resume` swap
    /// confirmations and `/compact` summaries.
    Info(String),
    /// Worker pushed an ephemeral status line — rendered as a
    /// **single-row floating slot** above the input box, replacing
    /// the previous status rather than appending to the transcript.
    /// Used by `retry-on-overflow` so the user sees retry progress
    /// without N rows of stderr corrupting the alt screen.
    Status(String),
    /// Worker scanned `plugins_dir` and returns the discovered
    /// `lazy.gagent` plugin set. Populates the `/plugins` overlay
    /// with both the plugin list and any plugin-contributed footer
    /// hint commands (`[[ui_command]]`).
    PluginsListed {
        plugins: Vec<grain_ai_agent_headless::PluginInfo>,
        ui_commands: Vec<grain_ai_agent_headless::BoundUiCommand>,
    },
    /// Worker successfully dispatched a plugin UI handler and got
    /// back an [`OverlayDescriptor`]. The TUI pushes the
    /// corresponding [`crate::app::Overlay`] variant (Form / Modal
    /// / Confirm) onto the overlay stack.
    UiOverlay(grain_ai_agent_headless::OverlayDescriptor),
    /// Plugin UI handler dispatch failed (missing handler, Rhai
    /// runtime error, malformed return value). Carries a
    /// pre-formatted user-facing message.
    UiHandlerError(String),
    /// Worker computed the set of plugin-contributed slash command
    /// overrides at boot (and re-emits on `Command::ReloadRhaiScripts`).
    /// The TUI stashes them in `AppState.plugin_slashes` and consults
    /// them before the built-in slash table.
    SlashCommandsRegistered(Vec<grain_ai_agent_headless::BoundPluginSlashCommand>),
}
