//! Translation between `grain-agent-core` types and `genai` chat types.
//!
//! Split by direction:
//! - [`outbound`] — `LlmContext` → `genai::chat::ChatRequest` (PR 3a).
//! - **`inbound`** — `genai::chat::ChatStreamEvent` → `AssistantMessageEvent` (PR 3b).
//!
//! Each direction is a pure function with no I/O so it can be exercised by
//! unit tests without spinning up an LLM.

pub mod outbound;
