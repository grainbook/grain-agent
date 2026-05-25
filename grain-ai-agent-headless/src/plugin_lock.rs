//! Plugin lock file — `<workspace>/.grain/plugin-lock.toml`.
//!
//! Auto-managed by the runtime plugin manager (`lazy_install` /
//! `lazy_remove`). Same TOML shape as the legacy `plugin.toml`
//! (a `[[plugin]]` array). Splitting it out means user-authored
//! declarations in `config.toml` are never touched by the engine —
//! all runtime mutations land here, so the user's hand-written
//! `[[plugin]]` blocks keep their comments and ordering.
//!
//! # Effective spec
//!
//! At boot the engine computes the **effective spec** =
//!
//! 1. `config.toml`'s `[[plugin]]` blocks (declarative; user-authored)
//! 2. `plugin-lock.toml`'s `[[plugin]]` blocks (auto-managed)
//! 3. `plugin.toml`'s `[[plugin]]` blocks (legacy; deprecated)
//!
//! …with first-source-wins on `name` collision. Removing a plugin
//! that lives in `config.toml` is refused — the user must edit
//! that file directly. Lock / legacy entries can be removed via
//! `lazy_remove` as usual.

use std::path::{Path, PathBuf};

use crate::config::ConfigFile;
use crate::plugin_spec::{PluginSpecFile, default_spec_path, load_plugin_spec};

/// Default location: `<workspace>/.grain/plugin-lock.toml`.
pub fn default_lock_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".grain").join("plugin-lock.toml")
}

/// Load (or default-empty) the lock file. Same wire format as
/// `plugin.toml` — a `[[plugin]]` array. Missing file →
/// `Ok(empty)`.
pub fn load_plugin_lock(path: &Path) -> std::io::Result<PluginSpecFile> {
    load_plugin_spec(path)
}

/// Write `lock` back to `path`, creating the parent directory if
/// missing. Pretty-print so a hand-inspecting user sees a readable
/// file.
pub fn save_plugin_lock(path: &Path, lock: &PluginSpecFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = toml::to_string_pretty(lock)
        .map_err(|e| std::io::Error::other(format!("lock serialize: {e}")))?;
    std::fs::write(path, raw)
}

/// Where a plugin entry came from. Used by callers to decide
/// whether a remove is allowed and which file to mutate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginOrigin {
    /// Declared in `config.toml`. Read-only from the engine's POV
    /// — removing requires the user to edit the file directly.
    Config,
    /// Added at runtime by `lazy_install`; lives in
    /// `plugin-lock.toml`. Mutable by `lazy_remove`.
    Lock,
    /// Pre-consolidation `plugin.toml` entry. Deprecated; the
    /// engine still reads it and `lazy_remove` still mutates it
    /// for back-compat. Boot emits a one-line warning suggesting
    /// migration.
    LegacySpec,
}

/// Compute the boot-time **effective** plugin spec: the merged
/// union of config + lock + legacy spec, with first-source-wins on
/// `name` collisions. Returns the merged spec plus a list of
/// human-facing warnings (e.g. "found N entries in legacy
/// plugin.toml; consider migrating to config.toml").
pub fn effective_spec(workspace_root: &Path, config: &ConfigFile) -> (PluginSpecFile, Vec<String>) {
    let mut out = PluginSpecFile::default();
    let mut warnings = Vec::new();

    // 1. config.toml — authoritative.
    for p in &config.plugins {
        if !out.plugins.iter().any(|e| e.name == p.name) {
            out.plugins.push(p.clone());
        }
    }

    // 2. plugin-lock.toml — runtime-added.
    let lock_path = default_lock_path(workspace_root);
    if let Ok(lock) = load_plugin_lock(&lock_path) {
        for p in lock.plugins {
            if !out.plugins.iter().any(|e| e.name == p.name) {
                out.plugins.push(p);
            }
        }
    }

    // 3. plugin.toml — legacy.
    let legacy_path = default_spec_path(workspace_root);
    if legacy_path.exists()
        && let Ok(legacy) = load_plugin_spec(&legacy_path)
        && !legacy.plugins.is_empty()
    {
        let mut added = 0usize;
        for p in legacy.plugins {
            if !out.plugins.iter().any(|e| e.name == p.name) {
                out.plugins.push(p);
                added += 1;
            }
        }
        if added > 0 {
            warnings.push(format!(
                "{} entries in legacy {}; consider migrating to config.toml [[plugin]] blocks",
                added,
                legacy_path.display()
            ));
        }
    }

    (out, warnings)
}

