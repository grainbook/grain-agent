//! Plugin system — manifest format, discovery, and integration into
//! the agent boot path (skills / system prompt / scripts).
//!
//! # Mental model
//!
//! Think Neovim + lazy.nvim: this module is "Neovim" — the engine
//! that loads plugins from a known directory and merges their
//! contributions into the agent's runtime. A separate `lazy-gagent`
//! crate (and future installs under `<workspace>/.grain/plugins/
//! lazy-gagent/`) is a *plugin* that runs on top of this system; it
//! manages other plugins. Headless deliberately does not know about
//! lazy-gagent — that's the "user's init file" job, not the engine's.
//!
//! # Convention
//!
//! A **plugin** is a directory under `<workspace>/.grain/plugins/<name>/`
//! containing a `plugin.toml` manifest. Its contents extend the
//! agent's built-in catalogs by directory convention:
//!
//! ```text
//! <plugins_dir>/<name>/
//!   plugin.toml              # required — identifies the plugin
//!   skills/<skill>/SKILL.md  # optional — merged into find_skills
//!   themes/<theme>.toml      # optional — picked up by the TUI
//!   prompts/*.md             # optional — appended to system prompt
//!   scripts/*.js             # optional — Boa scripts (needs `scripts-boa`)
//! ```
//!
//! # Phase status
//!
//! - **Phase A** — discovery + façade over `find_skills` and themes.
//! - **Phase B** — skill / theme / system-prompt / script integration
//!   (this module today).
//! - **Phase C** — runtime install / update / remove via the
//!   `lazy-gagent` plugin (not yet shipped).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hooks::HookRule;
use crate::plugin_ui::{SlashCommand, UiCommand};
use crate::skills::{SkillsError, find_skills, find_skills_in_dirs};
use grain_agent_harness::Skill;

// ---------------------------------------------------------------------------
// Manifest + discovery
// ---------------------------------------------------------------------------

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
    /// Declarative UI extensions. Each entry registers a footer hint
    /// and key binding on an existing overlay (today: `"plugins"`);
    /// pressing the key dispatches to the named Rhai handler.
    ///
    /// TOML form (note `[[ui_command]]`, singular, to match
    /// `array-of-tables` convention):
    ///
    /// ```toml
    /// [[ui_command]]
    /// target  = "plugins"
    /// key     = "i"
    /// label   = "Install"
    /// handler = "ui_install_prompt"
    /// ```
    #[serde(default, rename = "ui_command")]
    pub ui_commands: Vec<UiCommand>,
    /// Declarative slash-command takeovers. Each entry binds a
    /// `/<trigger>` slash to a Rhai handler — when the user types
    /// it, the TUI dispatches into the plugin instead of the
    /// built-in slash table. Plugin entries override built-ins
    /// with the same trigger.
    ///
    /// ```toml
    /// [[slash_command]]
    /// trigger     = "plugins"
    /// description = "Plugin manager (lazy.gagent)"
    /// handler     = "ui_plugins_panel"
    /// ```
    #[serde(default, rename = "slash_command")]
    pub slash_commands: Vec<SlashCommand>,
    /// Declarative runtime hooks contributed by this plugin.
    #[serde(default, rename = "hook")]
    pub hooks: Vec<HookRule>,
    /// Optional WebAssembly Component Model configuration. When
    /// present (or when `<root>/plugin.wasm` exists on disk), the
    /// plugin engine loads the `.wasm` module and registers the
    /// tools it exports via the `grain:plugin` WIT world.
    ///
    /// ```toml
    /// [wasm]
    /// module       = "plugin.wasm"
    /// capabilities = ["log"]
    /// ```
    #[serde(default)]
    pub wasm: Option<WasmConfig>,
}

