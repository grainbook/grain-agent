//! `lazy.gagent` plugin SDK.
//!
//! A **plugin** is a directory under `<workspace>/.grain/plugins/<name>/`
//! containing a `plugin.toml` manifest. The plugin's contents extend
//! the TUI's built-in catalogs (skills, themes, scripts) by convention:
//!
//! ```text
//! <plugins_dir>/<name>/
//!   plugin.toml              # required — identifies the plugin
//!   skills/<skill>/SKILL.md  # optional — picked up alongside --skills-dir
//!   themes/<theme>.toml      # optional — picked up alongside --themes-dir
//!   scripts/*.js             # optional — Boa scripts (Phase B integration)
//! ```
//!
//! # Phase status
//!
//! - **Phase A (today)** — façade. [`discover_plugins`] walks
//!   `<plugins_dir>` and returns a list of [`Plugin`] records; the TUI
//!   hands each plugin's `skills/` to its existing `find_skills(...)`
//!   pass and `themes/` to `load_user_themes(...)`. No new runtime
//!   mechanics — just a unifying convention so related skills + themes
//!   + scripts can live under one named folder.
//! - **Phase B (planned)** — Boa scripts integration, in-TUI
//!   `/plugins` overlay, manifest-declared system-prompt fragments,
//!   slash-command + keybinding registries, hook injection, and a
//!   `lazy.gagent` UX for install / update / disable.
//! - **Phase C (planned)** — remote git sources with a local cache.
//!
//! This crate stays pure-data on purpose: no `tokio`, no UI deps. The
//! TUI (and any future headless harness) decides how to consume the
//! discovered plugin set.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// On-disk shape of `plugin.toml`. Fields beyond `name` are optional
/// and decay to empty strings so a minimal plugin can ship just
/// `name = "..."` on day one.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginManifest {
    /// Human-readable plugin id. Should match the containing directory
    /// name (we don't enforce it, but a mismatch reads poorly in the
    /// startup log).
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
}

/// One discovered plugin: parsed manifest + the directory it lives in.
/// Methods derive the per-convention subdirectory paths and only
/// return them when the directory actually exists on disk.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: PluginManifest,
    pub root: PathBuf,
}

impl Plugin {
    /// `<root>/skills/` if it exists. Caller hands the path to
    /// `find_skills(...)` alongside the primary `--skills-dir`.
    pub fn skills_dir(&self) -> Option<PathBuf> {
        let p = self.root.join("skills");
        p.is_dir().then_some(p)
    }

    /// `<root>/themes/` if it exists. Caller hands the path to
    /// `load_user_themes(...)` alongside the primary `--themes-dir`.
    pub fn themes_dir(&self) -> Option<PathBuf> {
        let p = self.root.join("themes");
        p.is_dir().then_some(p)
    }

    /// `<root>/scripts/` if it exists. Phase B will iterate these
    /// through `grain_script_boa::BoaExtension::from_scripts_dir`;
    /// Phase A only exposes the path for downstream wiring.
    pub fn scripts_dir(&self) -> Option<PathBuf> {
        let p = self.root.join("scripts");
        p.is_dir().then_some(p)
    }
}

/// Default `<workspace>/.grain/plugins/` location. Mirrors the
/// convention used by `themes`, `sessions`, `providers.toml`, etc.
pub fn default_plugins_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".grain").join("plugins")
}

