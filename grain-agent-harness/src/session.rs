//! Session tree, storage trait, and in-memory implementation.
//!
//! Ports `packages/agent/src/harness/session/*` (excluding `jsonl-repo.ts`).
//!
//! The data model is a tree of entries (messages, model changes, compactions,
//! custom payloads, labels, ...). Each session keeps a `leaf_id` cursor that
//! identifies the tip of the active branch; `get_path_to_root` walks
//! `parent_id` links from any leaf back to the root.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use grain_agent_core::AgentMessage;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::messages::{branch_summary_message, compaction_summary_message};

// ---------------------------------------------------------------------------
// IDs / timestamps
// ---------------------------------------------------------------------------

/// Generate a sortable, time-based UUIDv7 string. Used as the default
/// session / entry id throughout the session tree.
pub fn uuidv7() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let nanos = now.subsec_nanos();
    // Minimal RFC3339-ish formatter: "1970-01-01T00:00:00.000Z".
    // We avoid pulling chrono just for this.
    let (year, month, day, hour, min, sec) = ymdhms_from_unix(secs);
    let millis = nanos / 1_000_000;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, min, sec, millis
    )
}

fn ymdhms_from_unix(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    // Days since 1970-01-01.
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400) as u32;
    let hour = time / 3600;
    let min = (time % 3600) / 60;
    let sec = time % 60;

    // Convert `days` to (year, month, day) using the Howard Hinnant algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + (era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, min, sec)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid fork target: {0}")]
    InvalidForkTarget(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Metadata + entries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    pub id: String,
    pub created_at: String,
    /// Backend-specific extra fields (e.g. file paths for a JSONL backend).
    #[serde(default, flatten, skip_serializing_if = "serde_json::Value::is_null")]
    pub extra: serde_json::Value,
}

impl Default for SessionMetadata {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionMetadata {
    pub fn new() -> Self {
        SessionMetadata {
            id: uuidv7(),
            created_at: now_iso(),
            extra: serde_json::Value::Null,
        }
    }