/// WebAssembly plugin configuration block inside `plugin.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WasmConfig {
    /// Path to the `.wasm` module, relative to the plugin root.
    /// Defaults to `"plugin.wasm"`.
    #[serde(default = "default_wasm_module")]
    pub module: PathBuf,
    /// Subset of host capabilities the plugin needs. Host calls
    /// into anything not listed here return an error.
    /// Valid: `["log", "env", "http"]`. Default: `["log"]` only.
    #[serde(default = "default_wasm_capabilities")]
    pub capabilities: Vec<String>,
}

fn default_wasm_module() -> PathBuf {
    PathBuf::from("plugin.wasm")
}

fn default_wasm_capabilities() -> Vec<String> {
    vec!["log".to_string()]
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
    /// `<root>/skills/` if it exists.
    pub fn skills_dir(&self) -> Option<PathBuf> {
        let p = self.root.join("skills");
        p.is_dir().then_some(p)
    }

    /// `<root>/themes/` if it exists.
    pub fn themes_dir(&self) -> Option<PathBuf> {
        let p = self.root.join("themes");
        p.is_dir().then_some(p)
    }

    /// `<root>/scripts/` if it exists. Fed into
    /// `grain_script_boa::BoaExtension::from_scripts_dirs` alongside
    /// the primary scripts dir.
    pub fn scripts_dir(&self) -> Option<PathBuf> {
        let p = self.root.join("scripts");
        p.is_dir().then_some(p)
    }

    /// `<root>/prompts/` if it exists. Walked by
    /// [`read_plugin_prompt_fragments`] to find `*.md` system-prompt
    /// extensions.
    pub fn prompts_dir(&self) -> Option<PathBuf> {
        let p = self.root.join("prompts");
        p.is_dir().then_some(p)
    }

    /// Resolved path to the plugin's `.wasm` module, if one exists.
    ///
    /// Resolution order:
    /// 1. If `[wasm]` is set in the manifest, use `<root>/<wasm.module>`.
    /// 2. Otherwise, fall back to `<root>/plugin.wasm`.
    ///
    /// Returns `Some(path)` only when the file actually exists on disk.
    pub fn wasm_module(&self) -> Option<PathBuf> {
        let path = match &self.manifest.wasm {
            Some(cfg) => self.root.join(&cfg.module),
            None => self.root.join("plugin.wasm"),
        };
        path.is_file().then_some(path)
    }

    /// Declared wasm capabilities. Falls back to `["log"]` when no
    /// `[wasm]` block is present.
    pub fn wasm_capabilities(&self) -> Vec<String> {
        match &self.manifest.wasm {
            Some(cfg) => cfg.capabilities.clone(),
            None => default_wasm_capabilities(),
        }
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
            eprintln!("[warn] plugins: read_dir {} ({e})", plugins_dir.display());
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
        let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
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
                eprintln!("[warn] plugins: skipping {} ({e})", manifest_path.display());
            }
        }
    }
    // Sort alphabetically by name so startup logs are deterministic
    // (and the `/plugins` overlay can rely on the order).
    out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    out
}

/// Parse a `plugin.toml` file.
pub fn parse_manifest(path: &Path) -> std::io::Result<PluginManifest> {
    let raw = std::fs::read_to_string(path)?;
    toml::from_str(&raw).map_err(|e| std::io::Error::other(format!("manifest parse: {e}")))
}

