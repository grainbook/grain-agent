//! TOML-backed config file: persistent defaults for `grain-headless`.
//!
//! Resolution order (highest priority wins):
//! 1. Command-line flag (handled by clap).
//! 2. Per-workspace `<workspace>/.grain/config.toml`.
//! 3. User `~/.config/grain/config.toml` (XDG via the `dirs` crate).
//! 4. Hard-coded defaults baked into `Args`.
//!
//! TOML schema (all fields optional):
//!
//! ```toml
//! model = "anthropic/claude-sonnet-4-5"
//! headroom_tokens = 4096
//! show_thinking = false
//! openai_compat = "common"        # "none" | "common"
//! allow_write = false
//! allow_bash = false
//! allow_web = false
//! allow_semantic_search = false
//! skills_dir = ".claude/skills"
//! session_dir = ".grain/sessions" # base dir for JSONL sessions; --session overrides
//!
//! # Declarative plugin set. Equivalent to a hand-edited
//! # plugin-spec.toml entry. The runtime plugin manager
//! # (lazy_install / lazy_remove) writes to .grain/plugin-lock.toml
//! # instead so it never has to rewrite this file (no comment-loss /
//! # ordering churn). Boot-time spec = union(config.plugin,
//! # plugin-lock.plugin, legacy plugin-spec.toml).
//! [[plugin]]
//! name = "lazy-gagent"
//! src  = "../lazy-gagent"
//!
//! # Declarative provider profile. Equivalent to a [[profile]] block
//! # in the legacy .grain/providers.toml. Both files are read; if a
//! # name appears in both, config.toml wins.
//! [[provider]]
//! name     = "anthropic"
//! kind     = "anthropic"
//! model    = "anthropic/claude-sonnet-4-5"
//! auth     = { kind = "api_key", env = "ANTHROPIC_API_KEY" }
//!
//! # The `value` field (optional) auto-populates the env var at
//! # startup so you don't need to `export` it beforehand:
//! auth     = { kind = "api_key", env = "ANTHROPIC_API_KEY",
//!              value = "sk-ant-..." }
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::plugin_spec::PluginSpec;
use grain_llm_genai::ProfileEntry;

/// User-overridable defaults, deserialized from TOML. Every field is
/// optional; a missing field falls through to the next layer.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ConfigFile {
    pub model: Option<String>,
    pub headroom_tokens: Option<u64>,
    pub show_thinking: Option<bool>,
    pub openai_compat: Option<String>,
    pub allow_write: Option<bool>,
    pub allow_bash: Option<bool>,
    pub allow_web: Option<bool>,
    pub allow_semantic_search: Option<bool>,
    pub skills_dir: Option<PathBuf>,
    pub session_dir: Option<PathBuf>,
    /// Override for the proxy-bypass behavior of the genai HTTP client.
    /// `None` (unset) keeps the default auto-detect (bypass when a
    /// configured OpenAI-compat endpoint is on loopback). `Some(true)`
    /// always bypasses; `Some(false)` always respects proxy env vars.
    pub bypass_proxy: Option<bool>,
    /// Default fold state for **tool-call blocks** in the TUI
    /// transcript. `Some(true)` (the default) collapses each
    /// tool-call block to a one-line summary; user expands
    /// individually with the transcript cursor. `Some(false)` keeps
    /// them expanded inline like the legacy renderer.
    pub fold_tool_calls: Option<bool>,
    /// Default fold state for **thinking blocks** in the TUI
    /// transcript. Same shape as [`Self::fold_tool_calls`].
    pub fold_thinking: Option<bool>,
    /// Declarative plugin entries — same shape as
    /// `plugin-spec.toml`'s `[[plugin]]` blocks. Authoritative when
    /// the same `name` appears in both files; the runtime plugin
    /// manager writes to `plugin-lock.toml` (a separate file) to
    /// keep this one's comments / ordering intact.
    #[serde(default, rename = "plugin")]
    pub plugins: Vec<PluginSpec>,
    /// Declarative provider entries — same shape as the legacy
    /// `providers.toml` `[[profile]]` blocks but renamed to
    /// `[[provider]]` so the section reads naturally. If both
    /// `providers.toml` and this list set the same `name`, this
    /// list wins.
    #[serde(default, rename = "provider")]
    pub providers: Vec<ProfileEntry>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("toml parse error in {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
}

