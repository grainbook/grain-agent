//! `grain-agent-harness` — Rust port of the `@earendil-works/pi-agent-core` harness layer.
//!
//! Sits on top of [`grain-agent-core`](::grain_agent_core) and adds the engineering
//! plumbing needed to ship a real agent process:
//!
//! - [`session`] — typed session tree (entries, branches, leaf cursor) and an
//!   `InMemorySessionRepo`. JSONL persistence is intended to live alongside it
//!   under the same trait.
//! - [`messages`] — custom-message helpers (`branch_summary`, `compaction_summary`,
//!   `custom_message`) and the harness-aware `convert_to_llm`.
//! - [`system_prompt`] — assembles skill descriptors into the XML block injected
//!   into the system prompt.
//! - [`truncate`] — head/tail truncation utilities for tool output.
//!
//! What is NOT here yet (deliberately scoped out of this slice; see TS source):
//! - context compaction (`harness/compaction/*`)
//! - skills loading from disk (`harness/skills.ts`)
//! - execution environment (`harness/env/*`) and shell-output capture
//! - top-level `AgentHarness` constructor
//! - JSONL session storage

pub mod context_guard;
pub mod messages;
pub mod session;
pub mod system_prompt;
pub mod truncate;

pub use context_guard::{ContextGuard, ContextGuardPolicy, TokenEstimator};
pub use messages::{
    BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
    COMPACTION_SUMMARY_SUFFIX, BranchSummaryMessage, CompactionSummaryMessage, CustomMessage,
    branch_summary_message, compaction_summary_message, convert_to_llm, custom_message,
};
pub use session::{
    InMemorySessionRepo, InMemorySessionStorage, Session, SessionContext, SessionError,
    SessionMetadata, SessionRepo, SessionStorage, SessionTreeEntry, SessionTreeEntryKind,
    uuidv7,
};
pub use system_prompt::{Skill, format_skills_for_system_prompt};
pub use truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, GREP_MAX_LINE_LENGTH, TruncationOptions,
    TruncationResult, format_size, truncate_head, truncate_line, truncate_tail,
};