/// Discover both:
/// 1. Plugins installed under `plugins_dir` (the existing
///    `discover_plugins` filesystem walk — git-cloned plugins +
///    any hand-placed dirs + legacy symlinks).
/// 2. Plugins declared as **local sources** in `spec` — these live
///    at their `src` path on disk and intentionally have no entry
///    under `plugins_dir` (no filesystem side-effect, no symlink).
///    The engine reads them directly from `src`.
///
/// `base_dir` is used to anchor relative local-source paths,
/// matching the rule [`crate::plugin_spec::sync_plugins`] uses.
///
/// Returns the union, sorted alphabetically by manifest name.
/// Duplicate names (filesystem entry + spec entry) prefer the
/// filesystem version — that lets users locally override a
/// git-installed plugin by `rm -rf`ing the installed dir + adding
/// a local-src entry, without losing the spec entry's intent.
pub fn discover_plugins_with_spec(
    plugins_dir: &Path,
    spec: &crate::plugin_spec::PluginSpecFile,
    base_dir: &Path,
) -> Vec<Plugin> {
    use crate::plugin_spec::{SourceKind, resolve_local_src};

    let mut out = discover_plugins(plugins_dir);
    for entry in &spec.plugins {
        if !matches!(entry.resolved_kind(), SourceKind::Local) {
            continue;
        }
        if out.iter().any(|p| p.manifest.name == entry.name) {
            // Filesystem already has this name — fs wins.
            continue;
        }
        let resolved = match resolve_local_src(&entry.src, base_dir) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "[warn] plugin '{}' local src {}: {e}",
                    entry.name, entry.src
                );
                continue;
            }
        };
        let manifest_path = resolved.join("plugin.toml");
        match parse_manifest(&manifest_path) {
            Ok(manifest) => out.push(Plugin {
                manifest,
                root: resolved,
            }),
            Err(e) => {
                eprintln!(
                    "[warn] plugin '{}' manifest at {}: {e}",
                    entry.name,
                    manifest_path.display()
                );
            }
        }
    }
    out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    out
}

// ---------------------------------------------------------------------------
// Summaries
// ---------------------------------------------------------------------------

/// Serializable plugin summary for IPC between the worker task and
/// the TUI thread (or any future headless lister). Carries manifest
/// metadata plus a shallow count of each convention subdirectory's
/// entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// Absolute path to the plugin's root directory (where
    /// `plugin.toml` lives). Useful for the `/plugins` overlay's
    /// "open in editor" / "reveal in finder" actions in later phases.
    pub root: PathBuf,
    pub skills: usize,
    pub themes: usize,
    pub scripts: usize,
    pub prompts: usize,
    /// Whether the plugin has a `.wasm` module on disk.
    #[serde(default)]
    pub wasm: bool,
}

/// Derive a [`PluginInfo`] snapshot from a [`Plugin`].
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
        prompts: count_dir(plugin.prompts_dir()),
        wasm: plugin.wasm_module().is_some(),
    }
}

/// One-line summary for the startup log.
pub fn summarize_plugin(plugin: &Plugin) -> String {
    let info = plugin_info(plugin);
    let wasm_tag = if info.wasm { ", wasm: yes" } else { "" };
    format!(
        "plugin '{}' (skills: {}, themes: {}, scripts: {}, prompts: {}{})",
        info.name, info.skills, info.themes, info.scripts, info.prompts, wasm_tag
    )
}

// ---------------------------------------------------------------------------
// Integration helpers
// ---------------------------------------------------------------------------

/// Scan `primary_dir` for skills, then fold in each plugin's
/// `<plugin>/skills/` directory (when present). Plugin skills append
/// in discovery order (alphabetical by manifest name). One broken
/// plugin emits a `[warn]` line and is skipped — never breaks the rest.
pub fn find_skills_with_plugins(
    primary_dir: &Path,
    plugins: &[Plugin],
) -> Result<Vec<Skill>, SkillsError> {
    let mut out = find_skills(primary_dir)?;
    for plugin in plugins {
        let Some(d) = plugin.skills_dir() else {
            continue;
        };
        match find_skills(&d) {
            Ok(extra) => out.extend(extra),
            Err(e) => {
                eprintln!("[warn] plugin '{}' skills scan: {e}", plugin.manifest.name);
            }
        }
    }
    Ok(out)
}

pub fn find_skills_in_dirs_with_plugins(
    primary_dirs: &[PathBuf],
    plugins: &[Plugin],
) -> Result<Vec<Skill>, SkillsError> {
    let mut out = find_skills_in_dirs(primary_dirs)?;
    for plugin in plugins {
        let Some(d) = plugin.skills_dir() else {
            continue;
        };
        match find_skills(&d) {
            Ok(extra) => out.extend(extra),
            Err(e) => {
                eprintln!("[warn] plugin '{}' skills scan: {e}", plugin.manifest.name);
            }
        }
    }
    Ok(out)
}