/// Locate which file declares `name`. `None` if nowhere.
pub fn origin_of(workspace_root: &Path, config: &ConfigFile, name: &str) -> Option<PluginOrigin> {
    if config.plugins.iter().any(|p| p.name == name) {
        return Some(PluginOrigin::Config);
    }
    let lock_path = default_lock_path(workspace_root);
    if let Ok(lock) = load_plugin_lock(&lock_path)
        && lock.plugins.iter().any(|p| p.name == name)
    {
        return Some(PluginOrigin::Lock);
    }
    let legacy_path = default_spec_path(workspace_root);
    if legacy_path.exists()
        && let Ok(legacy) = load_plugin_spec(&legacy_path)
        && legacy.plugins.iter().any(|p| p.name == name)
    {
        return Some(PluginOrigin::LegacySpec);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_spec::PluginSpec;

    fn write_toml(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn lock_path_lives_next_to_legacy_spec() {
        let p = default_lock_path(Path::new("/ws"));
        assert_eq!(p, PathBuf::from("/ws/.grain/plugin-lock.toml"));
    }

    #[test]
    fn effective_spec_is_empty_when_no_sources_set() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigFile::default();
        let (eff, warnings) = effective_spec(dir.path(), &cfg);
        assert!(eff.plugins.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn effective_spec_unions_all_three_sources_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigFile {
            plugins: vec![PluginSpec {
                name: "lazy-gagent".into(),
                src: "config-src".into(),
                rev: None,
                kind: None,
                auth: Vec::new(),
            }],
            ..Default::default()
        };
        write_toml(
            &default_lock_path(dir.path()),
            r#"
[[plugin]]
name = "lazy-gagent"
src  = "lock-src"

[[plugin]]
name = "lock-only"
src  = "lock-only-src"
"#,
        );
        write_toml(
            &default_spec_path(dir.path()),
            r#"
[[plugin]]
name = "lazy-gagent"
src  = "legacy-src"

[[plugin]]
name = "legacy-only"
src  = "legacy-src"
"#,
        );
        let (eff, warnings) = effective_spec(dir.path(), &cfg);
        let names: Vec<_> = eff.plugins.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"lazy-gagent"));
        assert!(names.contains(&"lock-only"));
        assert!(names.contains(&"legacy-only"));
        // Config wins.
        let lz = eff
            .plugins
            .iter()
            .find(|p| p.name == "lazy-gagent")
            .unwrap();
        assert_eq!(lz.src, "config-src");
        // Legacy migration warning fires when legacy contributes
        // at least one new name.
        assert!(warnings.iter().any(|w| w.contains("legacy")));
    }

    #[test]
    fn save_then_load_lock_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_lock_path(dir.path());
        let lock = PluginSpecFile {
            plugins: vec![PluginSpec {
                name: "demo".into(),
                src: "./demo".into(),
                rev: None,
                kind: None,
                auth: Vec::new(),
            }],
        };
        save_plugin_lock(&path, &lock).unwrap();
        let back = load_plugin_lock(&path).unwrap();
        assert_eq!(back, lock);
    }

    #[test]
    fn origin_of_resolves_each_source() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigFile {
            plugins: vec![PluginSpec {
                name: "in-config".into(),
                src: "x".into(),
                rev: None,
                kind: None,
                auth: Vec::new(),
            }],
            ..Default::default()
        };
        write_toml(
            &default_lock_path(dir.path()),
            "[[plugin]]\nname = \"in-lock\"\nsrc = \"x\"\n",
        );
        write_toml(
            &default_spec_path(dir.path()),
            "[[plugin]]\nname = \"in-legacy\"\nsrc = \"x\"\n",
        );
        assert_eq!(
            origin_of(dir.path(), &cfg, "in-config"),
            Some(PluginOrigin::Config)
        );
        assert_eq!(
            origin_of(dir.path(), &cfg, "in-lock"),
            Some(PluginOrigin::Lock)
        );
        assert_eq!(
            origin_of(dir.path(), &cfg, "in-legacy"),
            Some(PluginOrigin::LegacySpec)
        );
        assert_eq!(origin_of(dir.path(), &cfg, "nowhere"), None);
    }
}
