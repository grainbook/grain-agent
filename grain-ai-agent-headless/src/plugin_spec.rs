//! Plugin spec file — `<workspace>/.grain/plugin-spec.toml`.
//!
//! Declarative plugin manifest read at startup. Each entry says
//! "I want plugin `<name>` available, sourced from `<src>`". The
//! engine syncs missing plugins (git clone for URLs, symlink for
//! local paths) **before** [`crate::plugins::discover_plugins`]
//! runs, so subsequent boot uses them as if they had been hand-
//! placed under `<workspace>/.grain/plugins/<name>/`.
//!
//! This is the bootstrap mechanism that lets the plugin manager
//! (`lazy-gagent`, Phase C-1+) live as just another plugin without
//! a chicken-and-egg problem — list it in the spec like anything
//! else.
//!
//! ```toml
//! # <workspace>/.grain/plugin-spec.toml
//!
//! [[plugin]]
//! name = "rust-helper"
//! src  = "https://github.com/me/rust-helper.git"
//! rev  = "v1.0.0"          # optional — default branch otherwise
//!
//! [[plugin]]
//! name = "local-dev"
//! src  = "/Users/me/dev/my-plugin"  # auto-detected as local → symlink
//!
//! [[plugin]]
//! name = "lazy-gagent"
//! src  = "git@github.com:me/lazy-gagent.git"
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// On-disk wrapper around the `[[plugin]]` array.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSpecFile {
    #[serde(default, rename = "plugin")]
    pub plugins: Vec<PluginSpec>,
}

/// One plugin entry in the spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSpec {
    /// Directory name under `<plugins_dir>/<name>/`. Must match the
    /// plugin's `plugin.toml` `name` field at runtime — we don't
    /// enforce parity here so the spec can be authored before the
    /// plugin is fetched.
    pub name: String,
    /// Source identifier: a git URL or a local filesystem path.
    /// Auto-detected by [`detect_source_kind`] unless [`Self::kind`]
    /// overrides.
    pub src: String,
    /// Git ref (tag, branch, or commit). Honored only when the
    /// resolved [`SourceKind`] is `Git`. `None` → leave the cloned
    /// repo on its default branch.
    #[serde(default)]
    pub rev: Option<String>,
    /// Force the source treatment. `None` → auto-detect from
    /// [`Self::src`].
    #[serde(default)]
    pub kind: Option<SourceKind>,
}

/// How the engine treats [`PluginSpec::src`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// Treat `src` as a git URL — `git clone src target` then
    /// optionally `git checkout rev`.
    Git,
    /// Treat `src` as a filesystem path — symlink it into
    /// `<plugins_dir>/<name>/` so source-tree edits show up live
    /// without an explicit copy step.
    Local,
}

impl PluginSpec {
    /// The resolved [`SourceKind`] — explicit `kind` if set, else
    /// auto-detected from [`Self::src`].
    pub fn resolved_kind(&self) -> SourceKind {
        self.kind.unwrap_or_else(|| detect_source_kind(&self.src))
    }
}

/// Heuristic: a `src` looks like a git URL when it begins with a
/// known scheme (`http://`, `https://`, `git@`, `ssh://`) or ends in
/// `.git`. Everything else is treated as a local path.
pub fn detect_source_kind(src: &str) -> SourceKind {
    let lower = src.to_ascii_lowercase();
    if lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("git@")
        || lower.starts_with("ssh://")
        || lower.starts_with("git://")
        || lower.ends_with(".git")
    {
        SourceKind::Git
    } else {
        SourceKind::Local
    }
}

/// Read + parse `plugin-spec.toml`. Missing file → `Ok(empty spec)`
/// so callers can use the path as "create on first declared plugin".
/// Malformed TOML returns the parser error verbatim.
pub fn load_plugin_spec(path: &Path) -> std::io::Result<PluginSpecFile> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PluginSpecFile::default());
        }
        Err(e) => return Err(e),
    };
    toml::from_str::<PluginSpecFile>(&raw)
        .map_err(|e| std::io::Error::other(format!("spec parse: {e}")))
}

/// Default location for the spec file: `<workspace>/.grain/plugin-spec.toml`.
pub fn default_spec_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".grain").join("plugin-spec.toml")
}

/// Outcome of [`sync_plugins`]. Each plugin name lands in exactly
/// one of the three buckets.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// Newly created under `<plugins_dir>/`.
    pub installed: Vec<String>,
    /// Already had a directory under `<plugins_dir>/` — left alone.
    /// (Re-install requires manual removal first.)
    pub skipped: Vec<String>,
    /// Failed to install. `(name, reason)` pairs.
    pub failed: Vec<(String, String)>,
}

impl SyncReport {
    pub fn is_empty(&self) -> bool {
        self.installed.is_empty() && self.skipped.is_empty() && self.failed.is_empty()
    }

