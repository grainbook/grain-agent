//! Session discovery — scan a directory of JSONL session files and
//! return metadata for each (title preview / model id / mtime).
//!
//! Used by the TUI's `/resume` overlay (and any future CLI tool that
//! wants to list past sessions). Pure I/O + parsing; no UI concerns.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use grain_agent_core::{AgentMessage, Message, UserContent};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One session file's metadata, parsed from a JSONL transcript on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    /// Session id derived from filename stem.
    pub id: String,
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Last user prompt's text (clamped to ~80 chars). `None` when
    /// the session never recorded a user message yet.
    pub title: Option<String>,
    /// First assistant message's `model` field. `None` when no
    /// assistant turn has finished yet.
    pub model: Option<String>,
    /// Number of finalized messages on disk (user / assistant /
    /// tool-result combined).
    pub message_count: usize,
    /// File mtime — what the picker sorts by ("most recently used
    /// first").
    pub modified_at: SystemTime,
    /// `true` when another grain process currently holds the
    /// advisory exclusive lock on this jsonl (see
    /// [`crate::is_session_locked`]). The TUI's `/resume` picker
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
/// and return the path `dir / <id>.jsonl`. Caller is responsible for
/// `create_dir_all(dir)` before opening the file.
///
/// # Examples
///
/// ```
/// use grain_ai_agent_headless::new_session_path;
/// use std::path::Path;
///
/// let dir = Path::new("/tmp/sessions");
/// let path = new_session_path(dir);
/// assert_eq!(path.extension().and_then(|s| s.to_str()), Some("jsonl"));
/// assert!(path.starts_with(dir));
/// ```
pub fn new_session_path(dir: &Path) -> PathBuf {
    let id = Uuid::now_v7();
    dir.join(format!("{id}.jsonl"))
}

/// Scan `dir` for `*.jsonl` files, parse each into a [`SessionMeta`],
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
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        match parse_session_meta(&path) {
            Ok(mut meta) => {
                // Fill the lock-state snapshot here so the picker
                // can render `[locked]` without re-probing each
                // row at render time.
                meta.locked = crate::is_session_locked(&path);
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

/// Read a single JSONL session and derive its metadata.
///
/// # Errors
///
/// Returns an I/O error when the file can't be opened / read.
/// Malformed individual lines inside the file are silently skipped
/// — a file with no parseable messages still returns a valid
/// [`SessionMeta`] with `title = None` and `message_count = 0`.
pub fn parse_session_meta(path: &Path) -> std::io::Result<SessionMeta> {
    use std::io::{BufRead, BufReader};

    let file = std::fs::File::open(path)?;
    let modified_at = file
        .metadata()?
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let reader = BufReader::new(file);

    let mut title: Option<String> = None;
    let mut model: Option<String> = None;
    let mut count: usize = 0;

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<AgentMessage>(trimmed) else {
            continue;
        };
        count += 1;
        let AgentMessage::Standard(msg) = &msg else {
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

    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(SessionMeta {
        id,
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
        let path = dir.join(format!("{name}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        for m in msgs {
            writeln!(f, "{}", serde_json::to_string(m).unwrap()).unwrap();
        }
        path
    }

    #[test]
    fn new_session_path_uses_uuidv7_extension() {
        let path = new_session_path(Path::new("/tmp"));
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        assert!(path.extension().and_then(|s| s.to_str()) == Some("jsonl"));
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
        std::fs::write(tmp.path().join("bad.jsonl"), "{not valid\n").unwrap();
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
}
