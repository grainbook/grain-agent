//! Project-level long-term memory derived from persisted session trees.
//!
//! This is deliberately a local, deterministic first pass: startup reads a
//! small `memory_summary.md` into the system prompt, while refreshes scan
//! existing session trees and regenerate the memory files for future starts.
//! No LLM call is made here, so the TUI's boot path stays predictable.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use grain_agent_core::{AgentMessage, AssistantContent, Message, TextContent, UserContent};
use grain_agent_harness::{SessionMetadata, SessionTreeEntry, SessionTreeEntryKind};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const META_FILE: &str = "meta.json";
const STATE_FILE: &str = "state.json";
const ENTRIES_FILE: &str = "entries.jsonl";
const MEMORY_STATE_FILE: &str = "state.json";
const RAW_MEMORIES_FILE: &str = "raw_memories.jsonl";
const PROJECT_MEMORY_FILE: &str = "project_memory.md";
const MEMORY_SUMMARY_FILE: &str = "memory_summary.md";
const SCHEMA_VERSION: u32 = 1;

/// Default cap on sessions scanned during one refresh. The newest sessions are
/// considered first because those are most likely to contain still-relevant
/// project facts.
pub const DEFAULT_MAX_SESSIONS: usize = 128;
/// Hard cap on generated memory records retained on disk.
pub const DEFAULT_MAX_RECORDS: usize = 160;
/// Hard cap on the summary injected into future system prompts.
pub const DEFAULT_SUMMARY_MAX_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub struct ProjectMemorySettings {
    pub workspace_root: PathBuf,
    pub sessions_dir: PathBuf,
    pub memory_dir: PathBuf,
    pub max_sessions: usize,
    pub max_records: usize,
    pub summary_max_bytes: usize,
}

impl ProjectMemorySettings {
    pub fn for_workspace(workspace_root: impl Into<PathBuf>) -> Self {
        let workspace_root = workspace_root.into();
        ProjectMemorySettings {
            sessions_dir: workspace_root.join(".grain").join("sessions"),
            memory_dir: workspace_root.join(".grain").join("memory"),
            workspace_root,
            max_sessions: DEFAULT_MAX_SESSIONS,
            max_records: DEFAULT_MAX_RECORDS,
            summary_max_bytes: DEFAULT_SUMMARY_MAX_BYTES,
        }
    }

    pub fn with_sessions_dir(mut self, sessions_dir: impl Into<PathBuf>) -> Self {
        self.sessions_dir = sessions_dir.into();
        self
    }

