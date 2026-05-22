//! `lazy.gagent` — Neovim/lazy.nvim-style plugin manager.
//!
//! # Architecture
//!
//! The **plugin system** (manifest format, discovery, integration of
//! skills / themes / system-prompt fragments / scripts into the agent
//! boot path) lives in [`grain_ai_agent_headless::plugins`]. That
//! module is the "engine" — analogous to Neovim itself.
//!
//! This crate, `lazy-gagent`, is the higher-level *plugin manager*
//! that runs **on top of** the headless plugin system — analogous to
//! lazy.nvim running on top of Neovim. The headless engine has zero
//! knowledge of lazy.nvim, and headless has zero knowledge of
//! lazy-gagent: it's just another `.grain/plugins/<name>/` directory
//! that ships skills + scripts implementing install / update / remove.
//!
//! Phase A and B (today) intentionally leave this crate empty — the
//! plugin engine is enough to load hand-curated `.grain/plugins/`
//! directories. Phase C will fill in:
//!
//! - `lazy_gagent::install(spec)` — clone a plugin from git into
//!   `.grain/plugins/<name>/`.
//! - `lazy_gagent::update(name)` — git pull on an installed plugin.
//! - `lazy_gagent::remove(name)` — rm -rf an installed plugin.
//! - Spec file format (`<workspace>/.grain/plugin-spec.toml`) so users
//!   can declare desired plugins the way they declare dependencies in
//!   `Cargo.toml`.
//!
//! For now this crate just re-exports the headless plugin types so
//! that any future code in this crate sees the same shape as the rest
//! of the project, without consumers having to remember which crate
//! defines what.

pub use grain_ai_agent_headless::plugins::{
    Plugin, PluginInfo, PluginManifest, PromptFragment, compose_system_prompt_with_plugins,
    default_plugins_dir, discover_plugins, find_skills_with_plugins, parse_manifest, plugin_info,
    plugin_script_dirs, read_plugin_prompt_fragments, summarize_plugin,
};
