//! Custom message types and harness `convert_to_llm`.
//!
//! Ports `packages/agent/src/harness/messages.ts`. The TS code uses
//! declaration-merging to add typed message variants to `CustomAgentMessages`;
//! in Rust, custom messages live under [`grain_agent_core::AgentMessage::Custom`]
//! as JSON values with a `role` discriminator.

use grain_agent_core::{AgentMessage, Message, TextContent, UserContent, UserMessage};
use serde::{Deserialize, Serialize};

pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    /// Always `"branchSummary"`.
    pub role: String,
    pub summary: String,
    pub from_id: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    /// Always `"compactionSummary"`.
    pub role: String,
    pub summary: String,
    pub tokens_before: u64,
    pub timestamp: i64,
}

/// Arbitrary user-defined message variant (e.g. a UI artifact).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    /// Always `"custom"`.
    pub role: String,
    pub custom_type: String,
    /// `String` for plain text, or an array of `UserContent` for rich content.
    pub content: serde_json::Value,
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub timestamp: i64,
}

/// Build an [`AgentMessage::Custom`] carrying a branch summary.
pub fn branch_summary_message(
    summary: impl Into<String>,
    from_id: impl Into<String>,
    timestamp: i64,
) -> AgentMessage {
    let msg = BranchSummaryMessage {
        role: "branchSummary".into(),
        summary: summary.into(),
        from_id: from_id.into(),
        timestamp,
    };
    AgentMessage::Custom(serde_json::to_value(msg).expect("branch summary serialises"))
}

/// Build an [`AgentMessage::Custom`] carrying a compaction summary.
pub fn compaction_summary_message(
    summary: impl Into<String>,
    tokens_before: u64,
    timestamp: i64,
) -> AgentMessage {
    let msg = CompactionSummaryMessage {
        role: "compactionSummary".into(),
        summary: summary.into(),
        tokens_before,
        timestamp,
    };
    AgentMessage::Custom(serde_json::to_value(msg).expect("compaction summary serialises"))
}

/// Build an [`AgentMessage::Custom`] carrying an arbitrary application payload.
///
/// `content` may be either a string or an array of `UserContent`.
pub fn custom_message(
    custom_type: impl Into<String>,
    content: serde_json::Value,
    display: bool,
    details: Option<serde_json::Value>,
    timestamp: i64,
) -> AgentMessage {
    let msg = CustomMessage {
        role: "custom".into(),
        custom_type: custom_type.into(),
        content,
        display,
        details,
        timestamp,
    };
    AgentMessage::Custom(serde_json::to_value(msg).expect("custom message serialises"))
}

fn parse_user_content(value: &serde_json::Value) -> Vec<UserContent> {
    if let Some(text) = value.as_str() {
        return vec![UserContent::Text(TextContent { text: text.into() })];
    }
    if value.is_array()
        && let Ok(parsed) = serde_json::from_value::<Vec<UserContent>>(value.clone())
    {
        return parsed;
    }
    Vec::new()
}

/// Harness-aware [`grain_agent_core::ConvertToLlmFn`] body.
///
/// Translates harness-specific custom-message variants into plain user messages
/// before they reach the LLM, then passes through standard messages.
pub fn convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message> {
    messages
        .into_iter()
        .filter_map(|m| match m {
            AgentMessage::Standard(m) => Some(m),
            AgentMessage::Custom(value) => convert_custom(value),
        })
        .collect()
}

fn convert_custom(value: serde_json::Value) -> Option<Message> {
    let role = value.get("role").and_then(|r| r.as_str())?;
    let timestamp = value.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0);

    match role {
        "branchSummary" => {
            let summary = value.get("summary").and_then(|s| s.as_str()).unwrap_or("");
            let text = format!(
                "{}{}{}",
                BRANCH_SUMMARY_PREFIX, summary, BRANCH_SUMMARY_SUFFIX
            );
            Some(Message::User(UserMessage {
                content: vec![UserContent::Text(TextContent { text })],
                timestamp,
            }))
        }
        "compactionSummary" => {
            let summary = value.get("summary").and_then(|s| s.as_str()).unwrap_or("");
            let text = format!(
                "{}{}{}",
                COMPACTION_SUMMARY_PREFIX, summary, COMPACTION_SUMMARY_SUFFIX
            );
            Some(Message::User(UserMessage {
                content: vec![UserContent::Text(TextContent { text })],
                timestamp,
            }))
        }
        "custom" => {
            let content = value
                .get("content")
                .map(parse_user_content)
                .unwrap_or_default();
            if content.is_empty() {
                return None;
            }
            Some(Message::User(UserMessage { content, timestamp }))
        }
        _ => None,
    }
}
