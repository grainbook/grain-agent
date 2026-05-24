//! Plugin-manager operations — install / update / remove on top of
//! the engine's [`crate::plugin_spec`] machinery.
//!
//! These are the **mutating** primitives that any plugin-manager UX
//! (the TUI's slash commands, a future `lazy-gagent` plugin's JS /
//! Rhai scripts, a CLI binary) calls into. The engine owns them so
//! that:
//!
//! 1. Multiple front-ends don't reimplement file edits + git spawn.
//! 2. The `lazy-gagent` crate isn't a privileged Cargo dep of the
//!    TUI — it's a plugin (directory) like any other.
//!
//! See [`crate::plugin_spec`] for the underlying spec-file shape +
//! syncing rules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::plugin_spec::{
    PluginSpec, PluginSpecFile, SyncReport, load_plugin_spec, sync_plugins,
};

/// Outcome of [`install`]. Combines the spec-file write with the
/// per-plugin sync result — callers usually only care that the new
/// plugin name shows up in `report.installed`.
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    /// Result of the post-write `sync_plugins(...)` call. The
    /// freshly-added entry should appear in `installed` on success
    /// or `failed` on a clone / symlink error.
    pub report: SyncReport,
}

/// Errors from the plugin manager. Sync errors aren't here — those
/// land in `InstallOutcome.report.failed` so a partial install can
/// still surface the spec write that succeeded.
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("read spec {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("write spec {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("serialize spec: {0}")]
    Serialize(String),
    #[error("plugin '{0}' already declared in spec")]
    AlreadyExists(String),
    #[error("plugin '{0}' not declared in spec")]
    NotFound(String),
    #[error("plugin name '{0}' contains a path separator")]
    BadName(String),
    #[error("git pull in {path}: {reason}")]
    GitPull { path: PathBuf, reason: String },
    #[error("remove {path}: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("spawn git: {0}")]
    SpawnGit(std::io::Error),
}

/// Add a `[[plugin]]` entry to `spec_path` and run a sync pass so
/// the new plugin is materialized under `plugins_dir`.
///
/// `src` follows the same rules as the spec file:
/// - URL-like → git clone
/// - filesystem path → symlink (relative paths anchored at the
///   spec file's parent directory)
///
/// `rev` is honored only for git sources. `kind` is auto-detected.
///
/// Returns the [`SyncReport`] from the engine; on success the new
/// name appears in `report.installed`. Failures from the underlying
/// `git clone` / symlink (e.g. network down, missing local source)
/// land in `report.failed` — the spec write itself succeeded, so a
/// retry just needs the network back.
pub fn install(
    spec_path: &Path,
    plugins_dir: &Path,
    name: &str,
    src: &str,
    rev: Option<&str>,
) -> Result<InstallOutcome, ManagerError> {
    validate_name(name)?;
    let mut spec = read_spec(spec_path)?;
    if spec.plugins.iter().any(|p| p.name == name) {
        return Err(ManagerError::AlreadyExists(name.into()));
    }
    spec.plugins.push(PluginSpec {
        name: name.into(),
        src: src.into(),
        rev: rev.map(str::to_string),
        kind: None,
        auth: Vec::new(),
    });
    write_spec(spec_path, &spec)?;
    let base_dir = spec_path.parent().unwrap_or_else(|| Path::new("."));
    let report = sync_plugins(&spec, plugins_dir, base_dir);
    Ok(InstallOutcome { report })
}

