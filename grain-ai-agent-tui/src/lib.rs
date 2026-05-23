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

// Lint philosophy: be strict about correctness, warn on common mistakes,
// and let pedantic lints be opt-in so they don't turn into noise.
#![deny(clippy::correctness)]
#![warn(clippy::suspicious)]
#![warn(clippy::style)]
#![warn(clippy::complexity)]
#![warn(clippy::perf)]
#![warn(clippy::undocumented_unsafe_blocks)]
// missing_docs is noisy for a binary-first crate with many internal
// modules; enable selectively on the public API surface once that
// surface stabilises.
// #![warn(missing_docs)]
pub mod agent_worker;
pub mod anim;
pub mod app;
pub mod cli;
pub mod config_apply;
pub mod event;
pub mod md_render;
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