    /// Stream the report onto stderr as one line per installed /
    /// failed plugin (skipped plugins are silent — that's the normal
    /// case after the first boot).
    pub fn log_to_stderr(&self) {
        for name in &self.installed {
            eprintln!("[info] installed plugin '{name}'");
        }
        for (name, reason) in &self.failed {
            eprintln!("[warn] plugin '{name}' install failed: {reason}");
        }
    }
}

/// Apply the spec against `plugins_dir`. For each declared plugin
/// whose directory doesn't already exist, git-clone (URL sources) or
/// symlink (local paths). Existing directories are left untouched —
/// the engine never auto-removes or overwrites user data.
///
/// Returns a per-name report; never fails as a whole (one bad source
/// shouldn't block the others).
pub fn sync_plugins(spec: &PluginSpecFile, plugins_dir: &Path) -> SyncReport {
    if let Err(e) = std::fs::create_dir_all(plugins_dir) {
        // Can't create the parent — every plugin is going to fail
        // with the same root cause; report the error once.
        let mut r = SyncReport::default();
        for p in &spec.plugins {
            r.failed.push((
                p.name.clone(),
                format!("create_dir_all {}: {e}", plugins_dir.display()),
            ));
        }
        return r;
    }
    let mut report = SyncReport::default();
    for p in &spec.plugins {
        if p.name.is_empty() {
            report
                .failed
                .push(("(empty)".into(), "name is empty".into()));
            continue;
        }
        if p.name.contains('/') || p.name.contains('\\') || p.name.contains("..") {
            report
                .failed
                .push((p.name.clone(), format!("name {:?} contains path separator", p.name)));
            continue;
        }
        let target = plugins_dir.join(&p.name);
        // `exists()` matches symlinks (broken or live), regular dirs,
        // and files. All three count as "already installed — leave it".
        if target.symlink_metadata().is_ok() {
            report.skipped.push(p.name.clone());
            continue;
        }
        let outcome = match p.resolved_kind() {
            SourceKind::Git => clone_git(&p.src, p.rev.as_deref(), &target),
            SourceKind::Local => symlink_local(&p.src, &target),
        };
        match outcome {
            Ok(()) => report.installed.push(p.name.clone()),
            Err(e) => report.failed.push((p.name.clone(), e)),
        }
    }
    report
}

fn clone_git(src: &str, rev: Option<&str>, target: &Path) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .arg("clone")
        .arg(src)
        .arg(target)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git clone {src}: {}", stderr.trim()));
    }
    if let Some(rev) = rev {
        let output = std::process::Command::new("git")
            .current_dir(target)
            .arg("checkout")
            .arg(rev)
            .output()
            .map_err(|e| format!("spawn git checkout: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git checkout {rev}: {}", stderr.trim()));
        }
    }
    Ok(())
}

