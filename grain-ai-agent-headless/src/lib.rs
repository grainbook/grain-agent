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
pub mod compaction;
pub mod config;
pub mod deepseek;
pub mod diagnostics;
pub mod dynamic_tools;
pub mod extensions;
pub mod hooks;
pub mod memory;
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
pub mod tool_names;
pub mod tools;
pub mod workspace;

#[cfg(feature = "rig")]
pub mod semantic;
#[cfg(feature = "wasm-plugins")]
pub mod wasm_orchestration;

pub use cli::{
    Args, EventPrinter, EventSink, JsonEventPrinter, OpenAiCompatChoice, OutputFormat, run,
};
pub use compaction::{AutoCompactionConfig, AutoCompactionPolicy, build_auto_compaction_policy};
pub use config::{ArgDefaults, ConfigError, ConfigFile};
pub use deepseek::DeepSeekPack;
pub use diagnostics::{SourceInfo, render_doctor_report, render_source_info_block, source_info};
pub use dynamic_tools::{
    BASE_READ_TOOLS, BASH_TOOLS, SEMANTIC_TOOLS, ToolActivationDecision, WEB_TOOLS, WRITE_TOOLS,
    filter_tools_by_names, select_dynamic_tool_names,
};
pub use extensions::{Extension, ExtensionRegistry};
pub use hooks::{
    HookAction, HookEvent, HookRegistry, HookRule, HookTrace, HookTraceSink, after_tool_call_hook,
    before_tool_call_hook, chain_after_hooks, chain_before_hooks, prepare_next_turn_hook,
};
pub use memory::{
    DEFAULT_MAX_RECORDS as MEMORY_DEFAULT_MAX_RECORDS,
    DEFAULT_MAX_SESSIONS as MEMORY_DEFAULT_MAX_SESSIONS,
    DEFAULT_SUMMARY_MAX_BYTES as MEMORY_DEFAULT_SUMMARY_MAX_BYTES, MemoryCategory, MemoryError,
    MemoryRecord, MemoryRefreshReport, ProjectMemorySettings, load_project_memory_prompt,
    read_memory_summary, refresh_project_memory,
};
pub use migrations::{
    CURRENT_SCHEMA_VERSION, Migration, MigrationError, default_migrations, migrate_all,
    migrate_session, schema_version_of, stamp_current_version, validate_migrations,
};
pub use plugin_lock::{
    PluginOrigin, default_lock_path, effective_spec, load_plugin_lock, origin_of, save_plugin_lock,
};
pub use plugin_manager::{
    InstallOutcome, ManagerError, RemoveOutcome, UpdateOutcome, install, remove, update,
};
pub use plugin_spec::{
    PluginAuthEntry, PluginSpec, PluginSpecFile, SourceKind, SyncReport, default_spec_path,
    detect_source_kind, load_plugin_spec, resolve_local_src, sync_plugins,
};
pub use plugin_ui::{
    BoundSlashCommand as BoundPluginSlashCommand, BoundUiCommand, FormField, ModalSeverity,
    OverlayDescriptor, SlashCommand as PluginSlashCommand, TextColor, TextLine, TextSpan,
    UiCommand, collect_slash_commands as collect_plugin_slash_commands, collect_ui_commands,
};
pub use plugins::{
    Plugin, PluginInfo, PluginManifest, PromptFragment, WasmConfig,
    compose_system_prompt_with_plugins, default_plugins_dir, discover_plugins,
    discover_plugins_with_spec, find_skills_in_dirs_with_plugins, find_skills_with_plugins,
    parse_manifest, plugin_info, plugin_script_dirs, read_plugin_prompt_fragments,
    summarize_plugin,
};
pub use prompt::{
    DEFAULT_CODING_AGENT_SYSTEM_PROMPT, FULL_CODING_AGENT_SYSTEM_PROMPT,
    WRITE_ENABLED_CODING_AGENT_SYSTEM_PROMPT, coding_agent_system_prompt,
    with_runtime_model_identity,
};
pub use runtime::{
    coding_all_tools, coding_bash_tools, coding_full_tools, coding_read_tools, coding_web_tools,
    coding_write_tools,
};
pub use session::{SessionError, SessionWriter, is_session_locked, load_messages};
pub use session_discovery::{
    SessionMeta, TITLE_PREVIEW_MAX, copy_session_tree_snapshot, list_sessions,
    list_sessions_excluding_active, new_session_path, open_or_create_session_dir, open_session_dir,
    parse_session_meta, session_id_from_path,
};
pub use skills::{
    DEFAULT_SKILLS_DIR, SkillsError, find_skills, find_skills_in_dirs, load_project_context_skills,
    maybe_load_agents_md, resolve_skill_dirs, resolve_skill_dirs_with_scope, resolve_skills_dir,
};
pub use slash::{HELP_TEXT, SlashCommand, parse as parse_slash_command};
pub use telemetry::{TelemetryError, TelemetrySink};
pub use tool_names::{
    make_unique_tool_name, normalize_tool_names_for_provider, provider_safe_tool_name,
};
pub use tools::{
    BashTool, EditTool, GlobTool, GrepTool, ListTool, ReadTool, SourceInfoTool, WebFetchTool,
    WriteTool,
};
pub use workspace::{Workspace, WorkspaceError};

#[cfg(feature = "rig")]
pub use semantic::{SemanticIndexConfig, SemanticInitError, SemanticSearchTool};
#[cfg(feature = "wasm-plugins")]
pub use wasm_orchestration::{WasmOrchestrator, WasmUiSink, WasmUiUpdate};
