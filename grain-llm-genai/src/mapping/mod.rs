//! Translation between `grain-agent-core` types and `genai` chat types.
//!
//! Split by direction:
//! - [`outbound`] — `LlmContext` → `genai::chat::ChatRequest`.
//! - [`inbound`] — `genai::chat::ChatStreamEvent` → `AssistantMessageEvent`,
//!   accumulating an `AssistantMessage` as events flow.
//! - [`usage`] — shared scalar conversions (usage tokens, stop reason).
//!
//! Each direction is a pure function / state machine with no I/O so they can
//! be exercised by unit tests without spinning up an LLM.

pub mod inbound;
pub mod outbound;
pub mod usage;
