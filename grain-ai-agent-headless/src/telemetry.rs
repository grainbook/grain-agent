//! Opt-in local telemetry: per-event JSONL log to a file the user
//! explicitly chose. Nothing is sent over the network. Mirrors pi's
//! core/telemetry.ts at the smallest viable surface — events get
//! timestamped and appended; analysis is the user's job.
//!
//! Wiring: `cli::run` creates a [`TelemetrySink`] if `--telemetry-file`
//! is set, subscribes it to the agent's event bus, and the sink writes
//! one `{ "ts": ..., "event": ... }` line per `AgentEvent`.
//!
//! # Sensitive data warning
//!
//! Event payloads include the **full** content of `MessageEnd`
//! (user prompts), `ToolExecutionStart { args }`, and
//! `ToolExecutionEnd { result }`. Tool arguments can contain API keys,
//! credentials, or other secrets the user typed; tool results contain
//! whatever file contents the agent read. Treat the telemetry file as
//! you would a shell-history log: do not share it without redaction.
//! No automatic redaction is applied — that's intentional, so callers
//! who need a complete audit trail get one, but it means callers who
//! want to publish or ship the file should run their own scrubbing
//! pass.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use grain_agent_core::AgentEvent;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Telemetry log: opens an append-only file and emits one JSONL line per
/// event. Recovers from `Mutex` poisoning the same way the session writer
/// does — a partial write doesn't take down the whole agent.
pub struct TelemetrySink {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

#[derive(Serialize)]
struct TelemetryLine<'a> {
    ts_ms: i64,
    #[serde(flatten)]
    event: &'a AgentEvent,
}

impl TelemetrySink {
    /// Open (or create) `path` and prepare it for append writes.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TelemetryError> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| TelemetryError::Io {
                path: path.display().to_string(),
                source,
            })?;
        Ok(TelemetrySink {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record one event. Returns immediately on serialization failure
    /// after logging to stderr — telemetry must never break the agent
    /// loop, so we trade strict reporting for resilience.
    pub fn record(&self, event: &AgentEvent) {
        let line = TelemetryLine {
            ts_ms: now_ms(),
            event,
        };
        let json = match serde_json::to_string(&line) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[warn] telemetry serialize: {e}");
                return;
            }
        };
        let mut guard = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(e) = guard
            .write_all(json.as_bytes())
            .and_then(|_| guard.write_all(b"\n"))
            .and_then(|_| guard.flush())
        {
            eprintln!(
                "[warn] telemetry write to {}: {e}",
                self.path.display()
            );
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_writes_one_line_per_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let sink = TelemetrySink::open(&path).unwrap();
        sink.record(&AgentEvent::AgentStart);
        sink.record(&AgentEvent::TurnStart);
        drop(sink);
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"ts_ms\""));
        assert!(lines[0].contains("\"agent_start\""));
        assert!(lines[1].contains("\"turn_start\""));
    }

    #[test]
    fn reopen_appends_does_not_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        TelemetrySink::open(&path).unwrap().record(&AgentEvent::AgentStart);
        TelemetrySink::open(&path).unwrap().record(&AgentEvent::TurnStart);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }
}
