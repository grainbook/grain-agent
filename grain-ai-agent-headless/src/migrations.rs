//! Session-schema migrations. The session JSONL files live for as long
//! as the user keeps them around, so when we change the on-disk shape
//! we need a way to upgrade old sessions without forcing the user to
//! delete history. This module mirrors pi's `core/migrations.ts` at a
//! minimal-but-extensible level.
//!
//! Layout:
//! - A session directory carries an integer `schema_version` (in
//!   `meta.json`'s `extra` blob, key `schemaVersion`).
//! - `CURRENT_SCHEMA_VERSION` is the version everything in this build
//!   writes. Sessions older than that get walked through every
//!   registered migration in order; sessions newer than that are
//!   refused (forward-incompatible).
//! - Each [`Migration`] takes the `meta.json` + `entries.jsonl` paths
//!   and rewrites them as needed. Migrations are pure file operations;
//!   they don't open a `SessionStorage` because doing so would itself
//!   be subject to schema assumptions.
//!
//! v1 ships **no** migrations — `CURRENT_SCHEMA_VERSION = 1`. The
//! module is plumbed so future versions can add steps without
//! touching the surrounding code.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use thiserror::Error;

/// Schema version this build of `grain-headless` writes / understands.
/// Bump alongside any on-disk change to session files.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse error in {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "session at {path} has schema version {found}, which is newer than this build's version {expected}; refusing to load"
    )]
    Forward {
        path: String,
        found: u32,
        expected: u32,
    },
    #[error("migration {step} failed: {reason}")]
    Step { step: &'static str, reason: String },
}

/// One step from version `source` to version `source + 1`.
pub trait Migration: Send + Sync {
    /// Source version this migration upgrades from (target is `source + 1`).
    /// Named `source_version` (not `from_version`) to avoid tripping
    /// clippy's `wrong_self_convention` lint on `from_*` accessors.
    fn source_version(&self) -> u32;
    /// Human-readable name for logs / errors.
    fn name(&self) -> &'static str;
    /// Apply the migration to a session directory.
    fn run(&self, session_dir: &Path) -> Result<(), MigrationError>;
}

/// Read the schema version out of a session's `meta.json`. Missing /
/// absent → 0, treated as "old, pre-version-tracking".
pub fn schema_version_of(session_dir: &Path) -> Result<u32, MigrationError> {
    let meta_path = session_dir.join("meta.json");
    if !meta_path.exists() {
        return Ok(0);
    }
    let raw = fs::read_to_string(&meta_path).map_err(|source| MigrationError::Io {
        path: meta_path.display().to_string(),
        source,
    })?;
    let val: Value = serde_json::from_str(&raw).map_err(|source| MigrationError::Parse {
        path: meta_path.display().to_string(),
        source,
    })?;
    let v = val
        .get("schemaVersion")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    Ok(v)
}

/// Write the current schema version into a session's `meta.json`.
pub fn stamp_current_version(session_dir: &Path) -> Result<(), MigrationError> {
    let meta_path = session_dir.join("meta.json");
    let mut val: Value = if meta_path.exists() {
        let raw = fs::read_to_string(&meta_path).map_err(|source| MigrationError::Io {
            path: meta_path.display().to_string(),
            source,
        })?;
        serde_json::from_str(&raw).map_err(|source| MigrationError::Parse {
            path: meta_path.display().to_string(),
            source,
        })?
    } else {
        Value::Object(Default::default())
    };
    if let Some(obj) = val.as_object_mut() {
        obj.insert(
            "schemaVersion".into(),
            Value::Number(CURRENT_SCHEMA_VERSION.into()),
        );
    }
    fs::write(
        &meta_path,
        serde_json::to_string_pretty(&val).map_err(|source| MigrationError::Parse {
            path: meta_path.display().to_string(),
            source,
        })?,
    )
    .map_err(|source| MigrationError::Io {
        path: meta_path.display().to_string(),
        source,
    })?;
    Ok(())
}

