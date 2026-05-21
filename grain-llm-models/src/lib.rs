//! `grain-llm-models` — standardized LLM model registry for the grain-agent stack.
//!
//! Mirrors the role of `models.dev` integration in `@earendil-works/pi-ai`:
//! a single source of truth for context window, capability flags, pricing,
//! and provider-specific quirks (thinking / reasoning fields) so callers
//! don't have to hard-code per-model knowledge.
//!
//! - [`descriptor`] — normalized data types.
//! - [`registry`] — in-memory lookup with conversion to [`grain_agent_core::Model`].
//! - [`snapshot`] — embedded vendored snapshot loaded at startup; refreshable
//!   via a separate `refresh-models` binary (added in a follow-up).
//!
//! The vendored snapshot lives at `data/models-dev.json` and is checked into
//! the repository. `cargo build` is **never** allowed to depend on the network.

pub mod descriptor;
pub mod registry;
pub mod snapshot;

pub use descriptor::{
    ApiKind, Capabilities, ModelDescriptor, ProviderId, ThinkingProfile,
};
pub use registry::{Registry, RegistryError};
pub use snapshot::{EMBEDDED_SNAPSHOT_JSON, Snapshot, SnapshotError};