impl ConfigFile {
    /// Load and merge config from the per-workspace file (if any) and the
    /// user XDG config (if any). Workspace overrides user where both set a
    /// field; CLI flags override the merged result on top.
    ///
    /// Either source missing is fine — the function returns an empty config
    /// rather than an error. Hard I/O failures (e.g. permission denied) and
    /// TOML parse failures are surfaced.
    pub fn load(workspace_root: &Path) -> Result<Self, ConfigError> {
        let mut merged = ConfigFile::default();
        if let Some(user) = user_config_path()
            && user.exists()
        {
            let user_cfg = load_from(&user)?;
            merge_into(&mut merged, user_cfg);
        }
        let ws = workspace_config_path(workspace_root);
        if ws.exists() {
            let ws_cfg = load_from(&ws)?;
            merge_into(&mut merged, ws_cfg);
        }
        Ok(merged)
    }

    /// Apply this config's set fields onto `args`, but only for arguments
    /// the user did NOT explicitly pass on the command line. `explicit`
    /// is the set of clap argument ids whose value came from the user
    /// (computed via `ArgMatches::value_source`); anything not in it had
    /// the clap default and is overridable by config.
    ///
    /// Bool fields in the config are honored in both directions: a config
    /// `allow_bash = false` will turn off the corresponding flag if the
    /// CLI didn't enable it explicitly. This avoids the asymmetric "config
    /// can enable but not disable" surprise reported in code review (L-4).
    pub fn apply_to_args(
        &self,
        args: &mut crate::cli::Args,
        explicit: &std::collections::HashSet<String>,
        _defaults: &ArgDefaults,
    ) {
        if !explicit.contains("model")
            && let Some(m) = &self.model
        {
            args.model = m.clone();
        }
        if !explicit.contains("headroom_tokens")
            && let Some(h) = self.headroom_tokens
        {
            args.headroom_tokens = h;
        }
        if !explicit.contains("show_thinking")
            && let Some(b) = self.show_thinking
        {
            args.show_thinking = b;
        }
        if !explicit.contains("openai_compat")
            && let Some(s) = self.openai_compat.as_deref()
            && let Some(parsed) = parse_openai_compat(s)
        {
            args.openai_compat = parsed;
        }
        if !explicit.contains("allow_write")
            && let Some(b) = self.allow_write
        {
            args.allow_write = b;
        }
        if !explicit.contains("allow_bash")
            && let Some(b) = self.allow_bash
        {
            args.allow_bash = b;
        }
        if !explicit.contains("allow_web")
            && let Some(b) = self.allow_web
        {
            args.allow_web = b;
        }
        if !explicit.contains("allow_semantic_search")
            && let Some(b) = self.allow_semantic_search
        {
            args.allow_semantic_search = b;
        }
        if !explicit.contains("skills_dir")
            && args.skills_dir.is_none()
            && let Some(d) = &self.skills_dir
        {
            args.skills_dir = Some(d.clone());
        }
        // session_dir isn't on Args today — `--session` is an explicit
        // file path. Config callers that want auto-naming can set
        // session_dir; the CLI driver consults `config.session_dir` only
        // when `--session` isn't set (see cli::run).
    }
}

/// Snapshot of the CLI's hard-coded defaults. Built by `Args::cli_defaults()`
/// so the config-merge logic can tell "user accepted the default" from
/// "user explicitly set this on the command line" without duplicating the
/// default values across the codebase.
pub struct ArgDefaults {
    pub model: String,
    pub headroom_tokens: u64,
}

fn parse_openai_compat(s: &str) -> Option<crate::cli::OpenAiCompatChoice> {
    match s.to_ascii_lowercase().as_str() {
        "none" => Some(crate::cli::OpenAiCompatChoice::None),
        "common" => Some(crate::cli::OpenAiCompatChoice::Common),
        _ => None,
    }
}

