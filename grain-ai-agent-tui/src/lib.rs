//! `grain-ai-agent-tui` — ratatui-based terminal UI on top of
//! `grain-ai-agent-headless`. Same agent capabilities (file tools,
//! shell, web fetch, semantic search, session persistence, skills,
//! slash commands) wrapped in a multi-pane terminal interface.
//!
//! Architecture:
//!
//! - [`app::AppState`] is the pure UI state — what to display, what's
//!   focused, what's in the input line. No I/O, fully unit-testable.
//! - [`event::TuiEvent`] enumerates everything the main loop can react
//!   to: a key press, a terminal resize, an [`grain_agent_core::AgentEvent`]
//!   from the running Agent, a periodic tick.
//! - [`agent_worker`] owns the actual `Agent` on a dedicated tokio task
//!   and shuttles events to / commands from the UI via `mpsc` channels.
//! - [`ui`] renders [`AppState`] into a ratatui `Frame`.
//! - [`run::run_tui`] ties the terminal lifecycle, event polling, and
//!   render loop together. `src/bin/grain_tui.rs` is a tiny entry point
//!   that calls into it.

pub mod agent_worker;
pub mod app;
pub mod cli;
pub mod config_apply;
pub mod event;
pub mod persist;
pub mod run;
pub mod theme;
pub mod ui;

pub use app::{AppState, Focus, Overlay, TranscriptKind, TranscriptLine};
pub use cli::Args;
pub use event::TuiEvent;
// Provider profile types live in `grain-llm-genai` — the natural home,
// since that's where the genai service-target resolver they plug into
// also lives. Re-exported here for convenience to TUI callers.
pub use grain_llm_genai::{
    ProviderAuth, ProviderKind, ProviderProfile, load_profiles, resolve_providers_file,
};
pub use run::{TuiError, run_tui};
pub use theme::{Palette, Theme, ThemeSource, builtin_themes, load_user_themes};