    pub fn with_memory_dir(mut self, memory_dir: impl Into<PathBuf>) -> Self {
        self.memory_dir = memory_dir.into();
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecord {
    pub id: String,
    pub session_id: String,
    pub entry_id: String,
    pub category: MemoryCategory,
    pub text: String,
    pub source_role: String,
    pub score: u8,
    pub timestamp: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemoryCategory {
    Preference,
    Decision,
    ProjectContext,
}

impl MemoryCategory {
    fn heading(self) -> &'static str {
        match self {
            MemoryCategory::Preference => "User And Team Preferences",
            MemoryCategory::Decision => "Project Decisions",
            MemoryCategory::ProjectContext => "Project Context",
        }
    }

    fn rank(self) -> u8 {
        match self {
            MemoryCategory::Preference => 0,
            MemoryCategory::Decision => 1,
            MemoryCategory::ProjectContext => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRefreshReport {
    pub memory_dir: PathBuf,
    pub scanned_sessions: usize,
    pub refreshed_sessions: usize,
    pub reused_sessions: usize,
    pub record_count: usize,
    pub summary_bytes: usize,
}

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("json error in {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MemoryState {
    schema_version: u32,
    last_run_unix: u64,
    processed_sessions: BTreeMap<String, ProcessedSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProcessedSession {
    leaf_id: Option<String>,
    entry_count: usize,
    updated_unix: u64,
}

#[derive(Debug)]
struct SessionSnapshot {
    id: String,
    leaf_id: Option<String>,
    entries: Vec<SessionTreeEntry>,
    modified_at: SystemTime,
}

#[derive(Debug)]
struct Candidate {
    category: MemoryCategory,
    text: String,
    source_role: &'static str,
    score: u8,
}

/// Load the short prompt fragment that should be appended to the agent's
/// system prompt. Missing or empty memory is not an error.
pub fn load_project_memory_prompt(memory_dir: &Path) -> Result<Option<String>, MemoryError> {
    let path = memory_dir.join(MEMORY_SUMMARY_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(MemoryError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let summary = raw.trim();
    if summary.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "## Project Memory\n\n\
         These notes were extracted from previous sessions in this workspace. \
         Treat them as lower priority than the current user request, repository \
         files, and AGENTS.md.\n\n\
         <project_memory>\n{summary}\n</project_memory>"
    )))
}

/// Return the current short memory summary, if any.
pub fn read_memory_summary(memory_dir: &Path) -> Result<Option<String>, MemoryError> {
    let path = memory_dir.join(MEMORY_SUMMARY_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(MemoryError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Refresh `.grain/memory` from session tree data. Existing raw records are
/// reused for unchanged sessions, and the final files are rewritten from the
/// current capped record set so they do not grow without bound.
pub fn refresh_project_memory(
    settings: &ProjectMemorySettings,
) -> Result<MemoryRefreshReport, MemoryError> {
    std::fs::create_dir_all(&settings.memory_dir).map_err(|source| MemoryError::Io {
        path: settings.memory_dir.display().to_string(),
        source,
    })?;

    let mut state = load_state(&settings.memory_dir)?;
    if state.schema_version == 0 {
        state.schema_version = SCHEMA_VERSION;
    }
    let old_records = load_raw_records(&settings.memory_dir)?;
    let mut old_by_session: HashMap<String, Vec<MemoryRecord>> = HashMap::new();
    for record in old_records {
        old_by_session
            .entry(record.session_id.clone())
            .or_default()
            .push(record);
    }

    let mut snapshots = list_session_snapshots(&settings.sessions_dir)?;
    snapshots.sort_by_key(|snapshot| {
        std::cmp::Reverse(
            snapshot
                .modified_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        )
    });
    snapshots.truncate(settings.max_sessions);

    let mut scanned_sessions = 0usize;
    let mut refreshed_sessions = 0usize;
    let mut reused_sessions = 0usize;
    let mut records = Vec::new();
    let mut next_state = MemoryState {
        schema_version: SCHEMA_VERSION,
        last_run_unix: now_unix(),
        processed_sessions: BTreeMap::new(),
    };

    for snapshot in snapshots {
        scanned_sessions += 1;
        let current = ProcessedSession {
            leaf_id: snapshot.leaf_id.clone(),
            entry_count: snapshot.entries.len(),
            updated_unix: now_unix(),
        };
        let unchanged = state
            .processed_sessions
            .get(&snapshot.id)
            .map(|prev| prev.leaf_id == current.leaf_id && prev.entry_count == current.entry_count)
            .unwrap_or(false);

        if unchanged && let Some(mut old) = old_by_session.remove(&snapshot.id) {
            reused_sessions += 1;
            records.append(&mut old);
        } else {
            refreshed_sessions += 1;
            records.extend(extract_records_from_snapshot(&snapshot));
        }
        next_state
            .processed_sessions
            .insert(snapshot.id.clone(), current);
    }

    let mut records = dedupe_and_cap(records, settings.max_records);
    records.sort_by(|a, b| {
        a.category
            .rank()
            .cmp(&b.category.rank())
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| b.timestamp.cmp(&a.timestamp))
            .then_with(|| a.text.cmp(&b.text))
    });

    let project_memory = render_project_memory(&records, &settings.workspace_root);
    let summary = truncate_utf8(&render_memory_summary(&records), settings.summary_max_bytes);

    write_json_pretty_atomic(&settings.memory_dir.join(MEMORY_STATE_FILE), &next_state)?;
    write_raw_records_atomic(&settings.memory_dir.join(RAW_MEMORIES_FILE), &records)?;
    write_string_atomic(
        &settings.memory_dir.join(PROJECT_MEMORY_FILE),
        &project_memory,
    )?;
    write_string_atomic(&settings.memory_dir.join(MEMORY_SUMMARY_FILE), &summary)?;

    Ok(MemoryRefreshReport {
        memory_dir: settings.memory_dir.clone(),
        scanned_sessions,
        refreshed_sessions,
        reused_sessions,
        record_count: records.len(),
        summary_bytes: summary.len(),
    })
}

fn load_state(memory_dir: &Path) -> Result<MemoryState, MemoryError> {
    let path = memory_dir.join(MEMORY_STATE_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MemoryState {
                schema_version: SCHEMA_VERSION,
                ..MemoryState::default()
            });
        }
        Err(source) => {
            return Err(MemoryError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    serde_json::from_str::<MemoryState>(&raw).map_err(|source| MemoryError::Json {
        path: path.display().to_string(),
        source,
    })
}

fn load_raw_records(memory_dir: &Path) -> Result<Vec<MemoryRecord>, MemoryError> {
    let path = memory_dir.join(RAW_MEMORIES_FILE);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(MemoryError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|source| MemoryError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<MemoryRecord>(trimmed) {
            out.push(record);
        }
    }
    Ok(out)
}

fn list_session_snapshots(sessions_dir: &Path) -> Result<Vec<SessionSnapshot>, MemoryError> {
    let read_dir = match std::fs::read_dir(sessions_dir) {
        Ok(read_dir) => read_dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(MemoryError::Io {
                path: sessions_dir.display().to_string(),
                source,
            });
        }
    };
    let mut out = Vec::new();
    for entry in read_dir.flatten() {
        if !entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false) {
            continue;
        }
        match read_session_snapshot(&entry.path()) {
            Ok(Some(snapshot)) => out.push(snapshot),
            Ok(None) => {}
            Err(e) => eprintln!(
                "[warn] project-memory: skipping {} ({e})",
                entry.path().display()
            ),
        }
    }
    Ok(out)
}

fn read_session_snapshot(path: &Path) -> Result<Option<SessionSnapshot>, MemoryError> {
    let metadata_path = path.join(META_FILE);
    let metadata = match std::fs::read_to_string(&metadata_path) {
        Ok(raw) => {
            serde_json::from_str::<SessionMetadata>(&raw).map_err(|source| MemoryError::Json {
                path: metadata_path.display().to_string(),
                source,
            })?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(MemoryError::Io {
                path: metadata_path.display().to_string(),
                source,
            });
        }
    };

    let entries_path = path.join(ENTRIES_FILE);
    let file = match std::fs::File::open(&entries_path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(MemoryError::Io {
                path: entries_path.display().to_string(),
                source,
            });
        }
    };
    let modified_at = entries_path
        .metadata()
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut by_id = HashMap::new();
    for line in reader.lines() {
        let line = line.map_err(|source| MemoryError::Io {
            path: entries_path.display().to_string(),
            source,
        })?;
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
    if entries.is_empty() {
        return Ok(None);
    }

    let leaf_id = read_leaf_id(path).or_else(|| entries.last().map(|entry| entry.id.clone()));
    let mut active_path = Vec::new();
    let mut current = leaf_id.clone();
    while let Some(id) = current {
        let Some(idx) = by_id.get(&id).copied() else {
            break;
        };
        let entry = entries[idx].clone();
        current = entry.parent_id.clone();
        active_path.push(entry);
    }
    active_path.reverse();

    Ok(Some(SessionSnapshot {
        id: metadata.id,
        leaf_id,
        entries: active_path,
        modified_at,
    }))
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

fn extract_records_from_snapshot(snapshot: &SessionSnapshot) -> Vec<MemoryRecord> {
    let mut out = Vec::new();
    for entry in &snapshot.entries {
        let candidates = extract_candidates(entry);
        for (idx, candidate) in candidates.into_iter().enumerate() {
            out.push(MemoryRecord {
                id: format!("{}:{}:{idx}", snapshot.id, entry.id),
                session_id: snapshot.id.clone(),
                entry_id: entry.id.clone(),
                category: candidate.category,
                text: candidate.text,
                source_role: candidate.source_role.to_string(),
                score: candidate.score,
                timestamp: entry.timestamp.clone(),
            });
        }
    }
    out
}

fn extract_candidates(entry: &SessionTreeEntry) -> Vec<Candidate> {
    match &entry.kind {
        SessionTreeEntryKind::Message { message } => extract_from_agent_message(message),
        SessionTreeEntryKind::Compaction { summary, .. } => extract_from_text(
            summary,
            "compactionSummary",
            3,
            Some(MemoryCategory::ProjectContext),
        ),
        SessionTreeEntryKind::BranchSummary { summary, .. } => extract_from_text(
            summary,
            "branchSummary",
            3,
            Some(MemoryCategory::ProjectContext),
        ),
        SessionTreeEntryKind::CustomMessage {
            content, display, ..
        } if *display => content
            .as_str()
            .map(|text| extract_from_text(text, "custom", 1, None))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn extract_from_agent_message(message: &AgentMessage) -> Vec<Candidate> {
    match message {
        AgentMessage::Standard(Message::User(user)) => {
            extract_from_text(&user_content_text(&user.content), "user", 2, None)
        }
        AgentMessage::Standard(Message::Assistant(assistant)) => {
            let mut text = String::new();
            for content in &assistant.content {
                if let AssistantContent::Text(TextContent { text: t }) = content {
                    push_joined(&mut text, t);
                }
            }
            extract_from_text(&text, "assistant", 1, None)
        }
        AgentMessage::Custom(value) => match value.get("role").and_then(|role| role.as_str()) {
            Some("compactionSummary") => {
                let summary = value
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default();
                extract_from_text(
                    summary,
                    "compactionSummary",
                    3,
                    Some(MemoryCategory::ProjectContext),
                )
            }
            Some("branchSummary") => {
                let summary = value
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default();
                extract_from_text(
                    summary,
                    "branchSummary",
                    3,
                    Some(MemoryCategory::ProjectContext),
                )
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn user_content_text(content: &[UserContent]) -> String {
    let mut out = String::new();
    for item in content {
        if let UserContent::Text(TextContent { text }) = item {
            push_joined(&mut out, text);
        }
    }
    out
}

fn push_joined(out: &mut String, part: &str) {
    let part = part.trim();
    if part.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(part);
}

fn extract_from_text(
    text: &str,
    source_role: &'static str,
    base_score: u8,
    forced_category: Option<MemoryCategory>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for fragment in durable_fragments(text) {
        if looks_sensitive(&fragment) || looks_like_embedded_prompt(&fragment) {
            continue;
        }
        let Some((category, keyword_score)) = forced_category
            .map(|category| (category, 1))
            .or_else(|| classify_fragment(&fragment))
        else {
            continue;
        };
        let score = base_score.saturating_add(keyword_score).min(10);
        if score < 3 {
            continue;
        }
        out.push(Candidate {
            category,
            text: fragment,
            source_role,
            score,
        });
    }
    out
}

fn durable_fragments(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_code = false;
    for raw_line in text.lines() {
        let mut line = raw_line.trim();
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code || line.is_empty() {
            continue;
        }
        line = strip_bullet_prefix(line);
        if line.is_empty() {
            continue;
        }
        for part in split_sentence_like(line) {
            let normalized = normalize_space(part);
            if normalized.chars().count() < 8 {
                continue;
            }
            out.push(truncate_chars(&normalized, 280));
        }
    }
    out
}

fn strip_bullet_prefix(mut line: &str) -> &str {
    loop {
        let trimmed = line.trim_start_matches([' ', '\t']);
        let Some(first) = trimmed.chars().next() else {
            return "";
        };
        if matches!(first, '-' | '*' | '•' | '·') {
            line = trimmed[first.len_utf8()..].trim_start();
            continue;
        }
        if first.is_ascii_digit()
            && let Some((idx, ch)) = trimmed
                .char_indices()
                .find(|(_, ch)| matches!(ch, '.' | ')' | '、'))
        {
            let prefix = &trimmed[..idx];
            if prefix.chars().all(|ch| ch.is_ascii_digit()) {
                line = trimmed[idx + ch.len_utf8()..].trim_start();
                continue;
            }
        }
        return trimmed;
    }
}

fn split_sentence_like(line: &str) -> Vec<&str> {
    if line.chars().count() <= 220 {
        return vec![line];
    }
    line.split(['。', '；', ';', '!', '！', '?', '？'])
        .filter(|part| !part.trim().is_empty())
        .collect()
}

fn normalize_space(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn classify_fragment(fragment: &str) -> Option<(MemoryCategory, u8)> {
    let lower = fragment.to_ascii_lowercase();
    let checks: &[(&[&str], MemoryCategory, u8)] = &[
        (
            &[
                "记住",
                "以后",
                "偏好",
                "默认",
                "不要",
                "不用",
                "必须",
                "我想",
                "我希望",
                "prefer",
                "preference",
                "always",
                "never",
                "default",
                "must",
            ],
            MemoryCategory::Preference,
            3,
        ),
        (
            &[
                "决定",
                "核心区别",
                "设计",
                "方案",
                "实现",
                "切到",
                "改成",
                "采用",
                "架构",
                "decision",
                "decided",
                "design",
                "implemented",
                "use ",
            ],
            MemoryCategory::Decision,
            2,
        ),
        (
            &[
                "tui",
                "session",
                "jsonlsessionrepo",
                "compaction",
                "skill",
                ".grain",
                "agents.md",
                "cargo",
                "rust",
                "provider",
                "memory",
                "workspace",
            ],
            MemoryCategory::ProjectContext,
            1,
        ),
    ];

    for (needles, category, score) in checks {
        if needles
            .iter()
            .any(|needle| lower.contains(&needle.to_ascii_lowercase()))
        {
            return Some((*category, *score));
        }
    }
    if looks_like_path(fragment) {
        return Some((MemoryCategory::ProjectContext, 1));
    }
    None
}

fn looks_like_path(fragment: &str) -> bool {
    fragment.contains('/')
        && fragment
            .chars()
            .any(|ch| ch == '.' || ch == '_' || ch == '-')
}

fn looks_sensitive(fragment: &str) -> bool {
    let lower = fragment.to_ascii_lowercase();
    let sensitive = [
        "api_key",
        "apikey",
        "secret",
        "password",
        "passwd",
        "token",
        "authorization:",
        "bearer ",
        "sk-",
        "-----begin",
    ];
    sensitive.iter().any(|needle| lower.contains(needle))
}

fn looks_like_embedded_prompt(fragment: &str) -> bool {
    let lower = fragment.trim_start().to_ascii_lowercase();
    [
        "use when ",
        "you are ",
        "your task ",
        "follow these instructions",
        "instructions:",
        "<available_skills>",
        "</available_skills>",
        "<skill",
        "</skill",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

fn dedupe_and_cap(records: Vec<MemoryRecord>, max_records: usize) -> Vec<MemoryRecord> {
    let mut best_by_key: HashMap<String, MemoryRecord> = HashMap::new();
    for record in records {
        let key = normalize_memory_key(&record.text);
        match best_by_key.get(&key) {
            Some(existing)
                if existing.score > record.score
                    || (existing.score == record.score
                        && existing.timestamp >= record.timestamp) => {}
            _ => {
                best_by_key.insert(key, record);
            }
        }
    }
    let mut out: Vec<_> = best_by_key.into_values().collect();
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.timestamp.cmp(&a.timestamp))
            .then_with(|| a.category.rank().cmp(&b.category.rank()))
            .then_with(|| a.text.cmp(&b.text))
    });
    out.truncate(max_records);
    out
}

fn normalize_memory_key(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .filter(|ch| !ch.is_whitespace() && !ch.is_ascii_punctuation())
        .collect()
}

fn render_project_memory(records: &[MemoryRecord], workspace_root: &Path) -> String {
    let mut out = String::new();
    out.push_str("# Project Memory\n\n");
    out.push_str(&format!("Workspace: `{}`\n\n", workspace_root.display()));
    out.push_str(
        "Generated from prior session trees. Current user instructions and repository files win over these notes.\n\n",
    );
    for category in [
        MemoryCategory::Preference,
        MemoryCategory::Decision,
        MemoryCategory::ProjectContext,
    ] {
        out.push_str(&format!("## {}\n\n", category.heading()));
        let mut wrote = false;
        for record in records.iter().filter(|record| record.category == category) {
            wrote = true;
            out.push_str(&format!(
                "- {}  \n  Source: session `{}`, entry `{}`, score {}\n",
                record.text, record.session_id, record.entry_id, record.score
            ));
        }
        if !wrote {
            out.push_str("- (none yet)\n");
        }
        out.push('\n');
    }
    out
}

fn render_memory_summary(records: &[MemoryRecord]) -> String {
    if records.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(
        "Current user instructions and repository files override these remembered notes.\n",
    );
    for category in [
        MemoryCategory::Preference,
        MemoryCategory::Decision,
        MemoryCategory::ProjectContext,
    ] {
        let mut seen = HashSet::new();
        let mut group: Vec<_> = records
            .iter()
            .filter(|record| record.category == category)
            .collect();
        group.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.timestamp.cmp(&a.timestamp))
        });
        let limit = match category {
            MemoryCategory::Preference => 12,
            MemoryCategory::Decision => 12,
            MemoryCategory::ProjectContext => 10,
        };
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("\n### {}\n", category.heading()));
        for record in group.into_iter().take(limit) {
            let key = normalize_memory_key(&record.text);
            if !seen.insert(key) {
                continue;
            }
            out.push_str("- ");
            out.push_str(&record.text);
            out.push('\n');
        }
    }
    out
}

fn write_json_pretty_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), MemoryError> {
    let raw = serde_json::to_string_pretty(value).map_err(|source| MemoryError::Json {
        path: path.display().to_string(),
        source,
    })?;
    write_string_atomic(path, &(raw + "\n"))
}

fn write_raw_records_atomic(path: &Path, records: &[MemoryRecord]) -> Result<(), MemoryError> {
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp).map_err(|source| MemoryError::Io {
        path: tmp.display().to_string(),
        source,
    })?;
    for record in records {
        let line = serde_json::to_string(record).map_err(|source| MemoryError::Json {
            path: path.display().to_string(),
            source,
        })?;
        writeln!(file, "{line}").map_err(|source| MemoryError::Io {
            path: tmp.display().to_string(),
            source,
        })?;
    }
    file.flush().map_err(|source| MemoryError::Io {
        path: tmp.display().to_string(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| MemoryError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn write_string_atomic(path: &Path, content: &str) -> Result<(), MemoryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| MemoryError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|source| MemoryError::Io {
        path: tmp.display().to_string(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| MemoryError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].trim_end().to_string();
    out.push_str("\n- (memory summary truncated)\n");
    out
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_core::{AgentMessage, Message, UserContent, UserMessage};
    use grain_agent_harness::{SessionMetadata, SessionTreeEntry, SessionTreeEntryKind};

    fn write_session(dir: &Path, id: &str, messages: Vec<AgentMessage>) {
        let session_dir = dir.join(id);
        std::fs::create_dir_all(&session_dir).unwrap();
        let meta = SessionMetadata::with_id(id);
        std::fs::write(
            session_dir.join(META_FILE),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        let mut parent_id = None;
        let mut leaf_id = None;
        let mut lines = String::new();
        for (idx, message) in messages.into_iter().enumerate() {
            let entry = SessionTreeEntry {
                id: format!("entry-{idx}"),
                parent_id: parent_id.clone(),
                timestamp: format!("2026-05-26T00:00:0{idx}.000Z"),
                kind: SessionTreeEntryKind::Message { message },
            };
            parent_id = Some(entry.id.clone());
            leaf_id = Some(entry.id.clone());
            lines.push_str(&serde_json::to_string(&entry).unwrap());
            lines.push('\n');
        }
        std::fs::write(session_dir.join(ENTRIES_FILE), lines).unwrap();
        std::fs::write(
            session_dir.join(STATE_FILE),
            serde_json::json!({ "leafId": leaf_id }).to_string(),
        )
        .unwrap();
    }

    fn user(text: &str) -> AgentMessage {
        AgentMessage::Standard(Message::User(UserMessage {
            content: vec![UserContent::text(text)],
            timestamp: 0,
        }))
    }

    #[test]
    fn refresh_extracts_summary_from_session_tree() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let memory = dir.path().join("memory");
        write_session(
            &sessions,
            "s1",
            vec![user(
                "记住：以后这个项目默认只用项目内 skills，不加载全局 skills。",
            )],
        );
        let settings = ProjectMemorySettings::for_workspace(dir.path())
            .with_sessions_dir(&sessions)
            .with_memory_dir(&memory);

        let report = refresh_project_memory(&settings).unwrap();

        assert_eq!(report.scanned_sessions, 1);
        assert!(report.record_count >= 1);
        let summary = std::fs::read_to_string(memory.join(MEMORY_SUMMARY_FILE)).unwrap();
        assert!(summary.contains("项目内 skills"));
        let prompt = load_project_memory_prompt(&memory).unwrap().unwrap();
        assert!(prompt.contains("<project_memory>"));
        assert!(prompt.contains("项目内 skills"));
    }

    #[test]
    fn refresh_drops_sensitive_lines() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let memory = dir.path().join("memory");
        write_session(
            &sessions,
            "s1",
            vec![user("记住：OPENAI_API_KEY=sk-secret 不要暴露。")],
        );
        let settings = ProjectMemorySettings::for_workspace(dir.path())
            .with_sessions_dir(&sessions)
            .with_memory_dir(&memory);

        refresh_project_memory(&settings).unwrap();

        let summary = std::fs::read_to_string(memory.join(MEMORY_SUMMARY_FILE)).unwrap();
        assert!(!summary.contains("sk-secret"));
        assert!(!summary.contains("OPENAI_API_KEY"));
    }
}
