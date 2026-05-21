//! JSONL-backed [`SessionStorage`] / [`SessionRepo`] implementation.
//!
//! Ports `packages/agent/src/harness/session/jsonl-repo.ts` from pi. Layout
//! on disk:
//!
//! ```text
//! <root>/
//!   <session_id>/
//!     meta.json       # SessionMetadata
//!     entries.jsonl   # one SessionTreeEntry per line, append-only
//!     state.json      # mutable: { "leafId": Option<String> }
//! ```
//!
//! `entries.jsonl` is the source of truth for the tree shape; labels are
//! reconstructed by replaying [`SessionTreeEntryKind::Label`] entries on
//! load. The mutable `state.json` carries the leaf cursor only.
//!
//! All writes are persisted *before* updating the in-memory state, so a
//! crash mid-write leaves on-disk and in-memory copies consistent (worst
//! case: we wrote the line and crashed before updating state.json; the
//! next open recomputes leaf from the last entry).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::session::{
    ForkPosition, Session, SessionError, SessionMetadata, SessionRepo, SessionStorage,
    SessionTreeEntry, SessionTreeEntryKind, uuidv7,
};

const META_FILE: &str = "meta.json";
const STATE_FILE: &str = "state.json";
const ENTRIES_FILE: &str = "entries.jsonl";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateFile {
    leaf_id: Option<String>,
}

/// JSONL-backed implementation of [`SessionStorage`].
pub struct JsonlSessionStorage {
    inner: Mutex<JsonlInner>,
}

struct JsonlInner {
    dir: PathBuf,
    metadata: SessionMetadata,
    entries: Vec<SessionTreeEntry>,
    index: HashMap<String, usize>,
    leaf_id: Option<String>,
    labels: HashMap<String, String>,
}

impl JsonlSessionStorage {
    /// Open an existing on-disk session or initialize a fresh one in `dir`.
    /// The directory is created if missing; `meta.json` is written when
    /// absent so re-opening with the same metadata is idempotent.
    pub async fn open_or_init(
        dir: PathBuf,
        metadata: SessionMetadata,
    ) -> Result<Self, SessionError> {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(io_err(dir.display().to_string()))?;

        let meta_path = dir.join(META_FILE);
        if !meta_path.exists() {
            let s = serde_json::to_string_pretty(&metadata)
                .map_err(|e| SessionError::Storage(e.to_string()))?;
            tokio::fs::write(&meta_path, s)
                .await
                .map_err(io_err(meta_path.display().to_string()))?;
        }

        let entries_path = dir.join(ENTRIES_FILE);
        let mut entries: Vec<SessionTreeEntry> = Vec::new();
        if entries_path.exists() {
            let raw = tokio::fs::read_to_string(&entries_path)
                .await
                .map_err(io_err(entries_path.display().to_string()))?;
            for (lineno, line) in raw.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<SessionTreeEntry>(line) {
                    Ok(e) => entries.push(e),
                    Err(e) => eprintln!(
                        "[warn] grain-agent-harness: skipping corrupt entry at \
                         {}:{}: {e}",
                        entries_path.display(),
                        lineno + 1
                    ),
                }
            }
        }

        let mut index = HashMap::new();
        let mut labels = HashMap::new();
        for (i, entry) in entries.iter().enumerate() {
            index.insert(entry.id.clone(), i);
            if let SessionTreeEntryKind::Label { target_id, label } = &entry.kind {
                match label {
                    Some(text) => {
                        labels.insert(target_id.clone(), text.clone());
                    }
                    None => {
                        labels.remove(target_id);
                    }
                }
            }
        }

        // Reconcile state.json with entries.jsonl. The append flow writes
        // an entry to JSONL **before** updating state.json, so a crash
        // between the two leaves entries.jsonl correct but state.json
        // pointing at the previous leaf. To recover, we trust
        // entries.jsonl when state.json's leaf is older than the last
        // entry (specifically: when the last entry's id isn't already in
        // the path-to-state.json-leaf).
        let state_path = dir.join(STATE_FILE);
        let state_leaf: Option<String> = if state_path.exists() {
            let raw = tokio::fs::read_to_string(&state_path)
                .await
                .map_err(io_err(state_path.display().to_string()))?;
            // Tolerate corrupt state.json — fall back to last entry id.
            serde_json::from_str::<StateFile>(&raw)
                .map(|s| s.leaf_id)
                .ok()
                .flatten()
        } else {
            None
        };
        let derived_leaf = entries.last().map(|e| e.id.clone());
        let leaf_id = match (state_leaf, derived_leaf) {
            (Some(s), Some(d)) if s == d => Some(s),
            // state.json points to a different (older) leaf than the
            // last entry on disk → trust the file, the JSONL is the
            // source of truth.
            (Some(_), Some(d)) => Some(d),
            (Some(s), None) => Some(s),
            (None, d) => d,
        };

