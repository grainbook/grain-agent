//! Session discovery — scan a directory of JSONL session trees and
//! return metadata for each (title preview / model id / mtime).
//!
//! Used by the TUI's `/resume` overlay (and any future CLI tool that
//! wants to list past sessions). Pure I/O + parsing; no UI concerns.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use grain_agent_core::{AgentMessage, Message, UserContent};
use grain_agent_harness::{
    JsonlSessionRepo, Session, SessionError, SessionMetadata, SessionTreeEntry,
    SessionTreeEntryKind,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const META_FILE: &str = "meta.json";
const STATE_FILE: &str = "state.json";
const ENTRIES_FILE: &str = "entries.jsonl";
const LOCK_FILE: &str = "session.lock";

/// One session tree's metadata, parsed from disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    /// Session id derived from metadata / directory name.
    pub id: String,
    /// Absolute session directory on disk.
    pub path: PathBuf,
    /// Last user prompt's text (clamped to ~80 chars). `None` when
    /// the session never recorded a user message yet.
    pub title: Option<String>,
    /// First assistant message's `model` field. `None` when no
    /// assistant turn has finished yet.
    pub model: Option<String>,
    /// Number of active-branch context messages on disk.
    pub message_count: usize,
    /// File mtime — what the picker sorts by ("most recently used
    /// first").
    pub modified_at: SystemTime,
    /// `true` when another grain process currently holds the
    /// advisory exclusive lock on this session tree. The TUI's `/resume` picker
    /// renders `[locked]` next to these rows and gates Enter on
    /// them through a "fresh / fork / cancel" dialog instead of
    /// emitting an in-place resume.
    ///
    /// Populated by `list_sessions` at scan time; not refreshed
    /// after — the field is a snapshot, callers must re-scan if
    /// they need fresher state. Note this is non-`Serialize`-safe
    /// for cross-process state, so the field has `#[serde(skip)]`
    /// — serialized SessionMeta blobs (if any consumers persist
    /// them) intentionally drop the lock state.
    #[serde(skip, default)]
    pub locked: bool,
}

impl SessionMeta {
    /// Compact one-line preview for the picker UI: `[mtime] [model]
    /// [title]`. The caller formats the timestamp; we just hand back
    /// the raw bits so the renderer can use the theme's accent
    /// colors on individual fields.
    pub fn title_or_placeholder(&self) -> &str {
        self.title.as_deref().unwrap_or("(empty session)")
    }
}

/// Cap on `SessionMeta::title` length. Picker rows are width-limited;
/// 80 chars wraps cleanly in most overlays.
pub const TITLE_PREVIEW_MAX: usize = 80;

/// Generate a fresh session id (UUIDv7 — sortable by creation time)
/// and return the path `dir / <id>`. Caller is responsible for
/// `create_dir_all(dir)` before opening the session.
///
/// # Examples
///
/// ```
/// use grain_ai_agent_headless::new_session_path;
/// use std::path::Path;
///
/// let dir = Path::new("/tmp/sessions");
/// let path = new_session_path(dir);
/// assert!(path.starts_with(dir));
/// ```
pub fn new_session_path(dir: &Path) -> PathBuf {
    let id = Uuid::now_v7();
    dir.join(id.to_string())
}