fn merge_into(dst: &mut ConfigFile, src: ConfigFile) {
    if src.model.is_some() {
        dst.model = src.model;
    }
    if src.headroom_tokens.is_some() {
        dst.headroom_tokens = src.headroom_tokens;
    }
    if src.show_thinking.is_some() {
        dst.show_thinking = src.show_thinking;
    }
    if src.openai_compat.is_some() {
        dst.openai_compat = src.openai_compat;
    }
    if src.allow_write.is_some() {
        dst.allow_write = src.allow_write;
    }
    if src.allow_bash.is_some() {
        dst.allow_bash = src.allow_bash;
    }
    if src.allow_web.is_some() {
        dst.allow_web = src.allow_web;
    }
    if src.allow_semantic_search.is_some() {
        dst.allow_semantic_search = src.allow_semantic_search;
    }
    if src.skills_dir.is_some() {
        dst.skills_dir = src.skills_dir;
    }
    if src.session_dir.is_some() {
        dst.session_dir = src.session_dir;
    }
    if src.bypass_proxy.is_some() {
        dst.bypass_proxy = src.bypass_proxy;
    }
    if src.fold_tool_calls.is_some() {
        dst.fold_tool_calls = src.fold_tool_calls;
    }
    if src.fold_thinking.is_some() {
        dst.fold_thinking = src.fold_thinking;
    }
    // Plugin and provider lists: layered merge. Workspace entries
    // win on `name` collision with user-XDG entries; otherwise
    // both are kept. Order: dst (existing) first, then any new
    // entries from src.
    for p in src.plugins {
        if let Some(slot) = dst.plugins.iter_mut().find(|e| e.name == p.name) {
            *slot = p;
        } else {
            dst.plugins.push(p);
        }
    }
    for p in src.providers {
        if let Some(slot) = dst.providers.iter_mut().find(|e| e.name == p.name) {
            *slot = p;
        } else {
            dst.providers.push(p);
        }
    }
}

fn load_from(path: &Path) -> Result<ConfigFile, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    toml::from_str::<ConfigFile>(&raw).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })
}

fn workspace_config_path(root: &Path) -> PathBuf {
    root.join(".grain").join("config.toml")
}

