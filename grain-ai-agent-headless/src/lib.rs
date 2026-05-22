//! `grain-ai-agent-headless` — building blocks for a headless coding-agent.
//!
//! Plays the role of `@earendil-works/pi/packages/coding-agent` but with no UI
//! layer and no SDK surface — what we ship here are reusable pieces other
//! binaries can compose.
//!
//! ## v1 surface
//!
//! - [`Workspace`] — root-anchored path validator. Every file tool resolves
//!   user-supplied paths through it and refuses anything that escapes the
//!   workspace root.
//! - [`tools`] — read-only filesystem tools that implement
//!   [`grain_agent_core::AgentTool`]:
//!   - [`ReadTool`] — read a UTF-8 text file with optional line-range trim.
//!   - [`ListTool`] — list a directory's immediate entries.
//!   - [`GlobTool`] — gitignore-aware glob search.
//!   - [`GrepTool`] — gitignore-aware regex search across files.
//! - [`runtime::coding_read_tools`] — convenience that returns all four
//!   tools as `Vec<Arc<dyn AgentTool>>` ready to drop into
//!   [`grain_agent_core::AgentOptions::tools`].
//! - [`prompt::DEFAULT_CODING_AGENT_SYSTEM_PROMPT`] — recommended base
//!   system prompt for coding-agent flows.
//!
//! ## Not yet
//!
//! - Write tools (Write / Edit), shell exec (Bash) — separate PR.
//! - CLI driver (single-prompt / interactive) — separate PR.
//! - rig-backed semantic search (`SemanticSearch` tool) — separate PR.

pub mod cli;
pub mod config;
pub mod diagnostics;
pub mod extensions;
pub mod migrations;
pub mod plugin_lock;
pub mod plugin_manager;
pub mod plugin_spec;
pub mod plugin_ui;
pub mod plugins;
pub mod prompt;
pub mod runtime;
pub mod session;
pub mod session_discovery;
pub mod skills;
pub mod slash;
pub mod telemetry;
pub mod tools;
pub mod workspace;

#[cfg(feature = "rig")]
pub mod semantic;

pub use cli::{
    Args, EventPrinter, EventSink, JsonEventPrinter, OpenAiCompatChoice, OutputFormat, run,
};
pub use config::{ArgDefaults, ConfigError, ConfigFile};
pub use extensions::{Extension, ExtensionRegistry};
pub use migrations::{
    CURRENT_SCHEMA_VERSION, Migration, MigrationError, default_migrations, migrate_all,
    migrate_session, schema_version_of, stamp_current_version, validate_migrations,
};
pub use telemetry::{TelemetryError, TelemetrySink};
pub use prompt::{
    DEFAULT_CODING_AGENT_SYSTEM_PROMPT, FULL_CODING_AGENT_SYSTEM_PROMPT,
    WRITE_ENABLED_CODING_AGENT_SYSTEM_PROMPT, coding_agent_system_prompt,
};
pub use runtime::{
    coding_all_tools, coding_bash_tools, coding_full_tools, coding_read_tools,
    coding_web_tools, coding_write_tools,
};
pub use session::{SessionError, SessionWriter, load_messages};
pub use session_discovery::{
    SessionMeta, TITLE_PREVIEW_MAX, list_sessions, new_session_path, parse_session_meta,
};
pub use skills::{DEFAULT_SKILLS_DIR, SkillsError, find_skills, resolve_skills_dir};
pub use plugins::{
    Plugin, PluginInfo, PluginManifest, PromptFragment, compose_system_prompt_with_plugins,
    default_plugins_dir, discover_plugins, discover_plugins_with_spec, find_skills_with_plugins,
    parse_manifest, plugin_info, plugin_script_dirs, read_plugin_prompt_fragments,
    summarize_plugin,
};
pub use plugin_spec::{
    PluginSpec, PluginSpecFile, SourceKind, SyncReport, default_spec_path, detect_source_kind,
    load_plugin_spec, resolve_local_src, sync_plugins,
};
pub use plugin_ui::{
    BoundSlashCommand as BoundPluginSlashCommand, BoundUiCommand, FormField, ModalSeverity,
    OverlayDescriptor, SlashCommand as PluginSlashCommand, TextColor, TextLine, TextSpan,
    UiCommand, collect_slash_commands as collect_plugin_slash_commands, collect_ui_commands,
};
pub use plugin_lock::{
    PluginOrigin, default_lock_path, effective_spec, load_plugin_lock, origin_of, save_plugin_lock,
};
pub use plugin_manager::{
    InstallOutcome, ManagerError, RemoveOutcome, UpdateOutcome, install, remove, update,
};
pub use diagnostics::{SourceInfo, render_doctor_report, render_source_info_block, source_info};
pub use slash::{HELP_TEXT, SlashCommand, parse as parse_slash_command};
pub use tools::{
    BashTool, EditTool, GlobTool, GrepTool, ListTool, ReadTool, SourceInfoTool, WebFetchTool,
    WriteTool,
};
pub use workspace::{Workspace, WorkspaceError};

#[cfg(feature = "rig")]
pub use semantic::{SemanticIndexConfig, SemanticInitError, SemanticSearchTool};