/// Apply every needed migration to bring a session up to
/// [`CURRENT_SCHEMA_VERSION`]. No-op when already current. Refuses
/// forward-incompatible sessions.
pub fn migrate_session(
    session_dir: &Path,
    migrations: &[Box<dyn Migration>],
) -> Result<(), MigrationError> {
    let mut version = schema_version_of(session_dir)?;
    if version > CURRENT_SCHEMA_VERSION {
        return Err(MigrationError::Forward {
            path: session_dir.display().to_string(),
            found: version,
            expected: CURRENT_SCHEMA_VERSION,
        });
    }
    while version < CURRENT_SCHEMA_VERSION {
        let step = migrations
            .iter()
            .find(|m| m.source_version() == version)
            .ok_or_else(|| MigrationError::Step {
                step: "missing",
                reason: format!("no migration registered to upgrade from version {version}"),
            })?;
        step.run(session_dir)?;
        version += 1;
    }
    stamp_current_version(session_dir)?;
    Ok(())
}

/// Default (empty) migration table. v1 has no migrations to perform;
/// fresh sessions get `schemaVersion: 1` stamped on first save.
pub fn default_migrations() -> Vec<Box<dyn Migration>> {
    Vec::new()
}

/// Convenience: enumerate session sub-directories under a sessions root
/// and migrate each one. Returns the per-session results so callers can
/// report what was upgraded vs skipped vs failed.
pub fn migrate_all(
    sessions_root: &Path,
    migrations: &[Box<dyn Migration>],
) -> Vec<(PathBuf, Result<(), MigrationError>)> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(sessions_root) {
        Ok(e) => e,
        Err(source) => {
            out.push((
                sessions_root.to_path_buf(),
                Err(MigrationError::Io {
                    path: sessions_root.display().to_string(),
                    source,
                }),
            ));
            return out;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().map(|t| !t.is_dir()).unwrap_or(true) {
            continue;
        }
        let res = migrate_session(&path, migrations);
        out.push((path, res));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_meta_returns_version_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(schema_version_of(dir.path()).unwrap(), 0);
    }

    #[test]
    fn missing_field_returns_version_zero() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("meta.json"),
            "{\"id\": \"x\", \"createdAt\": \"2024-01-01T00:00:00.000Z\"}",
        )
        .unwrap();
        assert_eq!(schema_version_of(dir.path()).unwrap(), 0);
    }

    #[test]
    fn explicit_version_returned() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("meta.json"),
            "{\"id\":\"x\",\"createdAt\":\"\",\"schemaVersion\":3}",
        )
        .unwrap();
        assert_eq!(schema_version_of(dir.path()).unwrap(), 3);
    }

    #[test]
    fn stamp_writes_current_version() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("meta.json"), "{\"id\":\"x\"}").unwrap();
        stamp_current_version(dir.path()).unwrap();
        let v = schema_version_of(dir.path()).unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn forward_incompatible_session_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("meta.json"),
            format!(
                "{{\"id\":\"x\",\"schemaVersion\":{}}}",
                CURRENT_SCHEMA_VERSION + 1
            ),
        )
        .unwrap();
        let err = migrate_session(dir.path(), &[]).unwrap_err();
        assert!(matches!(err, MigrationError::Forward { .. }));
    }

    #[test]
    fn missing_migration_step_errors() {
        let dir = tempfile::tempdir().unwrap();
        // schemaVersion 0 → CURRENT_SCHEMA_VERSION (currently 1) requires
        // a step from 0; default_migrations() is empty so this errors.
        if CURRENT_SCHEMA_VERSION > 0 {
            fs::write(dir.path().join("meta.json"), "{\"id\":\"x\"}").unwrap();
            let err = migrate_session(dir.path(), &default_migrations()).unwrap_err();
            assert!(matches!(err, MigrationError::Step { step: "missing", .. }));
        }
    }

    #[test]
    fn current_session_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("meta.json"),
            format!(
                "{{\"id\":\"x\",\"schemaVersion\":{CURRENT_SCHEMA_VERSION}}}"
            ),
        )
        .unwrap();
        migrate_session(dir.path(), &default_migrations()).unwrap();
        assert_eq!(schema_version_of(dir.path()).unwrap(), CURRENT_SCHEMA_VERSION);
    }
}