fn symlink_local(src: &str, target: &Path) -> Result<(), String> {
    let resolved = expand_tilde(src);
    let canonical = resolved
        .canonicalize()
        .map_err(|e| format!("local path {}: {e}", resolved.display()))?;
    if !canonical.is_dir() {
        return Err(format!("local path is not a directory: {}", canonical.display()));
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&canonical, target)
            .map_err(|e| format!("symlink {} → {}: {e}", target.display(), canonical.display()))
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(&canonical, target)
            .map_err(|e| format!("symlink {} → {}: {e}", target.display(), canonical.display()))
    }
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn detect_source_kind_recognizes_git_urls() {
        assert_eq!(
            detect_source_kind("https://github.com/x/y.git"),
            SourceKind::Git
        );
        assert_eq!(detect_source_kind("https://example.com/x"), SourceKind::Git);
        assert_eq!(detect_source_kind("git@github.com:x/y.git"), SourceKind::Git);
        assert_eq!(detect_source_kind("ssh://git@host/x.git"), SourceKind::Git);
        assert_eq!(detect_source_kind("git://host/x.git"), SourceKind::Git);
    }

    #[test]
    fn detect_source_kind_recognizes_local_paths() {
        assert_eq!(detect_source_kind("/abs/path"), SourceKind::Local);
        assert_eq!(detect_source_kind("./relative"), SourceKind::Local);
        assert_eq!(detect_source_kind("../parent"), SourceKind::Local);
        assert_eq!(detect_source_kind("~/home"), SourceKind::Local);
        assert_eq!(detect_source_kind("just-a-name"), SourceKind::Local);
    }

    #[test]
    fn explicit_kind_overrides_auto_detection() {
        let spec = PluginSpec {
            name: "x".into(),
            src: "https://example.com/x.git".into(),
            rev: None,
            kind: Some(SourceKind::Local),
        };
        assert_eq!(spec.resolved_kind(), SourceKind::Local);
    }

    #[test]
    fn load_missing_spec_returns_empty() {
        let s = load_plugin_spec(Path::new("/tmp/grain-nonexistent-spec-xyz-123.toml")).unwrap();
        assert!(s.plugins.is_empty());
    }

    #[test]
    fn load_parses_full_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("spec.toml");
        std::fs::write(
            &path,
            r#"
[[plugin]]
name = "alpha"
src = "https://github.com/x/alpha.git"

[[plugin]]
name = "beta"
src = "/home/me/beta"
rev = "main"
"#,
        )
        .unwrap();
        let s = load_plugin_spec(&path).unwrap();
        assert_eq!(s.plugins.len(), 2);
        assert_eq!(s.plugins[0].name, "alpha");
        assert_eq!(s.plugins[0].resolved_kind(), SourceKind::Git);
        assert_eq!(s.plugins[1].name, "beta");
        assert_eq!(s.plugins[1].resolved_kind(), SourceKind::Local);
        assert_eq!(s.plugins[1].rev.as_deref(), Some("main"));
    }

    #[test]
    fn malformed_spec_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.toml");
        std::fs::write(&path, "this = not = valid = toml = ==").unwrap();
        assert!(load_plugin_spec(&path).is_err());
    }

    #[test]
    fn default_spec_path_under_grain() {
        assert_eq!(
            default_spec_path(Path::new("/workspace")),
            PathBuf::from("/workspace/.grain/plugin-spec.toml")
        );
    }

    #[test]
    fn sync_skips_already_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        std::fs::create_dir_all(plugins_dir.join("preinstalled")).unwrap();
        // Source path is bogus but won't be touched because target exists.
        let spec = PluginSpecFile {
            plugins: vec![PluginSpec {
                name: "preinstalled".into(),
                src: "/does/not/exist".into(),
                rev: None,
                kind: None,
            }],
        };
        let report = sync_plugins(&spec, &plugins_dir);
        assert_eq!(report.installed, Vec::<String>::new());
        assert_eq!(report.skipped, vec!["preinstalled".to_string()]);
        assert!(report.failed.is_empty());
    }

    #[test]
    fn sync_rejects_names_with_path_separators() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        let spec = PluginSpecFile {
            plugins: vec![
                PluginSpec {
                    name: "../escape".into(),
                    src: "/whatever".into(),
                    rev: None,
                    kind: None,
                },
                PluginSpec {
                    name: "a/b".into(),
                    src: "/whatever".into(),
                    rev: None,
                    kind: None,
                },
            ],
        };
        let report = sync_plugins(&spec, &plugins_dir);
        assert_eq!(report.failed.len(), 2);
        assert!(report.failed[0].1.contains("path separator"));
    }

    #[test]
    fn sync_symlinks_local_source() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let mut f = std::fs::File::create(source.join("plugin.toml")).unwrap();
        writeln!(f, "name = \"linked\"").unwrap();
        let plugins_dir = tmp.path().join("plugins");
        let spec = PluginSpecFile {
            plugins: vec![PluginSpec {
                name: "linked".into(),
                src: source.to_string_lossy().into_owned(),
                rev: None,
                kind: None,
            }],
        };
        let report = sync_plugins(&spec, &plugins_dir);
        assert_eq!(report.installed, vec!["linked".to_string()]);
        assert!(report.failed.is_empty());
        // Target should now be a symlink resolving to the source.
        let target = plugins_dir.join("linked");
        assert!(target.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(target.join("plugin.toml").exists());
    }

    #[test]
    fn sync_fails_when_local_source_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        let spec = PluginSpecFile {
            plugins: vec![PluginSpec {
                name: "missing".into(),
                src: tmp.path().join("does-not-exist").to_string_lossy().into_owned(),
                rev: None,
                kind: None,
            }],
        };
        let report = sync_plugins(&spec, &plugins_dir);
        assert!(report.installed.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "missing");
    }

    #[test]
    fn sync_fails_when_local_source_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file_src = tmp.path().join("notadir");
        std::fs::write(&file_src, "").unwrap();
        let plugins_dir = tmp.path().join("plugins");
        let spec = PluginSpecFile {
            plugins: vec![PluginSpec {
                name: "filey".into(),
                src: file_src.to_string_lossy().into_owned(),
                rev: None,
                kind: None,
            }],
        };
        let report = sync_plugins(&spec, &plugins_dir);
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].1.contains("not a directory"));
    }

    #[test]
    fn sync_report_log_to_stderr_does_not_panic() {
        let r = SyncReport {
            installed: vec!["a".into()],
            skipped: vec!["b".into()],
            failed: vec![("c".into(), "oops".into())],
        };
        r.log_to_stderr();
    }
}
