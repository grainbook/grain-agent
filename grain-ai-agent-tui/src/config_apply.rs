//! Bridge `grain_ai_agent_headless::ConfigFile` ⇒ the TUI's [`Args`].
//!
//! `grain-headless` already owns the TOML schema + load logic for
//! `<workspace>/.grain/config.toml` and `~/.config/grain/config.toml`
//! (see `grain_ai_agent_headless::config`). The schema is binary-
//! agnostic, but the `apply_to_args` method there overlays onto the
//! *headless* `Args` struct.
//!
//! This module does the same overlay against the TUI's `Args`, so a
//! user's TOML file gives identical behavior in both binaries.
//!
//! Precedence (highest wins):
//!
//! 1. CLI flag explicitly passed by the user.
//! 2. `<workspace>/.grain/config.toml`.
//! 3. `~/.config/grain/config.toml`.
//! 4. Hard-coded defaults baked into [`Args`].

use std::collections::HashSet;
use std::path::Path;

use grain_ai_agent_headless::config::{ConfigError, ConfigFile};

use crate::cli::{Args, OpenAiCompatChoice};

/// Load + apply config in one shot, in place. Emits `[warn]` lines to
/// stderr on disk-level / parse errors but never fails the boot —
/// missing config is the common case.
///
/// Call this right after `Args::parse()` in the TUI binary, before
/// handing off to [`crate::run_tui`].
pub fn load_and_apply(args: &mut Args, argv: &[String]) {
    let explicit = Args::explicit_arg_ids(argv);
    let cfg = match ConfigFile::load(&args.workspace) {
        Ok(c) => c,
        Err(e) => {
            warn_load(&e);
            return;
        }
    };
    apply_config_to_args(&cfg, args, &explicit);
}

/// Pure overlay function — exposed for tests so callers can supply a
/// hand-crafted [`ConfigFile`] without touching the filesystem.
pub fn apply_config_to_args(cfg: &ConfigFile, args: &mut Args, explicit: &HashSet<String>) {
    if !explicit.contains("model")
        && let Some(m) = &cfg.model
    {
        args.model.clone_from(m);
    }
    if !explicit.contains("headroom_tokens")
        && let Some(h) = cfg.headroom_tokens
    {
        args.headroom_tokens = h;
    }
    if !explicit.contains("show_thinking")
        && let Some(b) = cfg.show_thinking
    {
        args.show_thinking = b;
    }
    if !explicit.contains("openai_compat")
        && let Some(s) = cfg.openai_compat.as_deref()
        && let Some(parsed) = parse_openai_compat(s)
    {
        args.openai_compat = parsed;
    }
    if !explicit.contains("allow_write")
        && let Some(b) = cfg.allow_write
    {
        args.allow_write = b;
    }
    if !explicit.contains("allow_bash")
        && let Some(b) = cfg.allow_bash
    {
        args.allow_bash = b;
    }
    if !explicit.contains("allow_web")
        && let Some(b) = cfg.allow_web
    {
        args.allow_web = b;
    }
    if !explicit.contains("allow_semantic_search")
        && let Some(b) = cfg.allow_semantic_search
    {
        args.allow_semantic_search = b;
    }
    if !explicit.contains("bypass_proxy")
        && args.bypass_proxy.is_none()
        && cfg.bypass_proxy.is_some()
    {
        args.bypass_proxy = cfg.bypass_proxy;
    }
    if !explicit.contains("skills_dir")
        && args.skills_dir.is_none()
        && let Some(d) = &cfg.skills_dir
    {
        args.skills_dir = Some(d.clone());
    }
    if !explicit.contains("workspace_skills_only")
        && let Some(b) = cfg.workspace_skills_only
    {
        args.workspace_skills_only = b;
    }
}

fn parse_openai_compat(s: &str) -> Option<OpenAiCompatChoice> {
    match s.to_ascii_lowercase().as_str() {
        "none" => Some(OpenAiCompatChoice::None),
        "common" => Some(OpenAiCompatChoice::Common),
        _ => None,
    }
}

fn warn_load(e: &ConfigError) {
    eprintln!("[warn] config: {e}");
}

