//! Persisted TUI preferences — small TOML file at
//! `<workspace>/.grain/tui-state.toml` capturing user state that
//! should survive a process restart (active theme, etc.).
//!
//! Kept intentionally small: this file is **preferences**, not
//! configuration. Anything that belongs in `--theme` / `--provider` /
//! `--model` flags stays a flag. We only persist state that the user
//! changed at runtime and would expect to find unchanged next time.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Serialized shape of `tui-state.toml`. New fields land here as
/// `#[serde(default)]` so we stay forward-compatible across upgrades.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedState {
    /// Last-applied theme name (built-in or user-defined). `None`
    /// when nothing was saved yet → caller falls back to CLI default.
    #[serde(default)]
    pub last_theme: Option<String>,
    /// Last-applied provider profile name. Survives restarts so the
    /// user doesn't have to re-select via `/provider` every time.
    #[serde(default)]
    pub last_provider: Option<String>,
    /// Last-applied model id (e.g. `deepseek/deepseek-chat`). Set
    /// when the user picks a model via `/model`; restored on next
    /// launch when no explicit `--model` flag is passed.
    #[serde(default)]
    pub last_model: Option<String>,
}

impl PersistedState {
    /// Read + parse the file at `path`. Missing file / parse errors
    /// downgrade to `PersistedState::default()` with a `[warn]` line —
    /// a corrupt state file should never block a TUI launch.
    pub fn load(path: &Path) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                eprintln!("[warn] tui-state read {}: {e}", path.display());
                return Self::default();
            }
        };
        match toml::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[warn] tui-state parse {}: {e}", path.display());
                Self::default()
            }
        }
    }

    /// Write the state to `path`, creating parent directories as
    /// needed. Returns the I/O error untouched — callers usually
    /// downgrade to a stderr warning.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let serialized = toml::to_string_pretty(self)
            .map_err(|e| io::Error::other(format!("serialize tui-state: {e}")))?;
        std::fs::write(path, serialized)
    }
}

/// Canonical persistence path inside a workspace:
/// `<workspace>/.grain/tui-state.toml`.
pub fn default_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".grain").join("tui-state.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_returns_default() {
        let s = PersistedState::load(Path::new(
            "/tmp/grain-nonexistent-tui-state-xyz-123.toml",
        ));
        assert_eq!(s, PersistedState::default());
    }

    #[test]
    fn roundtrip_preserves_theme() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tui-state.toml");
        let original = PersistedState {
            last_theme: Some("dracula".into()),
            ..PersistedState::default()
        };
        original.save(&path).unwrap();
        let loaded = PersistedState::load(&path);
        assert_eq!(loaded.last_theme.as_deref(), Some("dracula"));
    }

    #[test]
    fn save_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deep").join("subdir").join("state.toml");
        let s = PersistedState {
            last_theme: Some("nord".into()),
            ..PersistedState::default()
        };
        s.save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn malformed_toml_returns_default_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.toml");
        std::fs::write(&path, "not = valid = toml = ===").unwrap();
        let s = PersistedState::load(&path);
        assert_eq!(s, PersistedState::default());
    }

    #[test]
    fn empty_file_loads_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.toml");
        std::fs::write(&path, "").unwrap();
        let s = PersistedState::load(&path);
        assert_eq!(s, PersistedState::default());
    }

    #[test]
    fn default_path_matches_workspace_convention() {
        let p = default_path(Path::new("/workspace"));
        assert_eq!(
            p,
            PathBuf::from("/workspace/.grain/tui-state.toml")
        );
    }
}
