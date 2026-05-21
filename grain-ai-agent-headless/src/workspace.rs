//! Root-anchored path validator.
//!
//! Every tool that touches the filesystem resolves user-supplied paths
//! through [`Workspace`] so the agent can't read outside the workspace root.
//! `Workspace::resolve` canonicalizes the input (so symlinks and `..`
//! segments are flattened) and then confirms the result still lives under
//! the canonicalized root.

use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace root does not exist or is not a directory: {0}")]
    RootMissing(PathBuf),
    #[error("path escapes the workspace root: {0}")]
    Escape(String),
    #[error("path not found: {0}")]
    NotFound(String),
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Workspace root. Cheap to clone (single `PathBuf`).
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Build a `Workspace` from a path that must exist and be a directory.
    ///
    /// The path is canonicalized — symlinks are followed once at construction
    /// time so subsequent containment checks compare against a stable
    /// canonical form.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let root = root.into();
        let canon = root.canonicalize().map_err(|source| WorkspaceError::Io {
            path: root.display().to_string(),
            source,
        })?;
        if !canon.is_dir() {
            return Err(WorkspaceError::RootMissing(canon));
        }
        Ok(Workspace { root: canon })
    }

    /// Canonicalized workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a (relative or absolute) path against the workspace root.
    ///
    /// Returns the canonicalized absolute path on success. Errors:
    /// - `NotFound` if the path doesn't exist.
    /// - `Escape` if the resolved canonical path is outside the workspace
    ///   root (e.g. a symlink pointing out of the tree, or `..` escapes).
    pub fn resolve(&self, path: &str) -> Result<PathBuf, WorkspaceError> {
        let raw = Path::new(path);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        };
        let canon = match joined.canonicalize() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(WorkspaceError::NotFound(path.to_string()));
            }
            Err(source) => {
                return Err(WorkspaceError::Io {
                    path: path.to_string(),
                    source,
                });
            }
        };
        if !canon.starts_with(&self.root) {
            return Err(WorkspaceError::Escape(path.to_string()));
        }
        Ok(canon)
    }

    /// Resolve a path for writing: the file itself need not exist, but the
    /// **parent directory** must exist and live inside the workspace.
    ///
    /// Returns the absolute target path (parent canonicalized + file name
    /// appended). Refuses paths that would land on the workspace root.
    pub fn resolve_for_write(&self, path: &str) -> Result<PathBuf, WorkspaceError> {
        let raw = Path::new(path);
        let abs = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        };
        let parent = abs
            .parent()
            .ok_or_else(|| WorkspaceError::Escape(path.to_string()))?;
        let name = abs
            .file_name()
            .ok_or_else(|| WorkspaceError::Escape(path.to_string()))?;

        let parent_canon = parent.canonicalize().map_err(|source| WorkspaceError::Io {
            path: parent.display().to_string(),
            source,
        })?;
        if !parent_canon.starts_with(&self.root) {
            return Err(WorkspaceError::Escape(path.to_string()));
        }
        Ok(parent_canon.join(name))
    }

    /// Render a workspace-relative display path for tool output. Falls back
    /// to the absolute path if the input isn't actually inside the root.
    pub fn display_relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string())
    }
}