fn user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|c| c.join("grain").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_toml(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn missing_workspace_config_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigFile::load(dir.path()).unwrap();
        // No fields set when both files are absent.
        assert!(cfg.model.is_none());
        assert!(cfg.skills_dir.is_none());
    }

    #[test]
    fn workspace_config_overrides_fields() {
        let dir = tempfile::tempdir().unwrap();
        write_toml(
            &workspace_config_path(dir.path()),
            "model = \"openai/gpt-4o\"\nallow_write = true\nheadroom_tokens = 8192\n",
        );
        let cfg = ConfigFile::load(dir.path()).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("openai/gpt-4o"));
        assert_eq!(cfg.allow_write, Some(true));
        assert_eq!(cfg.headroom_tokens, Some(8192));
    }

    #[test]
    fn apply_to_args_respects_explicit_cli_flag() {
        use crate::cli::Args;
        use clap::Parser;
        let mut args =
            Args::try_parse_from(["grain-headless", "--model", "anthropic/claude-sonnet-4-5"])
                .unwrap();
        let cfg = ConfigFile {
            model: Some("openai/gpt-4o".into()),
            ..Default::default()
        };
        let mut explicit = std::collections::HashSet::new();
        explicit.insert("model".to_string());
        cfg.apply_to_args(&mut args, &explicit, &Args::cli_defaults());
        // CLI explicitly set model — config must not override.
        assert_eq!(args.model, "anthropic/claude-sonnet-4-5");
    }

    #[test]
    fn apply_to_args_uses_config_when_cli_implicit() {
        use crate::cli::Args;
        use clap::Parser;
        let mut args = Args::try_parse_from(["grain-headless"]).unwrap();
        let cfg = ConfigFile {
            model: Some("openai/gpt-4o".into()),
            allow_bash: Some(true),
            ..Default::default()
        };
        let explicit = std::collections::HashSet::new();
        cfg.apply_to_args(&mut args, &explicit, &Args::cli_defaults());
        assert_eq!(args.model, "openai/gpt-4o");
        assert!(args.allow_bash);
    }

    #[test]
    fn apply_to_args_honors_explicit_false_in_config() {
        use crate::cli::Args;
        use clap::Parser;
        // Args::default for allow_bash is false; config explicitly says
        // false; expected behavior: stay false (i.e. config-as-stated wins
        // when CLI wasn't given).
        let mut args = Args::try_parse_from(["grain-headless"]).unwrap();
        let cfg = ConfigFile {
            allow_bash: Some(false),
            ..Default::default()
        };
        let explicit = std::collections::HashSet::new();
        cfg.apply_to_args(&mut args, &explicit, &Args::cli_defaults());
        assert!(!args.allow_bash);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_toml(
            &workspace_config_path(dir.path()),
            "model = \"x\"\nbogus_field = 1\n",
        );
        let err = ConfigFile::load(dir.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn config_parses_plugin_blocks() {
        let dir = tempfile::tempdir().unwrap();
        write_toml(
            &workspace_config_path(dir.path()),
            r#"
model = "anthropic/claude-sonnet-4-5"

[[plugin]]
name = "lazy-gagent"
src  = "../lazy-gagent"

[[plugin]]
name = "rust-helper"
src  = "https://github.com/me/rust-helper.git"
rev  = "v1.0.0"
"#,
        );
        let cfg = ConfigFile::load(dir.path()).unwrap();
        assert_eq!(cfg.plugins.len(), 2);
        assert_eq!(cfg.plugins[0].name, "lazy-gagent");
        assert_eq!(cfg.plugins[0].src, "../lazy-gagent");
        assert_eq!(cfg.plugins[1].name, "rust-helper");
        assert_eq!(cfg.plugins[1].rev.as_deref(), Some("v1.0.0"));
    }

    #[test]
    fn config_parses_provider_blocks() {
        let dir = tempfile::tempdir().unwrap();
        write_toml(
            &workspace_config_path(dir.path()),
            r#"
[[provider]]
name  = "anthropic"
kind  = "anthropic"
model = "anthropic/claude-sonnet-4-5"
auth  = { kind = "api_key", env = "ANTHROPIC_API_KEY" }

[[provider]]
name  = "openai-work"
kind  = "openai"
model = "openai/gpt-4o"
auth  = { kind = "api_key", env = "OPENAI_API_KEY", value = "sk-openai-123" }
"#,
        );
        let cfg = ConfigFile::load(dir.path()).unwrap();
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.providers[0].name, "anthropic");
        assert_eq!(cfg.providers[0].kind, "anthropic");
        assert_eq!(cfg.providers[0].auth.kind, "api_key");
        assert_eq!(
            cfg.providers[0].auth.env.as_deref(),
            Some("ANTHROPIC_API_KEY")
        );
        assert_eq!(cfg.providers[0].auth.value.as_deref(), None);
        assert_eq!(cfg.providers[1].name, "openai-work");
        assert_eq!(cfg.providers[1].auth.env.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(
            cfg.providers[1].auth.value.as_deref(),
            Some("sk-openai-123")
        );
    }

    #[test]
    fn plugin_and_provider_blocks_are_optional() {
        let dir = tempfile::tempdir().unwrap();
        write_toml(
            &workspace_config_path(dir.path()),
            "model = \"openai/gpt-4o\"\n",
        );
        let cfg = ConfigFile::load(dir.path()).unwrap();
        assert!(cfg.plugins.is_empty());
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn workspace_plugin_overrides_user_plugin_by_name() {
        // User-XDG config declares one plugin; workspace config
        // overrides its src. The merged list keeps the workspace
        // entry and adds non-overlapping ones.
        let dir = tempfile::tempdir().unwrap();
        // Layout: we simulate the user-XDG file by writing to a temp
        // home and shimming via `dirs::config_dir()`... easier: just
        // exercise merge_into directly.
        let mut dst = ConfigFile::default();
        dst.plugins.push(PluginSpec {
            name: "shared".into(),
            src: "user-src".into(),
            rev: None,
            kind: None,
            auth: Vec::new(),
        });
        let src = ConfigFile {
            plugins: vec![PluginSpec {
                name: "shared".into(),
                src: "ws-src".into(),
                rev: None,
                kind: None,
                auth: Vec::new(),
            }],
            ..Default::default()
        };
        merge_into(&mut dst, src);
        assert_eq!(dst.plugins.len(), 1);
        assert_eq!(dst.plugins[0].src, "ws-src");
    }

    #[test]
    fn parse_openai_compat_accepts_known_values() {
        assert!(matches!(
            parse_openai_compat("none"),
            Some(crate::cli::OpenAiCompatChoice::None)
        ));
        assert!(matches!(
            parse_openai_compat("COMMON"),
            Some(crate::cli::OpenAiCompatChoice::Common)
        ));
        assert!(parse_openai_compat("nonsense").is_none());
    }
}
