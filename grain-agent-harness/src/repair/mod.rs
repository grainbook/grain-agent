//! Tool-call repair hooks — provider-agnostic shock absorbers for the
//! agent loop. Currently exposes:
//!
//! - [`storm`] — sliding-window dedup that catches a model stuck in a
//!   tight loop calling the same tool with the same args. Wires into
//!   the loop via [`grain_agent_core::BeforeToolCallFn`].
//!
//! Future siblings (planned, not yet implemented): `flatten` (deep tool
//! schemas → dot-notation) and provider-specific scavenges that live
//! in companion crates like `grain-deepseek-pack`.

pub mod storm;

pub use storm::{StormConfig, storm_hook};