    pub fn with_id(id: impl Into<String>) -> Self {
        SessionMetadata {
            id: id.into(),
            created_at: now_iso(),
            extra: serde_json::Value::Null,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    #[serde(flatten)]
    pub kind: SessionTreeEntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionTreeEntryKind {
    Message {
        message: AgentMessage,
    },
    ThinkingLevelChange {
        thinking_level: String,
    },
    ModelChange {
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    Compaction {
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    Custom {
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    CustomMessage {
        custom_type: String,
        content: serde_json::Value,
        display: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    Label {
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    SessionInfo {
        name: String,
    },
    BranchSummary {
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
}

impl SessionTreeEntryKind {
    pub fn type_tag(&self) -> &'static str {
        match self {
            SessionTreeEntryKind::Message { .. } => "message",
            SessionTreeEntryKind::ThinkingLevelChange { .. } => "thinking_level_change",
            SessionTreeEntryKind::ModelChange { .. } => "model_change",
            SessionTreeEntryKind::Compaction { .. } => "compaction",
            SessionTreeEntryKind::Custom { .. } => "custom",
            SessionTreeEntryKind::CustomMessage { .. } => "custom_message",
            SessionTreeEntryKind::Label { .. } => "label",
            SessionTreeEntryKind::SessionInfo { .. } => "session_info",
            SessionTreeEntryKind::BranchSummary { .. } => "branch_summary",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<(String, String)>,
}

// ---------------------------------------------------------------------------
// Storage trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait SessionStorage: Send + Sync {
    async fn get_metadata(&self) -> SessionMetadata;
    async fn get_leaf_id(&self) -> Option<String>;
    async fn set_leaf_id(&self, leaf_id: Option<String>) -> Result<(), SessionError>;
    async fn get_entry(&self, id: &str) -> Option<SessionTreeEntry>;
    async fn get_entries(&self) -> Vec<SessionTreeEntry>;
    async fn get_path_to_root(&self, leaf_id: Option<&str>) -> Vec<SessionTreeEntry>;
    async fn append_entry(&self, entry: SessionTreeEntry) -> Result<(), SessionError>;
    async fn find_entries(&self, type_tag: &str) -> Vec<SessionTreeEntry>;
    async fn get_label(&self, id: &str) -> Option<String>;
    async fn create_entry_id(&self) -> String;
}

#[async_trait]
pub trait SessionRepo: Send + Sync {
    async fn create(&self, id: Option<String>) -> Result<Session, SessionError>;
    async fn open(&self, metadata: &SessionMetadata) -> Result<Session, SessionError>;
    async fn list(&self) -> Result<Vec<SessionMetadata>, SessionError>;
    async fn delete(&self, metadata: &SessionMetadata) -> Result<(), SessionError>;
    async fn fork(
        &self,
        source: &SessionMetadata,
        entry_id: Option<&str>,
        position: ForkPosition,
        id: Option<String>,
    ) -> Result<Session, SessionError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForkPosition {
    #[default]
    Before,
    At,
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A `Session` is a thin handle around a shared
/// `Arc<dyn SessionStorage>`. Cloning is intentionally cheap — every
/// clone points at the same underlying storage, so subscribers and
/// the orchestrator (e.g. [`crate::agent_harness::AgentHarness`])
/// share state without contention.
#[derive(Clone)]
pub struct Session {
    storage: Arc<dyn SessionStorage>,
}

impl Session {
    /// Wrap a shared storage backend. Cheap to clone — all copies
    /// share the same underlying storage.
    pub fn new(storage: Arc<dyn SessionStorage>) -> Self {
        Session { storage }
    }

    /// Access the underlying storage.
    pub fn storage(&self) -> &Arc<dyn SessionStorage> {
        &self.storage
    }

    /// Return session-level metadata (id, creation time, extra).
    pub async fn metadata(&self) -> SessionMetadata {
        self.storage.get_metadata().await
    }

    /// The current leaf entry id, or `None` for an empty session.
    pub async fn leaf_id(&self) -> Option<String> {
        self.storage.get_leaf_id().await
    }

    /// Look up a single entry by id.
    pub async fn entry(&self, id: &str) -> Option<SessionTreeEntry> {
        self.storage.get_entry(id).await
    }

    /// All entries in the session (unordered).
    pub async fn entries(&self) -> Vec<SessionTreeEntry> {
        self.storage.get_entries().await
    }

    /// Walk the path from `from_id` (or the current leaf) to the root,
    /// returning entries in chronological order.
    pub async fn branch(&self, from_id: Option<&str>) -> Vec<SessionTreeEntry> {
        if let Some(id) = from_id {
            self.storage.get_path_to_root(Some(id)).await
        } else {
            let leaf = self.storage.get_leaf_id().await;
            self.storage.get_path_to_root(leaf.as_deref()).await
        }
    }

    /// Build the agent-facing context from the current branch:
    /// turn the ordered entry chain into `Vec<AgentMessage>` via
    /// [`build_session_context`].
    pub async fn build_context(&self) -> SessionContext {
        build_session_context(&self.branch(None).await)
    }

    /// The human-readable label for an entry, if one was set.
    pub async fn label(&self, id: &str) -> Option<String> {
        self.storage.get_label(id).await
    }

    /// The last `session_info` name saved, or `None` if never set.
    pub async fn session_name(&self) -> Option<String> {
        let entries = self.storage.find_entries("session_info").await;
        let last = entries.into_iter().next_back();
        last.and_then(|e| match e.kind {
            SessionTreeEntryKind::SessionInfo { name } => {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }
            _ => None,
        })
    }

    async fn next_entry(
        &self,
        kind: SessionTreeEntryKind,
    ) -> Result<SessionTreeEntry, SessionError> {
        let id = self.storage.create_entry_id().await;
        let parent_id = self.storage.get_leaf_id().await;
        let entry = SessionTreeEntry {
            id,
            parent_id,
            timestamp: now_iso(),
            kind,
        };
        self.storage.append_entry(entry.clone()).await?;
        Ok(entry)
    }

    /// Append a message to the session tree. Returns the new entry id.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the underlying storage write fails.
    pub async fn append_message(&self, message: AgentMessage) -> Result<String, SessionError> {
        let entry = self
            .next_entry(SessionTreeEntryKind::Message { message })
            .await?;
        Ok(entry.id)
    }

    /// Record a thinking-level change in the session tree.
    pub async fn append_thinking_level_change(
        &self,
        thinking_level: impl Into<String>,
    ) -> Result<String, SessionError> {
        let entry = self
            .next_entry(SessionTreeEntryKind::ThinkingLevelChange {
                thinking_level: thinking_level.into(),
            })
            .await?;
        Ok(entry.id)
    }

    /// Record a model change (provider + model id) in the session tree.
    pub async fn append_model_change(
        &self,
        provider: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Result<String, SessionError> {
        let entry = self
            .next_entry(SessionTreeEntryKind::ModelChange {
                provider: provider.into(),
                model_id: model_id.into(),
            })
            .await?;
        Ok(entry.id)
    }

    /// Record a compaction event: which entries were dropped and why.
    pub async fn append_compaction(
        &self,
        summary: impl Into<String>,
        first_kept_entry_id: impl Into<String>,
        tokens_before: u64,
        details: Option<serde_json::Value>,
        from_hook: Option<bool>,
    ) -> Result<String, SessionError> {
        let entry = self
            .next_entry(SessionTreeEntryKind::Compaction {
                summary: summary.into(),
                first_kept_entry_id: first_kept_entry_id.into(),
                tokens_before,
                details,
                from_hook,
            })
            .await?;
        Ok(entry.id)
    }

    /// Append an opaque custom entry to the session tree.
    pub async fn append_custom(
        &self,
        custom_type: impl Into<String>,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        let entry = self
            .next_entry(SessionTreeEntryKind::Custom {
                custom_type: custom_type.into(),
                data,
            })
            .await?;
        Ok(entry.id)
    }

    /// Append a custom message entry (displayable in transcript UIs).
    pub async fn append_custom_message(
        &self,
        custom_type: impl Into<String>,
        content: serde_json::Value,
        display: bool,
        details: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        let entry = self
            .next_entry(SessionTreeEntryKind::CustomMessage {
                custom_type: custom_type.into(),
                content,
                display,
                details,
            })
            .await?;
        Ok(entry.id)
    }

    /// Attach a human-readable label to an existing entry.
    pub async fn append_label(
        &self,
        target_id: impl Into<String>,
        label: Option<String>,
    ) -> Result<String, SessionError> {
        let target = target_id.into();
        if self.storage.get_entry(&target).await.is_none() {
            return Err(SessionError::NotFound(format!("Entry {target} not found")));
        }
        let entry = self
            .next_entry(SessionTreeEntryKind::Label {
                target_id: target,
                label,
            })
            .await?;
        Ok(entry.id)
    }

    /// Set the session display name (persisted as a `session_info` entry).
    pub async fn append_session_name(
        &self,
        name: impl Into<String>,
    ) -> Result<String, SessionError> {
        let name = name.into().trim().to_string();
        let entry = self
            .next_entry(SessionTreeEntryKind::SessionInfo { name })
            .await?;
        Ok(entry.id)
    }

    /// Move the leaf cursor to `entry_id` (or to `None` for "before root").
    /// Optionally emit a branch-summary entry capturing the moved-from branch.
    pub async fn move_to(
        &self,
        entry_id: Option<&str>,
        summary: Option<MoveToSummary>,
    ) -> Result<Option<String>, SessionError> {
        if let Some(id) = entry_id
            && self.storage.get_entry(id).await.is_none()
        {
            return Err(SessionError::NotFound(format!("Entry {id} not found")));
        }
        self.storage
            .set_leaf_id(entry_id.map(|s| s.to_string()))
            .await?;
        let Some(summary) = summary else {
            return Ok(None);
        };
        let id = self.storage.create_entry_id().await;
        let entry = SessionTreeEntry {
            id: id.clone(),
            parent_id: entry_id.map(|s| s.to_string()),
            timestamp: now_iso(),
            kind: SessionTreeEntryKind::BranchSummary {
                from_id: entry_id
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "root".into()),
                summary: summary.summary,
                details: summary.details,
                from_hook: summary.from_hook,
            },
        };
        self.storage.append_entry(entry).await?;
        Ok(Some(id))
    }
}

#[derive(Debug, Clone, Default)]
pub struct MoveToSummary {
    pub summary: String,
    pub details: Option<serde_json::Value>,
    pub from_hook: Option<bool>,
}

/// Reduce a `path_entries` slice into the `SessionContext` an agent run consumes.
pub fn build_session_context(path_entries: &[SessionTreeEntry]) -> SessionContext {
    let mut thinking_level = "off".to_string();
    let mut model: Option<(String, String)> = None;
    let mut compaction_idx: Option<usize> = None;

    for (i, entry) in path_entries.iter().enumerate() {
        match &entry.kind {
            SessionTreeEntryKind::ThinkingLevelChange {
                thinking_level: lvl,
            } => {
                thinking_level = lvl.clone();
            }
            SessionTreeEntryKind::ModelChange { provider, model_id } => {
                model = Some((provider.clone(), model_id.clone()));
            }
            SessionTreeEntryKind::Message { message } => {
                if let Some(asst) = message.as_assistant() {
                    model = Some((asst.provider.clone(), asst.model.clone()));
                }
            }
            SessionTreeEntryKind::Compaction { .. } => {
                compaction_idx = Some(i);
            }
            _ => {}
        }
    }

    let mut messages: Vec<AgentMessage> = Vec::new();

    let append_message = |entry: &SessionTreeEntry, out: &mut Vec<AgentMessage>| match &entry.kind {
        SessionTreeEntryKind::Message { message } => out.push(message.clone()),
        SessionTreeEntryKind::CustomMessage {
            custom_type,
            content,
            display,
            details,
        } => {
            // Mirror `createCustomMessage` from the TS harness.
            let timestamp = parse_iso_to_ms(&entry.timestamp);
            out.push(crate::messages::custom_message(
                custom_type,
                content.clone(),
                *display,
                details.clone(),
                timestamp,
            ));
        }
        SessionTreeEntryKind::BranchSummary {
            summary, from_id, ..
        } => {
            if !summary.is_empty() {
                out.push(branch_summary_message(
                    summary,
                    from_id,
                    parse_iso_to_ms(&entry.timestamp),
                ));
            }
        }
        _ => {}
    };

    if let Some(idx) = compaction_idx {
        let SessionTreeEntryKind::Compaction {
            summary,
            first_kept_entry_id,
            tokens_before,
            ..
        } = &path_entries[idx].kind
        else {
            unreachable!()
        };
        messages.push(compaction_summary_message(
            summary,
            *tokens_before,
            parse_iso_to_ms(&path_entries[idx].timestamp),
        ));
        let mut found_first_kept = false;
        for entry in &path_entries[..idx] {
            if entry.id == *first_kept_entry_id {
                found_first_kept = true;
            }
            if found_first_kept {
                append_message(entry, &mut messages);
            }
        }
        for entry in &path_entries[idx + 1..] {
            append_message(entry, &mut messages);
        }
    } else {
        for entry in path_entries {
            append_message(entry, &mut messages);
        }
    }

    SessionContext {
        messages,
        thinking_level,
        model,
    }
}

fn parse_iso_to_ms(iso: &str) -> i64 {
    // Very small RFC3339 parser handling the format we emit:
    // "YYYY-MM-DDTHH:MM:SS.mmmZ". Returns ms since epoch, 0 on failure.
    if iso.len() < 20 {
        return 0;
    }
    let bytes = iso.as_bytes();
    let parse = |start: usize, len: usize| -> Option<i64> {
        std::str::from_utf8(&bytes[start..start + len])
            .ok()?
            .parse()
            .ok()
    };
    let year: i64 = parse(0, 4).unwrap_or(0);
    let month: i64 = parse(5, 2).unwrap_or(0);
    let day: i64 = parse(8, 2).unwrap_or(0);
    let hour: i64 = parse(11, 2).unwrap_or(0);
    let min: i64 = parse(14, 2).unwrap_or(0);
    let sec: i64 = parse(17, 2).unwrap_or(0);
    let ms: i64 = if iso.len() >= 23 {
        parse(20, 3).unwrap_or(0)
    } else {
        0
    };

    let days_from_civil = days_from_civil(year, month as u32, day as u32);
    let secs = days_from_civil * 86_400 + hour * 3600 + min * 60 + sec;
    secs * 1000 + ms
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = (y - era * 400) as u64;
    let m32 = if m > 2 { m as u64 - 3 } else { m as u64 + 9 };
    let doy = (153 * m32 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

pub struct InMemorySessionStorage {
    inner: Mutex<InMemoryInner>,
}

struct InMemoryInner {
    metadata: SessionMetadata,
    entries: Vec<SessionTreeEntry>,
    index: HashMap<String, usize>,
    leaf_id: Option<String>,
    labels: HashMap<String, String>,
}

impl InMemorySessionStorage {
    pub fn new(metadata: SessionMetadata) -> Self {
        InMemorySessionStorage {
            inner: Mutex::new(InMemoryInner {
                metadata,
                entries: Vec::new(),
                index: HashMap::new(),
                leaf_id: None,
                labels: HashMap::new(),
            }),
        }
    }

    pub fn with_entries(metadata: SessionMetadata, entries: Vec<SessionTreeEntry>) -> Self {
        let mut storage = InMemoryInner {
            metadata,
            entries: Vec::new(),
            index: HashMap::new(),
            leaf_id: None,
            labels: HashMap::new(),
        };
        for entry in entries {
            storage
                .index
                .insert(entry.id.clone(), storage.entries.len());
            if let SessionTreeEntryKind::Label { target_id, label } = &entry.kind {
                match label {
                    Some(text) => {
                        storage.labels.insert(target_id.clone(), text.clone());
                    }
                    None => {
                        storage.labels.remove(target_id);
                    }
                }
            }
            storage.leaf_id = Some(entry.id.clone());
            storage.entries.push(entry);
        }
        InMemorySessionStorage {
            inner: Mutex::new(storage),
        }
    }
}

#[async_trait]
impl SessionStorage for InMemorySessionStorage {
    async fn get_metadata(&self) -> SessionMetadata {
        self.inner.lock().await.metadata.clone()
    }

    async fn get_leaf_id(&self) -> Option<String> {
        self.inner.lock().await.leaf_id.clone()
    }

    async fn set_leaf_id(&self, leaf_id: Option<String>) -> Result<(), SessionError> {
        self.inner.lock().await.leaf_id = leaf_id;
        Ok(())
    }

    async fn get_entry(&self, id: &str) -> Option<SessionTreeEntry> {
        let guard = self.inner.lock().await;
        guard.index.get(id).map(|&idx| guard.entries[idx].clone())
    }

    async fn get_entries(&self) -> Vec<SessionTreeEntry> {
        self.inner.lock().await.entries.clone()
    }

    async fn get_path_to_root(&self, leaf_id: Option<&str>) -> Vec<SessionTreeEntry> {
        let Some(start) = leaf_id else {
            return Vec::new();
        };
        let guard = self.inner.lock().await;
        let mut path = Vec::new();
        let mut current = Some(start.to_string());
        while let Some(id) = current {
            let Some(&idx) = guard.index.get(&id) else {
                break;
            };
            let entry = guard.entries[idx].clone();
            current = entry.parent_id.clone();
            path.push(entry);
        }
        path.reverse();
        path
    }

    async fn append_entry(&self, entry: SessionTreeEntry) -> Result<(), SessionError> {
        let mut guard = self.inner.lock().await;
        if guard.index.contains_key(&entry.id) {
            return Err(SessionError::Storage(format!(
                "duplicate entry id: {}",
                entry.id
            )));
        }
        if let SessionTreeEntryKind::Label { target_id, label } = &entry.kind {
            match label {
                Some(text) => {
                    guard.labels.insert(target_id.clone(), text.clone());
                }
                None => {
                    guard.labels.remove(target_id);
                }
            }
        }
        guard.leaf_id = Some(entry.id.clone());
        let idx = guard.entries.len();
        guard.index.insert(entry.id.clone(), idx);
        guard.entries.push(entry);
        Ok(())
    }

    async fn find_entries(&self, type_tag: &str) -> Vec<SessionTreeEntry> {
        let guard = self.inner.lock().await;
        guard
            .entries
            .iter()
            .filter(|e| e.kind.type_tag() == type_tag)
            .cloned()
            .collect()
    }

    async fn get_label(&self, id: &str) -> Option<String> {
        self.inner.lock().await.labels.get(id).cloned()
    }

    async fn create_entry_id(&self) -> String {
        uuidv7()
    }
}

pub struct InMemorySessionRepo {
    sessions: Mutex<HashMap<String, Arc<InMemorySessionStorage>>>,
}

impl Default for InMemorySessionRepo {
    fn default() -> Self {
        InMemorySessionRepo::new()
    }
}

impl InMemorySessionRepo {
    pub fn new() -> Self {
        InMemorySessionRepo {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl SessionRepo for InMemorySessionRepo {
    async fn create(&self, id: Option<String>) -> Result<Session, SessionError> {
        let metadata = if let Some(id) = id {
            SessionMetadata::with_id(id)
        } else {
            SessionMetadata::new()
        };
        let storage = Arc::new(InMemorySessionStorage::new(metadata.clone()));
        self.sessions
            .lock()
            .await
            .insert(metadata.id.clone(), storage.clone());
        Ok(Session::new(storage))
    }

    async fn open(&self, metadata: &SessionMetadata) -> Result<Session, SessionError> {
        let guard = self.sessions.lock().await;
        let storage = guard
            .get(&metadata.id)
            .ok_or_else(|| SessionError::NotFound(format!("Session not found: {}", metadata.id)))?
            .clone();
        Ok(Session::new(storage))
    }

    async fn list(&self) -> Result<Vec<SessionMetadata>, SessionError> {
        let guard = self.sessions.lock().await;
        let mut out = Vec::with_capacity(guard.len());
        for s in guard.values() {
            out.push(s.get_metadata().await);
        }
        Ok(out)
    }

    async fn delete(&self, metadata: &SessionMetadata) -> Result<(), SessionError> {
        self.sessions.lock().await.remove(&metadata.id);
        Ok(())
    }

    async fn fork(
        &self,
        source: &SessionMetadata,
        entry_id: Option<&str>,
        position: ForkPosition,
        id: Option<String>,
    ) -> Result<Session, SessionError> {
        let storage = {
            let guard = self.sessions.lock().await;
            guard
                .get(&source.id)
                .ok_or_else(|| SessionError::NotFound(format!("Session not found: {}", source.id)))?
                .clone()
        };
        let fork_entries = entries_to_fork(&*storage, entry_id, position).await?;
        let metadata = if let Some(id) = id {
            SessionMetadata::with_id(id)
        } else {
            SessionMetadata::new()
        };
        let new_storage = Arc::new(InMemorySessionStorage::with_entries(
            metadata.clone(),
            fork_entries,
        ));
        self.sessions
            .lock()
            .await
            .insert(metadata.id.clone(), new_storage.clone());
        Ok(Session::new(new_storage))
    }
}

async fn entries_to_fork(
    storage: &dyn SessionStorage,
    entry_id: Option<&str>,
    position: ForkPosition,
) -> Result<Vec<SessionTreeEntry>, SessionError> {
    let Some(id) = entry_id else {
        return Ok(storage.get_entries().await);
    };
    let target = storage
        .get_entry(id)
        .await
        .ok_or_else(|| SessionError::InvalidForkTarget(format!("Entry {id} not found")))?;
    let effective_leaf = match position {
        ForkPosition::At => Some(target.id.clone()),
        ForkPosition::Before => match &target.kind {
            SessionTreeEntryKind::Message { message } if message.role() == "user" => {
                target.parent_id.clone()
            }
            _ => {
                return Err(SessionError::InvalidForkTarget(format!(
                    "Entry {id} is not a user message"
                )));
            }
        },
    };
    Ok(storage.get_path_to_root(effective_leaf.as_deref()).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_core::{TextContent, UserContent, UserMessage};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: text.into() })],
            timestamp: 0,
        })
    }

    #[tokio::test]
    async fn create_open_append_branch() {
        let repo = InMemorySessionRepo::new();
        let session = repo.create(None).await.unwrap();
        let meta = session.metadata().await;

        session.append_message(user("hello")).await.unwrap();
        session.append_message(user("world")).await.unwrap();
        session.append_session_name("My chat").await.unwrap();

        let reopened = repo.open(&meta).await.unwrap();
        let branch = reopened.branch(None).await;
        // 2 messages + 1 session_info entry, on a linear branch.
        assert_eq!(branch.len(), 3);
        assert_eq!(reopened.session_name().await.as_deref(), Some("My chat"));
    }

    #[tokio::test]
    async fn build_context_drops_pre_compaction_messages() {
        let repo = InMemorySessionRepo::new();
        let session = repo.create(None).await.unwrap();
        session.append_message(user("dropped 1")).await.unwrap();
        let kept_id = session.append_message(user("kept")).await.unwrap();
        session
            .append_compaction("prior summary", &kept_id, 1234, None, None)
            .await
            .unwrap();
        session
            .append_message(user("after compaction"))
            .await
            .unwrap();

        let ctx = session.build_context().await;
        // Expect: compaction summary + "kept" + "after compaction" (3 entries).
        assert_eq!(ctx.messages.len(), 3);
    }
}