/// Scan `plugins_dir` for `<name>/plugin.toml` and return one [`Plugin`]
/// per parsed manifest. Subdirectories without `plugin.toml`, with an
/// unreadable manifest, or with leading `.` / `_` (treated as cache /
/// scratch) are skipped — corruption in one plugin should never block
/// the others.
///
/// Missing `plugins_dir` returns `Vec::new()` so callers can use the
/// path as "create on first install".
pub fn discover_plugins(plugins_dir: &Path) -> Vec<Plugin> {
    let read_dir = match std::fs::read_dir(plugins_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!(
                "[warn] plugins: read_dir {} ({e})",
                plugins_dir.display()
            );
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Skip hidden and `_cache`-style scratch directories.
        let stem = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if stem.is_empty() || stem.starts_with('.') || stem.starts_with('_') {
            continue;
        }
        let manifest_path = path.join("plugin.toml");
        if !manifest_path.is_file() {
            continue;
        }
        match parse_manifest(&manifest_path) {
            Ok(manifest) => out.push(Plugin {
                manifest,
                root: path,
            }),
            Err(e) => {
                eprintln!(
                    "[warn] plugins: skipping {} ({e})",
                    manifest_path.display()
                );
            }
        }
    }
    // Sort alphabetically by name so startup logs are deterministic
    // (and the `/plugins` overlay in Phase B can rely on the order).
    out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    out
}

/// Parse a `plugin.toml` file. Returns the manifest verbatim — caller
/// decides how to handle empty `name` etc.
pub fn parse_manifest(path: &Path) -> std::io::Result<PluginManifest> {
    let raw = std::fs::read_to_string(path)?;
    toml::from_str(&raw).map_err(|e| std::io::Error::other(format!("manifest parse: {e}")))
}

/// One-line summary suitable for the startup log: `plugin 'name'
/// (skills: 3, themes: 1, scripts: 2)`. Counts are derived by
/// shallow-scanning the convention subdirectories — we don't validate
/// individual skill / theme files here (that happens when the existing
/// `find_skills` / `load_user_themes` paths re-walk the same dirs).
pub fn summarize_plugin(plugin: &Plugin) -> String {
    let info = plugin_info(plugin);
    format!(
        "plugin '{}' (skills: {}, themes: {}, scripts: {})",
        info.name, info.skills, info.themes, info.scripts
    )
}

/// Serializable plugin summary suitable for IPC between the worker
/// task and the TUI thread (or any future headless lister). Carries
/// manifest metadata plus a shallow count of each convention
/// subdirectory's entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// Absolute path to the plugin's root directory (where `plugin.toml`
    /// lives). Useful for the `/plugins` overlay's "open in editor" /
    /// "reveal in finder" actions in later phases.
    pub root: PathBuf,
    pub skills: usize,
    pub themes: usize,
    pub scripts: usize,
}