#[allow(dead_code)]
pub fn workspace_config_exists(root: &Path) -> bool {
    root.join(".grain").join("config.toml").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(argv: &[&str]) -> Args {
        let v: Vec<String> = std::iter::once("grain-tui".to_string())
            .chain(argv.iter().map(|s| s.to_string()))
            .collect();
        Args::parse_from(v)
    }

    #[test]
    fn config_overrides_default_when_cli_silent() {
        let mut args = parse(&[]);
        assert!(!args.allow_write); // clap default
        let cfg = ConfigFile {
            allow_write: Some(true),
            ..ConfigFile::default()
        };
        apply_config_to_args(&cfg, &mut args, &HashSet::new());
        assert!(args.allow_write, "config must turn the flag on");
    }

    #[test]
    fn explicit_cli_flag_beats_config() {
        let mut args = parse(&["--allow-write"]);
        assert!(args.allow_write);
        let cfg = ConfigFile {
            allow_write: Some(false),
            ..ConfigFile::default()
        };
        let mut explicit = HashSet::new();
        explicit.insert("allow_write".to_string());
        apply_config_to_args(&cfg, &mut args, &explicit);
        assert!(
            args.allow_write,
            "CLI explicit must win even when config says false"
        );
    }

    #[test]
    fn config_can_disable_a_flag_when_cli_silent() {
        // The user happens to have `allow_bash = false` in their
        // config; CLI didn't set it. Apply should leave it false.
        let mut args = parse(&[]);
        args.allow_bash = false;
        let cfg = ConfigFile {
            allow_bash: Some(false),
            ..ConfigFile::default()
        };
        apply_config_to_args(&cfg, &mut args, &HashSet::new());
        assert!(!args.allow_bash);
    }

    #[test]
    fn openai_compat_parses_known_values() {
        let mut args = parse(&[]);
        let cfg = ConfigFile {
            openai_compat: Some("none".into()),
            ..ConfigFile::default()
        };
        apply_config_to_args(&cfg, &mut args, &HashSet::new());
        assert!(matches!(args.openai_compat, OpenAiCompatChoice::None));
    }

    #[test]
    fn unknown_openai_compat_value_is_ignored() {
        let mut args = parse(&[]);
        let baseline = args.openai_compat;
        let cfg = ConfigFile {
            openai_compat: Some("garbage".into()),
            ..ConfigFile::default()
        };
        apply_config_to_args(&cfg, &mut args, &HashSet::new());
        // Untouched.
        assert!(matches!(
            (baseline, args.openai_compat),
            (OpenAiCompatChoice::Common, OpenAiCompatChoice::Common)
                | (OpenAiCompatChoice::None, OpenAiCompatChoice::None)
        ));
    }

    #[test]
    fn bypass_proxy_overrides_default_unset() {
        let mut args = parse(&[]);
        assert!(args.bypass_proxy.is_none());
        let cfg = ConfigFile {
            bypass_proxy: Some(true),
            ..ConfigFile::default()
        };
        apply_config_to_args(&cfg, &mut args, &HashSet::new());
        assert_eq!(args.bypass_proxy, Some(true));
    }

    #[test]
    fn bypass_proxy_cli_true_beats_config_false() {
        let mut args = parse(&["--bypass-proxy", "true"]);
        assert_eq!(args.bypass_proxy, Some(true));
        let mut explicit = HashSet::new();
        explicit.insert("bypass_proxy".to_string());
        let cfg = ConfigFile {
            bypass_proxy: Some(false),
            ..ConfigFile::default()
        };
        apply_config_to_args(&cfg, &mut args, &explicit);
        assert_eq!(args.bypass_proxy, Some(true), "CLI must win when explicit");
    }

    #[test]
    fn bypass_proxy_config_false_disables_default() {
        let mut args = parse(&[]);
        let cfg = ConfigFile {
            bypass_proxy: Some(false),
            ..ConfigFile::default()
        };
        apply_config_to_args(&cfg, &mut args, &HashSet::new());
        assert_eq!(args.bypass_proxy, Some(false));
    }

    #[test]
    fn skills_dir_only_overrides_when_args_is_none() {
        // CLI passed --skills-dir; config must NOT clobber.
        let mut args = parse(&["--skills-dir", "/from/cli"]);
        let mut explicit = HashSet::new();
        explicit.insert("skills_dir".to_string());
        let cfg = ConfigFile {
            skills_dir: Some("/from/config".into()),
            ..ConfigFile::default()
        };
        apply_config_to_args(&cfg, &mut args, &explicit);
        assert_eq!(
            args.skills_dir.as_deref().unwrap().to_str(),
            Some("/from/cli")
        );
    }

    #[test]
    fn workspace_skills_only_config_applies_when_cli_silent() {
        let mut args = parse(&[]);
        let explicit = HashSet::new();
        let cfg = ConfigFile {
            workspace_skills_only: Some(true),
            ..ConfigFile::default()
        };

        apply_config_to_args(&cfg, &mut args, &explicit);

        assert!(args.workspace_skills_only);
    }

    #[test]
    fn workspace_skills_only_cli_beats_config() {
        let mut args = parse(&["--workspace-skills-only"]);
        let mut explicit = HashSet::new();
        explicit.insert("workspace_skills_only".to_string());
        let cfg = ConfigFile {
            workspace_skills_only: Some(false),
            ..ConfigFile::default()
        };

        apply_config_to_args(&cfg, &mut args, &explicit);

        assert!(args.workspace_skills_only);
    }
}