        Ok(JsonlSessionStorage {
            inner: Mutex::new(JsonlInner {
                dir,
                metadata,
                entries,
                index,
                leaf_id,
                labels,
            }),
        })
    }

    async fn persist_state(inner: &JsonlInner) -> Result<(), SessionError> {
        let state = StateFile {
            leaf_id: inner.leaf_id.clone(),
        };
        let s = serde_json::to_string(&state).map_err(|e| SessionError::Storage(e.to_string()))?;
        let tmp = inner.dir.join(format!("{STATE_FILE}.tmp"));
        let final_path = inner.dir.join(STATE_FILE);
        tokio::fs::write(&tmp, s)
            .await
            .map_err(io_err(tmp.display().to_string()))?;
        tokio::fs::rename(&tmp, &final_path)
            .await
            .map_err(io_err(final_path.display().to_string()))?;
        Ok(())
    }

    async fn append_to_jsonl(
        inner: &JsonlInner,
        entry: &SessionTreeEntry,
    ) -> Result<(), SessionError> {
        let line = serde_json::to_string(entry).map_err(|e| SessionError::Storage(e.to_string()))?;
        let path = inner.dir.join(ENTRIES_FILE);
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(io_err(path.display().to_string()))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(io_err(path.display().to_string()))?;
        file.write_all(b"\n")
            .await
            .map_err(io_err(path.display().to_string()))?;
        file.flush()
            .await
            .map_err(io_err(path.display().to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl SessionStorage for JsonlSessionStorage {
    async fn get_metadata(&self) -> SessionMetadata {
        self.inner.lock().await.metadata.clone()
    }

    async fn get_leaf_id(&self) -> Option<String> {
        self.inner.lock().await.leaf_id.clone()
    }

    async fn set_leaf_id(&self, leaf_id: Option<String>) -> Result<(), SessionError> {
        let mut g = self.inner.lock().await;
        g.leaf_id = leaf_id;
        Self::persist_state(&g).await
    }

    async fn get_entry(&self, id: &str) -> Option<SessionTreeEntry> {
        let g = self.inner.lock().await;
        g.index.get(id).map(|&idx| g.entries[idx].clone())
    }

    async fn get_entries(&self) -> Vec<SessionTreeEntry> {
        self.inner.lock().await.entries.clone()
    }

    async fn get_path_to_root(&self, leaf_id: Option<&str>) -> Vec<SessionTreeEntry> {
        let Some(start) = leaf_id else {
            return Vec::new();
        };
        let g = self.inner.lock().await;
        let mut path = Vec::new();
        let mut current = Some(start.to_string());
        while let Some(id) = current {
            let Some(&idx) = g.index.get(&id) else {
                break;
            };
            let entry = g.entries[idx].clone();
            current = entry.parent_id.clone();
            path.push(entry);
        }
        path.reverse();
        path
    }

    async fn append_entry(&self, entry: SessionTreeEntry) -> Result<(), SessionError> {
        let mut g = self.inner.lock().await;
        if g.index.contains_key(&entry.id) {
            return Err(SessionError::Storage(format!(
                "duplicate entry id: {}",
                entry.id
            )));
        }
        // Persist the entry first; on crash we re-load from disk.
        Self::append_to_jsonl(&g, &entry).await?;
        if let SessionTreeEntryKind::Label { target_id, label } = &entry.kind {
            match label {
                Some(text) => {
                    g.labels.insert(target_id.clone(), text.clone());
                }
                None => {
                    g.labels.remove(target_id);
                }
            }
        }
        g.leaf_id = Some(entry.id.clone());
        let idx = g.entries.len();
        g.index.insert(entry.id.clone(), idx);
        g.entries.push(entry);
        Self::persist_state(&g).await
    }

    async fn find_entries(&self, type_tag: &str) -> Vec<SessionTreeEntry> {
        let g = self.inner.lock().await;
        g.entries
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

fn io_err(path: String) -> impl FnOnce(std::io::Error) -> SessionError {
    move |source| SessionError::Storage(format!("{path}: {source}"))
}

/// JSONL-backed [`SessionRepo`]: a directory of sessions on disk.
pub struct JsonlSessionRepo {
    root: PathBuf,
}

impl JsonlSessionRepo {
    /// Create or open a sessions root. The directory is created if missing.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|e| SessionError::Storage(format!("{}: {e}", root.display())))?;
        Ok(JsonlSessionRepo { root })
    }

    /// Path on disk where `session_id` lives.
    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    async fn read_metadata_in(dir: &Path) -> Option<SessionMetadata> {
        let raw = tokio::fs::read_to_string(dir.join(META_FILE)).await.ok()?;
        serde_json::from_str::<SessionMetadata>(&raw).ok()
    }
}

#[async_trait]
impl SessionRepo for JsonlSessionRepo {
    async fn create(&self, id: Option<String>) -> Result<Session, SessionError> {
        let metadata = if let Some(id) = id {
            SessionMetadata::with_id(id)
        } else {
            SessionMetadata::new()
        };
        let dir = self.session_dir(&metadata.id);
        let storage = Arc::new(JsonlSessionStorage::open_or_init(dir, metadata).await?);
        Ok(Session::new(storage))
    }

    async fn open(&self, metadata: &SessionMetadata) -> Result<Session, SessionError> {
        let dir = self.session_dir(&metadata.id);
        if !dir.exists() {
            return Err(SessionError::NotFound(format!(
                "Session not found: {}",
                metadata.id
            )));
        }
        let storage = Arc::new(JsonlSessionStorage::open_or_init(dir, metadata.clone()).await?);
        Ok(Session::new(storage))
    }

    async fn list(&self) -> Result<Vec<SessionMetadata>, SessionError> {
        let mut out = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.root)
            .await
            .map_err(io_err(self.root.display().to_string()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(io_err(self.root.display().to_string()))?
        {
            if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if let Some(m) = Self::read_metadata_in(&entry.path()).await {
                out.push(m);
            }
        }
        // Deterministic order for callers that iterate.
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    async fn delete(&self, metadata: &SessionMetadata) -> Result<(), SessionError> {
        let dir = self.session_dir(&metadata.id);
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir)
                .await
                .map_err(io_err(dir.display().to_string()))?;
        }
        Ok(())
    }

    async fn fork(
        &self,
        source: &SessionMetadata,
        entry_id: Option<&str>,
        position: ForkPosition,
        id: Option<String>,
    ) -> Result<Session, SessionError> {
        let src_dir = self.session_dir(&source.id);
        if !src_dir.exists() {
            return Err(SessionError::NotFound(format!(
                "Session not found: {}",
                source.id
            )));
        }
        let src_storage = JsonlSessionStorage::open_or_init(src_dir, source.clone()).await?;
        let fork_entries = compute_fork_entries(&src_storage, entry_id, position).await?;

        let metadata = if let Some(id) = id {
            SessionMetadata::with_id(id)
        } else {
            SessionMetadata::new()
        };
        let dir = self.session_dir(&metadata.id);
        let storage = JsonlSessionStorage::open_or_init(dir, metadata).await?;
        for entry in fork_entries {
            storage.append_entry(entry).await?;
        }
        Ok(Session::new(Arc::new(storage)))
    }
}

async fn compute_fork_entries(
    storage: &JsonlSessionStorage,
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
    use grain_agent_core::{AgentMessage, TextContent, UserContent, UserMessage};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: text.into() })],
            timestamp: 0,
        })
    }

    #[tokio::test]
    async fn create_open_append_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let session = repo.create(None).await.unwrap();
        let meta = session.metadata().await;

        let id_a = session.append_message(user("alpha")).await.unwrap();
        let id_b = session.append_message(user("bravo")).await.unwrap();
        session.append_session_name("first chat").await.unwrap();

        drop(session); // simulate process restart

        let reopened_repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let reopened = reopened_repo.open(&meta).await.unwrap();
        let branch = reopened.branch(None).await;
        assert_eq!(branch.len(), 3);
        assert_eq!(branch[0].id, id_a);
        assert_eq!(branch[1].id, id_b);
        assert_eq!(branch[1].parent_id.as_deref(), Some(id_a.as_str()));
        assert_eq!(reopened.session_name().await.as_deref(), Some("first chat"));
    }

    #[tokio::test]
    async fn duplicate_entry_id_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let session = repo.create(None).await.unwrap();
        let id = session.append_message(user("once")).await.unwrap();
        // Reach into the storage and try to re-append the same id by hand.
        let dup = SessionTreeEntry {
            id: id.clone(),
            parent_id: None,
            timestamp: "2024-01-01T00:00:00.000Z".into(),
            kind: SessionTreeEntryKind::Message {
                message: user("dup"),
            },
        };
        let storage = session.storage().clone();
        let err = storage.append_entry(dup).await.unwrap_err();
        assert!(matches!(err, SessionError::Storage(s) if s.contains("duplicate")));
    }

    #[tokio::test]
    async fn list_returns_sessions_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let _s1 = repo.create(Some("zebra".into())).await.unwrap();
        let _s2 = repo.create(Some("alpha".into())).await.unwrap();
        let _s3 = repo.create(Some("mango".into())).await.unwrap();
        let listed = repo.list().await.unwrap();
        let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "mango", "zebra"]);
    }

    #[tokio::test]
    async fn delete_removes_session_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let session = repo.create(Some("trash".into())).await.unwrap();
        let meta = session.metadata().await;
        drop(session);
        assert!(dir.path().join("trash").exists());
        repo.delete(&meta).await.unwrap();
        assert!(!dir.path().join("trash").exists());
        // Listing the now-empty repo should be Ok([]), not an error.
        assert!(repo.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn open_missing_session_errors_notfound() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let meta = SessionMetadata::with_id("nowhere");
        // `Session` doesn't implement Debug, so we can't use `unwrap_err`.
        match repo.open(&meta).await {
            Ok(_) => panic!("expected NotFound for missing session"),
            Err(SessionError::NotFound(_)) => {}
            Err(other) => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_at_target_copies_path_to_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let session = repo.create(Some("src".into())).await.unwrap();
        let _a = session.append_message(user("a")).await.unwrap();
        let b = session.append_message(user("b")).await.unwrap();
        let _c = session.append_message(user("c")).await.unwrap();
        let meta = session.metadata().await;
        drop(session);

        let forked = repo
            .fork(&meta, Some(&b), ForkPosition::At, Some("fork".into()))
            .await
            .unwrap();
        let branch = forked.branch(None).await;
        // Path-to-root for `b` includes [a, b].
        assert_eq!(branch.len(), 2);
        assert_eq!(branch[1].id, b);
    }

    #[tokio::test]
    async fn corrupt_state_falls_back_to_last_entry() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let session = repo.create(Some("corrupt".into())).await.unwrap();
        let last_id = session.append_message(user("only")).await.unwrap();
        drop(session);

        // Overwrite state.json with garbage.
        tokio::fs::write(dir.path().join("corrupt").join(STATE_FILE), "not json")
            .await
            .unwrap();

        let reopened = repo
            .open(&SessionMetadata::with_id("corrupt"))
            .await
            .unwrap();
        assert_eq!(reopened.leaf_id().await.as_deref(), Some(last_id.as_str()));
    }

    #[tokio::test]
    async fn corrupt_jsonl_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let session = repo.create(Some("partial".into())).await.unwrap();
        let _good = session.append_message(user("good")).await.unwrap();
        drop(session);

        // Append a corrupt line at the end and an extra good one.
        let entries_path = dir.path().join("partial").join(ENTRIES_FILE);
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&entries_path)
            .await
            .unwrap();
        file.write_all(b"{ not json }\n").await.unwrap();
        file.flush().await.unwrap();

        let reopened = repo
            .open(&SessionMetadata::with_id("partial"))
            .await
            .unwrap();
        // The corrupt line was skipped; the good entry survives.
        assert_eq!(reopened.entries().await.len(), 1);
    }
}