/// Derive a [`PluginInfo`] snapshot from a [`Plugin`]. Counts come from
/// a shallow `read_dir` of the convention subdirectories — entries
/// that fail to validate later (malformed `SKILL.md`, broken theme
/// TOML) still get counted here.
pub fn plugin_info(plugin: &Plugin) -> PluginInfo {
    let count_dir = |dir: Option<PathBuf>| -> usize {
        let Some(d) = dir else {
            return 0;
        };
        std::fs::read_dir(&d)
            .map(|r| r.flatten().count())
            .unwrap_or(0)
    };
    PluginInfo {
        name: plugin.manifest.name.clone(),
        version: plugin.manifest.version.clone(),
        description: plugin.manifest.description.clone(),
        author: plugin.manifest.author.clone(),
        root: plugin.root.clone(),
        skills: count_dir(plugin.skills_dir()),
        themes: count_dir(plugin.themes_dir()),
        scripts: count_dir(plugin.scripts_dir()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_manifest(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("plugin.toml");
        let mut f = std::fs::File::create(&p).unwrap();
        write!(
            f,
            "name = \"{name}\"\n{body}"
        )
        .unwrap();
        p
    }

    #[test]
    fn missing_plugins_dir_returns_empty() {
        let p = discover_plugins(Path::new("/tmp/grain-nonexistent-plugins-xyz-12345"));
        assert!(p.is_empty());
    }

    #[test]
    fn empty_dir_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let p = discover_plugins(tmp.path());
        assert!(p.is_empty());
    }

    #[test]
    fn discovers_well_formed_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("hello");
        write_manifest(&plugin_root, "hello", "version = \"0.1.0\"\n");
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.name, "hello");
        assert_eq!(plugins[0].manifest.version, "0.1.0");
        assert_eq!(plugins[0].root, plugin_root);
    }

    #[test]
    fn skips_subdirs_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        // Directory present but no plugin.toml → ignored.
        std::fs::create_dir_all(tmp.path().join("not-a-plugin")).unwrap();
        write_manifest(&tmp.path().join("real"), "real", "");
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.name, "real");
    }

    #[test]
    fn skips_hidden_and_cache_directories() {
        let tmp = tempfile::tempdir().unwrap();
        write_manifest(&tmp.path().join(".hidden"), "hidden", "");
        write_manifest(&tmp.path().join("_cache"), "cache", "");
        write_manifest(&tmp.path().join("visible"), "visible", "");
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.name, "visible");
    }

    #[test]
    fn skips_plugin_with_malformed_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("bad");
        std::fs::create_dir_all(&plugin_root).unwrap();
        std::fs::write(plugin_root.join("plugin.toml"), "this is = not = toml ===")
            .unwrap();
        write_manifest(&tmp.path().join("good"), "good", "");
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.name, "good");
    }

    #[test]
    fn sorts_plugins_alphabetically_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_manifest(&tmp.path().join("zebra"), "zebra", "");
        write_manifest(&tmp.path().join("alpha"), "alpha", "");
        write_manifest(&tmp.path().join("mango"), "mango", "");
        let plugins = discover_plugins(tmp.path());
        assert_eq!(
            plugins.iter().map(|p| p.manifest.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "mango", "zebra"]
        );
    }

    #[test]
    fn plugin_subdirs_are_detected_only_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("with-skills");
        write_manifest(&plugin_root, "with-skills", "");
        std::fs::create_dir_all(plugin_root.join("skills")).unwrap();
        // themes/ deliberately absent.
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert!(plugins[0].skills_dir().is_some());
        assert!(plugins[0].themes_dir().is_none());
        assert!(plugins[0].scripts_dir().is_none());
    }

    #[test]
    fn default_plugins_dir_matches_grain_convention() {
        assert_eq!(
            default_plugins_dir(Path::new("/workspace")),
            PathBuf::from("/workspace/.grain/plugins")
        );
    }

    #[test]
    fn summarize_counts_entries_in_each_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("counted");
        write_manifest(&plugin_root, "counted", "");
        std::fs::create_dir_all(plugin_root.join("skills/a")).unwrap();
        std::fs::create_dir_all(plugin_root.join("skills/b")).unwrap();
        std::fs::create_dir_all(plugin_root.join("themes")).unwrap();
        std::fs::write(plugin_root.join("themes").join("ocean.toml"), "").unwrap();
        let plugins = discover_plugins(tmp.path());
        let summary = summarize_plugin(&plugins[0]);
        assert!(summary.contains("skills: 2"), "{summary}");
        assert!(summary.contains("themes: 1"), "{summary}");
        assert!(summary.contains("scripts: 0"), "{summary}");
    }

    #[test]
    fn plugin_info_carries_metadata_and_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("info-test");
        write_manifest(
            &plugin_root,
            "info-test",
            "version = \"1.2.3\"\ndescription = \"hello\"\nauthor = \"alice\"\n",
        );
        std::fs::create_dir_all(plugin_root.join("skills/x")).unwrap();
        let plugins = discover_plugins(tmp.path());
        let info = plugin_info(&plugins[0]);
        assert_eq!(info.name, "info-test");
        assert_eq!(info.version, "1.2.3");
        assert_eq!(info.description, "hello");
        assert_eq!(info.author, "alice");
        assert_eq!(info.root, plugin_root);
        assert_eq!(info.skills, 1);
        assert_eq!(info.themes, 0);
        assert_eq!(info.scripts, 0);
    }

    #[test]
    fn plugin_info_roundtrips_through_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("serde-test");
        write_manifest(&plugin_root, "serde-test", "");
        let plugins = discover_plugins(tmp.path());
        let info = plugin_info(&plugins[0]);
        let s = toml::to_string(&info).expect("serialize");
        let back: PluginInfo = toml::from_str(&s).expect("deserialize");
        assert_eq!(info, back);
    }
}