/// One markdown prompt fragment loaded from a plugin's `prompts/`
/// directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptFragment {
    pub plugin_name: String,
    pub source: PathBuf,
    pub body: String,
}

impl PromptFragment {
    /// Banner line composed above the body in the final prompt.
    pub fn banner(&self) -> String {
        format!("## Plugin: {}", self.plugin_name)
    }
}

/// Read each plugin's `<plugin>/prompts/*.md` files (sorted) and
/// return one [`PromptFragment`] per file.
pub fn read_plugin_prompt_fragments(plugins: &[Plugin]) -> Vec<PromptFragment> {
    let mut out = Vec::new();
    for plugin in plugins {
        let Some(dir) = plugin.prompts_dir() else {
            continue;
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                eprintln!(
                    "[warn] plugin '{}' prompts read_dir {}: {e}",
                    plugin.manifest.name,
                    dir.display()
                );
                continue;
            }
        };
        let mut md_files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
            .collect();
        md_files.sort();
        for path in md_files {
            match std::fs::read_to_string(&path) {
                Ok(body) => out.push(PromptFragment {
                    plugin_name: plugin.manifest.name.clone(),
                    source: path.clone(),
                    body,
                }),
                Err(e) => {
                    eprintln!(
                        "[warn] plugin '{}' prompt read {}: {e}",
                        plugin.manifest.name,
                        path.display()
                    );
                }
            }
        }
    }
    out
}

