//! Vendored snapshot loader.
//!
//! The snapshot lives at `data/models-dev.json` (sibling of `Cargo.toml`) and
//! is checked into the repository. It is **never** fetched at build time —
//! that would break offline builds. The forthcoming `refresh-models` binary
//! is the only way to regenerate it.
//!
//! Snapshot schema is intentionally simple:
//!
//! ```json
//! { "version": 1, "models": [ { /* ModelDescriptor */ } ] }
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::descriptor::ModelDescriptor;

/// Raw JSON for the embedded snapshot. Exposed so tests can re-parse without
/// touching the filesystem.
pub const EMBEDDED_SNAPSHOT_JSON: &str = include_str!("../data/models-dev.json");

/// Current snapshot schema version. Bump alongside any breaking JSON change.
pub const CURRENT_SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    /// Schema version — refuse to load unrecognized versions.
    pub version: u32,
    pub models: Vec<ModelDescriptor>,
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot JSON is invalid: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported snapshot version {found}; expected {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
}

impl Snapshot {
    /// Parse [`EMBEDDED_SNAPSHOT_JSON`].
    pub fn from_embedded() -> Result<Self, SnapshotError> {
        Self::from_json_str(EMBEDDED_SNAPSHOT_JSON)
    }

    /// Parse arbitrary snapshot JSON.
    pub fn from_json_str(s: &str) -> Result<Self, SnapshotError> {
        let snapshot: Snapshot = serde_json::from_str(s)?;
        if snapshot.version != CURRENT_SNAPSHOT_VERSION {
            return Err(SnapshotError::UnsupportedVersion {
                found: snapshot.version,
                expected: CURRENT_SNAPSHOT_VERSION,
            });
        }
        Ok(snapshot)
    }
}