/// Refresh an installed plugin in place. Behavior depends on how the
/// plugin was sourced:
///
/// - **Symlink** (local source) — no-op. The link already points at
///   a live source tree; the user is the one editing it.
/// - **Git clone** — runs `git pull` in `<plugins_dir>/<name>/` with
///   the same `GIT_TERMINAL_PROMPT=0` / closed-stdin guards as the
///   install path, so a missing credential fails fast instead of
///   hanging on `/dev/tty`.
///
/// `name` must be present under `plugins_dir`. The spec file is not
/// consulted — `update` only touches the installed directory.
pub fn update(plugins_dir: &Path, name: &str) -> Result<UpdateOutcome, ManagerError> {
    validate_name(name)?;
    let target = plugins_dir.join(name);
    let meta = target
        .symlink_metadata()
        .map_err(|_| ManagerError::NotFound(name.into()))?;
    if meta.file_type().is_symlink() {
        return Ok(UpdateOutcome::Symlink);
    }
    let output = std::process::Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .current_dir(&target)
        .arg("pull")
        .output()
        .map_err(ManagerError::SpawnGit)?;
    if !output.status.success() {
        return Err(ManagerError::GitPull {
            path: target,
            reason: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(UpdateOutcome::Pulled)
}

/// Drop a `[[plugin]]` entry from `spec_path` and optionally remove
/// the installed directory under `plugins_dir`.
///
/// `delete_files`:
/// - `false` — only edit the spec file. Useful when the user wants
///   to keep their local edits around in `<plugins_dir>/<name>/`
///   but stop the engine from auto-loading.
/// - `true` — also tear down the installed directory. Symlinks are
///   unlinked (the source tree is left untouched); real directories
///   are `rm -rf`'d.
pub fn remove(
    spec_path: &Path,
    plugins_dir: &Path,
    name: &str,
    delete_files: bool,
) -> Result<RemoveOutcome, ManagerError> {
    validate_name(name)?;
    let mut spec = read_spec(spec_path)?;
    let before = spec.plugins.len();
    spec.plugins.retain(|p| p.name != name);
    if spec.plugins.len() == before {
        return Err(ManagerError::NotFound(name.into()));
    }
    write_spec(spec_path, &spec)?;

    let mut files_removed = false;
    if delete_files {
        let target = plugins_dir.join(name);
        if let Ok(meta) = target.symlink_metadata() {
            let path = target.clone();
            let result = if meta.file_type().is_symlink() {
                std::fs::remove_file(&target)
            } else {
                std::fs::remove_dir_all(&target)
            };
            result.map_err(|source| ManagerError::Remove { path, source })?;
            files_removed = true;
        }
    }
    Ok(RemoveOutcome {
        spec_updated: true,
        files_removed,
    })
}

/// What [`update`] actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Source was a symlink — nothing to pull; user's edits to the
    /// linked tree are already visible.
    Symlink,
    /// `git pull` succeeded.
    Pulled,
}

/// What [`remove`] actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveOutcome {
    /// The spec file no longer mentions this plugin.
    pub spec_updated: bool,
    /// The installed directory under `plugins_dir` was deleted.
    /// `false` when `delete_files=false` was passed, or when the dir
    /// didn't exist in the first place.
    pub files_removed: bool,
}

// ----- internal helpers ------------------------------------------------

fn validate_name(name: &str) -> Result<(), ManagerError> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ManagerError::BadName(name.into()));
    }
    Ok(())
}

