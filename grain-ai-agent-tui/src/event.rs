//! Cross-thread event envelope. Everything the UI reacts to — a key
//! press, a tick, a terminal resize, an `AgentEvent` from the worker —
//! lands here so the main loop has a single point of dispatch into
//! [`crate::AppState::on_event`].

use crossterm::event::KeyEvent;
use grain_agent_core::{AgentEvent, Cost};

/// Tagged union of every event the TUI main loop knows how to consume.
///
/// `AgentEvent`'s `MessageUpdate` carries a full streaming partial,
/// which makes the union ~470 bytes — the same size trade-off
/// `grain_agent_core::AgentEvent` itself takes. We cross-pollinate the
/// `#[allow]` so the TUI doesn't pay a `Box` allocation per key press.
#[allow(clippy::large_enum_variant)]
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
    Agent(AgentEvent),
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
    /// Worker pushed an informational status line. Rendered as a
    /// `TranscriptKind::Info` row. Used for `/resume` swap
    /// confirmations and `/compact` summaries.
    Info(String),
    /// Worker scanned `plugins_dir` and returns the discovered
    /// `lazy.gagent` plugin set. Populates the `/plugins` overlay.
    PluginsListed(Vec<grain_ai_agent_headless::PluginInfo>),
}
