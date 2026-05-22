//! In-memory model registry.
//!
//! `Registry` is the lookup surface every adapter and harness hook talks to.
//! Construct it once at startup ([`Registry::from_embedded_snapshot`]) and
//! share via `Arc<Registry>`; mutation isn't supported by design — refresh
//! produces a new `Registry`.

use std::collections::HashMap;
use std::sync::Arc;

use grain_agent_core::Model;
use thiserror::Error;

use crate::descriptor::ModelDescriptor;
use crate::snapshot::{Snapshot, SnapshotError};

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("model not found: {0}")]
    NotFound(String),
    #[error("duplicate model id in source: {0}")]
    DuplicateId(String),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

/// Read-only model lookup.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    models: Arc<HashMap<String, ModelDescriptor>>,
}

impl Registry {
    /// Build from the embedded vendored snapshot.
    ///
    /// Panics only if the embedded JSON is malformed — that's a build-time
    /// bug, not a runtime failure mode.
    pub fn from_embedded_snapshot() -> Self {
        let snapshot = Snapshot::from_embedded()
            .expect("embedded models-dev.json must parse — refresh-models broke the snapshot");
        Self::from_snapshot(snapshot).expect("embedded snapshot must be unique by id")
    }

    /// Build from an arbitrary parsed [`Snapshot`].
    pub fn from_snapshot(snapshot: Snapshot) -> Result<Self, RegistryError> {
        Self::from_descriptors(snapshot.models)
    }

    /// Build from a flat list of descriptors.
    pub fn from_descriptors(
        items: impl IntoIterator<Item = ModelDescriptor>,
    ) -> Result<Self, RegistryError> {
        let mut models: HashMap<String, ModelDescriptor> = HashMap::new();
        for descriptor in items {
            if models.contains_key(&descriptor.id) {
                return Err(RegistryError::DuplicateId(descriptor.id));
            }
            models.insert(descriptor.id.clone(), descriptor);
        }
        Ok(Registry {
            models: Arc::new(models),
        })
    }

    /// Number of registered models.
    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// Look up a model by id (`"<provider>/<model>"`).
    pub fn lookup(&self, id: &str) -> Option<&ModelDescriptor> {
        self.models.get(id)
    }

    /// Return every model whose canonical provider id matches `provider`.
    /// E.g. `provider = "anthropic"` yields all ids starting with
    /// `"anthropic/"`.
    pub fn models_for_provider(&self, provider: &str) -> Vec<ModelDescriptor> {
        let prefix = format!("{provider}/");
        self.models
            .values()
            .filter(|d| d.id.starts_with(&prefix))
            .cloned()
            .collect()
    }

    /// Iterate over all registered descriptors.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ModelDescriptor)> {
        self.models.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Project a registered model into a [`grain_agent_core::Model`]; returns
    /// `None` when the id is unknown.
    pub fn to_core_model(&self, id: &str) -> Option<Model> {
        self.lookup(id).map(ModelDescriptor::to_core_model)
    }

    /// Merge another registry on top of this one. Entries in `other` overwrite
    /// existing entries with the same id; returns a fresh `Registry`.
    ///
    /// Intended for `runtime_fetch.merged_over(embedded)` flows added in a
    /// follow-up — pure data merge, no I/O.
    pub fn merged_with(&self, other: &Registry) -> Registry {
        let mut merged: HashMap<String, ModelDescriptor> =
            (*self.models).clone();
        for (k, v) in other.models.iter() {
            merged.insert(k.clone(), v.clone());
        }
        Registry {
            models: Arc::new(merged),
        }
    }
}
