//! Minimal JSONL session persistence — one `AgentMessage` per line.
//!
//! This is the smallest useful resume primitive: load prior transcript on
//! startup, append new messages as they finalize. Branching / forking /
//! compaction tree semantics from `grain_agent_harness::session` aren't
//! used here — that's a richer surface for a future PR; v1 just gives the
//! CLI a way to keep a conversation alive across invocations.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;

use grain_agent_core::AgentMessage;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("serialize: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Read a JSONL session file and return the contained messages.
///
/// - Empty / whitespace-only lines are skipped.
/// - Lines that fail to parse are logged to stderr and skipped (so a
///   single corrupted entry doesn't lose the whole transcript).
/// - Missing file → returns `Ok(vec![])` so callers can use the path as
///   "create on first save".
pub fn load_messages(path: &Path) -> Result<Vec<AgentMessage>, SessionError> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(SessionError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let reader = BufReader::new(file);
    let mut out: Vec<AgentMessage> = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.map_err(|source| SessionError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<AgentMessage>(trimmed) {
            Ok(m) => out.push(m),
            Err(e) => {
                eprintln!(
                    "[warn] session {}: skipping malformed line {} ({e})",
                    path.display(),
                    idx + 1
                );
            }
        }
    }
    Ok(out)
}

/// Append-only JSONL writer for new `AgentMessage`s.
///
/// Opens with `append + create`. `Mutex` around the inner `File` keeps the
/// per-line writes serialized when called from a subscriber callback that
/// can fire concurrently with other listeners.
pub struct SessionWriter {
    path: std::path::PathBuf,
    file: Mutex<std::fs::File>,
}

impl SessionWriter {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| SessionError::Io {
                path: path.display().to_string(),
                source,
            })?;
        Ok(SessionWriter {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one message as a single line. Calls `flush` so a crash
    /// between turns doesn't lose already-finalized messages.
    ///
    /// Poison recovery: a panic inside a listener that held this mutex
    /// would poison it. The file handle itself stays valid, so we recover
    /// via `into_inner()` instead of letting every subsequent append panic
    /// and drag the agent down.
    pub fn append(&self, message: &AgentMessage) -> Result<(), SessionError> {
        let line = serde_json::to_string(message)?;
        let mut guard = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .write_all(line.as_bytes())
            .and_then(|_| guard.write_all(b"\n"))
            .and_then(|_| guard.flush())
            .map_err(|source| SessionError::Io {
                path: self.path.display().to_string(),
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_core::{
        AssistantMessage, StopReason, TextContent, Usage, UserContent, UserMessage,
    };

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user(UserMessage {
            content: vec![UserContent::Text(TextContent { text: text.into() })],
            timestamp: 0,
        })
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::assistant(AssistantMessage {
            content: vec![grain_agent_core::AssistantContent::Text(TextContent {
                text: text.into(),
            })],
            api: "t".into(),
            provider: "t".into(),
            model: "t".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        })
    }

    #[test]
    fn load_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let msgs = load_messages(&path).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn round_trip_through_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let w = SessionWriter::open(&path).unwrap();
        w.append(&user("hi")).unwrap();
        w.append(&assistant("hey")).unwrap();
        drop(w);

        let loaded = load_messages(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].role(), "user");
        assert_eq!(loaded[1].role(), "assistant");
    }

    #[test]
    fn second_writer_appends_does_not_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        SessionWriter::open(&path).unwrap().append(&user("a")).unwrap();
        SessionWriter::open(&path).unwrap().append(&user("b")).unwrap();
        let loaded = load_messages(&path).unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn malformed_line_is_skipped_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let good = serde_json::to_string(&user("only good one")).unwrap();
        std::fs::write(&path, format!("{{not json}}\n{good}\n\n")).unwrap();
        let loaded = load_messages(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].role(), "user");
    }
}
