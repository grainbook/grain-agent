//! Cross-thread event envelope. Everything the UI reacts to — a key
//! press, a tick, a terminal resize, an `AgentEvent` from the worker —
//! lands here so the main loop has a single point of dispatch into
//! [`crate::AppState::on_event`].

use crossterm::event::KeyEvent;
use grain_agent_core::AgentEvent;

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
    /// Worker hit a fatal-ish error (e.g. agent ended with `error_message`,
    /// or a slash command sub-call failed). Already user-facing.
    AgentWorkerError(String),
    /// Worker successfully switched to a new provider profile. Carries
    /// the profile name + resolved model id so the UI can log a status
    /// line ("(provider: openai-work · openai/gpt-4o)").
    ProviderApplied { profile: String, model: String },
}