pub fn paths_match(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn list_sessions_excluding_active(
    sessions_dir: &Path,
    active_session_path: Option<&Path>,
) -> Vec<SessionMeta> {
    let mut list = list_sessions(sessions_dir);
    if let Some(active) = active_session_path {
        list.retain(|meta| !paths_match(&meta.path, active));
    }
    list
}

pub fn session_id_from_path(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

pub async fn open_session_dir(
    repo: &JsonlSessionRepo,
    path: &Path,
) -> Result<Session, SessionError> {
    let id = session_id_from_path(path).ok_or_else(|| {
        SessionError::NotFound(format!("invalid session path: {}", path.display()))
    })?;
    repo.open_id(&id).await
}

pub async fn open_or_create_session_dir(
    repo: &JsonlSessionRepo,
    path: &Path,
) -> Result<Session, SessionError> {
    let id = session_id_from_path(path).ok_or_else(|| {
        SessionError::NotFound(format!("invalid session path: {}", path.display()))
    })?;
    repo.open_or_create_id(&id).await
}

pub fn copy_session_tree_snapshot(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for file in [META_FILE, ENTRIES_FILE, STATE_FILE] {
        let src_file = src.join(file);
        if src_file.exists() {
            std::fs::copy(src_file, dest.join(file))?;
        }
    }
    if let Some(id) = session_id_from_path(dest) {
        let meta_path = dest.join(META_FILE);
        if meta_path.exists() {
            let raw = std::fs::read_to_string(&meta_path)?;
            if let Ok(mut meta) = serde_json::from_str::<SessionMetadata>(&raw) {
                meta.id = id;
                let body = serde_json::to_string_pretty(&meta).map_err(std::io::Error::other)?;
                std::fs::write(meta_path, body)?;
            }
        }
    }
    Ok(())
}

/// Scan `dir` for session directories, parse each into a [`SessionMeta`],
/// and return the list sorted by `modified_at` descending (most
/// recent first). Files that fail to read / parse are skipped with a
/// `[warn]` line — corruption in one shouldn't hide the rest.
///
/// Missing directory → returns an empty `Vec` so callers can use the
/// path as "create on first session".
///
/// # Examples
///
/// ```no_run
/// use grain_ai_agent_headless::list_sessions;
/// use std::path::Path;
///
/// let sessions = list_sessions(Path::new("/tmp/sessions"));
/// for s in &sessions {
///     println!("{} — {:?}", s.id, s.title);
/// }
/// ```
pub fn list_sessions(dir: &Path) -> Vec<SessionMeta> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!("[warn] session-discovery: {} ({e})", dir.display());
            return Vec::new();
        }
    };
    let mut out: Vec<SessionMeta> = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        match parse_session_meta(&path) {
            Ok(mut meta) => {
                // Fill the lock-state snapshot here so the picker
                // can render `[locked]` without re-probing each
                // row at render time.
                meta.locked = is_tree_session_locked(&path);
                out.push(meta);
            }
            Err(e) => {
                eprintln!(
                    "[warn] session-discovery: skipping {} ({e})",
                    path.display()
                );
            }
        }
    }
    out.sort_by_key(|m| Reverse(m.modified_at));
    out
}

/// Read a single session directory and derive its metadata.
///
/// # Errors
///
/// Returns an I/O error when required files can't be opened / read.
/// Malformed individual lines inside the file are silently skipped
/// — a session with no parseable messages still returns a valid
/// [`SessionMeta`] with `title = None` and `message_count = 0`.
pub fn parse_session_meta(path: &Path) -> std::io::Result<SessionMeta> {
    use std::io::{BufRead, BufReader};

    let metadata = read_session_metadata(path)?;
    let entries_path = path.join(ENTRIES_FILE);
    let file = match std::fs::File::open(&entries_path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let modified_at = path
                .join(META_FILE)
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            return Ok(SessionMeta {
                id: metadata.id,
                path: path.to_path_buf(),
                title: None,
                model: None,
                message_count: 0,
                modified_at,
                locked: false,
            });
        }
        Err(e) => return Err(e),
    };
    let modified_at = entries_path
        .metadata()?
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let reader = BufReader::new(file);

    let mut entries: Vec<SessionTreeEntry> = Vec::new();
    let mut by_id = std::collections::HashMap::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<SessionTreeEntry>(trimmed) else {
            continue;
        };
        by_id.insert(entry.id.clone(), entries.len());
        entries.push(entry);
    }

    let mut path_entries = Vec::new();
    let mut current = read_leaf_id(path).or_else(|| entries.last().map(|entry| entry.id.clone()));
    while let Some(id) = current {
        let Some(idx) = by_id.get(&id).copied() else {
            break;
        };
        let entry = entries[idx].clone();
        current = entry.parent_id.clone();
        path_entries.push(entry);
    }
    path_entries.reverse();

    let mut title: Option<String> = None;
    let mut model: Option<String> = None;
    let mut count: usize = 0;
    for entry in &path_entries {
        let SessionTreeEntryKind::Message { message } = &entry.kind else {
            continue;
        };
        count += 1;
        let AgentMessage::Standard(msg) = message else {
            continue;
        };
        match msg {
            Message::User(u) => {
                title = Some(extract_user_text_preview(&u.content));
            }
            Message::Assistant(a) if model.is_none() && !a.model.is_empty() => {
                model = Some(a.model.clone());
                // Some assistants emit text/tool_calls but no model
                // string; in that case keep looking on later turns.
            }
            _ => {}
        }
    }

    Ok(SessionMeta {
        id: metadata.id,
        path: path.to_path_buf(),
        title,
        model,
        message_count: count,
        modified_at,
        // parse_session_meta is pure-parse; lock state is filled
        // in by list_sessions which probes each file. Direct
        // callers that need the flag should call `is_session_locked`
        // themselves.
        locked: false,
    })
}

