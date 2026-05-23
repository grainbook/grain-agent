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
    /// Another process already holds an advisory write lock on this
    /// JSONL — opening a second writer would let the two processes
    /// interleave appends and diverge their in-memory transcripts.
    /// Callers should either fork to a new path or surface a picker
    /// to the user (see TUI's session-lock-conflict overlay).
    #[error("{path} is held by another process")]
    Locked { path: String },
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
///
/// Also takes an OS-level **advisory exclusive lock** (`flock` on Unix,
/// `LockFileEx` on Windows) on the underlying file via the `fs2` crate so
/// two TUI processes can't both auto-resume the same session and
/// interleave their appends. The lock is per-fd and released
/// automatically when the inner `File` is dropped (i.e. when this
/// `SessionWriter` falls out of scope, or the process exits — even on a
/// crash, since the kernel cleans up the fd).
pub struct SessionWriter {
    path: std::path::PathBuf,
    file: Mutex<std::fs::File>,
}

impl SessionWriter {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        use fs2::FileExt;
        let path = path.as_ref().to_path_buf();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| SessionError::Io {
                path: path.display().to_string(),
                source,
            })?;
        // try_lock_exclusive returns Err with kind WouldBlock (Unix
        // EWOULDBLOCK, Windows ERROR_LOCK_VIOLATION) when another
        // process holds the lock. Treat both as `Locked`; other I/O
        // errors propagate as `Io`.
        if let Err(e) = file.try_lock_exclusive() {
            return Err(match e.kind() {
                std::io::ErrorKind::WouldBlock => SessionError::Locked {
                    path: path.display().to_string(),
                },
                _ => SessionError::Io {
                    path: path.display().to_string(),
                    source: e,
                },
            });
        }
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

/// Probe whether `path` is currently held by another process's
/// `SessionWriter`. Opens a temporary read-only fd, attempts a
/// non-blocking exclusive lock, then drops the fd (releasing the
/// probe lock immediately if we got it). Returns `false` when the
/// file doesn't exist yet — there's nothing to be locked.
///
/// Used by the TUI's `/resume` picker to render the `[locked]`
/// annotation; not used to gate the actual open, since that would
/// race with another process opening between the probe and the
/// real lock. Always pass the `SessionError::Locked` returned by
/// `SessionWriter::open` to the user as authoritative.
pub fn is_session_locked(path: &Path) -> bool {
    use fs2::FileExt;
    let file = match std::fs::OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            // We got the lock — was unlocked. Release before dropping
            // for symmetry (Drop would do it anyway).
            let _ = file.unlock();
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
        Err(_) => false,
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
    }

    #[test]
    fn second_open_while_first_held_returns_locked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contested.jsonl");
        let _held = SessionWriter::open(&path).unwrap();
        match SessionWriter::open(&path) {
            Err(SessionError::Locked { .. }) => {}
            Err(e) => panic!("expected Locked, got Err({e:?})"),
            Ok(_) => panic!("expected Locked, got Ok"),
        }
        // is_session_locked should agree with the writer's view.
        assert!(is_session_locked(&path));
    }

    #[test]
    fn lock_releases_when_writer_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dropped.jsonl");
        {
            let _w = SessionWriter::open(&path).unwrap();
            assert!(is_session_locked(&path));
        }
        // After Drop the lock is gone.
        assert!(!is_session_locked(&path));
        // And a fresh open succeeds.
        let _w2 = SessionWriter::open(&path).unwrap();
    }

    #[test]
    fn is_session_locked_false_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.jsonl");
        assert!(!is_session_locked(&path));
    }

    #[test]
    fn second_writer_after_first_drops_appends_both() {
        // Smoke test for the previous "second_writer_appends_does_not_truncate"
        // case, now adjusted for advisory locking: opening twice on the same
        // file is fine as long as the first writer dropped first. The
        // chained `open().append()` form drops the temporary at statement
        // end, releasing the lock for the next statement.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seq.jsonl");
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
