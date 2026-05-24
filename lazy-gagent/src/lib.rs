//! `lazy.gagent` — placeholder crate.
//!
//! # Architecture
//!
//! The **plugin system** (discovery + integration into the agent
//! boot path) and the **plugin-manager primitives** (install /
//! update / remove) both live in [`grain_ai_agent_headless`]. This
//! crate is just a brand-name re-export shim so callers who think of
//! "lazy-gagent" as a thing have a Cargo import that resolves.
//!
//! The *actual* lazy-gagent UX is delivered as a plugin under
//! `<workspace>/.grain/plugins/lazy-gagent/` — Rhai/JS scripts that
//! expose `install`/`update`/`remove` as agent-callable tools by
//! wrapping the engine's primitives. No Cargo-level coupling between
//! the TUI and "the plugin manager" — exactly the Neovim model.

pub use grain_ai_agent_headless::plugin_manager::{
    InstallOutcome, ManagerError, RemoveOutcome, UpdateOutcome, install, remove, update,
};
pub use grain_ai_agent_headless::plugin_spec::{
    PluginSpec, PluginSpecFile, SourceKind, SyncReport, default_spec_path, detect_source_kind,
    load_plugin_spec, sync_plugins,
};
pub use grain_ai_agent_headless::plugin_ui::{
    BoundUiCommand, FormField, ModalSeverity, OverlayDescriptor, UiCommand, collect_ui_commands,
};
pub use grain_ai_agent_headless::plugins::{
    Plugin, PluginInfo, PluginManifest, PromptFragment, compose_system_prompt_with_plugins,
    default_plugins_dir, discover_plugins, discover_plugins_with_spec, find_skills_with_plugins,
    parse_manifest, plugin_info, plugin_script_dirs, read_plugin_prompt_fragments,
    summarize_plugin,
};