fn read_spec(path: &Path) -> Result<PluginSpecFile, ManagerError> {
    load_plugin_spec(path).map_err(|source| ManagerError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn write_spec(path: &Path, spec: &PluginSpecFile) -> Result<(), ManagerError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ManagerError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    }
    let serialized =
        toml::to_string_pretty(spec).map_err(|e| ManagerError::Serialize(e.to_string()))?;
    std::fs::write(path, serialized).map_err(|source| ManagerError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write_file(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn install_appends_to_spec_and_syncs_local_source() {
        let tmp = make_workspace();
        let grain = tmp.path().join(".grain");
        std::fs::create_dir_all(&grain).unwrap();
        let spec_path = grain.join("plugin-spec.toml");
        let plugins_dir = grain.join("plugins");

        let source = tmp.path().join("source-plugin");
        write_file(&source.join("plugin.toml"), "name = \"source-plugin\"\n");

        let outcome = install(
            &spec_path,
            &plugins_dir,
            "source-plugin",
            source.to_str().unwrap(),
            None,
        )
        .unwrap();
        assert!(outcome.report.installed.contains(&"source-plugin".to_string()));
        let parsed = load_plugin_spec(&spec_path).unwrap();
        assert_eq!(parsed.plugins.len(), 1);
        // Local plugins are virtual — sync validates the source path
        // but doesn't create any entry under `plugins_dir`. The
        // engine reads them from `src` directly via
        // `discover_plugins_with_spec`.
        assert!(plugins_dir.join("source-plugin").symlink_metadata().is_err());
    }

    #[test]
    fn install_refuses_duplicate_name() {
        let tmp = make_workspace();
        let spec_path = tmp.path().join(".grain").join("plugin-spec.toml");
        write_file(&spec_path, "[[plugin]]\nname = \"x\"\nsrc = \"/whatever\"\n");
        let err = install(
            &spec_path,
            &tmp.path().join(".grain").join("plugins"),
            "x",
            "/another",
            None,
        )
        .err()
        .unwrap();
        assert!(matches!(err, ManagerError::AlreadyExists(ref n) if n == "x"));
    }

    #[test]
    fn install_rejects_bad_names() {
        let tmp = make_workspace();
        let spec_path = tmp.path().join("plugin-spec.toml");
        let plugins_dir = tmp.path().join("plugins");
        for bad in ["", "a/b", "..", "..foo"] {
            let err = install(&spec_path, &plugins_dir, bad, "/whatever", None)
                .err()
                .unwrap();
            assert!(matches!(err, ManagerError::BadName(_)), "{bad:?}");
        }
    }

    #[test]
    fn remove_drops_spec_entry_and_keeps_files_by_default() {
        let tmp = make_workspace();
        let spec_path = tmp.path().join(".grain").join("plugin-spec.toml");
        let plugins_dir = tmp.path().join(".grain").join("plugins");
        write_file(&spec_path, "[[plugin]]\nname = \"x\"\nsrc = \"/whatever\"\n");
        std::fs::create_dir_all(plugins_dir.join("x")).unwrap();
        write_file(&plugins_dir.join("x").join("plugin.toml"), "name = \"x\"\n");

        let outcome = remove(&spec_path, &plugins_dir, "x", false).unwrap();
        assert!(outcome.spec_updated);
        assert!(!outcome.files_removed);
        let parsed = load_plugin_spec(&spec_path).unwrap();
        assert!(parsed.plugins.is_empty());
        assert!(plugins_dir.join("x").exists());
    }

    #[test]
    fn remove_with_delete_files_also_tears_down_dir() {
        let tmp = make_workspace();
        let spec_path = tmp.path().join(".grain").join("plugin-spec.toml");
        let plugins_dir = tmp.path().join(".grain").join("plugins");
        write_file(&spec_path, "[[plugin]]\nname = \"x\"\nsrc = \"/whatever\"\n");
        std::fs::create_dir_all(plugins_dir.join("x")).unwrap();
        write_file(&plugins_dir.join("x").join("plugin.toml"), "name = \"x\"\n");

        let outcome = remove(&spec_path, &plugins_dir, "x", true).unwrap();
        assert!(outcome.spec_updated);
        assert!(outcome.files_removed);
        assert!(!plugins_dir.join("x").exists());
    }

    #[test]
    fn remove_local_drops_spec_without_touching_source() {
        // Local plugins don't have a filesystem entry under
        // `plugins_dir`, so `remove(.., delete_files=true)` only
        // edits the spec. The source tree stays intact.
        let tmp = make_workspace();
        let spec_path = tmp.path().join(".grain").join("plugin-spec.toml");
        let plugins_dir = tmp.path().join(".grain").join("plugins");
        let source = tmp.path().join("source-x");
        write_file(&source.join("plugin.toml"), "name = \"x\"\n");
        install(&spec_path, &plugins_dir, "x", source.to_str().unwrap(), None).unwrap();
        let outcome = remove(&spec_path, &plugins_dir, "x", true).unwrap();
        // Spec entry dropped, but no files to clean (none were
        // created for the local install in the first place).
        assert!(outcome.spec_updated);
        assert!(!outcome.files_removed);
        assert!(plugins_dir.join("x").symlink_metadata().is_err());
        // Source tree intact.
        assert!(source.join("plugin.toml").exists());
    }

    #[test]
    fn remove_legacy_symlink_with_delete_files_unlinks_without_touching_source() {
        // Backward-compat: an existing workspace may have a symlink
        // from before the spec switched to virtual local installs.
        // `remove(.., true)` should still tear that link down.
        let tmp = make_workspace();
        let spec_path = tmp.path().join(".grain").join("plugin-spec.toml");
        let plugins_dir = tmp.path().join(".grain").join("plugins");
        let source = tmp.path().join("legacy-source");
        write_file(&source.join("plugin.toml"), "name = \"legacy\"\n");
        write_file(
            &spec_path,
            &format!(
                "[[plugin]]\nname = \"legacy\"\nsrc = \"{}\"\n",
                source.display()
            ),
        );
        std::fs::create_dir_all(&plugins_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, plugins_dir.join("legacy")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&source, plugins_dir.join("legacy")).unwrap();

        let outcome = remove(&spec_path, &plugins_dir, "legacy", true).unwrap();
        assert!(outcome.spec_updated);
        assert!(outcome.files_removed);
        assert!(plugins_dir.join("legacy").symlink_metadata().is_err());
        // Source tree intact.
        assert!(source.join("plugin.toml").exists());
    }

    #[test]
    fn remove_refuses_unknown_name() {
        let tmp = make_workspace();
        let spec_path = tmp.path().join(".grain").join("plugin-spec.toml");
        let plugins_dir = tmp.path().join(".grain").join("plugins");
        write_file(&spec_path, "[[plugin]]\nname = \"x\"\nsrc = \"/_\"\n");
        let err = remove(&spec_path, &plugins_dir, "y", false).err().unwrap();
        assert!(matches!(err, ManagerError::NotFound(ref n) if n == "y"));
    }

    #[test]
    fn update_local_install_returns_not_found_since_no_install_dir() {
        // Local plugins don't have an entry under `plugins_dir`, so
        // `update()` (which is filesystem-only) returns NotFound.
        // The TUI's `lazy_update` host fn can layer spec-aware
        // semantics on top if it wants a friendlier message.
        let tmp = make_workspace();
        let spec_path = tmp.path().join(".grain").join("plugin-spec.toml");
        let plugins_dir = tmp.path().join(".grain").join("plugins");
        let source = tmp.path().join("src");
        write_file(&source.join("plugin.toml"), "name = \"linked\"\n");
        install(&spec_path, &plugins_dir, "linked", source.to_str().unwrap(), None).unwrap();
        let err = update(&plugins_dir, "linked").err().unwrap();
        assert!(matches!(err, ManagerError::NotFound(_)));
    }

    #[test]
    fn update_legacy_symlink_returns_symlink_outcome() {
        // Backward-compat: pre-existing symlinks (from before local
        // plugins went virtual) still get the "live, no pull"
        // semantics from `update()`.
        let tmp = make_workspace();
        let plugins_dir = tmp.path().join(".grain").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, plugins_dir.join("legacy")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&source, plugins_dir.join("legacy")).unwrap();
        assert_eq!(update(&plugins_dir, "legacy").unwrap(), UpdateOutcome::Symlink);
    }

    #[test]
    fn update_returns_not_found_for_missing_plugin() {
        let tmp = make_workspace();
        let plugins_dir = tmp.path().join(".grain").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let err = update(&plugins_dir, "nope").err().unwrap();
        assert!(matches!(err, ManagerError::NotFound(_)));
    }

    #[test]
    fn install_then_remove_round_trips_spec() {
        let tmp = make_workspace();
        let spec_path = tmp.path().join(".grain").join("plugin-spec.toml");
        let plugins_dir = tmp.path().join(".grain").join("plugins");
        let source = tmp.path().join("src");
        write_file(&source.join("plugin.toml"), "name = \"roundtrip\"\n");
        install(&spec_path, &plugins_dir, "roundtrip", source.to_str().unwrap(), None).unwrap();
        remove(&spec_path, &plugins_dir, "roundtrip", true).unwrap();
        let spec = load_plugin_spec(&spec_path).unwrap();
        assert!(spec.plugins.is_empty());
        assert!(!plugins_dir.join("roundtrip").exists());
    }

}