/// Append all plugin prompt fragments to `base`, separated by a
/// `## Plugin: <name>` banner + blank line. Returns the new string;
/// if no plugins contributed anything, returns `base` verbatim.
pub fn compose_system_prompt_with_plugins(base: &str, plugins: &[Plugin]) -> String {
    let fragments = read_plugin_prompt_fragments(plugins);
    if fragments.is_empty() {
        return base.to_string();
    }
    let mut out = String::with_capacity(base.len() + 256);
    out.push_str(base);
    for f in &fragments {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&f.banner());
        out.push_str("\n\n");
        out.push_str(&f.body);
        if !f.body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Return each plugin's existing `scripts/` directory. Caller hands
/// the slice straight to
/// `grain_script_boa::BoaExtension::from_scripts_dirs(...)`.
pub fn plugin_script_dirs(plugins: &[Plugin]) -> Vec<PathBuf> {
    plugins.iter().filter_map(|p| p.scripts_dir()).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        write!(f, "{body}").unwrap();
    }

    fn write_manifest(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("plugin.toml");
        write_file(&p, &format!("name = \"{name}\"\n{body}"));
        p
    }

    fn write_plugin(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        write_manifest(&dir, name, "");
        dir
    }

    fn write_skill(plugin_root: &Path, name: &str, body: &str) {
        let dir = plugin_root.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        write_file(
            &dir.join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: {body}\n---\n\nbody\n"),
        );
    }

    #[test]
    fn missing_plugins_dir_returns_empty() {
        let p = discover_plugins(Path::new("/tmp/grain-nonexistent-plugins-xyz-12345"));
        assert!(p.is_empty());
    }

    #[test]
    fn discovers_well_formed_plugin_with_full_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("hello");
        write_manifest(
            &plugin_root,
            "hello",
            "version = \"0.1.0\"\ndescription = \"hi\"\nauthor = \"alice\"\n",
        );
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.name, "hello");
        assert_eq!(plugins[0].manifest.version, "0.1.0");
        assert_eq!(plugins[0].manifest.description, "hi");
        assert_eq!(plugins[0].manifest.author, "alice");
    }

    #[test]
    fn skips_subdirs_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("not-a-plugin")).unwrap();
        write_plugin(tmp.path(), "real");
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.name, "real");
    }

    #[test]
    fn skips_hidden_and_cache_directories() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin(tmp.path(), ".hidden");
        write_plugin(tmp.path(), "_cache");
        write_plugin(tmp.path(), "visible");
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.name, "visible");
    }

    #[test]
    fn skips_plugin_with_malformed_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("bad");
        std::fs::create_dir_all(&plugin_root).unwrap();
        std::fs::write(plugin_root.join("plugin.toml"), "this is = not = toml ===").unwrap();
        write_plugin(tmp.path(), "good");
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.name, "good");
    }

    #[test]
    fn sorts_plugins_alphabetically_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin(tmp.path(), "zebra");
        write_plugin(tmp.path(), "alpha");
        write_plugin(tmp.path(), "mango");
        let plugins = discover_plugins(tmp.path());
        assert_eq!(
            plugins
                .iter()
                .map(|p| p.manifest.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "mango", "zebra"]
        );
    }

    #[test]
    fn convention_subdirs_only_detected_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = write_plugin(tmp.path(), "with-skills");
        std::fs::create_dir_all(plugin_root.join("skills")).unwrap();
        let plugins = discover_plugins(tmp.path());
        assert!(plugins[0].skills_dir().is_some());
        assert!(plugins[0].themes_dir().is_none());
        assert!(plugins[0].scripts_dir().is_none());
        assert!(plugins[0].prompts_dir().is_none());
    }

    #[test]
    fn default_plugins_dir_matches_grain_convention() {
        assert_eq!(
            default_plugins_dir(Path::new("/workspace")),
            PathBuf::from("/workspace/.grain/plugins")
        );
    }

    #[test]
    fn plugin_info_carries_metadata_and_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = write_plugin(tmp.path(), "info-test");
        std::fs::create_dir_all(plugin_root.join("skills/x")).unwrap();
        let plugins = discover_plugins(tmp.path());
        let info = plugin_info(&plugins[0]);
        assert_eq!(info.name, "info-test");
        assert_eq!(info.skills, 1);
        assert_eq!(info.themes, 0);
        assert_eq!(info.scripts, 0);
        assert_eq!(info.prompts, 0);
    }

    #[test]
    fn plugin_info_roundtrips_through_toml() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin(tmp.path(), "rt");
        let plugins = discover_plugins(tmp.path());
        let info = plugin_info(&plugins[0]);
        let s = toml::to_string(&info).expect("serialize");
        let back: PluginInfo = toml::from_str(&s).expect("deserialize");
        assert_eq!(info, back);
    }

    #[test]
    fn summarize_includes_all_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = write_plugin(tmp.path(), "counted");
        std::fs::create_dir_all(plugin_root.join("skills/a")).unwrap();
        std::fs::create_dir_all(plugin_root.join("themes")).unwrap();
        std::fs::write(plugin_root.join("themes").join("ocean.toml"), "").unwrap();
        let plugins = discover_plugins(tmp.path());
        let summary = summarize_plugin(&plugins[0]);
        assert!(summary.contains("skills: 1"), "{summary}");
        assert!(summary.contains("themes: 1"), "{summary}");
        assert!(summary.contains("scripts: 0"), "{summary}");
        assert!(summary.contains("prompts: 0"), "{summary}");
    }

    #[test]
    fn find_skills_with_plugins_concatenates_primary_and_plugin_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("primary");
        std::fs::create_dir_all(primary.join("alpha")).unwrap();
        write_file(
            &primary.join("alpha").join("SKILL.md"),
            "---\nname: alpha\ndescription: primary\n---\n\nbody\n",
        );
        let plugins_dir = tmp.path().join("plugins");
        let p1 = write_plugin(&plugins_dir, "p1");
        write_skill(&p1, "beta", "from p1");
        let plugins = discover_plugins(&plugins_dir);
        let skills = find_skills_with_plugins(&primary, &plugins).unwrap();
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn read_plugin_prompt_fragments_picks_up_md_files_in_sort_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = write_plugin(tmp.path(), "p1");
        write_file(&root.join("prompts").join("01-intro.md"), "intro body");
        write_file(&root.join("prompts").join("02-extra.md"), "extra body");
        write_file(&root.join("prompts").join("README.txt"), "ignore");
        let plugins = discover_plugins(tmp.path());
        let frags = read_plugin_prompt_fragments(&plugins);
        assert_eq!(frags.len(), 2);
        assert_eq!(frags[0].body, "intro body");
        assert_eq!(frags[1].body, "extra body");
    }

    #[test]
    fn compose_system_prompt_returns_base_when_no_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = discover_plugins(tmp.path());
        let out = compose_system_prompt_with_plugins("base policy", &plugins);
        assert_eq!(out, "base policy");
    }

    #[test]
    fn compose_system_prompt_appends_fragments_with_banner() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_plugin(tmp.path(), "rust-helper");
        write_file(&p.join("prompts").join("rules.md"), "always run clippy");
        let plugins = discover_plugins(tmp.path());
        let out = compose_system_prompt_with_plugins("base policy\n", &plugins);
        assert!(out.contains("base policy"));
        assert!(out.contains("\n## Plugin: rust-helper\n"));
        assert!(out.contains("always run clippy"));
    }

    #[test]
    fn manifest_parses_ui_command_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("uihost");
        write_manifest(
            &plugin_root,
            "uihost",
            r#"
[[ui_command]]
target = "plugins"
key = "i"
label = "Install"
handler = "ui_install_prompt"

[[ui_command]]
target = "plugins"
key = "d"
label = "Remove"
handler = "ui_remove_prompt"
"#,
        );
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        let cmds = &plugins[0].manifest.ui_commands;
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].key, "i");
        assert_eq!(cmds[0].handler, "ui_install_prompt");
        assert_eq!(cmds[1].key, "d");
        assert_eq!(cmds[1].handler, "ui_remove_prompt");
    }

    #[test]
    fn manifest_without_ui_commands_yields_empty_vec() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin(tmp.path(), "noui");
        let plugins = discover_plugins(tmp.path());
        assert!(plugins[0].manifest.ui_commands.is_empty());
    }

    #[test]
    fn manifest_parses_slash_command_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("slashhost");
        write_manifest(
            &plugin_root,
            "slashhost",
            r#"
[[slash_command]]
trigger     = "plugins"
description = "Plugin manager"
handler     = "ui_plugins_panel"

[[slash_command]]
trigger     = "lazy"
description = "Lazy debug panel"
handler     = "ui_lazy_panel"
"#,
        );
        let plugins = discover_plugins(tmp.path());
        let slashes = &plugins[0].manifest.slash_commands;
        assert_eq!(slashes.len(), 2);
        assert_eq!(slashes[0].trigger, "plugins");
        assert_eq!(slashes[0].handler, "ui_plugins_panel");
        assert_eq!(slashes[1].trigger, "lazy");
    }

    #[test]
    fn collect_slash_commands_walks_every_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let pa = tmp.path().join("alpha");
        write_manifest(
            &pa,
            "alpha",
            r#"
[[slash_command]]
trigger     = "a"
description = "alpha"
handler     = "h_a"
"#,
        );
        let pb = tmp.path().join("beta");
        write_manifest(
            &pb,
            "beta",
            r#"
[[slash_command]]
trigger     = "b"
description = "beta"
handler     = "h_b"
"#,
        );
        let plugins = discover_plugins(tmp.path());
        let collected = crate::plugin_ui::collect_slash_commands(&plugins);
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].plugin_name, "alpha");
        assert_eq!(collected[0].command.trigger, "a");
        assert_eq!(collected[1].plugin_name, "beta");
    }

    #[test]
    fn plugin_script_dirs_returns_existing_subdirs_only() {
        let tmp = tempfile::tempdir().unwrap();
        let p1 = write_plugin(tmp.path(), "p1");
        std::fs::create_dir_all(p1.join("scripts")).unwrap();
        write_plugin(tmp.path(), "p2"); // no scripts/
        let plugins = discover_plugins(tmp.path());
        let dirs = plugin_script_dirs(&plugins);
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].ends_with("p1/scripts"));
    }
}