fn read_session_metadata(path: &Path) -> std::io::Result<SessionMetadata> {
    let raw = std::fs::read_to_string(path.join(META_FILE))?;
    serde_json::from_str::<SessionMetadata>(&raw)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn read_leaf_id(path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct StateFile {
        leaf_id: Option<String>,
    }
    let raw = std::fs::read_to_string(path.join(STATE_FILE)).ok()?;
    serde_json::from_str::<StateFile>(&raw).ok()?.leaf_id
}

fn is_tree_session_locked(path: &Path) -> bool {
    use fs2::FileExt;
    let lock_path = path.join(LOCK_FILE);
    let file = match std::fs::OpenOptions::new().read(true).open(&lock_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
}

fn extract_user_text_preview(contents: &[UserContent]) -> String {
    let mut s = String::new();
    for c in contents {
        match c {
            UserContent::Text(t) => {
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&t.text);
            }
            UserContent::Image(_) => {}
        }
        if s.len() >= TITLE_PREVIEW_MAX {
            break;
        }
    }
    // Collapse whitespace (newlines + tabs would shred the picker row).
    let collapsed: String = s.split_whitespace().collect::<Vec<&str>>().join(" ");
    truncate_char_boundary(&collapsed, TITLE_PREVIEW_MAX)
}

fn truncate_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_core::{
        AssistantMessage, StopReason, TextContent, Usage, UserContent, UserMessage,
    };
    use grain_agent_harness::{JsonlSessionRepo, SessionRepo, SessionTreeEntry, uuidv7};
    use std::io::Write;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: text.into() })],
            timestamp: 0,
        })
    }

    fn assistant(text: &str, model: &str) -> AgentMessage {
        use grain_agent_core::AssistantContent;
        AgentMessage::assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent { text: text.into() })],
            api: "openai".into(),
            provider: "openai".into(),
            model: model.into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        })
    }

    fn write_session(dir: &Path, name: &str, msgs: &[AgentMessage]) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).unwrap();
        let metadata = SessionMetadata::with_id(name);
        std::fs::write(
            path.join(META_FILE),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();

        let mut parent_id: Option<String> = None;
        let mut last_id: Option<String> = None;
        let mut f = std::fs::File::create(path.join(ENTRIES_FILE)).unwrap();
        for m in msgs {
            let id = uuidv7();
            let entry = SessionTreeEntry {
                id: id.clone(),
                parent_id: parent_id.clone(),
                timestamp: "2026-01-01T00:00:00.000Z".into(),
                kind: SessionTreeEntryKind::Message { message: m.clone() },
            };
            writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
            parent_id = Some(id.clone());
            last_id = Some(id);
        }
        std::fs::write(
            path.join(STATE_FILE),
            serde_json::json!({ "leafId": last_id }).to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn new_session_path_uses_uuidv7_directory_name() {
        let path = new_session_path(Path::new("/tmp"));
        let stem = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(path.extension().is_none());
        // UUIDv7 is 36 chars hyphen-separated.
        assert_eq!(stem.len(), 36);
        assert!(stem.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn list_sessions_missing_dir_returns_empty() {
        let sessions = list_sessions(Path::new("/tmp/nonexistent-grain-sessions-xyz-12345"));
        assert!(sessions.is_empty());
    }

    #[test]
    fn parse_meta_extracts_title_and_model() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_session(
            tmp.path(),
            "sess1",
            &[
                user("how do I read a file?"),
                assistant("Use the read tool", "anthropic/claude-sonnet-4-5"),
            ],
        );
        let meta = parse_session_meta(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("how do I read a file?"));
        assert_eq!(meta.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.id, "sess1");
    }

    #[test]
    fn parse_meta_handles_empty_session() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_session(tmp.path(), "empty", &[]);
        let meta = parse_session_meta(&path).unwrap();
        assert!(meta.title.is_none());
        assert!(meta.model.is_none());
        assert_eq!(meta.message_count, 0);
    }

    #[test]
    fn parse_meta_truncates_long_titles_at_char_boundary() {
        let long = "中".repeat(100); // 300 bytes; cap is 80 → should truncate around the
        // 26th char (78 bytes) + '…'
        let tmp = tempfile::tempdir().unwrap();
        let path = write_session(tmp.path(), "long", &[user(&long)]);
        let meta = parse_session_meta(&path).unwrap();
        let title = meta.title.unwrap();
        assert!(title.ends_with('…'));
        // No panic from mid-char slice.
        assert!(title.is_char_boundary(title.len()));
    }

    #[test]
    fn list_sessions_skips_non_jsonl_and_sorts_by_mtime_desc() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(tmp.path(), "old", &[user("old session")]);
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_session(tmp.path(), "new", &[user("new session")]);
        // Decoy file that should be ignored.
        std::fs::write(tmp.path().join("README.md"), "ignore me").unwrap();

        let sessions = list_sessions(tmp.path());
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "new");
        assert_eq!(sessions[1].id, "old");
    }

    #[test]
    fn list_sessions_skips_malformed_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(tmp.path(), "good", &[user("hi")]);
        let bad = tmp.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(
            bad.join(META_FILE),
            serde_json::to_string_pretty(&SessionMetadata::with_id("bad")).unwrap(),
        )
        .unwrap();
        std::fs::write(bad.join(ENTRIES_FILE), "{not valid\n").unwrap();
        let sessions = list_sessions(tmp.path());
        // Parsing logic skips bad lines but still returns the file
        // (with title=None / count=0).
        assert_eq!(sessions.len(), 2);
        let good = sessions.iter().find(|s| s.id == "good").unwrap();
        assert_eq!(good.title.as_deref(), Some("hi"));
    }
    #[test]
    fn parse_meta_title_is_last_user_message() {
        // When a session has multiple user turns, the title shown in
        // the /resume picker should be the **last** user prompt.
        let tmp = tempfile::tempdir().unwrap();
        let path = write_session(
            tmp.path(),
            "multi",
            &[
                user("first question"),
                assistant("first answer", "gpt-4"),
                user("second question"),
                assistant("second answer", "gpt-4"),
                user("final question — this should be the title"),
            ],
        );
        let meta = parse_session_meta(&path).unwrap();
        assert_eq!(
            meta.title.as_deref(),
            Some("final question — this should be the title")
        );
    }

    #[tokio::test]
    async fn list_sessions_excluding_active_hides_current_empty_locked_session() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let _active_session = repo.create(Some("active".into())).await.unwrap();
        let active = repo.session_dir("active");

        let list = list_sessions_excluding_active(dir.path(), Some(active.as_path()));

        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn list_sessions_excluding_active_keeps_other_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let _active_session = repo.create(Some("active".into())).await.unwrap();
        let active = repo.session_dir("active");
        let other_session = repo.create(Some("other".into())).await.unwrap();
        let other = repo.session_dir("other");
        other_session
            .append_message(user("older prompt"))
            .await
            .unwrap();

        let list = list_sessions_excluding_active(dir.path(), Some(active.as_path()));

        assert_eq!(list.len(), 1);
        assert_eq!(list[0].path, other);
        assert_eq!(list[0].message_count, 1);
    }

    #[tokio::test]
    async fn open_or_create_session_dir_uses_directory_name_as_id() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path()).unwrap();
        let path = dir.path().join("session-a");

        let session = open_or_create_session_dir(&repo, &path).await.unwrap();

        assert_eq!(session.metadata().await.id, "session-a");
    }

    #[test]
    fn copy_session_tree_snapshot_rewrites_destination_id() {
        let tmp = tempfile::tempdir().unwrap();
        let src = write_session(tmp.path(), "src", &[user("hello")]);
        let dest = tmp.path().join("dest");

        copy_session_tree_snapshot(&src, &dest).unwrap();

        let meta = parse_session_meta(&dest).unwrap();
        assert_eq!(meta.id, "dest");
        assert_eq!(meta.title.as_deref(), Some("hello"));
    }
}
