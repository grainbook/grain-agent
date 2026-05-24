//! Pure UI state — no I/O. Every state transition is a method on
//! [`AppState`] taking a [`crate::event::TuiEvent`] or a key event, and
//! returning zero-or-more [`Command`]s for the agent worker to execute.
//!
//! Keeping the state machine pure lets us unit-test render-relevant
//! behavior without touching a real terminal or LLM.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use grain_agent_core::{
    AgentEvent, AgentMessage, AssistantMessageEvent, Cost, Message, UserContent,
};

use crate::anim::EffectManager;
use crate::event::TuiEvent;
use crate::md_render::MarkdownCache;
use crate::theme::Theme;
use grain_llm_genai::ProviderProfile;

/// Top-level UI focus. Drives which pane receives key events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// Typing into the prompt input line (default).
    #[default]
    Input,
    /// Scrolling the transcript pane.
    Transcript,
}

/// Pop-up overlays shown on top of the main layout. Only one at a time;
/// `Esc` always closes the active overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Help,
    /// Doctor report from `grain_ai_agent_headless::render_doctor_report`,
    /// plus per-overlay search state: live filter `query` (matched
    /// case-insensitively against each line) and `scroll` offset for
    /// when the (possibly filtered) report is taller than the body
    /// pane.
    Doctor {
        report: String,
        query: String,
        scroll: usize,
    },
    /// Loaded skill names + descriptions + disabled flag.
    Skills(Vec<(String, String, bool)>),
    /// Theme picker — `focused` is the index into [`AppState::themes`].
    /// Up/Down navigate, Enter applies, Esc cancels.
    ThemePicker {
        focused: usize,
    },
    /// Provider profile picker — `focused` is the index into
    /// [`AppState::providers`]. Same key model as ThemePicker.
    ProviderPicker {
        focused: usize,
    },
    /// Model picker — `focused` is the index into the filtered list of
    /// models returned by the worker for the current provider. Same key
    /// model as ThemePicker / ProviderPicker. `query` is a live search
    /// filter matched case-insensitively against model id and name.
    ModelPicker {
        focused: usize,
        models: Vec<(String, String)>, // (id, name)
        query: String,
    },
    /// Request-body log overlay. Joins entries from
    /// [`AppState::request_log`] with blank-line separators; `scroll`
    /// is the rendered-row offset for paging. Opened via `/log`.
    /// The buffer is only populated when `--debug-log` is on.
    Log {
        scroll: usize,
    },
    /// Session-resume picker (Claude-Code-style `/resume`). Shows
    /// past `<uuidv7>.jsonl` files discovered by the worker. `Enter`
    /// in Phase 1 prints a `relaunch with --session <path>` hint into
    /// the transcript; true in-place swap lands in Phase 4 after the
    /// TUI worker migrates to `AgentHarness::new`.
    SessionResume {
        focused: usize,
        sessions: Vec<grain_ai_agent_headless::SessionMeta>,
        /// `Delete` is a two-step action: the first press arms this
        /// flag (UI flips the hint into a "press Delete again to
        /// confirm" warning); the second press emits
        /// [`Command::DeleteSession`]. Any other navigation key (Up,
        /// Down, Home, End, Enter) resets it so users don't trigger
        /// a delete after moving focus.
        confirm_delete: bool,
    },
    /// `lazy.gagent` plugin overlay (`/plugins`). Read-only listing
    /// of plugins discovered under `<workspace>/.grain/plugins/`,
    /// plus any `[[ui_command]]` entries plugins contributed —
    /// rendered as footer hints + key bindings that dispatch into
    /// the plugin's Rhai handler.
    Plugins {
        plugins: Vec<grain_ai_agent_headless::PluginInfo>,
        ui_commands: Vec<grain_ai_agent_headless::BoundUiCommand>,
    },
    /// Dynamic multi-field form pushed by a plugin UI handler's
    /// `OverlayDescriptor::Form`. The TUI owns input state per
    /// field; on Enter, the focused-field buffers are bundled into
    /// a JSON map and dispatched to the named `on_submit` handler.
    DynamicForm {
        title: String,
        fields: Vec<DynamicFormFieldState>,
        on_submit: String,
        focused: usize,
    },
    /// Dynamic message box pushed by a plugin UI handler's
    /// `OverlayDescriptor::Modal`. Dismissed with Esc / Enter.
    DynamicModal {
        title: String,
        body: String,
        severity: grain_ai_agent_headless::ModalSeverity,
    },
    /// Dynamic yes/no prompt pushed by a plugin UI handler's
    /// `OverlayDescriptor::Confirm`. Yes dispatches `on_yes` with
    /// `yes_args`; No dismisses.
    DynamicConfirm {
        title: String,
        body: String,
        on_yes: String,
        yes_args: serde_json::Value,
    },
    /// Selectable list (`OverlayDescriptor::List`). Up/Down
    /// navigates; Enter dispatches `on_select` with
    /// `{ index, value }` if set, else dismisses.
    DynamicList {
        title: String,
        items: Vec<String>,
        on_select: Option<String>,
        focused: usize,
    },
    /// Tabular display with optional row selection
    /// (`OverlayDescriptor::Table`). Up/Down navigates; Enter
    /// dispatches `on_select` with `{ row_index, row }` if set.
    DynamicTable {
        title: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        on_select: Option<String>,
        focused: usize,
    },
    /// Display-only styled-text panel (`OverlayDescriptor::TextPanel`).
    /// Esc dismisses.
    DynamicTextPanel {
        title: String,
        lines: Vec<grain_ai_agent_headless::TextLine>,
        footer: Option<String>,
    },
    /// Display-only progress bar (`OverlayDescriptor::Progress`).
    /// Esc dismisses. Plugin re-issues to animate.
    DynamicProgress {
        title: String,
        value: i64,
        max: i64,
        label: String,
    },
    /// Display-only vertical stack of widgets
    /// (`OverlayDescriptor::Stack`). Each child renders in
    /// declaration order; no key routing across children — Esc
    /// dismisses the whole stack. For interactivity, the plugin
    /// returns a single non-Stack widget.
    DynamicStack {
        title: String,
        children: Vec<grain_ai_agent_headless::OverlayDescriptor>,
    },
    /// Modal shown when the live `SessionWriter` can't take the
    /// advisory lock on a jsonl — either at boot (auto-resume
    /// candidate held by another grain process) or at runtime when
    /// the user picks a `[locked]` row in the `/resume` overlay.
    /// Default focus is on the safest option ("Start a fresh
    /// session"), matching the rule of least destruction.
    SessionLockConflict {
        source: SessionLockSource,
        locked_path: std::path::PathBuf,
        choices: Vec<SessionConflictChoice>,
        focused: usize,
    },
}

/// Where a [`Overlay::SessionLockConflict`] originated. Drives the
/// choice list (boot offers `Quit`, resume offers `Cancel`) and the
/// rendered title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLockSource {
    /// Resolved at startup: auto-resume picked a candidate that
    /// another grain process holds. The worker already swapped in a
    /// fresh path so the user can dismiss safely.
    Boot,
    /// Triggered from inside the `/resume` overlay when the user
    /// hit Enter on a `[locked]` row. Dismissing returns to the
    /// running session.
    Resume,
}

/// Choices presented in [`Overlay::SessionLockConflict`]. Boot shows
/// `Fresh / Fork / Quit`; resume shows `Fresh / Fork / Cancel`. The
/// variant carries no payload — the overlay holds `locked_path`
/// itself, the dispatcher reads both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionConflictChoice {
    /// Discard the locked candidate, stay on the fresh session the
    /// worker minted. No copy, no resume.
    Fresh,
    /// Copy the locked jsonl to a new uuidv7 path and resume the
    /// copy in place (worker handles via `Command::ForkSession`).
    Fork,
    /// Jump to the `/resume` picker to browse past sessions.
    Resume,
    /// (Boot only) Quit the TUI without touching anything.
    Quit,
    /// (Resume only) Close the modal and stay on the current
    /// session, no swap.
    Cancel,
}

/// Per-field editor state inside [`Overlay::DynamicForm`]. The TUI
/// holds the live buffer; the field's declarative shape (label,
/// placeholder, initial) came from the plugin descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicFormFieldState {
    pub name: String,
    pub label: String,
    pub placeholder: String,
    /// Editable buffer. Initialized from
    /// [`grain_ai_agent_headless::FormField::initial`].
    pub value: String,
}

/// One row in the transcript. Kept as plain strings so the renderer can
/// re-flow and re-style without re-parsing transcript history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub kind: TranscriptKind,
    pub text: String,
}

#[derive(Debug, Clone)]
struct ActiveToolDisplay {
    name: String,
    args: serde_json::Value,
    start_line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptKind {
    UserPrompt,
    AssistantText,
    ThinkingText,
    ToolCallStart,
    ToolCallEnd,
    /// A tool-call result line that came back with `is_error == true`.
    /// Renders the same `⎿  …` continuation as [`Self::ToolCallEnd`]
    /// but styled with the error palette so failures jump out. Treated
    /// as a tool-call terminator everywhere — see
    /// [`is_tool_call_terminator`].
    ToolCallError,
    Info,
    Error,
}

/// True when `kind` ends a tool-call block (either a normal
/// completion or an error). Used by block grouping, turn-boundary
/// detection, and the message counter to treat both end-of-call
/// variants uniformly.
pub(crate) fn is_tool_call_terminator(kind: TranscriptKind) -> bool {
    matches!(
        kind,
        TranscriptKind::ToolCallEnd | TranscriptKind::ToolCallError
    )
}

/// Does this transcript kind warrant a fade-in animation when it
/// first appears? Skip the user's own keystrokes (a flash on
/// content they just typed reads as a flicker, not a transition)
/// and bare `Info` / `Error` rows (the status / error rows already
/// have their own dedicated flash effects).
fn kind_should_fade(kind: TranscriptKind) -> bool {
    matches!(
        kind,
        TranscriptKind::AssistantText
            | TranscriptKind::ThinkingText
            | TranscriptKind::ToolCallStart
            | TranscriptKind::ToolCallEnd
            | TranscriptKind::ToolCallError
    )
}

/// A logical group of consecutive [`TranscriptLine`]s that the
/// renderer treats as one foldable unit.
///
/// - `Plain` blocks are always single lines; they're never folded.
/// - `Thinking` blocks aggregate consecutive `ThinkingText` lines.
/// - `ToolCall` blocks span from a `ToolCallStart` line through its
///   matching `ToolCallEnd` (or the buffer tail if the call is
///   still in-flight). Any lines in between (e.g. AssistantText
///   that arrived mid-call) come along for the ride so the visual
///   group reads as one chunk.
///
/// Block IDs are stable across re-renders because they're derived
/// from `first_line`, which never shifts (transcript is append-only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptBlock {
    pub first_line: usize,
    pub last_line: usize,
    pub kind: BlockKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Plain,
    Thinking,
    ToolCall,
}

impl TranscriptBlock {
    pub fn line_count(&self) -> usize {
        self.last_line.saturating_sub(self.first_line) + 1
    }

    /// Stable id usable as a HashMap key for fold overrides.
    pub fn id(&self) -> usize {
        self.first_line
    }

    /// `true` when this block kind participates in fold/unfold.
    /// `Plain` blocks always render their single line; folding
    /// them adds no value.
    pub fn is_foldable(&self) -> bool {
        !matches!(self.kind, BlockKind::Plain)
    }
}

/// Walk `transcript` and group consecutive lines into
/// [`TranscriptBlock`]s. Append-only safety: existing block
/// `first_line` values stay stable as new lines are appended to
/// the buffer, so fold-state stored against those ids survives
/// every subsequent render.
pub(crate) fn build_transcript_blocks(transcript: &[TranscriptLine]) -> Vec<TranscriptBlock> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < transcript.len() {
        let line = &transcript[i];
        match line.kind {
            TranscriptKind::ThinkingText => {
                let start = i;
                let mut end = i;
                while end + 1 < transcript.len()
                    && transcript[end + 1].kind == TranscriptKind::ThinkingText
                {
                    end += 1;
                }
                out.push(TranscriptBlock {
                    first_line: start,
                    last_line: end,
                    kind: BlockKind::Thinking,
                });
                i = end + 1;
            }
            TranscriptKind::ToolCallStart => {
                let start = i;
                let mut end = i;
                let mut k = i + 1;
                while k < transcript.len() {
                    end = k;
                    if is_tool_call_terminator(transcript[k].kind) {
                        break;
                    }
                    k += 1;
                }
                // If we walked off the end without finding the
                // matching `End`, the call is still in-flight;
                // `end` already points at the last line we have.
                if k == transcript.len() {
                    end = transcript.len().saturating_sub(1);
                }
                out.push(TranscriptBlock {
                    first_line: start,
                    last_line: end,
                    kind: BlockKind::ToolCall,
                });
                i = end + 1;
            }
            _ => {
                out.push(TranscriptBlock {
                    first_line: i,
                    last_line: i,
                    kind: BlockKind::Plain,
                });
                i += 1;
            }
        }
    }
    out
}

impl AppState {
    /// Returns the cached transcript blocks, rebuilding the cache
    /// only when new lines have been appended since the last build.
    /// The transcript is append-only, so previously-computed blocks
    /// stay valid across frames.
    /// Returns a clone of the cached transcript blocks, rebuilding
    /// the cache only when new lines have been appended since the
    /// last build. Returns an owned `Vec` so callers can freely
    /// access other `AppState` fields without borrow conflicts.
    pub(crate) fn cached_blocks(&mut self) -> Vec<TranscriptBlock> {
        if self.cached_blocks.is_empty() || self.transcript_len_cached != self.transcript.len() {
            self.cached_blocks = build_transcript_blocks(&self.transcript);
            self.transcript_len_cached = self.transcript.len();
            // Pre-compute block summaries so the hot render path
            // avoids `format!()` per frame.
            self.block_summary_cache.clear();
            for block in &self.cached_blocks {
                let summary = Self::compute_block_summary(block, &self.transcript);
                self.block_summary_cache.insert(block.id(), summary);
            }
        }
        self.cached_blocks.clone()
    }

    /// Returns a pre-computed one-line display summary for `block`.
    /// Callers must have called [`Self::cached_blocks`] at least
    /// once this frame so the cache is populated.
    pub(crate) fn block_summary(&self, block: &TranscriptBlock) -> &str {
        self.block_summary_cache
            .get(&block.id())
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    /// Build the display summary string for a single block.
    /// Mirrors the old `ui::block_summary` free function.
    fn compute_block_summary(block: &TranscriptBlock, transcript: &[TranscriptLine]) -> String {
        let count = block.line_count();
        match block.kind {
            BlockKind::ToolCall => {
                let head = transcript
                    .get(block.first_line)
                    .map(|l| l.text.as_str())
                    .unwrap_or("");
                let cleaned = head
                    .strip_prefix("●! ")
                    .or_else(|| head.strip_prefix("● "))
                    .unwrap_or(head)
                    .trim();
                let label = format!("tool: {}", truncate_oneline(cleaned, 60));
                if count > 1 {
                    format!("{label} ({count} lines)")
                } else {
                    label
                }
            }
            BlockKind::Thinking => {
                format!(
                    "thinking ({count} line{})",
                    if count == 1 { "" } else { "s" }
                )
            }
            BlockKind::Plain => transcript
                .get(block.first_line)
                .map(|l| l.text.clone())
                .unwrap_or_default(),
        }
    }
}

/// Commands the agent worker should execute on behalf of the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    SendPrompt(String),
    AbortCurrentTurn,
    Reset,
    /// Render a doctor report — worker computes it (may shell out to
    /// `git`) and replies via [`TuiEvent::OverlayDoctor`].
    ReturnDoctor,
    /// Re-load skills from disk and reply via [`TuiEvent::OverlaySkills`].
    ReturnSkills,
    /// Switch the active provider profile. Worker rebuilds routing
    /// and calls `Agent::set_model(...)` then replies with
    /// [`TuiEvent::ProviderApplied`].
    ApplyProvider(usize),
    /// Request the list of models for the current provider. Worker
    /// replies via [`TuiEvent::ModelsListed`], which populates the
    /// `/model` overlay.
    ListModels(String),
    /// Switch to a specific model id (e.g. `deepseek/deepseek-v4-pro`).
    /// Worker resolves the model via [`Registry::to_core_model`] and
    /// calls `Agent::set_model(...)` then replies with
    /// [`TuiEvent::ModelApplied`].
    SetModel(String),
    /// Scan `sessions_dir` for past `<uuidv7>.jsonl` files. Worker
    /// returns the list via [`TuiEvent::SessionsListed`], which
    /// populates the `/resume` overlay.
    ReturnSessions,
    /// Tear down the current harness and re-build it on top of the
    /// JSONL transcript at this path. Worker re-installs all
    /// subscriptions (event fan-out, telemetry, session writer) and
    /// emits a [`TuiEvent::Info`] when the swap completes.
    ResumeSession(std::path::PathBuf),
    /// Copy a (possibly-locked) source jsonl to a fresh uuidv7
    /// path in `sessions_dir` and swap the harness onto it — same
    /// effect as [`Command::ResumeSession`] but against a copy, so
    /// the original keeps appending under whichever process owns
    /// its lock. Used by the session-lock-conflict overlay's
    /// "Fork" choice. Worker emits [`TuiEvent::SessionResumed`] +
    /// [`TuiEvent::Info`] on success.
    ForkSession(std::path::PathBuf),
    /// Permanently remove a session's `<uuidv7>.jsonl` from disk.
    /// Refused by the worker when `path` is the **currently active**
    /// session (the one the live `SessionWriter` is appending to) —
    /// the user must `/clear` or `/resume` away first. On success the
    /// worker re-emits [`TuiEvent::SessionsListed`] so the open
    /// `/resume` overlay reflects the new list, plus an
    /// [`TuiEvent::Info`] confirmation; on failure it emits
    /// [`TuiEvent::AgentWorkerError`].
    DeleteSession(std::path::PathBuf),
    /// Run a compaction pass on the harness's session: summarize all
    /// but the last `keep_recent` messages. Worker emits a
    /// [`TuiEvent::Info`] on success or [`TuiEvent::AgentWorkerError`]
    /// on failure (e.g. empty transcript).
    Compact {
        keep_recent: usize,
    },
    /// Re-scan `plugins_dir` and reply via [`TuiEvent::PluginsListed`].
    /// Cheap (one shallow `read_dir` + N manifest parses); safe to
    /// call every time the user opens the overlay.
    ReturnPlugins,
    /// `/install <name> <src> [rev]` — append a `[[plugin]]` block to
    /// `<workspace>/.grain/plugin-spec.toml` and sync. Worker emits
    /// a [`TuiEvent::Info`] on success or
    /// [`TuiEvent::AgentWorkerError`] on failure.
    InstallPlugin {
        name: String,
        src: String,
        rev: Option<String>,
    },
    /// `/update <name>` — `git pull` on a git-sourced plugin or
    /// no-op for a symlink. Same event channel as `InstallPlugin`.
    UpdatePlugin {
        name: String,
    },
    /// `/remove <name> [--keep-files]` — drop the `[[plugin]]` entry
    /// from the spec; by default also tear down the installed
    /// directory. `--keep-files` preserves the install dir.
    RemovePlugin {
        name: String,
        delete_files: bool,
    },
    /// Dispatch a plugin-contributed UI handler. Sent from the
    /// `/plugins` overlay when the user presses a key registered
    /// via `[[ui_command]]`, or from inside a Form / Confirm widget
    /// to invoke the `on_submit` / `on_yes` follow-up. Worker calls
    /// the named Rhai function via `ScriptHandle::call_fn_json`,
    /// parses the return value as `OverlayDescriptor`, and emits
    /// either [`TuiEvent::UiOverlay`] or [`TuiEvent::UiHandlerError`].
    InvokePluginUi {
        handler: String,
        args: serde_json::Value,
    },
    /// Reload the Rhai script catalog without restarting the TUI:
    /// rebuilds the Rhai engine, re-loads every `*.rhai` from the
    /// captured script dirs, and swaps the agent's tool list to
    /// `base_tools + fresh_rhai`. Sent manually via the `/reload`
    /// slash command or automatically by the `hot-reload` feature's
    /// file watcher. Worker emits a [`TuiEvent::Info`] on success.
    ReloadRhaiScripts,
    Quit,
}

/// Snapshot of transcript-rendering measurements written by the UI
/// at the end of each frame so subsequent key handlers can reason
/// about wrapped row counts they can't compute themselves.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderMetrics {
    pub total_rows: usize,
    pub visible_rows: usize,
    /// Full terminal area from the most recent frame. Used by
    /// `set_overlay` to size effect rects when the exact overlay
    /// bounds aren't cheaply available.
    pub full_area: Rect,
}

/// One terminal row's worth of rendered transcript content, after our
/// own soft-wrap pass. Captured by `ui::draw_transcript` and stashed in
/// [`AppState::rendered_rows`] so mouse handlers can:
/// 1. Translate `(terminal_row, terminal_col)` into a `(row_idx, col)`
///    position inside the transcript buffer.
/// 2. Extract substrings under a drag selection when the user releases
///    the mouse button.
#[derive(Debug, Clone)]
pub struct RenderedRow {
    pub text: String,
    pub kind: TranscriptKind,
    /// `Some(id)` when this row is the **chrome** (collapsed
    /// summary or expanded header) of a foldable block. Mouse
    /// handlers consult this to convert a click into a fold
    /// toggle: click on a chrome row → toggle that block, instead
    /// of starting a text selection.
    pub chrome_for_block: Option<usize>,
    /// Pre-parsed markdown spans produced by [`crate::md_render`].
    /// When `Some`, [`crate::ui::build_line`] renders styled spans
    /// instead of a single plain-text span. Byte offsets into the
    /// concatenated plain text of `md_source_spans`.
    pub md_spans: Option<(Arc<[crate::md_render::MdStyledSpan]>, usize, usize)>,
}

/// Active text selection inside the transcript. Coordinates are
/// `(row_idx, col)` pairs, where `row_idx` indexes into
/// [`AppState::rendered_rows`] (the current frame's wrapped rows).
///
/// `dragging = true` while the left mouse button is held down. Once
/// the button is released, we keep the selection visible briefly (so
/// the user sees what was copied) until the next user action clears
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub active: (usize, usize),
    pub dragging: bool,
}

impl Selection {
    /// Return `(min_row, max_row, min_col_at_min_row, max_col_at_max_row)`
    /// in lexicographic-(row, col) order. The "min" row is the one
    /// where the selection visually begins.
    pub fn normalized(self) -> (usize, usize, usize, usize) {
        if self.anchor <= self.active {
            (self.anchor.0, self.active.0, self.anchor.1, self.active.1)
        } else {
            (self.active.0, self.anchor.0, self.active.1, self.anchor.1)
        }
    }

    /// Highlight range `[lo, hi)` to apply to a single rendered row at
    /// index `row_idx`. `row_len` clamps `hi` so the renderer never
    /// asks for `&text[..past_end]`.
    pub fn col_range_for_row(self, row_idx: usize, row_len: usize) -> Option<(usize, usize)> {
        let (min_r, max_r, min_c, max_c) = self.normalized();
        if row_idx < min_r || row_idx > max_r {
            return None;
        }
        let (lo, hi) = if min_r == max_r {
            // Single-row selection: clamp both ends within the row.
            let l = min_c.min(row_len);
            let h = max_c.min(row_len);
            (l.min(h), l.max(h))
        } else if row_idx == min_r {
            (min_c.min(row_len), row_len)
        } else if row_idx == max_r {
            (0, max_c.min(row_len))
        } else {
            (0, row_len)
        };
        if lo == hi {
            return None;
        }
        Some((lo, hi))
    }
}

/// One entry in the slash-command palette. The renderer matches on
/// `trigger` (always including the leading `/`) and prints
/// `description` to the right.
#[derive(Debug, Clone, Copy)]
pub struct CommandCatalogItem {
    pub trigger: &'static str,
    pub description: &'static str,
}

/// An item shown in the slash palette — either a built-in slash command
/// or a dynamically loaded skill from `.claude/skills/`.
#[derive(Debug, Clone)]
pub struct PaletteItem {
    /// Display trigger shown on the left (e.g. `/help`, `skill: rust-helper`).
    pub trigger: String,
    pub description: String,
    pub action: PaletteAction,
}

/// What happens when the user presses Enter on a palette item.
#[derive(Debug, Clone)]
pub enum PaletteAction {
    /// Dispatch as a built-in slash command (snap input → trigger, then
    /// `dispatch_slash` on submit).
    DispatchSlash,
    /// Inject the skill body content into the input so the user can
    /// review / edit before submitting to the LLM.
    InjectBody(String),
}

/// Built-in slash commands shown in the palette dropdown. Order is the
/// presentation order when the input is just `/`.
pub(crate) const SLASH_CATALOG: &[CommandCatalogItem] = &[
    CommandCatalogItem {
        trigger: "/help",
        description: "show key bindings and slash commands",
    },
    CommandCatalogItem {
        trigger: "/clear",
        description: "clear the transcript",
    },
    CommandCatalogItem {
        trigger: "/doctor",
        description: "show workspace + provider diagnostic report",
    },
    CommandCatalogItem {
        trigger: "/skills",
        description: "list discovered skills",
    },
    CommandCatalogItem {
        trigger: "/theme",
        description: "open the theme picker",
    },
    CommandCatalogItem {
        trigger: "/provider",
        description: "switch provider profile from providers.toml",
    },
    CommandCatalogItem {
        trigger: "/model",
        description: "switch model for the current provider",
    },
    CommandCatalogItem {
        trigger: "/log",
        description: "show recent request bodies (needs --debug-log)",
    },
    CommandCatalogItem {
        trigger: "/resume",
        description: "open the session-resume picker (past transcripts)",
    },
    CommandCatalogItem {
        trigger: "/compact",
        description: "summarize transcript prefix (keeps last 4 turns)",
    },
    CommandCatalogItem {
        trigger: "/plugins",
        description: "list lazy.gagent plugins discovered under .grain/plugins/",
    },
    CommandCatalogItem {
        trigger: "/install",
        description: "/install <name> <src> [rev] — add to plugin-spec.toml + sync",
    },
    CommandCatalogItem {
        trigger: "/update",
        description: "/update <name> — git pull a previously installed plugin",
    },
    CommandCatalogItem {
        trigger: "/remove",
        description: "/remove <name> [--keep-files] — drop from spec (and dir)",
    },
    CommandCatalogItem {
        trigger: "/reload",
        description: "reload Rhai scripts (no restart) — requires --features scripts-rhai",
    },
    CommandCatalogItem {
        trigger: "/exit",
        description: "quit grain-tui",
    },
];

/// Snapshot of which capability gates are active. Driven by CLI flags +
/// config file at startup; mirrored into UI for the footer / status bar.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub allow_write: bool,
    pub allow_bash: bool,
    pub allow_web: bool,
    pub allow_semantic_search: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GitPromptState {
    pub branch: Option<String>,
    pub dirty_count: usize,
}

/// Everything the renderer needs to draw a frame. Pure data — no
/// `Arc<Mutex<...>>`, no `tokio` types — so it can be cloned cheaply for
/// snapshot testing.
#[derive(Debug)]
pub struct AppState {
    pub transcript: Vec<TranscriptLine>,
    // ── Input ──────────────────────────────────────────────────
    pub input: String,
    pub cursor: usize,
    pub focus: Focus,
    // ── Overlay & Focus ───────────────────────────────────────
    pub overlay: Option<Overlay>,
    /// Scroll position when `!follow_bottom`. Counted as "rendered
    /// rows from the top of the wrapped transcript" so new content
    /// arriving doesn't shift the user's frozen view.
    // ── Scroll & View ─────────────────────────────────────────
    pub scroll_offset: usize,
    /// `true` = anchor the transcript view to the bottom (tail mode,
    /// the default). `false` = freeze at `scroll_offset` so prior
    /// content stays put while new messages arrive offscreen below.
    /// PgUp flips to frozen, End / PgDn at bottom flip back to tail.
    pub follow_bottom: bool,
    /// Rendered-row counts the UI writes back each frame so on_key
    /// handlers can convert "scroll up from current bottom" into an
    /// absolute `scroll_offset`. `Cell` keeps it interior-mutable
    /// without breaking the `&AppState` render contract.
    pub render_metrics: Cell<RenderMetrics>,
    // ── Streaming / Token tracking ────────────────────────────
    pub streaming: bool,
    /// Wall-clock start of the current agent run. Reset on
    /// `AgentStart`; cleared on `AgentEnd`. The footer renders an
    /// elapsed counter against this whenever it's set.
    pub streaming_started_at: Option<Instant>,
    /// Cumulative LLM token usage for the current run. Reset on
    /// `AgentStart`; updated from each finalized assistant message's
    /// `usage` field. Shown next to the elapsed counter in the
    /// footer ("↑ 4.2k · ↓ 32.8k tokens").
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Cumulative cached-prompt tokens for the current run — the
    /// `cache_read` subset of `tokens_in`. Drives the `cache N%` chip
    /// in the footer and the cost calculation (cached tokens are billed
    /// at the cache-read rate, not the full input rate).
    pub tokens_cache_read: u64,
    /// Consecutive recent turns with per-turn hit rate ≥ `CACHE_HIGH_RATE`.
    /// Once this counter passes `CACHE_HIGH_STREAK_THRESHOLD`, we assume
    /// the session has a stable prefix cache and any subsequent
    /// large drop is suspicious (probably caused by a mid-session
    /// prefix mutation).
    pub cache_high_streak: u8,
    /// Sticky red-flag — set when a session that had a healthy hit-rate
    /// baseline suddenly drops below `CACHE_LOW_RATE` on the latest
    /// turn. Cleared on `AgentStart`. Drives the chip's color in
    /// `draw_footer`.
    pub cache_dropped: bool,
    /// **Session-cumulative** usage across every assistant turn since
    /// the app started. Unlike `tokens_in / tokens_out / tokens_cache_read`
    /// (which reset on each `AgentStart`), this rolls up across
    /// multiple `prompt` cycles so the footer can surface a `Σ $X.XX`
    /// chip even when the agent is idle.
    pub session_usage: grain_agent_core::Usage,
    /// USD → CNY conversion rate. `None` ⇒ render costs in USD.
    /// `Some(rate)` ⇒ render `¥X.XX` instead, multiplied by `rate`.
    /// Set from `--cny-rate` or auto-detected from `$LANG` at startup.
    pub cny_rate: Option<f64>,
    // ── Model Info ────────────────────────────────────────────
    pub pending_tool_calls: usize,
    active_tools: HashMap<String, ActiveToolDisplay>,
    pub model_id: String,
    /// Per-million-token pricing for the active model. Driven from the
    /// embedded `models.dev` snapshot at startup, refreshed on
    /// `TuiEvent::ProviderApplied` so a runtime provider switch keeps
    /// the cost chip accurate. `Cost::default()` (all zeros) when
    /// pricing is unknown — the footer suppresses the chip then.
    pub model_cost: Cost,
    /// Context window size in tokens for the active model. Drives the
    /// `[ctx N%]` chip in the footer. `0` when unknown — the chip is
    /// omitted in that case.
    pub context_window: u64,
    /// Length of the pinned system prompt in bytes. Used together with
    /// a local scan of the transcript to estimate context-window
    /// occupancy without waiting for an API response.
    pub system_prompt_chars: usize,
    /// Number of compaction events in this session. Bumped when a
    /// `TuiEvent::SessionCompacted` is received. Rendered as
    /// `[compact N]` in the footer when > 0.
    pub compaction_count: u32,
    // ── Capabilities & Config ─────────────────────────────────
    pub workspace_display: String,
    pub git_prompt: GitPromptState,
    pub capabilities: Capabilities,
    pub show_thinking: bool,
    pub last_error: Option<String>,
    /// Ephemeral status line rendered above the input box. Each new
    /// [`TuiEvent::Status`] replaces the previous value — no append,
    /// no transcript pollution. Cleared on `AgentEnd` / turn error so
    /// the slot doesn't linger across turns. Used today by
    /// `retry-on-overflow` to surface mid-turn retry progress without
    /// corrupting the alt screen via stderr.
    // ── Status ────────────────────────────────────────────────
    pub ephemeral_status: Option<String>,
    /// Available themes (built-ins + user). Index 0 is the default
    /// chosen at startup; the picker walks this list.
    // ── Themes & Providers ─────────────────────────────────────
    pub themes: Vec<Theme>,
    /// Index of the currently applied theme within [`Self::themes`].
    pub current_theme_idx: usize,
    /// Provider profiles loaded from disk (workspace + user fallback).
    /// May be empty when no `providers.toml` exists.
    pub providers: Vec<ProviderProfile>,
    /// Active profile index (when one was resolved at startup), else
    /// `None` — meaning the CLI `--model` flag governs and the picker
    /// shows no `✓` marker.
    pub current_provider_idx: Option<usize>,
    /// Optional UI-only provider label override supplied by a trusted
    /// host-side plugin action.
    pub ui_provider_label: Option<String>,
    /// Optional UI-only model label override supplied by a trusted
    /// host-side plugin action.
    pub ui_model_label: Option<String>,
    /// Plugin-contributed slash command overrides. When the user
    /// types `/<trigger>`, this list is consulted **before** the
    /// built-in slash table; a match dispatches into the plugin's
    /// Rhai handler via [`Command::InvokePluginUi`].
    // ── Plugins & Skills ───────────────────────────────────────
    pub plugin_slashes: Vec<grain_ai_agent_headless::BoundPluginSlashCommand>,
    /// Skills loaded from `.claude/skills/` at startup. Used in the
    /// slash palette for prompt injection alongside built-in commands.
    pub skills: Vec<grain_agent_harness::Skill>,
    /// Highlighted row inside the slash-command palette. Reset to 0
    /// whenever the filter (the input text) changes, so a fresh typed
    /// character always lands on the top match.
    pub palette_focused: usize,
    /// Submitted prompts, oldest first. Walked via Up/Down when the
    /// input pane has focus and the slash palette isn't visible.
    /// Bounded by [`MAX_HISTORY`] to keep memory tidy in long
    /// sessions.
    // ── History ────────────────────────────────────────────────
    pub history: Vec<String>,
    /// Position inside [`Self::history`] while the user is walking
    /// it. `None` means "fresh input — not recalling anything", in
    /// which case the buffer is whatever the user typed live.
    pub history_cursor: Option<usize>,
    /// When the user starts walking history, the unsent buffer they
    /// were holding gets saved here so Down past the newest entry
    /// can restore it instead of dropping their draft.
    pub history_draft: String,
    /// Set to true once `Command::Quit` has been issued. The main loop
    /// breaks on this.
    pub should_quit: bool,
    /// Whether mouse capture is currently enabled. `true` = scroll
    /// wheel works but the terminal can't drag-select text natively.
    /// `false` = native selection / right-click-copy works but the
    /// scroll wheel does nothing (PgUp/PgDn still scroll).
    ///
    /// Toggled at runtime by F6. The main loop in `run::event_loop`
    /// observes changes and re-issues `EnableMouseCapture` /
    /// `DisableMouseCapture` on the terminal.
    // ── Render state (interior-mutable / frame-local) ─────────
    pub mouse_capture_on: bool,
    /// Wrapped transcript rows from the most recent frame. Built by
    /// `ui::draw_transcript`, consumed by mouse handlers to translate
    /// `(terminal_row, terminal_col)` into a position inside the
    /// transcript and to extract text under a drag selection.
    pub rendered_rows: RefCell<Vec<RenderedRow>>,
    /// Bounding `Rect` of the transcript pane on the current frame.
    /// Same write-side / read-side pattern as [`Self::render_metrics`]:
    /// `ui::draw_transcript` writes it at frame start, mouse handlers
    /// read it on the next event.
    pub transcript_area: Cell<Rect>,
    /// Active in-app text selection. `None` when there's no live
    /// selection. Set on `MouseDown`, updated on `MouseDrag`,
    /// finalized on `MouseUp` (the drag flag flips off but the
    /// highlight stays until the next event).
    pub selection: Option<Selection>,
    /// Ring buffer of the most recent outbound request bodies
    /// (pretty-printed JSON of the projected LLM messages). Populated
    /// only when the worker was started with `--debug-log`; the
    /// `/log` overlay renders them top-down most-recent-first.
    pub request_log: VecDeque<String>,
    /// Default fold state for tool-call blocks. From
    /// `config.toml::fold_tool_calls` (defaults `true`).
    // ── Fold state ─────────────────────────────────────────────
    pub fold_tool_calls_default: bool,
    /// Default fold state for thinking blocks. From
    /// `config.toml::fold_thinking` (defaults `true`).
    pub fold_thinking_default: bool,
    /// Per-block fold override. Key = block id (first line index);
    /// value = `true` to force-expand a block that would normally
    /// be collapsed by default, `false` to force-collapse a block
    /// that would normally be expanded. Absent = use the default
    /// for the block's kind.
    pub fold_overrides: HashMap<usize, bool>,
    /// Currently focused transcript block (block id = first_line
    /// index). `None` when the input pane has focus or no block is
    /// selected. Cursor jumps via Up/Down in `on_key_transcript`;
    /// Space/Enter toggles fold of the focused block.
    pub transcript_cursor: Option<usize>,
    /// Active tachyonfx visual effects. Processed each frame in
    /// `ui::draw`; finished effects are auto-retired.
    // ── Effects & Caches ───────────────────────────────────────
    pub effects: EffectManager,
    /// Caches pre-parsed markdown spans for completed transcript
    /// lines so they aren't re-parsed every frame. The last streaming
    /// line is always re-parsed; everything before it hits this cache.
    pub markdown_cache: MarkdownCache,
    /// Cached result of [`build_transcript_blocks`]. Invalidated
    /// whenever new lines are appended to the transcript (which is
    /// append-only), so the hot render path avoids re-scanning the
    /// entire buffer each frame.
    cached_blocks: Vec<TranscriptBlock>,
    /// Number of transcript lines at the time [`Self::cached_blocks`]
    /// was last computed. When this differs from `transcript.len()`,
    /// the cache is stale and gets rebuilt on the next access.
    transcript_len_cached: usize,
    /// Pre-computed display summaries for each block, keyed by
    /// `block.id()` (= `first_line`). Populated alongside
    /// [`Self::cached_blocks`] so the renderer doesn't
    /// `format!()` per frame.
    block_summary_cache: std::collections::HashMap<usize, String>,
}

/// Cap on the in-memory prompt history. Old entries get truncated
/// from the front once we exceed this so long sessions don't grow
/// unbounded.
/// Trim a string to `max` chars, replacing newlines with spaces.
/// If the string exceeds `max`, truncates and appends a `…`.
fn truncate_oneline(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

pub(crate) const MAX_HISTORY: usize = 200;

/// Cap on the in-memory request-body log ring buffer (entries kept
/// in [`AppState::request_log`]). Each entry is a pretty-printed
/// JSON Message[] array — typically 2–20 KB. 20 entries keeps the
/// ring well under 500 KB even on heavy turns.
pub(crate) const MAX_REQUEST_LOG: usize = 20;
pub(crate) const MAX_REQUEST_LOG_ENTRY_BYTES: usize = 64 * 1024;

fn clamp_char_boundary_back(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn clamp_char_boundary_forward(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn truncate_request_log_entry(body: String) -> String {
    if body.len() <= MAX_REQUEST_LOG_ENTRY_BYTES {
        return body;
    }

    let marker = format!("\n\n[Request log truncated from {} bytes]\n\n", body.len());
    let budget = MAX_REQUEST_LOG_ENTRY_BYTES.saturating_sub(marker.len());
    let head_budget = budget / 2;
    let tail_budget = budget.saturating_sub(head_budget);
    let head_end = clamp_char_boundary_back(&body, head_budget);
    let tail_start = clamp_char_boundary_forward(&body, body.len().saturating_sub(tail_budget));

    let mut out = String::with_capacity(
        head_end
            .saturating_add(marker.len())
            .saturating_add(body.len().saturating_sub(tail_start)),
    );
    out.push_str(&body[..head_end]);
    out.push_str(&marker);
    out.push_str(&body[tail_start..]);
    out
}

fn read_git_prompt_state(workspace_display: &str) -> GitPromptState {
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace_display)
        .arg("branch")
        .arg("--show-current")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        });

    if branch.is_none() {
        return GitPromptState::default();
    }

    let dirty_count = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace_display)
        .arg("status")
        .arg("--porcelain")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0);

    GitPromptState {
        branch,
        dirty_count,
    }
}

/// Default USD → CNY rate when `--cny-rate` is unset but a `zh_*`
/// locale is detected. Picked as a stable round number; users in
/// rate-sensitive workflows should pass `--cny-rate` explicitly.
pub(crate) const DEFAULT_CNY_RATE: f64 = 7.20;

/// Resolve the CNY rate from CLI override + locale env var. Pure —
/// takes the env value as an argument so tests don't need to mutate
/// process state.
pub(crate) fn resolve_cny_rate(cli_override: Option<f64>, lang_env: Option<&str>) -> Option<f64> {
    if let Some(rate) = cli_override
        && rate > 0.0
    {
        return Some(rate);
    }
    lang_env
        .filter(|l| l.to_ascii_lowercase().starts_with("zh"))
        .map(|_| DEFAULT_CNY_RATE)
}

/// Per-turn hit rate at-or-above which a turn counts toward the
/// "healthy baseline" streak. Picked empirically — most long coding
/// sessions with stable prefixes sit above 90%, so 80% is a soft cut.
pub(crate) const CACHE_HIGH_RATE: f64 = 0.80;

/// Per-turn hit rate below which (after a healthy baseline) we flag
/// a "cache drop". A 30+ percentage-point drop relative to the
/// baseline is almost always a prefix-mutation bug.
pub(crate) const CACHE_LOW_RATE: f64 = 0.50;

/// Number of consecutive healthy turns required before drop detection
/// arms. Without this minimum, the first turn (mostly miss) would
/// trip the alarm.
pub(crate) const CACHE_HIGH_STREAK_THRESHOLD: u8 = 3;

/// Pure helper for [`AppState::on_agent_event`] cache-drop tracking.
/// Returns `(new_streak, new_dropped)`.
///
/// Logic:
/// - Turns with `input == 0` (no LLM call this message — rare) leave
///   the state untouched.
/// - Turns at or above `CACHE_HIGH_RATE` saturating-increment the
///   streak.
/// - Turns below `CACHE_LOW_RATE` reset the streak; if the prior
///   streak had armed the alarm (`prev_streak >= threshold`), the
///   `dropped` flag stays set forever (or until `AgentStart`).
/// - Anything in between (50% – 80%) is "neutral" — leaves both alone.
pub(crate) fn update_cache_drop_state(
    prev_streak: u8,
    prev_dropped: bool,
    per_turn_input: u64,
    per_turn_cache_read: u64,
) -> (u8, bool) {
    if per_turn_input == 0 {
        return (prev_streak, prev_dropped);
    }
    let rate = per_turn_cache_read as f64 / per_turn_input as f64;
    if rate >= CACHE_HIGH_RATE {
        (prev_streak.saturating_add(1), prev_dropped)
    } else if rate < CACHE_LOW_RATE {
        let armed = prev_streak >= CACHE_HIGH_STREAK_THRESHOLD;
        (0, prev_dropped || armed)
    } else {
        (prev_streak, prev_dropped)
    }
}

/// Filter and sort models by a case-insensitive query. Models are sorted
/// by provider (inferred from the id prefix) then by name.
pub(crate) fn filter_models(models: &[(String, String)], query: &str) -> Vec<(String, String)> {
    let needle = query.to_ascii_lowercase();
    let mut filtered: Vec<(String, String)> = models
        .iter()
        .filter(|(id, name)| {
            if needle.is_empty() {
                return true;
            }
            id.to_ascii_lowercase().contains(&needle) || name.to_ascii_lowercase().contains(&needle)
        })
        .cloned()
        .collect();
    filtered.sort_by(|a, b| {
        let provider_a = a.0.split('/').next().unwrap_or("");
        let provider_b = b.0.split('/').next().unwrap_or("");
        provider_a
            .cmp(provider_b)
            .then_with(|| a.1.to_ascii_lowercase().cmp(&b.1.to_ascii_lowercase()))
    });
    filtered
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model_id: String,
        model_cost: Cost,
        context_window: u64,
        system_prompt_chars: usize,
        workspace_display: String,
        capabilities: Capabilities,
        show_thinking: bool,
        themes: Vec<Theme>,
        initial_theme_idx: usize,
        providers: Vec<ProviderProfile>,
        initial_provider_idx: Option<usize>,
        cny_rate: Option<f64>,
        initial_history: Vec<String>,
    ) -> Self {
        assert!(!themes.is_empty(), "AppState needs at least one theme");
        let current_theme_idx = initial_theme_idx.min(themes.len() - 1);
        let git_prompt = read_git_prompt_state(&workspace_display);
        let mut s = AppState {
            transcript: Vec::new(),
            input: String::new(),
            cursor: 0,
            focus: Focus::Input,
            overlay: None,
            scroll_offset: 0,
            follow_bottom: true,
            render_metrics: Cell::new(RenderMetrics::default()),
            streaming: false,
            streaming_started_at: None,
            tokens_in: 0,
            tokens_out: 0,
            tokens_cache_read: 0,
            cache_high_streak: 0,
            cache_dropped: false,
            session_usage: grain_agent_core::Usage::default(),
            cny_rate,
            pending_tool_calls: 0,
            active_tools: HashMap::new(),
            model_id,
            model_cost,
            context_window,
            system_prompt_chars,
            compaction_count: 0,
            workspace_display,
            git_prompt,
            capabilities,
            show_thinking,
            last_error: None,
            ephemeral_status: None,
            themes,
            current_theme_idx,
            providers,
            current_provider_idx: initial_provider_idx,
            ui_provider_label: None,
            ui_model_label: None,
            plugin_slashes: Vec::new(),
            skills: Vec::new(),
            palette_focused: 0,
            history: {
                let mut h = initial_history;
                if h.len() > MAX_HISTORY {
                    let excess = h.len() - MAX_HISTORY;
                    h.drain(..excess);
                }
                h
            },
            history_cursor: None,
            history_draft: String::new(),
            should_quit: false,
            mouse_capture_on: true,
            rendered_rows: RefCell::new(Vec::new()),
            transcript_area: Cell::new(Rect::default()),
            selection: None,
            request_log: VecDeque::new(),
            // Defaults match the prevailing UX preference of
            // "fold the noisy bits, keep the conversation
            // foreground"; users can flip via config.toml.
            fold_tool_calls_default: true,
            fold_thinking_default: true,
            fold_overrides: HashMap::new(),
            transcript_cursor: None,
            effects: EffectManager::new(),
            markdown_cache: MarkdownCache::new(),
            cached_blocks: Vec::new(),
            transcript_len_cached: 0,
            block_summary_cache: std::collections::HashMap::new(),
        };
        s.push(
            TranscriptKind::Info,
            "grain-tui — F1 help · F2 doctor · F3 skills · /theme to change colors · Ctrl-C abort · Esc quit".into(),
        );
        s
    }

    /// Currently applied theme. The renderer reads palette colors
    /// through this accessor so it can't accidentally consume a stale
    /// reference if the user switches mid-stream.
    pub fn theme(&self) -> &Theme {
        &self.themes[self.current_theme_idx]
    }

    pub(crate) fn refresh_git_prompt(&mut self) {
        self.git_prompt = read_git_prompt_state(&self.workspace_display);
    }

    /// Transition the overlay, pushing open / close effects when the
    /// presence changes (None→Some or Some→None).
    pub fn set_overlay(&mut self, next: Option<Overlay>) {
        use crate::anim::{EffectKind, FxDuration, fx};
        let palette = &self.themes[self.current_theme_idx].palette;
        match (&self.overlay, &next) {
            (None, Some(_)) => {
                // Opening — fade from background color over full area.
                self.effects.clear_kind(EffectKind::OverlayClose);
                self.effects.push(
                    EffectKind::OverlayOpen,
                    fx::fade_from_fg(palette.surface, FxDuration::from_millis(200)),
                    self.render_metrics.get().full_area,
                );
            }
            (Some(_), None) => {
                // Closing — fade to background color.
                self.effects.clear_kind(EffectKind::OverlayOpen);
                self.effects.push(
                    EffectKind::OverlayClose,
                    fx::fade_to_fg(palette.surface, FxDuration::from_millis(150)),
                    self.render_metrics.get().full_area,
                );
            }
            _ => {}
        }
        self.overlay = next;
    }

    /// Shared scroll-up step. Used by PgUp and mouse-wheel-up. Same
    /// semantics: tail-follow → freeze at current bottom, then step
    /// back `amount` rows. Already-frozen → just step back.
    ///
    /// When the entire transcript already fits in the visible area
    /// there's nothing above the top row to scroll into view; we no-op
    /// instead of flipping `follow_bottom` off (which would silently
    /// break tail-follow for subsequent streaming deltas without
    /// the user seeing any feedback for the wheel-up).
    pub fn scroll_up(&mut self, amount: usize) {
        let m = self.render_metrics.get();
        if m.total_rows <= m.visible_rows {
            return;
        }
        if self.follow_bottom {
            self.scroll_offset = m.total_rows.saturating_sub(m.visible_rows);
            self.follow_bottom = false;
        }
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    /// Shared scroll-down step. Used by PgDn and mouse-wheel-down. If
    /// already tailing, no-op. Catching up to the live bottom
    /// re-engages tail follow so subsequent new content auto-scrolls.
    pub fn scroll_down(&mut self, amount: usize) {
        if !self.follow_bottom {
            self.scroll_offset = self.scroll_offset.saturating_add(amount);
            let m = self.render_metrics.get();
            let bottom = m.total_rows.saturating_sub(m.visible_rows);
            if self.scroll_offset >= bottom {
                self.follow_bottom = true;
            }
        }
    }

    /// Translate an absolute terminal `(row, col)` from a mouse event
    /// into a `(rendered_row_idx, col)` position inside the
    /// transcript's wrapped-row buffer. Returns `None` when the click
    /// falls outside the transcript pane, or when no rendered_rows
    /// are tracked yet (e.g. before the first frame).
    ///
    /// The mapping respects the active scroll offset (`scroll_offset`
    /// when frozen, end-of-buffer when `follow_bottom`).
    pub fn translate_mouse_to_rendered(&self, row: u16, col: u16) -> Option<(usize, usize)> {
        let area = self.transcript_area.get();
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if row < area.y || col < area.x {
            return None;
        }
        if row >= area.y + area.height || col >= area.x + area.width {
            return None;
        }
        let visible_row = (row - area.y) as usize;
        let visible_col = (col - area.x) as usize;
        let rendered = self.rendered_rows.borrow();
        let total = rendered.len();
        let visible_rows = area.height as usize;
        let skip = if self.follow_bottom {
            total.saturating_sub(visible_rows)
        } else {
            self.scroll_offset.min(total.saturating_sub(visible_rows))
        };
        let row_idx = skip + visible_row;
        // `visible_col` is in **terminal display columns**, but
        // selection coords are byte offsets into the row text.
        // Walk the row by char, accumulating display widths, until we
        // pass `visible_col` — that's the byte index to slice at.
        // Without this, multi-byte (CJK / emoji) chars made the
        // highlight rectangle slip half a glyph past where the user
        // dragged.
        let col_idx = rendered
            .get(row_idx)
            .map(|r| visual_col_to_byte_idx(&r.text, visible_col))
            .unwrap_or(0);
        Some((row_idx, col_idx))
    }

    /// The slash palette is visible while the input has focus, no
    /// overlay is open, and the input starts with `/`. Hidden once the
    /// user submits — re-appears when they type `/` again.
    pub fn palette_visible(&self) -> bool {
        self.focus == Focus::Input && self.overlay.is_none() && self.input.starts_with('/')
    }

    /// Append a submitted prompt to history (dedup immediate repeats,
    /// trim from the front when over the cap).
    fn push_history(&mut self, line: &str) {
        if line.is_empty() {
            return;
        }
        if self.history.last().map(|s| s.as_str()) == Some(line) {
            return;
        }
        self.history.push(line.to_string());
        if self.history.len() > MAX_HISTORY {
            let excess = self.history.len() - MAX_HISTORY;
            self.history.drain(0..excess);
        }
    }

    /// Walk backwards through history. Saves the user's in-progress
    /// draft on the first move so Down can restore it.
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_idx = match self.history_cursor {
            None => {
                // Stepping off the live buffer for the first time —
                // remember what they were typing.
                self.history_draft = std::mem::take(&mut self.input);
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(new_idx);
        self.input = self.history[new_idx].clone();
        self.cursor = self.input.len();
        self.palette_focused = 0;
    }

    /// Walk forward through history. Past the newest entry, restore
    /// the saved draft.
    fn history_down(&mut self) {
        match self.history_cursor {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.history_cursor = Some(i + 1);
                self.input = self.history[i + 1].clone();
                self.cursor = self.input.len();
            }
            Some(_) => {
                self.history_cursor = None;
                self.input = std::mem::take(&mut self.history_draft);
                self.cursor = self.input.len();
            }
        }
        self.palette_focused = 0;
    }

    /// Once the user edits a recalled history entry it becomes their
    /// own draft; clear the cursor so Down doesn't pull them away
    /// from their edits.
    fn detach_history(&mut self) {
        if self.history_cursor.is_some() {
            self.history_cursor = None;
            self.history_draft.clear();
        }
    }

    /// Catalog rows whose trigger starts with the user's input
    /// (case-insensitive). When the user has typed just `/`, every
    /// row matches. Includes both built-in slash commands and loaded
    /// skills so the user can inject skill prompts via `/`.
    pub fn palette_matches(&self) -> Vec<PaletteItem> {
        if !self.input.starts_with('/') {
            return Vec::new();
        }
        let needle = self.input.to_ascii_lowercase();
        let mut items: Vec<PaletteItem> = SLASH_CATALOG
            .iter()
            .filter(|item| item.trigger.starts_with(&needle))
            .map(|item| PaletteItem {
                trigger: item.trigger.to_string(),
                description: item.description.to_string(),
                action: PaletteAction::DispatchSlash,
            })
            .collect();

        // Add skills: when the needle is just `/`, show all skills.
        // When the needle is e.g. `/ru`, also include skills whose
        // name or description contains the text after `/`.
        let skill_filter = needle.strip_prefix('/').unwrap_or(&needle);
        for skill in &self.skills {
            if skill.disable_model_invocation {
                continue;
            }
            let show = skill_filter.is_empty()
                || skill.name.to_ascii_lowercase().contains(skill_filter)
                || skill
                    .description
                    .to_ascii_lowercase()
                    .contains(skill_filter);
            if show {
                items.push(PaletteItem {
                    trigger: format!("skill: {}", skill.name),
                    description: skill.description.clone(),
                    action: PaletteAction::InjectBody(skill.body.clone()),
                });
            }
        }
        items
    }

    fn push(&mut self, kind: TranscriptKind, text: String) {
        self.transcript.push(TranscriptLine { kind, text });
        // No scroll mutation here. When `follow_bottom == true`
        // (the default), the renderer pins to tail and ignores
        // `scroll_offset`, so new content shows up automatically.
        // When the user has scrolled up (`follow_bottom == false`),
        // their `scroll_offset` is the anchor row index — leaving
        // it alone is exactly what keeps the visible window stable
        // as new lines append at the end.

        // Fade-in effect for new lines — only on assistant / tool
        // output that the user is actually reading for the first
        // time. Skip user-typed prompts (they know they typed it —
        // a flash on their own keystroke registers as a flicker)
        // and skip when scrolled away from tail (the row isn't on
        // screen).
        if !self.follow_bottom || !kind_should_fade(kind) {
            return;
        }
        let ta = self.transcript_area.get();
        if ta.height > 0 {
            use crate::anim::{EffectKind, FxDuration, fx};
            let palette = &self.themes[self.current_theme_idx].palette;
            let row_area = Rect {
                x: ta.x,
                y: ta.y.saturating_add(ta.height.saturating_sub(1)),
                width: ta.width,
                height: 1,
            };
            self.effects.push(
                EffectKind::NewMessage,
                fx::fade_from_fg(palette.surface, FxDuration::from_millis(220)),
                row_area,
            );
        }
    }

    /// Whether `block` should currently render expanded. Combines
    /// per-block overrides (`fold_overrides`) with the kind-keyed
    /// defaults (`fold_*_default`). Plain blocks are always
    /// expanded — folding them adds no value.
    pub fn is_block_expanded(&self, block: &TranscriptBlock) -> bool {
        if let Some(&v) = self.fold_overrides.get(&block.id()) {
            return v;
        }
        match block.kind {
            BlockKind::Plain => true,
            BlockKind::ToolCall => !self.fold_tool_calls_default,
            BlockKind::Thinking => !self.fold_thinking_default,
        }
    }

    /// Toggle the fold state of `block`. If no override existed,
    /// the new value flips the current effective state.
    pub fn toggle_block_fold(&mut self, block: &TranscriptBlock) {
        if !block.is_foldable() {
            return;
        }
        let next = !self.is_block_expanded(block);
        self.fold_overrides.insert(block.id(), next);
    }

    /// Move the transcript cursor to the next foldable block in
    /// `direction`. Direction `-1` moves toward older blocks, `+1`
    /// toward newer ones. When the cursor is currently `None`,
    /// initializes to the **last** foldable block (for `-1`) or
    /// the **first** foldable block (for `+1`) — friendliest first
    /// jump for a user who just hit the navigation key.
    pub fn move_transcript_cursor(&mut self, direction: isize) {
        let blocks = build_transcript_blocks(&self.transcript);
        let foldable_ids: Vec<usize> = blocks
            .iter()
            .filter(|b| b.is_foldable())
            .map(|b| b.id())
            .collect();
        if foldable_ids.is_empty() {
            self.transcript_cursor = None;
            return;
        }
        self.transcript_cursor = Some(match self.transcript_cursor {
            None => {
                if direction < 0 {
                    *foldable_ids.last().unwrap()
                } else {
                    foldable_ids[0]
                }
            }
            Some(current) => {
                let idx = foldable_ids
                    .iter()
                    .position(|&id| id == current)
                    .unwrap_or(foldable_ids.len() - 1);
                if direction < 0 {
                    foldable_ids[idx.saturating_sub(1)]
                } else {
                    foldable_ids[(idx + 1).min(foldable_ids.len() - 1)]
                }
            }
        });
    }

    /// Toggle the fold of the block currently under the transcript
    /// cursor (no-op when the cursor is `None` or points at a stale
    /// block id that no longer matches any foldable block).
    pub fn toggle_focused_block(&mut self) {
        let Some(target) = self.transcript_cursor else {
            return;
        };
        let blocks = build_transcript_blocks(&self.transcript);
        if let Some(block) = blocks.iter().find(|b| b.id() == target) {
            self.toggle_block_fold(block);
        }
    }

    /// Reset all per-run streaming counters. Called when the user
    /// `/resume`s into a different session so stale per-turn token
    /// counts don't bleed into the new transcript.
    fn reset_streaming_state(&mut self) {
        self.streaming = false;
        self.streaming_started_at = None;
        self.pending_tool_calls = 0;
        self.active_tools.clear();
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.tokens_cache_read = 0;
        self.cache_high_streak = 0;
        self.cache_dropped = false;
    }

    fn clear_transcript_storage(&mut self) {
        self.transcript.clear();
        self.transcript.shrink_to(0);
        self.markdown_cache = MarkdownCache::new();
        self.cached_blocks.clear();
        self.cached_blocks.shrink_to(0);
        self.transcript_len_cached = 0;
        self.block_summary_cache.clear();
        self.block_summary_cache.shrink_to_fit();
        self.fold_overrides.clear();
        self.fold_overrides.shrink_to_fit();
        self.selection = None;

        let mut rendered_rows = self.rendered_rows.borrow_mut();
        rendered_rows.clear();
        rendered_rows.shrink_to(0);
    }

    /// Push a single historical [`AgentMessage`] into the transcript
    /// as one or more lines. Used by [`Self::on_event`] for
    /// `TuiEvent::SessionResumed` so the user sees the loaded
    /// conversation in the scrollback.
    fn push_agent_message(&mut self, msg: &AgentMessage) {
        use grain_agent_core::AssistantContent;
        let AgentMessage::Standard(msg) = msg else {
            return;
        };
        match msg {
            Message::User(u) => {
                let text: String = u
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.as_str()),
                        UserContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                self.push(TranscriptKind::UserPrompt, text);
            }
            Message::Assistant(a) => {
                for c in &a.content {
                    match c {
                        AssistantContent::Text(t) => {
                            // Push the full assistant text as ONE
                            // entry per turn. The renderer wraps the
                            // multi-paragraph content correctly via
                            // `textwrap`, so the historical "split
                            // every `\n` into its own row" code only
                            // fragmented the transcript — turning a
                            // 5-paragraph reply into 5 lines that
                            // each looked like a separate response,
                            // exactly what the user saw as "repeated
                            // rendering" after a session resume.
                            if !t.text.is_empty() {
                                self.push(TranscriptKind::AssistantText, t.text.clone());
                            }
                        }
                        AssistantContent::Thinking(t) => {
                            if !t.thinking.is_empty() {
                                self.push(TranscriptKind::ThinkingText, t.thinking.clone());
                            }
                        }
                        AssistantContent::ToolCall(tc) => {
                            // Claude-Code-style header. The matching
                            // ToolCallEnd / ToolCallError line is
                            // emitted later when the corresponding
                            // `Message::ToolResult` is replayed below,
                            // so the rendered block ends up shaped
                            // exactly like a live tool call.
                            self.push(
                                TranscriptKind::ToolCallStart,
                                format_tool_start_line(&tc.name, &tc.arguments),
                            );
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(tr) => {
                let text: String = tr
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.as_str()),
                        UserContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let preview = truncate_oneline(&text, 500);
                let (kind, line) = if tr.is_error {
                    (
                        TranscriptKind::ToolCallError,
                        format!("  └ Error\n{}", indent_preview_lines(&preview, 500, 4)),
                    )
                } else {
                    (TranscriptKind::ToolCallEnd, format!("  └ {preview}"))
                };
                self.push(kind, line);
            }
        }
    }

    /// React to one [`TuiEvent`]. Returns commands for the worker.
    pub fn on_event(&mut self, ev: TuiEvent) -> Vec<Command> {
        match ev {
            TuiEvent::Key(k) => self.on_key(k),
            TuiEvent::Tick => Vec::new(),
            TuiEvent::Resize(_, _) => Vec::new(),
            TuiEvent::Agent(e) => {
                self.on_agent_event(*e);
                Vec::new()
            }
            TuiEvent::OverlayDoctor(text) => {
                // If the user already opened a doctor placeholder
                // (via F2 or /doctor), keep their typed query and
                // scroll position and just swap the report contents.
                if let Some(Overlay::Doctor { query, scroll, .. }) = &self.overlay {
                    let query = query.clone();
                    let scroll = *scroll;
                    self.set_overlay(Some(Overlay::Doctor {
                        report: text,
                        query,
                        scroll,
                    }));
                } else {
                    self.set_overlay(Some(Overlay::Doctor {
                        report: text,
                        query: String::new(),
                        scroll: 0,
                    }));
                }
                Vec::new()
            }
            TuiEvent::OverlaySkills(skills) => {
                self.set_overlay(Some(Overlay::Skills(skills)));
                Vec::new()
            }
            TuiEvent::SkillsLoaded(skills) => {
                self.skills = skills;
                Vec::new()
            }
            TuiEvent::AgentWorkerError(msg) => {
                self.push(TranscriptKind::Error, msg);
                Vec::new()
            }
            TuiEvent::RequestLogged { body } => {
                self.request_log.push_back(truncate_request_log_entry(body));
                while self.request_log.len() > MAX_REQUEST_LOG {
                    self.request_log.pop_front();
                }
                Vec::new()
            }
            TuiEvent::SessionsListed(list) => {
                // If no overlay is open and we're at boot (transcript has
                // only the welcome line + maybe a provider-applied info),
                // auto-open the resume picker so the user can choose which
                // session to continue.  The auto-resumed row is focused;
                // Enter confirms it, ↑↓ picks a different one.
                if self.overlay.is_none() && self.transcript.len() <= 3 && !list.is_empty() {
                    self.set_overlay(Some(Overlay::SessionResume {
                        sessions: list,
                        focused: 0,
                        confirm_delete: false,
                    }));
                    return Vec::new();
                }
                // Only swap when the overlay is still open (user may
                // have hit Esc while the scan was in flight).
                if let Some(Overlay::SessionResume {
                    sessions,
                    focused,
                    confirm_delete,
                }) = &mut self.overlay
                {
                    *sessions = list;
                    if *focused >= sessions.len() {
                        *focused = sessions.len().saturating_sub(1);
                    }
                    // A fresh list invalidates whatever the user was
                    // about to confirm — the row they armed might
                    // even be gone now. Re-arm explicitly.
                    *confirm_delete = false;
                }
                Vec::new()
            }
            TuiEvent::SessionLockedAtBoot { locked_path } => {
                // Boot detected the auto-resume target was held by
                // another grain process. Worker already swapped to
                // a fresh session; surface the dialog so the user
                // can pick fork / quit / stay-on-fresh. Boot list
                // omits "Cancel" because there's nothing to cancel
                // back to (we're not mid-overlay flow); "Quit"
                // takes its place.
                self.push(
                    TranscriptKind::Info,
                    format!(
                        "(another grain process holds {} — started a fresh session)",
                        locked_path.display()
                    ),
                );
                self.set_overlay(Some(Overlay::SessionLockConflict {
                    source: SessionLockSource::Boot,
                    locked_path,
                    choices: vec![
                        SessionConflictChoice::Fresh,
                        SessionConflictChoice::Fork,
                        SessionConflictChoice::Resume,
                        SessionConflictChoice::Quit,
                    ],
                    focused: 0,
                }));
                Vec::new()
            }
            TuiEvent::SessionResumed { path: _, messages } => {
                self.clear_transcript_storage();
                self.reset_streaming_state();
                // Replace the input-history ring buffer with the
                // resumed session's user prompts so Up/Down in the
                // input box walks through the *new* session's past
                // inputs — not the prior session's, which become
                // meaningless after the swap.
                self.history.clear();
                self.history_cursor = None;
                self.history_draft.clear();
                for msg in &messages {
                    self.push_agent_message(msg);
                    if let AgentMessage::Standard(Message::User(u)) = msg {
                        let text: String = u
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                UserContent::Text(t) => Some(t.text.as_str()),
                                UserContent::Image(_) => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        self.push_history(&text);
                    }
                }
                Vec::new()
            }
            TuiEvent::SessionCompacted { messages } => {
                self.clear_transcript_storage();
                self.reset_streaming_state();
                self.compaction_count = self.compaction_count.saturating_add(1);
                for msg in &messages {
                    self.push_agent_message(msg);
                }
                // Keep input history — the user's past prompts are
                // still relevant after compacting the transcript.
                Vec::new()
            }
            TuiEvent::Info(text) => {
                self.push(TranscriptKind::Info, text);
                Vec::new()
            }
            TuiEvent::Status(text) => {
                // Replace, don't append — the slot exists exactly so the
                // user doesn't see a stack of "attempt 1/8, 2/8, 3/8..."
                // rows piling up. Empty string clears the slot.
                let prev = self.ephemeral_status.as_deref().unwrap_or("");
                let changed = !text.is_empty() && text != prev;
                self.ephemeral_status = if text.is_empty() { None } else { Some(text) };
                // Flash the status row when the text is new / changed.
                if changed {
                    use crate::anim::{EffectKind, FxDuration, fx};
                    let palette = &self.themes[self.current_theme_idx].palette;
                    self.effects.clear_kind(EffectKind::StatusFlash);
                    self.effects.push(
                        EffectKind::StatusFlash,
                        fx::sequence(&[
                            fx::paint_fg(palette.warning, FxDuration::from_millis(50)),
                            fx::fade_to_fg(palette.muted, FxDuration::from_millis(300)),
                        ]),
                        self.render_metrics.get().full_area,
                    );
                }
                Vec::new()
            }
            TuiEvent::PluginsListed {
                plugins: list,
                ui_commands: cmds,
            } => {
                // Only swap when the overlay is still open (user may
                // have hit Esc while the scan was in flight).
                if let Some(Overlay::Plugins {
                    plugins,
                    ui_commands,
                }) = &mut self.overlay
                {
                    *plugins = list;
                    *ui_commands = cmds;
                }
                Vec::new()
            }
            TuiEvent::UiOverlay(descriptor) => {
                use grain_ai_agent_headless::OverlayDescriptor as D;
                self.set_overlay(Some(match descriptor {
                    D::Form {
                        title,
                        fields,
                        on_submit,
                    } => Overlay::DynamicForm {
                        title,
                        fields: fields
                            .into_iter()
                            .map(|f| DynamicFormFieldState {
                                name: f.name,
                                label: f.label,
                                placeholder: f.placeholder,
                                value: f.initial,
                            })
                            .collect(),
                        on_submit,
                        focused: 0,
                    },
                    D::Modal {
                        title,
                        body,
                        severity,
                    } => Overlay::DynamicModal {
                        title,
                        body,
                        severity,
                    },
                    D::Confirm {
                        title,
                        body,
                        on_yes,
                        yes_args,
                    } => Overlay::DynamicConfirm {
                        title,
                        body,
                        on_yes,
                        yes_args,
                    },
                    D::List {
                        title,
                        items,
                        on_select,
                    } => Overlay::DynamicList {
                        title,
                        items,
                        on_select,
                        focused: 0,
                    },
                    D::Table {
                        title,
                        columns,
                        rows,
                        on_select,
                    } => Overlay::DynamicTable {
                        title,
                        columns,
                        rows,
                        on_select,
                        focused: 0,
                    },
                    D::TextPanel {
                        title,
                        lines,
                        footer,
                    } => Overlay::DynamicTextPanel {
                        title,
                        lines,
                        footer,
                    },
                    D::Progress {
                        title,
                        value,
                        max,
                        label,
                    } => Overlay::DynamicProgress {
                        title,
                        value,
                        max,
                        label,
                    },
                    D::Stack { title, children } => Overlay::DynamicStack { title, children },
                }));
                Vec::new()
            }
            TuiEvent::UiHandlerError(msg) => {
                self.push(TranscriptKind::Error, msg);
                Vec::new()
            }
            TuiEvent::SlashCommandsRegistered(list) => {
                self.plugin_slashes = list;
                Vec::new()
            }
            TuiEvent::ScrollUp { amount } => {
                // Wheel routes to the overlay when one is open and
                // scrollable; otherwise to the transcript.
                if let Some(Overlay::Log { scroll }) = &mut self.overlay {
                    *scroll = scroll.saturating_sub(amount as usize);
                } else {
                    self.scroll_up(amount as usize);
                }
                Vec::new()
            }
            TuiEvent::ScrollDown { amount } => {
                if let Some(Overlay::Log { scroll }) = &mut self.overlay {
                    *scroll = scroll.saturating_add(amount as usize);
                } else {
                    self.scroll_down(amount as usize);
                }
                Vec::new()
            }
            TuiEvent::MouseDown { row, col } => {
                if let Some(pos) = self.translate_mouse_to_rendered(row, col) {
                    // Click on a fold-chrome row (the summary or
                    // header line of a foldable block) toggles
                    // the block instead of starting a selection.
                    // The cursor jumps to the clicked block so a
                    // subsequent Ctrl-J / Ctrl-K resumes from a
                    // sensible spot.
                    let chrome_block = self
                        .rendered_rows
                        .borrow()
                        .get(pos.0)
                        .and_then(|r| r.chrome_for_block);
                    if let Some(block_id) = chrome_block {
                        self.transcript_cursor = Some(block_id);
                        let blocks = build_transcript_blocks(&self.transcript);
                        if let Some(block) = blocks.iter().find(|b| b.id() == block_id) {
                            self.toggle_block_fold(block);
                        }
                        // Skip selection start — the user is
                        // clicking to toggle, not to drag-copy.
                        return Vec::new();
                    }
                    self.selection = Some(Selection {
                        anchor: pos,
                        active: pos,
                        dragging: true,
                    });
                }
                Vec::new()
            }
            TuiEvent::MouseDrag { row, col } => {
                let pos = self.translate_mouse_to_rendered(row, col);
                if let (Some(p), Some(sel)) = (pos, self.selection.as_mut())
                    && sel.dragging
                {
                    sel.active = p;
                }
                Vec::new()
            }
            TuiEvent::MouseUp => {
                let copy_result = self.selection.as_mut().and_then(|sel| {
                    if !sel.dragging {
                        return None;
                    }
                    sel.dragging = false;
                    let rendered = self.rendered_rows.borrow();
                    let text = extract_selection(&rendered, *sel);
                    if text.is_empty() {
                        return Some(Err(String::from("empty")));
                    }
                    Some(write_clipboard(&text).map(|()| text))
                });
                match copy_result {
                    Some(Ok(text)) => {
                        self.push(
                            TranscriptKind::Info,
                            format!("(copied {} chars to clipboard)", text.chars().count()),
                        );
                    }
                    Some(Err(msg)) if msg == "empty" => {
                        // Click without drag — clear the selection so the
                        // highlight goes away.
                        self.selection = None;
                    }
                    Some(Err(e)) => {
                        self.push(
                            TranscriptKind::Info,
                            format!("(clipboard unavailable: {e})"),
                        );
                    }
                    None => {}
                }
                Vec::new()
            }
            TuiEvent::ProviderApplied {
                profile,
                model,
                cost,
            } => {
                // Mark this profile as the active one so the picker's
                // ✓ moves immediately. Match by name so applies that
                // happened out-of-band (e.g. CLI) still align.
                self.current_provider_idx = self.providers.iter().position(|p| p.name == profile);
                self.model_id = model.clone();
                self.ui_provider_label = None;
                self.ui_model_label = None;
                self.model_cost = cost;
                self.push(
                    TranscriptKind::Info,
                    format!("(provider: {profile} · {model})"),
                );
                Vec::new()
            }
            TuiEvent::ModelApplied { model, cost } => {
                self.model_id = model.clone();
                self.ui_model_label = None;
                self.model_cost = cost;
                self.push(TranscriptKind::Info, format!("(model: {model})"));
                Vec::new()
            }
            TuiEvent::UiHeaderUpdated { provider, model } => {
                if let Some(provider) = provider {
                    self.ui_provider_label = Some(provider);
                }
                if let Some(model) = model {
                    self.model_id = model.clone();
                    self.ui_model_label = Some(model);
                }
                Vec::new()
            }
            TuiEvent::ModelsListed(list) => {
                // Only swap when the overlay is still open (user may
                // have hit Esc while the scan was in flight).
                if let Some(Overlay::ModelPicker {
                    models,
                    focused,
                    query,
                }) = &mut self.overlay
                {
                    *models = list;
                    let filtered_len = filter_models(models, query).len();
                    if *focused >= filtered_len {
                        *focused = filtered_len.saturating_sub(1);
                    }
                }
                Vec::new()
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Command> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        // Ctrl-C convention:
        //   - while streaming / waiting on tools → abort current turn
        //     (you might just want to redirect the agent).
        //   - otherwise → quit. macOS users in particular expect
        //     Ctrl-C to be a hard exit when the app is idle, even
        //     under raw mode where the kernel no longer raises SIGINT.
        // Transcript-cursor controls — Ctrl-K / Ctrl-J navigate
        // between foldable blocks (works from any focus); Space
        // toggles the currently-focused block. Ctrl-K initializes
        // the cursor to the last foldable block on first press.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('k') => {
                    self.move_transcript_cursor(-1);
                    return Vec::new();
                }
                KeyCode::Char('j') => {
                    self.move_transcript_cursor(1);
                    return Vec::new();
                }
                _ => {}
            }
        }
        if self.transcript_cursor.is_some()
            && matches!(key.code, KeyCode::Char(' '))
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && self.input.is_empty()
        {
            // Plain Space toggles fold only when the user is in
            // cursor mode AND the input is empty — otherwise the
            // user is actually trying to type a space.
            self.toggle_focused_block();
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.streaming || self.pending_tool_calls > 0 {
                return vec![Command::AbortCurrentTurn];
            }
            self.should_quit = true;
            return vec![Command::Quit];
        }
        if key.code == KeyCode::Esc {
            if self.overlay.is_some() {
                self.set_overlay(None);
                return Vec::new();
            }
            // If the transcript cursor is engaged, the first Esc
            // clears it (exits "fold-navigation" mode). Only
            // subsequent Esc presses fall through to the input /
            // quit logic — saves the user from accidentally
            // quitting while they were folding.
            if self.transcript_cursor.is_some() {
                self.transcript_cursor = None;
                return Vec::new();
            }
            // Esc with a non-empty input clears the buffer instead of
            // quitting. Quit only takes the empty-input branch — that
            // way an accidental Esc mid-prompt doesn't trash the
            // session.
            if !self.input.is_empty() {
                self.input.clear();
                self.cursor = 0;
                self.palette_focused = 0;
                self.history_cursor = None;
                self.history_draft.clear();
                return Vec::new();
            }
            self.should_quit = true;
            return vec![Command::Quit];
        }
        // When the theme picker is open, arrow/Enter/PgUp/PgDn/Home/End
        // navigate or apply — they MUST NOT fall through to the input or
        // transcript handlers below.
        if matches!(self.overlay, Some(Overlay::ThemePicker { .. })) {
            return self.on_key_theme_picker(key);
        }
        if matches!(self.overlay, Some(Overlay::ProviderPicker { .. })) {
            return self.on_key_provider_picker(key);
        }
        if matches!(self.overlay, Some(Overlay::ModelPicker { .. })) {
            return self.on_key_model_picker(key);
        }
        // Doctor overlay owns its own input: typed chars edit a search
        // query that filters the report; arrows/PgUp/PgDn scroll the
        // (possibly filtered) body. F1/F2/F3 / Esc / Ctrl-C above this
        // line still work to switch out or quit.
        if matches!(self.overlay, Some(Overlay::Doctor { .. })) {
            return self.on_key_doctor(key);
        }
        if matches!(self.overlay, Some(Overlay::Log { .. })) {
            return self.on_key_log(key);
        }
        if matches!(self.overlay, Some(Overlay::SessionResume { .. })) {
            return self.on_key_session_resume(key);
        }
        if matches!(self.overlay, Some(Overlay::SessionLockConflict { .. })) {
            return self.on_key_session_lock_conflict(key);
        }
        if matches!(self.overlay, Some(Overlay::Plugins { .. })) {
            return self.on_key_plugins(key);
        }
        if matches!(self.overlay, Some(Overlay::DynamicForm { .. })) {
            return self.on_key_dynamic_form(key);
        }
        if matches!(
            self.overlay,
            Some(Overlay::DynamicModal { .. } | Overlay::DynamicConfirm { .. })
        ) {
            return self.on_key_dynamic_modal_or_confirm(key);
        }
        if matches!(self.overlay, Some(Overlay::DynamicList { .. })) {
            return self.on_key_dynamic_list(key);
        }
        if matches!(self.overlay, Some(Overlay::DynamicTable { .. })) {
            return self.on_key_dynamic_table(key);
        }
        // DynamicTextPanel, DynamicProgress, DynamicStack are
        // display-only; Esc (handled above) dismisses, everything
        // else is a no-op.
        if matches!(
            self.overlay,
            Some(
                Overlay::DynamicTextPanel { .. }
                    | Overlay::DynamicProgress { .. }
                    | Overlay::DynamicStack { .. }
            )
        ) {
            return Vec::new();
        }
        match key.code {
            KeyCode::F(1) => {
                self.set_overlay(Some(Overlay::Help));
                Vec::new()
            }
            KeyCode::F(2) => {
                self.set_overlay(Some(Overlay::Doctor {
                    report: "Running diagnostics…".into(),
                    query: String::new(),
                    scroll: 0,
                }));
                vec![Command::ReturnDoctor]
            }
            KeyCode::F(3) => {
                self.set_overlay(Some(Overlay::Skills(Vec::new())));
                vec![Command::ReturnSkills]
            }
            KeyCode::F(5) => {
                // Toggle thinking visibility. Affects both past
                // already-emitted lines (via the render filter in
                // `ui::draw_transcript`) and any future deltas.
                self.show_thinking = !self.show_thinking;
                self.push(
                    TranscriptKind::Info,
                    if self.show_thinking {
                        "(thinking: visible)".into()
                    } else {
                        "(thinking: hidden)".into()
                    },
                );
                Vec::new()
            }
            KeyCode::F(6) => {
                // Toggle terminal-level mouse capture so the user can
                // pick between scroll-wheel (on) and native text
                // selection / right-click-copy (off). The main loop
                // in `run::event_loop` notices the flag change and
                // re-applies `EnableMouseCapture` / `DisableMouseCapture`
                // on the next iteration.
                self.mouse_capture_on = !self.mouse_capture_on;
                self.push(
                    TranscriptKind::Info,
                    if self.mouse_capture_on {
                        "(mouse: wheel scroll · drag-select needs Option/Shift)".into()
                    } else {
                        "(mouse: native selection · wheel disabled, use PgUp/PgDn)".into()
                    },
                );
                Vec::new()
            }
            KeyCode::Tab => {
                // While the slash palette is open, Tab completes the
                // current input to the focused suggestion's trigger
                // (without submitting, so the user can keep editing).
                // Otherwise Tab is a no-op — explicitly NOT a focus
                // toggle: stranding focus on the transcript silently
                // dropped every typed Char and looked like the input
                // had frozen out.
                if self.palette_visible() {
                    let matches = self.palette_matches();
                    if let Some(item) = matches.get(self.palette_focused) {
                        // Tab only completes slash-command triggers,
                        // not skill names.
                        if matches!(item.action, PaletteAction::DispatchSlash) {
                            self.input = item.trigger.to_string();
                            self.cursor = self.input.len();
                        }
                    }
                }
                Vec::new()
            }
            // Transcript scroll keys work regardless of focus so the
            // user never needs to leave input focus to look back.
            KeyCode::PageUp => {
                self.scroll_up(10);
                Vec::new()
            }
            KeyCode::PageDown => {
                self.scroll_down(10);
                Vec::new()
            }
            KeyCode::End => {
                // Jump to live tail.
                self.follow_bottom = true;
                Vec::new()
            }
            KeyCode::Home => {
                // Jump to the very top, freezing there.
                self.follow_bottom = false;
                self.scroll_offset = 0;
                Vec::new()
            }
            _ => match self.focus {
                Focus::Input => self.on_key_input(key),
                Focus::Transcript => self.on_key_transcript(key),
            },
        }
    }

    fn on_key_input(&mut self, key: KeyEvent) -> Vec<Command> {
        // Slash palette absorbs Up/Down — they're not for cursor
        // motion in a single-line input anyway.
        if self.palette_visible() {
            let matches_len = self.palette_matches().len();
            match key.code {
                KeyCode::Up => {
                    self.palette_focused = self.palette_focused.saturating_sub(1);
                    return Vec::new();
                }
                KeyCode::Down => {
                    if matches_len > 0 {
                        self.palette_focused = (self.palette_focused + 1).min(matches_len - 1);
                    }
                    return Vec::new();
                }
                _ => {}
            }
        } else {
            // Palette hidden → Up/Down walk submitted-prompt history
            // (readline-style).
            match key.code {
                KeyCode::Up => {
                    self.history_up();
                    return Vec::new();
                }
                KeyCode::Down => {
                    self.history_down();
                    return Vec::new();
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter => {
                // If the palette is visible AND has a highlighted
                // match, handle it based on the action type.
                if self.palette_visible() {
                    let matches = self.palette_matches();
                    if let Some(item) = matches.get(self.palette_focused) {
                        match &item.action {
                            PaletteAction::InjectBody(body) => {
                                // Inject the skill body into the input
                                // so the user can review / edit before
                                // submitting to the LLM. Don't send yet.
                                self.input = body.clone();
                                self.cursor = self.input.len();
                                self.palette_focused = 0;
                                self.history_cursor = None;
                                self.history_draft.clear();
                                return Vec::new();
                            }
                            PaletteAction::DispatchSlash => {
                                // Snap the input to the command's trigger
                                // so partial typing (`/the`) submits as
                                // the full command (`/theme`).
                                self.input = item.trigger.to_string();
                                self.cursor = self.input.len();
                            }
                        }
                    }
                }
                let line = self.input.trim().to_string();
                if line.is_empty() {
                    return Vec::new();
                }
                self.push_history(&line);
                self.input.clear();
                self.cursor = 0;
                self.palette_focused = 0;
                self.history_cursor = None;
                self.history_draft.clear();
                self.push(TranscriptKind::UserPrompt, line.clone());
                if let Some(stripped) = line.strip_prefix('/') {
                    return self.dispatch_slash(stripped);
                }
                vec![Command::SendPrompt(line)]
            }
            KeyCode::Backspace => {
                if self.cursor > 0 && !self.input.is_empty() {
                    let new_cursor = prev_char_boundary(&self.input, self.cursor);
                    self.input.replace_range(new_cursor..self.cursor, "");
                    self.cursor = new_cursor;
                    self.palette_focused = 0;
                    self.detach_history();
                }
                Vec::new()
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    let end = next_char_boundary(&self.input, self.cursor);
                    self.input.replace_range(self.cursor..end, "");
                    self.palette_focused = 0;
                    self.detach_history();
                }
                Vec::new()
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.input, self.cursor);
                }
                Vec::new()
            }
            KeyCode::Right => {
                if self.cursor < self.input.len() {
                    self.cursor = next_char_boundary(&self.input, self.cursor);
                }
                Vec::new()
            }
            KeyCode::Home => {
                self.cursor = 0;
                Vec::new()
            }
            KeyCode::End => {
                self.cursor = self.input.len();
                Vec::new()
            }
            KeyCode::Char(c) => {
                self.input.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.palette_focused = 0;
                self.detach_history();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn on_key_provider_picker(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(Overlay::ProviderPicker { focused }) = &mut self.overlay else {
            return Vec::new();
        };
        let last = self.providers.len().saturating_sub(1);
        match key.code {
            KeyCode::Up => {
                *focused = focused.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Down => {
                if !self.providers.is_empty() {
                    *focused = (*focused + 1).min(last);
                }
                Vec::new()
            }
            KeyCode::PageUp => {
                *focused = focused.saturating_sub(5);
                Vec::new()
            }
            KeyCode::PageDown => {
                *focused = (*focused + 5).min(last);
                Vec::new()
            }
            KeyCode::Home => {
                *focused = 0;
                Vec::new()
            }
            KeyCode::End => {
                *focused = last;
                Vec::new()
            }
            KeyCode::Enter => {
                if self.providers.is_empty() {
                    self.set_overlay(None);
                    return Vec::new();
                }
                let chosen = *focused;
                // Phase 1: API-key profiles apply via the worker;
                // OAuth profiles surface a clear "not wired" line so
                // the user knows what's happening.
                let is_usable = self.providers[chosen].auth.is_usable();
                if !is_usable {
                    let name = self.providers[chosen].name.clone();
                    self.set_overlay(None);
                    self.push(
                        TranscriptKind::Info,
                        format!(
                            "(provider '{name}' uses OAuth; login flow not yet wired — \
                             this lands in a follow-up patch. Pick an api_key profile \
                             for now.)"
                        ),
                    );
                    return Vec::new();
                }
                self.set_overlay(None);
                vec![Command::ApplyProvider(chosen)]
            }
            _ => Vec::new(),
        }
    }

    /// Key handler for the `/model` picker overlay. Up/Down/PageUp/PageDown
    /// navigate the filtered list; typed characters edit the live search query.
    fn on_key_model_picker(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(Overlay::ModelPicker {
            focused,
            models,
            query,
        }) = &mut self.overlay
        else {
            return Vec::new();
        };
        let filtered = filter_models(models, query);
        let last = filtered.len().saturating_sub(1);
        match key.code {
            // Search-input editing.
            KeyCode::Char(c) => {
                query.push(c);
                *focused = 0;
            }
            KeyCode::Backspace => {
                query.pop();
                *focused = 0;
            }
            KeyCode::Up => {
                *focused = focused.saturating_sub(1);
            }
            KeyCode::Down => {
                if !filtered.is_empty() {
                    *focused = (*focused + 1).min(last);
                }
            }
            KeyCode::PageUp => {
                *focused = focused.saturating_sub(5);
            }
            KeyCode::PageDown => {
                *focused = (*focused + 5).min(last);
            }
            KeyCode::Home => {
                *focused = 0;
            }
            KeyCode::End => {
                *focused = last;
            }
            KeyCode::Enter => {
                if let Some((model_id, _)) = filtered.get(*focused) {
                    let model_id = model_id.clone();
                    self.set_overlay(None);
                    return vec![Command::SetModel(model_id)];
                }
                self.set_overlay(None);
            }
            _ => {}
        }
        Vec::new()
    }

    /// Return the current provider name, inferring from either the
    /// active profile index or the `model_id` prefix.
    fn current_provider_name(&self) -> Option<String> {
        self.current_provider_idx
            .map(|i| self.providers[i].name.clone())
            .or_else(|| self.model_id.split('/').next().map(|s| s.to_string()))
    }

    fn on_key_doctor(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(Overlay::Doctor { query, scroll, .. }) = &mut self.overlay else {
            return Vec::new();
        };
        match key.code {
            // Search-input editing.
            KeyCode::Char(c) => {
                query.push(c);
                *scroll = 0;
            }
            KeyCode::Backspace => {
                query.pop();
                *scroll = 0;
            }
            // Vertical scroll. `scroll` counts lines hidden from the
            // TOP of the body, so Down reveals later lines.
            KeyCode::Down => {
                *scroll = scroll.saturating_add(1);
            }
            KeyCode::Up => {
                *scroll = scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                *scroll = scroll.saturating_add(10);
            }
            KeyCode::PageUp => {
                *scroll = scroll.saturating_sub(10);
            }
            KeyCode::Home => {
                *scroll = 0;
            }
            KeyCode::End => {
                // Renderer clamps to actual max; use a big sentinel.
                *scroll = usize::MAX;
            }
            _ => {}
        }
        Vec::new()
    }

    /// Key handler for the `/resume` picker overlay. ↑↓ move focus;
    /// Enter prints the chosen session's path to the transcript and
    /// closes the overlay (Phase 1 — in-place swap lands later).
    fn on_key_session_resume(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(Overlay::SessionResume {
            focused,
            sessions,
            confirm_delete,
        }) = &mut self.overlay
        else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Up => {
                if *focused > 0 {
                    *focused -= 1;
                }
                *confirm_delete = false;
            }
            KeyCode::Down => {
                if *focused + 1 < sessions.len() {
                    *focused += 1;
                }
                *confirm_delete = false;
            }
            KeyCode::Home => {
                *focused = 0;
                *confirm_delete = false;
            }
            KeyCode::End => {
                *focused = sessions.len().saturating_sub(1);
                *confirm_delete = false;
            }
            KeyCode::Delete => {
                // First press arms; second press fires. Empty list
                // → no-op (UI already shows the empty-state hint).
                let Some(sel) = sessions.get(*focused) else {
                    return Vec::new();
                };
                if *confirm_delete {
                    let path = sel.path.clone();
                    *confirm_delete = false;
                    return vec![Command::DeleteSession(path)];
                }
                *confirm_delete = true;
            }
            KeyCode::Enter => {
                // Enter never confirms a delete — it always resumes.
                // Pressing Enter while armed cancels the arm and
                // resumes the focused row, matching the principle of
                // least destruction.
                *confirm_delete = false;
                let Some(sel) = sessions.get(*focused).cloned() else {
                    self.set_overlay(None);
                    return Vec::new();
                };
                // Locked rows must NOT directly resume — that would
                // race the lock and produce an immediate worker
                // error. Instead swap the resume overlay for the
                // session-lock-conflict overlay (source=Resume,
                // default-focus on the safe "fresh" choice).
                if sel.locked {
                    self.set_overlay(Some(Overlay::SessionLockConflict {
                        source: SessionLockSource::Resume,
                        locked_path: sel.path,
                        choices: vec![
                            SessionConflictChoice::Fresh,
                            SessionConflictChoice::Fork,
                            SessionConflictChoice::Resume,
                            SessionConflictChoice::Cancel,
                        ],
                        focused: 0,
                    }));
                    return Vec::new();
                }
                let path = sel.path.clone();
                self.push(
                    TranscriptKind::Info,
                    format!("(resuming session: {})", path.display()),
                );
                self.set_overlay(None);
                return vec![Command::ResumeSession(path)];
            }
            _ => {}
        }
        Vec::new()
    }

    /// Key handler for [`Overlay::SessionLockConflict`]. Up/Down
    /// navigates the choice list; Enter dispatches the focused
    /// choice; Esc dismisses (Resume source) or is rejected (Boot
    /// source — the user must pick one; Esc on boot also defaults
    /// to "Fresh" because we've already swapped to a fresh
    /// session, so dismissing is equivalent).
    fn on_key_session_lock_conflict(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(Overlay::SessionLockConflict {
            source,
            locked_path,
            choices,
            focused,
        }) = &mut self.overlay
        else {
            return Vec::new();
        };
        let source = *source;
        let last = choices.len().saturating_sub(1);
        match key.code {
            KeyCode::Up => {
                *focused = focused.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Down => {
                if !choices.is_empty() {
                    *focused = (*focused + 1).min(last);
                }
                Vec::new()
            }
            KeyCode::Home => {
                *focused = 0;
                Vec::new()
            }
            KeyCode::End => {
                *focused = last;
                Vec::new()
            }
            KeyCode::Enter => {
                let Some(choice) = choices.get(*focused).copied() else {
                    self.set_overlay(None);
                    return Vec::new();
                };
                let locked_path = locked_path.clone();
                self.set_overlay(None);
                match choice {
                    SessionConflictChoice::Fresh => {
                        // Boot already swapped to a fresh session;
                        // Resume needs an explicit Reset to land
                        // there from the live (non-locked) one.
                        match source {
                            SessionLockSource::Boot => Vec::new(),
                            SessionLockSource::Resume => {
                                self.push(
                                    TranscriptKind::Info,
                                    "(switching to a fresh session)".into(),
                                );
                                vec![Command::Reset]
                            }
                        }
                    }
                    SessionConflictChoice::Fork => {
                        self.push(
                            TranscriptKind::Info,
                            format!("(forking from snapshot: {})", locked_path.display()),
                        );
                        vec![Command::ForkSession(locked_path)]
                    }
                    SessionConflictChoice::Resume => {
                        // Close the lock-conflict modal and open the
                        // /resume picker so the user can browse all
                        // past sessions.
                        self.set_overlay(Some(Overlay::SessionResume {
                            focused: 0,
                            sessions: Vec::new(),
                            confirm_delete: false,
                        }));
                        vec![Command::ReturnSessions]
                    }
                    SessionConflictChoice::Quit => {
                        self.should_quit = true;
                        vec![Command::Quit]
                    }
                    SessionConflictChoice::Cancel => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    /// Key handler for the `/plugins` overlay. Plugin-contributed
    /// `[[ui_command]]` entries register single-character keys that
    /// dispatch to a Rhai handler; everything else is a no-op (Esc
    /// is owned by the outer dispatcher).
    fn on_key_plugins(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(Overlay::Plugins { ui_commands, .. }) = &self.overlay else {
            return Vec::new();
        };
        let KeyCode::Char(c) = key.code else {
            return Vec::new();
        };
        let typed = c.to_string();
        // Only the "plugins" target is honored today. Future
        // overlays will read the same `target` field with their own
        // string.
        for cmd in ui_commands {
            if cmd.command.target == "plugins" && cmd.command.key == typed {
                return vec![Command::InvokePluginUi {
                    handler: cmd.command.handler.clone(),
                    args: serde_json::Value::Null,
                }];
            }
        }
        Vec::new()
    }

    /// Key handler for [`Overlay::DynamicForm`]. Tab / Down cycles
    /// field focus; Shift-Tab / Up cycles backward; printable chars
    /// extend the focused buffer; Backspace shortens it; Enter
    /// bundles every field into a JSON object and dispatches
    /// `on_submit`.
    fn on_key_dynamic_form(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(Overlay::DynamicForm {
            fields,
            on_submit,
            focused,
            ..
        }) = &mut self.overlay
        else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                if !fields.is_empty() {
                    *focused = (*focused + 1) % fields.len();
                }
                Vec::new()
            }
            KeyCode::BackTab | KeyCode::Up => {
                if !fields.is_empty() {
                    *focused = (*focused + fields.len() - 1) % fields.len();
                }
                Vec::new()
            }
            KeyCode::Enter => {
                let handler = on_submit.clone();
                let mut obj = serde_json::Map::new();
                for f in fields.iter() {
                    obj.insert(f.name.clone(), serde_json::Value::String(f.value.clone()));
                }
                self.set_overlay(None);
                vec![Command::InvokePluginUi {
                    handler,
                    args: serde_json::Value::Object(obj),
                }]
            }
            KeyCode::Backspace => {
                if let Some(field) = fields.get_mut(*focused) {
                    field.value.pop();
                }
                Vec::new()
            }
            KeyCode::Char(c) => {
                if let Some(field) = fields.get_mut(*focused) {
                    field.value.push(c);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Key handler shared by [`Overlay::DynamicModal`] and
    /// [`Overlay::DynamicConfirm`]. Modal: Enter dismisses.
    /// Confirm: `y` / Enter dispatches `on_yes` with `yes_args`;
    /// `n` dismisses. Esc is owned by the outer dispatcher.
    fn on_key_dynamic_modal_or_confirm(&mut self, key: KeyEvent) -> Vec<Command> {
        match self.overlay.as_ref() {
            Some(Overlay::DynamicModal { .. }) => {
                if matches!(key.code, KeyCode::Enter) {
                    self.set_overlay(None);
                }
                Vec::new()
            }
            Some(Overlay::DynamicConfirm {
                on_yes, yes_args, ..
            }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let handler = on_yes.clone();
                    let args = yes_args.clone();
                    self.set_overlay(None);
                    vec![Command::InvokePluginUi { handler, args }]
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.set_overlay(None);
                    Vec::new()
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// Key handler for [`Overlay::DynamicList`]. Up/Down navigates;
    /// Enter dispatches `on_select` (if set) with
    /// `{ index, value }` and dismisses; Esc just dismisses.
    fn on_key_dynamic_list(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(Overlay::DynamicList {
            items,
            on_select,
            focused,
            ..
        }) = &mut self.overlay
        else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Up => {
                if *focused > 0 {
                    *focused -= 1;
                }
                Vec::new()
            }
            KeyCode::Down => {
                if *focused + 1 < items.len() {
                    *focused += 1;
                }
                Vec::new()
            }
            KeyCode::Home => {
                *focused = 0;
                Vec::new()
            }
            KeyCode::End => {
                *focused = items.len().saturating_sub(1);
                Vec::new()
            }
            KeyCode::Enter => {
                let handler = on_select.clone();
                let value = items.get(*focused).cloned().unwrap_or_default();
                let index = *focused;
                self.set_overlay(None);
                match handler {
                    Some(handler) => vec![Command::InvokePluginUi {
                        handler,
                        args: serde_json::json!({ "index": index, "value": value }),
                    }],
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    /// Key handler for [`Overlay::DynamicTable`]. Same navigation
    /// shape as DynamicList but dispatches with
    /// `{ row_index, row: [<column>...] }`.
    fn on_key_dynamic_table(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(Overlay::DynamicTable {
            rows,
            on_select,
            focused,
            ..
        }) = &mut self.overlay
        else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Up => {
                if *focused > 0 {
                    *focused -= 1;
                }
                Vec::new()
            }
            KeyCode::Down => {
                if *focused + 1 < rows.len() {
                    *focused += 1;
                }
                Vec::new()
            }
            KeyCode::Home => {
                *focused = 0;
                Vec::new()
            }
            KeyCode::End => {
                *focused = rows.len().saturating_sub(1);
                Vec::new()
            }
            KeyCode::Enter => {
                let handler = on_select.clone();
                let row = rows.get(*focused).cloned().unwrap_or_default();
                let index = *focused;
                self.set_overlay(None);
                match handler {
                    Some(handler) => vec![Command::InvokePluginUi {
                        handler,
                        args: serde_json::json!({ "row_index": index, "row": row }),
                    }],
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    /// Key handler for the `/log` overlay. PgUp/PgDn page through the
    /// joined entries; Home/End jump to ends. Esc is handled by the
    /// outer dispatcher.
    fn on_key_log(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(Overlay::Log { scroll }) = &mut self.overlay else {
            return Vec::new();
        };
        const PAGE: usize = 12;
        match key.code {
            KeyCode::PageUp => {
                *scroll = scroll.saturating_sub(PAGE);
            }
            KeyCode::PageDown => {
                *scroll = scroll.saturating_add(PAGE);
            }
            KeyCode::Up => {
                *scroll = scroll.saturating_sub(1);
            }
            KeyCode::Down => {
                *scroll = scroll.saturating_add(1);
            }
            KeyCode::Home => {
                *scroll = 0;
            }
            KeyCode::End => {
                // Jump well past the bottom; Paragraph::scroll clamps
                // visually so over-shoot is fine.
                *scroll = u16::MAX as usize;
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_key_theme_picker(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(Overlay::ThemePicker { focused }) = &mut self.overlay else {
            return Vec::new();
        };
        let last = self.themes.len().saturating_sub(1);
        match key.code {
            KeyCode::Up => {
                *focused = focused.saturating_sub(1);
            }
            KeyCode::Down => {
                *focused = (*focused + 1).min(last);
            }
            KeyCode::PageUp => {
                *focused = focused.saturating_sub(5);
            }
            KeyCode::PageDown => {
                *focused = (*focused + 5).min(last);
            }
            KeyCode::Home => {
                *focused = 0;
            }
            KeyCode::End => {
                *focused = last;
            }
            KeyCode::Enter => {
                let chosen = *focused;
                let name = self.themes[chosen].name.clone();
                self.current_theme_idx = chosen;
                self.set_overlay(None);
                self.push(TranscriptKind::Info, format!("(theme: {name})"));
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_key_transcript(&mut self, key: KeyEvent) -> Vec<Command> {
        match key.code {
            KeyCode::Up => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            KeyCode::Down => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
            }
            KeyCode::Home => {
                self.scroll_offset = self.transcript.len();
            }
            KeyCode::End => {
                self.scroll_offset = 0;
            }
            _ => {}
        }
        Vec::new()
    }

    fn dispatch_slash(&mut self, rest: &str) -> Vec<Command> {
        let head = rest.split_whitespace().next().unwrap_or("");
        // Plugin-contributed slash commands take precedence over
        // built-ins — `/plugins` typed by the user goes through
        // lazy-gagent's `ui_plugins_panel` handler (if installed),
        // falling back to the built-in overlay otherwise.
        if let Some(bound) = self
            .plugin_slashes
            .iter()
            .find(|b| b.command.trigger == head)
        {
            let handler = bound.command.handler.clone();
            return vec![Command::InvokePluginUi {
                handler,
                args: serde_json::Value::Null,
            }];
        }
        match head {
            "help" | "?" => {
                self.set_overlay(Some(Overlay::Help));
                Vec::new()
            }
            "clear" | "reset" => {
                self.push(TranscriptKind::Info, "(transcript cleared)".into());
                vec![Command::Reset]
            }
            "doctor" => {
                self.set_overlay(Some(Overlay::Doctor {
                    report: "Running diagnostics…".into(),
                    query: String::new(),
                    scroll: 0,
                }));
                vec![Command::ReturnDoctor]
            }
            "skills" => {
                self.set_overlay(Some(Overlay::Skills(Vec::new())));
                vec![Command::ReturnSkills]
            }
            "theme" | "themes" => {
                // Open the picker focused on the active theme so
                // up/down feel natural from the user's current pick.
                self.set_overlay(Some(Overlay::ThemePicker {
                    focused: self.current_theme_idx,
                }));
                Vec::new()
            }
            "provider" | "providers" => {
                if self.providers.is_empty() {
                    self.push(
                        TranscriptKind::Info,
                        "(no provider profiles configured; create \
                         <workspace>/.grain/providers.toml — see docs)"
                            .into(),
                    );
                    return Vec::new();
                }
                let focused = self.current_provider_idx.unwrap_or(0);
                self.set_overlay(Some(Overlay::ProviderPicker { focused }));
                Vec::new()
            }
            "model" | "models" => {
                let provider = self.current_provider_name();
                let Some(provider) = provider else {
                    self.push(
                        TranscriptKind::Info,
                        "(no provider selected — use /provider first, or pass --model)".into(),
                    );
                    return Vec::new();
                };
                self.set_overlay(Some(Overlay::ModelPicker {
                    focused: 0,
                    models: Vec::new(),
                    query: String::new(),
                }));
                vec![Command::ListModels(provider)]
            }
            "resume" => {
                // Open the picker immediately with an empty list;
                // the worker scans disk and replies via
                // `TuiEvent::SessionsListed`, which swaps the list in.
                self.set_overlay(Some(Overlay::SessionResume {
                    focused: 0,
                    sessions: Vec::new(),
                    confirm_delete: false,
                }));
                vec![Command::ReturnSessions]
            }
            "log" | "logs" => {
                if self.request_log.is_empty() {
                    self.push(
                        TranscriptKind::Info,
                        "(no requests logged — start the TUI with --debug-log \
                         to capture them)"
                            .into(),
                    );
                    return Vec::new();
                }
                self.set_overlay(Some(Overlay::Log { scroll: 0 }));
                Vec::new()
            }
            "compact" => {
                // Keep last 4 messages (≈2 turns) — sane default that
                // matches Claude Code's /compact behavior. Power users
                // can extend with `/compact <n>` later.
                self.push(
                    TranscriptKind::Info,
                    "(compacting transcript — keeping last 4 messages)".into(),
                );
                vec![Command::Compact { keep_recent: 4 }]
            }
            "plugins" => {
                // Open the overlay immediately with an empty list;
                // the worker scans disk and replies via
                // `TuiEvent::PluginsListed`, which swaps the list in.
                self.set_overlay(Some(Overlay::Plugins {
                    plugins: Vec::new(),
                    ui_commands: Vec::new(),
                }));
                vec![Command::ReturnPlugins]
            }
            "install" => {
                let mut parts = rest.split_whitespace();
                let _ = parts.next(); // skip "install"
                let Some(name) = parts.next() else {
                    self.push(
                        TranscriptKind::Info,
                        "(usage: /install <name> <src> [rev])".into(),
                    );
                    return Vec::new();
                };
                let Some(src) = parts.next() else {
                    self.push(
                        TranscriptKind::Info,
                        "(usage: /install <name> <src> [rev])".into(),
                    );
                    return Vec::new();
                };
                let rev = parts.next().map(String::from);
                self.push(
                    TranscriptKind::Info,
                    format!("(installing '{name}' from {src} …)"),
                );
                vec![Command::InstallPlugin {
                    name: name.into(),
                    src: src.into(),
                    rev,
                }]
            }
            "update" => {
                let mut parts = rest.split_whitespace();
                let _ = parts.next(); // skip "update"
                let Some(name) = parts.next() else {
                    self.push(TranscriptKind::Info, "(usage: /update <name>)".into());
                    return Vec::new();
                };
                self.push(TranscriptKind::Info, format!("(updating '{name}' …)"));
                vec![Command::UpdatePlugin { name: name.into() }]
            }
            "reload" => {
                self.push(TranscriptKind::Info, "(reloading Rhai scripts…)".into());
                vec![Command::ReloadRhaiScripts]
            }
            "remove" | "uninstall" => {
                let mut parts = rest.split_whitespace();
                let _ = parts.next(); // skip head
                let Some(name) = parts.next() else {
                    self.push(
                        TranscriptKind::Info,
                        "(usage: /remove <name> [--keep-files])".into(),
                    );
                    return Vec::new();
                };
                let delete_files = !parts.any(|p| p == "--keep-files");
                self.push(
                    TranscriptKind::Info,
                    format!(
                        "(removing '{name}'{} …)",
                        if delete_files { " + files" } else { "" }
                    ),
                );
                vec![Command::RemovePlugin {
                    name: name.into(),
                    delete_files,
                }]
            }
            "exit" | "quit" | "q" => {
                self.should_quit = true;
                vec![Command::Quit]
            }
            other => {
                self.push(
                    TranscriptKind::Info,
                    format!("(unknown command /{other}; try /help)"),
                );
                Vec::new()
            }
        }
    }

    fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::AgentStart => {
                self.streaming = true;
                self.streaming_started_at = Some(Instant::now());
                // Fresh prompt → fresh counters. We accumulate only
                // across the current run, not across the session,
                // matching Claude Code's "Marinating … (Xs · ↓ tokens)"
                // semantics.
                self.tokens_in = 0;
                self.tokens_out = 0;
                self.tokens_cache_read = 0;
                self.cache_high_streak = 0;
                self.cache_dropped = false;
            }
            AgentEvent::TurnStart => {}
            AgentEvent::MessageStart { .. } => {}
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => match assistant_message_event {
                AssistantMessageEvent::TextDelta { partial, .. } => {
                    // Use the *canonical* concatenated text from
                    // `partial.content` — the genai-side inbound
                    // already maintains the authoritative running
                    // buffer there. Trying to dedup raw `delta`
                    // values (cumulative-vs-incremental, jitter,
                    // out-of-order chunks) is brittle and was
                    // producing screenshots of N stacked snapshots
                    // of the same growing sentence on opencodezen
                    // and kimi-k2.6.
                    let canonical = concat_assistant_text(&partial);
                    self.set_streaming_canonical(TranscriptKind::AssistantText, &canonical);
                }
                AssistantMessageEvent::ThinkingDelta { partial, .. } => {
                    let canonical = concat_assistant_thinking(&partial);
                    self.set_streaming_canonical(TranscriptKind::ThinkingText, &canonical);
                }
                _ => {}
            },
            AgentEvent::MessageEnd { message } => {
                if let AgentMessage::Standard(Message::Assistant(am)) = &message {
                    // Update current run usage metrics to match the latest turn's physical context occupancy.
                    // Do not accumulate inputs/outputs across turns here, as multi-turn sessions inherently
                    // carry previous history (accumulating them causes quadratic double-counting).
                    self.tokens_in = am.usage.input;
                    self.tokens_out = am.usage.output;
                    self.tokens_cache_read = am.usage.cache_read;
                    // Session-cumulative — never resets, so footer
                    // can show `Σ $0.43` even when idle.
                    self.session_usage.input =
                        self.session_usage.input.saturating_add(am.usage.input);
                    self.session_usage.output =
                        self.session_usage.output.saturating_add(am.usage.output);
                    self.session_usage.cache_read = self
                        .session_usage
                        .cache_read
                        .saturating_add(am.usage.cache_read);
                    self.session_usage.cache_write = self
                        .session_usage
                        .cache_write
                        .saturating_add(am.usage.cache_write);
                    // Cache-drop detection runs on the *per-turn* hit
                    // rate (not the cumulative one). The cumulative
                    // rate moves slowly and would mask the abrupt
                    // shift caused by a prefix mutation; per-turn
                    // exposes it on the very next message.
                    let (streak, dropped) = update_cache_drop_state(
                        self.cache_high_streak,
                        self.cache_dropped,
                        am.usage.input,
                        am.usage.cache_read,
                    );
                    self.cache_high_streak = streak;
                    self.cache_dropped = dropped;
                    self.ensure_trailing_newline(TranscriptKind::AssistantText);
                    self.ensure_trailing_newline(TranscriptKind::ThinkingText);
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.pending_tool_calls = self.pending_tool_calls.saturating_add(1);
                let line = format_tool_start_line(&tool_name, &args);
                self.active_tools.insert(
                    tool_call_id,
                    ActiveToolDisplay {
                        name: tool_name,
                        args,
                        start_line: self.transcript.len(),
                    },
                );
                self.push(TranscriptKind::ToolCallStart, line);
            }
            AgentEvent::ToolExecutionUpdate { .. } => {}
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                is_error,
                result,
            } => {
                if self.pending_tool_calls > 0 {
                    self.pending_tool_calls -= 1;
                }
                let active = self.active_tools.remove(&tool_call_id);
                if is_error
                    && let Some(active) = &active
                    && let Some(start) = self.transcript.get_mut(active.start_line)
                {
                    start.text = mark_tool_start_error(&start.text);
                }
                let args = active
                    .as_ref()
                    .map(|a| &a.args)
                    .unwrap_or(&serde_json::Value::Null);
                let name = active
                    .as_ref()
                    .map(|a| a.name.as_str())
                    .unwrap_or(&tool_name);
                let (kind, line) = format_tool_result_line(
                    name,
                    args,
                    &result,
                    is_error,
                    Some(&self.workspace_display),
                );
                self.push(kind, line);
                if matches!(name, "edit" | "write" | "bash") {
                    self.refresh_git_prompt();
                }
            }
            AgentEvent::TurnEnd { message, .. } => {
                if let Some(err) = &message.error_message {
                    self.last_error = Some(err.clone());
                    self.push(TranscriptKind::Error, format!("[turn error] {err}"));
                    // Flash the error row.
                    {
                        use crate::anim::{EffectKind, FxDuration, fx};
                        let palette = &self.themes[self.current_theme_idx].palette;
                        self.effects.clear_kind(EffectKind::ErrorFlash);
                        self.effects.push(
                            EffectKind::ErrorFlash,
                            fx::sequence(&[
                                fx::paint_fg(palette.error, FxDuration::from_millis(50)),
                                fx::fade_to_fg(palette.muted, FxDuration::from_millis(300)),
                            ]),
                            self.render_metrics.get().full_area,
                        );
                    }
                }
            }
            AgentEvent::AgentEnd { messages } => {
                let turns = messages
                    .iter()
                    .filter(|m| matches!(m, AgentMessage::Standard(Message::Assistant(_))))
                    .count();
                self.push(
                    TranscriptKind::Info,
                    format!("[done] {turns} assistant turn(s)"),
                );
                self.streaming = false;
                self.streaming_started_at = None;
                self.pending_tool_calls = 0;
                // The retry-on-overflow status is per-turn — clear it
                // when the turn ends so it doesn't linger into idle.
                self.ephemeral_status = None;
            }
        }
    }

    /// Set the active streaming line of `kind` to `canonical` (the
    /// authoritative cumulative text from `AssistantMessageEvent::*
    /// .partial`). Walks back from the tail looking for a same-kind
    /// line, ignoring interleaved ThinkingText / Info / etc. Stops
    /// at hard turn boundaries (`UserPrompt` / `ToolCallEnd`) so we
    /// never merge across turns.
    ///
    /// Replaces text rather than appending — `canonical` already
    /// reflects every chunk emitted so far, so any "delta dedup"
    /// dance is a category error. This is the streamdown approach
    /// applied at the source: trust the agent-side running buffer
    /// instead of trying to reconstruct it from chunk fragments.
    fn set_streaming_canonical(&mut self, kind: TranscriptKind, canonical: &str) {
        for line in self.transcript.iter_mut().rev() {
            if line.kind == kind {
                line.text.clear();
                line.text.push_str(canonical);
                return;
            }
            if matches!(line.kind, TranscriptKind::UserPrompt) || is_tool_call_terminator(line.kind)
            {
                break;
            }
        }
        self.push(kind, canonical.to_string());
    }

    fn ensure_trailing_newline(&mut self, kind: TranscriptKind) {
        if let Some(last) = self.transcript.last_mut()
            && last.kind == kind
            && !last.text.ends_with('\n')
        {
            last.text.push('\n');
        }
    }
}

/// Concatenate every Text block in `partial.content` into one string.
/// Multiple Text blocks arise when reasoning chunks interrupt the
/// model's prose (DeepSeek's thinking mode is the most common
/// offender) — the genai inbound starts a fresh Text block after
/// every Thinking block, so the final assistant message can have
/// several. We render them as one continuous AssistantText line in
/// the TUI; ThinkingText is rendered separately on its own line.
fn concat_assistant_text(partial: &grain_agent_core::AssistantMessage) -> String {
    use grain_agent_core::AssistantContent;
    let mut out = String::new();
    for c in &partial.content {
        if let AssistantContent::Text(t) = c {
            out.push_str(&t.text);
        }
    }
    out
}

/// Same as [`concat_assistant_text`] but for Thinking blocks.
fn concat_assistant_thinking(partial: &grain_agent_core::AssistantMessage) -> String {
    use grain_agent_core::AssistantContent;
    let mut out = String::new();
    for c in &partial.content {
        if let AssistantContent::Thinking(t) = c {
            out.push_str(&t.thinking);
        }
    }
    out
}

fn format_tool_start_line(tool_name: &str, args: &serde_json::Value) -> String {
    let label = tool_display_label(tool_name);
    let subject = tool_primary_arg(tool_name, args).unwrap_or_else(|| preview_json(args, 96));
    if subject.is_empty() || subject == "{}" {
        format!("● {label}")
    } else {
        format!("● {label}({subject})")
    }
}

fn mark_tool_start_error(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("●! ") {
        format!("●! {rest}")
    } else if let Some(rest) = line.strip_prefix("● ") {
        format!("●! {rest}")
    } else {
        format!("●! {line}")
    }
}

fn tool_display_label(name: &str) -> &'static str {
    match name {
        "bash" => "Bash",
        "read" => "Read",
        "write" => "Write",
        "edit" => "Update",
        "grep" => "Search",
        "glob" => "Glob",
        "list" => "List",
        "web_fetch" => "Fetch",
        "source_info" => "SourceInfo",
        _ => "Tool",
    }
}

fn tool_primary_arg(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    let get = |key: &str| args.get(key).and_then(|v| v.as_str()).map(str::to_string);
    match tool_name {
        "bash" => get("command"),
        "read" | "write" | "edit" | "list" => get("path").or_else(|| get("file_path")),
        "grep" | "glob" => get("pattern"),
        "web_fetch" => get("url"),
        _ => None,
    }
    .map(|s| truncate_oneline(&s, 120))
}

fn format_tool_result_line(
    tool_name: &str,
    args: &serde_json::Value,
    result: &grain_agent_core::AgentToolResult,
    is_error: bool,
    workspace_root: Option<&str>,
) -> (TranscriptKind, String) {
    let text = tool_result_text(result);
    if is_error {
        let mut lines = vec![format!("  └ {}", tool_error_summary(tool_name))];
        lines.extend(preview_child_lines(&text, 6, 220));
        return (TranscriptKind::ToolCallError, lines.join("\n"));
    }

    let mut lines = vec![format!(
        "  └ {}",
        tool_success_summary(tool_name, args, result)
    )];
    match tool_name {
        "edit" | "write" => {
            lines.extend(tool_diff_preview(
                tool_name,
                args,
                result,
                workspace_root,
                24,
            ));
        }
        "bash" => {
            lines.extend(preview_child_lines(&text, 10, 220));
        }
        "read" | "grep" | "glob" | "list" | "web_fetch" => {
            lines.extend(preview_child_lines(&text, 8, 220));
        }
        _ => {
            lines.extend(preview_child_lines(&text, 4, 220));
        }
    }
    (TranscriptKind::ToolCallEnd, lines.join("\n"))
}

fn tool_error_summary(tool_name: &str) -> &'static str {
    match tool_name {
        "edit" => "Error editing file",
        "write" => "Error writing file",
        "read" => "Error reading file",
        "bash" => "Command failed",
        "grep" => "Search failed",
        "glob" => "Glob failed",
        "list" => "List failed",
        "web_fetch" => "Fetch failed",
        _ => "Tool failed",
    }
}

fn tool_success_summary(
    tool_name: &str,
    args: &serde_json::Value,
    result: &grain_agent_core::AgentToolResult,
) -> String {
    let d = &result.details;
    match tool_name {
        "edit" => {
            let path = detail_str(d, "path").or_else(|| tool_primary_arg(tool_name, args));
            let replacements = detail_u64(d, "replacements").unwrap_or(1);
            let bytes_delta = detail_i64(d, "bytesDelta").unwrap_or(0);
            let line_delta = edit_line_delta(args);
            let mut pieces = Vec::new();
            if line_delta.added > 0 || line_delta.removed > 0 {
                pieces.push(format!(
                    "added {} {}, removed {} {}",
                    line_delta.added,
                    plural(line_delta.added, "line", "lines"),
                    line_delta.removed,
                    plural(line_delta.removed, "line", "lines")
                ));
            }
            pieces.push(format!(
                "{replacements} {}",
                plural(replacements, "replacement", "replacements")
            ));
            pieces.push(format!("{bytes_delta:+} bytes"));
            format!(
                "{}{}",
                path.map(|p| format!("Updated {p}: "))
                    .unwrap_or_else(|| "Updated: ".into()),
                pieces.join(", ")
            )
        }
        "write" => {
            let path = detail_str(d, "path").or_else(|| tool_primary_arg(tool_name, args));
            let lines = detail_u64(d, "lines").unwrap_or(0);
            let bytes = detail_u64(d, "bytes").unwrap_or(0);
            let action = if detail_bool(d, "created").unwrap_or(false) {
                "Created"
            } else {
                "Wrote"
            };
            format!(
                "{action} {} ({lines} {}, {bytes} bytes)",
                path.unwrap_or_else(|| "file".into()),
                plural(lines, "line", "lines")
            )
        }
        "read" => {
            let path = detail_str(d, "path").or_else(|| tool_primary_arg(tool_name, args));
            let lines = detail_u64(d, "lines").unwrap_or(0);
            let total = detail_u64(d, "totalLines").unwrap_or(lines);
            if lines == total {
                format!(
                    "Read {lines} {} from {}",
                    plural(lines, "line", "lines"),
                    path.unwrap_or_else(|| "file".into())
                )
            } else {
                format!(
                    "Read {lines}/{total} lines from {}",
                    path.unwrap_or_else(|| "file".into())
                )
            }
        }
        "grep" => {
            let matches = detail_u64(d, "matches").unwrap_or(0);
            let files = detail_u64(d, "files").unwrap_or(0);
            if matches == 0 {
                "No matches".into()
            } else {
                format!(
                    "Found {matches} {} in {files} {}",
                    plural(matches, "match", "matches"),
                    plural(files, "file", "files")
                )
            }
        }
        "glob" => {
            let matches = detail_u64(d, "matches").unwrap_or(0);
            format!("Found {matches} {}", plural(matches, "file", "files"))
        }
        "list" => {
            let path = detail_str(d, "path").or_else(|| tool_primary_arg(tool_name, args));
            let dirs = detail_u64(d, "directories").unwrap_or(0);
            let files = detail_u64(d, "files").unwrap_or(0);
            format!(
                "Listed {} ({dirs} {}, {files} {})",
                path.unwrap_or_else(|| ".".into()),
                plural(dirs, "dir", "dirs"),
                plural(files, "file", "files")
            )
        }
        "bash" => {
            let code = detail_i64(d, "exitCode")
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".into());
            let dur = detail_u64(d, "durationMs")
                .map(format_duration_ms)
                .unwrap_or_else(|| "-".into());
            format!("Exited {code} in {dur}")
        }
        "web_fetch" => {
            let url = tool_primary_arg(tool_name, args).unwrap_or_else(|| "url".into());
            format!("Fetched {url}")
        }
        _ => {
            let text = truncate_oneline(&tool_result_text(result), 180);
            if text.is_empty() {
                "Completed".into()
            } else {
                text
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct LineDelta {
    added: u64,
    removed: u64,
}

fn edit_line_delta(args: &serde_json::Value) -> LineDelta {
    let old = args.get("old").and_then(|v| v.as_str()).unwrap_or("");
    let new = args.get("new").and_then(|v| v.as_str()).unwrap_or("");
    if old == new {
        return LineDelta {
            added: 0,
            removed: 0,
        };
    }
    LineDelta {
        added: line_count(new),
        removed: line_count(old),
    }
}

fn tool_diff_preview(
    tool_name: &str,
    args: &serde_json::Value,
    result: &grain_agent_core::AgentToolResult,
    workspace_root: Option<&str>,
    max_lines: usize,
) -> Vec<String> {
    if let Some(diff) = workspace_root.and_then(|root| git_diff_for_tool(root, args, result))
        && !diff.trim().is_empty()
    {
        return diff_text_preview(&diff, max_lines);
    }

    if let Some(diff) = detail_str(&result.details, "uiDiff")
        && !diff.trim().is_empty()
    {
        return diff_text_preview(&diff, max_lines);
    }

    match tool_name {
        "edit" => edit_diff_preview(args, max_lines),
        "write" => new_file_diff_preview(args, result, max_lines),
        _ => Vec::new(),
    }
}

fn git_diff_for_tool(
    workspace_root: &str,
    args: &serde_json::Value,
    result: &grain_agent_core::AgentToolResult,
) -> Option<String> {
    let path = detail_str(&result.details, "path")
        .or_else(|| {
            args.get("path")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            args.get("file_path")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })?;
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .arg("diff")
        .arg("--no-ext-diff")
        .arg("--no-color")
        .arg("--unified=3")
        .arg("--")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let diff = String::from_utf8_lossy(&output.stdout).into_owned();
    let diff = diff.trim_end();
    if diff.is_empty() {
        None
    } else {
        Some(limit_diff_text(diff, 32 * 1024))
    }
}

fn limit_diff_text(diff: &str, max_bytes: usize) -> String {
    if diff.len() <= max_bytes {
        return diff.to_string();
    }
    let mut cut = max_bytes.min(diff.len());
    while cut > 0 && !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n… diff truncated at {} bytes",
        diff[..cut].trim_end(),
        max_bytes
    )
}

fn diff_text_preview(diff: &str, max_lines: usize) -> Vec<String> {
    let mut kept = Vec::new();
    let mut total = 0usize;
    for line in diff.lines() {
        total += 1;
        if line.starts_with("diff --git ") || line.starts_with("index ") {
            continue;
        }
        if kept.len() < max_lines {
            kept.push(format!("    {}", truncate_oneline(line, 260)));
        }
    }
    if total > kept.len() {
        kept.push(format!(
            "    … {} diff line(s) hidden",
            total.saturating_sub(kept.len())
        ));
    }
    kept
}

fn new_file_diff_preview(
    args: &serde_json::Value,
    result: &grain_agent_core::AgentToolResult,
    max_lines: usize,
) -> Vec<String> {
    if !detail_bool(&result.details, "created").unwrap_or(false) {
        return Vec::new();
    }
    let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    let path = detail_str(&result.details, "path").or_else(|| tool_primary_arg("write", args));
    let mut out = Vec::new();
    out.push(format!(
        "    @@ new file{} @@",
        path.map(|p| format!(": {p}")).unwrap_or_default()
    ));
    let mut shown = 0usize;
    for line in content.lines().take(max_lines.saturating_sub(1)) {
        out.push(format!("    +{}", truncate_oneline(line, 260)));
        shown += 1;
    }
    let total = content.lines().count();
    if total > shown {
        out.push(format!("    … {} diff line(s) hidden", total - shown));
    }
    out
}

fn edit_diff_preview(args: &serde_json::Value, max_lines: usize) -> Vec<String> {
    let old = args.get("old").and_then(|v| v.as_str()).unwrap_or("");
    let new = args.get("new").and_then(|v| v.as_str()).unwrap_or("");
    if old.is_empty() && new.is_empty() {
        return Vec::new();
    }

    let ops = diff_line_ops(old, new);
    let mut out = vec!["    @@ edit @@".to_string()];
    let mut hidden = 0usize;
    for op in ops {
        if out.len() >= max_lines {
            hidden += 1;
            continue;
        }
        let (prefix, line) = match op {
            DiffOp::Same(line) => (" ", line),
            DiffOp::Delete(line) => ("-", line),
            DiffOp::Insert(line) => ("+", line),
        };
        out.push(format!("    {prefix}{}", truncate_oneline(&line, 260)));
    }
    if hidden > 0 {
        out.push(format!("    … {hidden} diff line(s) hidden"));
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DiffOp {
    Same(String),
    Delete(String),
    Insert(String),
}

fn diff_line_ops(old: &str, new: &str) -> Vec<DiffOp> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines.len().saturating_mul(new_lines.len()) > 20_000 {
        let mut out = Vec::new();
        out.extend(
            old_lines
                .into_iter()
                .map(|line| DiffOp::Delete(line.to_string())),
        );
        out.extend(
            new_lines
                .into_iter()
                .map(|line| DiffOp::Insert(line.to_string())),
        );
        return out;
    }

    let rows = old_lines.len() + 1;
    let cols = new_lines.len() + 1;
    let mut lcs = vec![0usize; rows * cols];
    let at = |i: usize, j: usize| i * cols + j;
    for i in (0..old_lines.len()).rev() {
        for j in (0..new_lines.len()).rev() {
            lcs[at(i, j)] = if old_lines[i] == new_lines[j] {
                lcs[at(i + 1, j + 1)] + 1
            } else {
                lcs[at(i + 1, j)].max(lcs[at(i, j + 1)])
            };
        }
    }

    let mut out = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < old_lines.len() && j < new_lines.len() {
        if old_lines[i] == new_lines[j] {
            out.push(DiffOp::Same(old_lines[i].to_string()));
            i += 1;
            j += 1;
        } else if lcs[at(i + 1, j)] >= lcs[at(i, j + 1)] {
            out.push(DiffOp::Delete(old_lines[i].to_string()));
            i += 1;
        } else {
            out.push(DiffOp::Insert(new_lines[j].to_string()));
            j += 1;
        }
    }
    while i < old_lines.len() {
        out.push(DiffOp::Delete(old_lines[i].to_string()));
        i += 1;
    }
    while j < new_lines.len() {
        out.push(DiffOp::Insert(new_lines[j].to_string()));
        j += 1;
    }
    out
}

fn preview_child_lines(text: &str, max_lines: usize, max_chars: usize) -> Vec<String> {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let total = trimmed.lines().count();
    let mut out: Vec<String> = trimmed
        .lines()
        .take(max_lines)
        .map(|line| format!("    {}", truncate_oneline(line, max_chars)))
        .collect();
    if total > max_lines {
        out.push(format!("    … {} more line(s)", total - max_lines));
    }
    out
}

fn indent_preview_lines(text: &str, max_chars: usize, max_lines: usize) -> String {
    preview_child_lines(text, max_lines, max_chars).join("\n")
}

fn tool_result_text(result: &grain_agent_core::AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text(t) => Some(t.text.as_str()),
            UserContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn detail_str(details: &serde_json::Value, key: &str) -> Option<String> {
    details.get(key)?.as_str().map(str::to_string)
}

fn detail_u64(details: &serde_json::Value, key: &str) -> Option<u64> {
    details.get(key)?.as_u64()
}

fn detail_i64(details: &serde_json::Value, key: &str) -> Option<i64> {
    details.get(key)?.as_i64()
}

fn detail_bool(details: &serde_json::Value, key: &str) -> Option<bool> {
    details.get(key)?.as_bool()
}

fn line_count(s: &str) -> u64 {
    if s.is_empty() {
        0
    } else {
        s.lines().count() as u64
    }
}

fn plural<'a>(n: u64, singular: &'a str, plural: &'a str) -> &'a str {
    if n == 1 { singular } else { plural }
}

fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn preview_json(v: &serde_json::Value, max_chars: usize) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    truncate(&s, max_chars)
}

fn truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}… [+{} chars]", count - max_chars)
    }
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

/// Translate a terminal display column into a byte index inside `s`.
///
/// "The char whose width interval `[acc, acc + w)` covers `target_col`
/// is the one we return, snapping to its start." Mid-wide-char clicks
/// (e.g. the right half of a 2-column CJK glyph) snap to the start of
/// that glyph, so a drag-selection started inside the glyph still
/// includes it. Clicks past the rendered width clamp to `s.len()`.
///
/// ASCII degenerates to `byte_idx == target_col`.
fn visual_col_to_byte_idx(s: &str, target_col: usize) -> usize {
    let mut acc = 0usize;
    for (byte_idx, ch) in s.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if target_col < acc + w {
            return byte_idx;
        }
        acc = acc.saturating_add(w);
    }
    s.len()
}

/// Clamp a byte index to a UTF-8 char boundary inside `s`. Snaps
/// **down** so we never slice past `idx`. Used by selection extraction
/// + render highlight to keep multi-byte chars intact.
fn clamp_to_char_boundary(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    if s.is_char_boundary(idx) {
        idx
    } else {
        prev_char_boundary(s, idx)
    }
}

/// Walk the wrapped-row buffer between `selection.anchor` and `active`
/// and pluck out the substring under the highlight. Newlines join
/// rows. Pure — testable without a real clipboard.
pub(crate) fn extract_selection(rendered: &[RenderedRow], selection: Selection) -> String {
    let (min_r, max_r, _, _) = selection.normalized();
    let mut out = String::new();
    for idx in min_r..=max_r {
        let Some(row) = rendered.get(idx) else {
            continue;
        };
        let Some((lo, hi)) = selection.col_range_for_row(idx, row.text.len()) else {
            continue;
        };
        let lo = clamp_to_char_boundary(&row.text, lo);
        let hi = clamp_to_char_boundary(&row.text, hi);
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&row.text[lo..hi]);
    }
    out
}

/// Write `text` to the OS clipboard via `arboard`. Returns the error
/// stringified — callers surface it to the user via a transcript info
/// line rather than propagating, because clipboard failure shouldn't
/// abort the agent loop.
pub(crate) fn write_clipboard(text: &str) -> Result<(), String> {
    let mut clip = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clip.set_text(text.to_string()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;
    use grain_agent_core::AssistantMessage;

    fn fresh() -> AppState {
        AppState::new(
            "deepseek/deepseek-chat".into(),
            Cost::default(),
            0,
            0, // system_prompt_chars — 0 means unknown (chip hidden when context_window is also 0)
            "/tmp/proj".into(),
            Capabilities::default(),
            false,
            crate::theme::builtin_themes(),
            0,
            Vec::new(),
            None,
            None,
            Vec::new(),
        )
    }

    fn fresh_with_providers(providers: Vec<ProviderProfile>) -> AppState {
        AppState::new(
            "deepseek/deepseek-chat".into(),
            Cost::default(),
            0,
            0, // system_prompt_chars — 0 means unknown (chip hidden when context_window is also 0)
            "/tmp/proj".into(),
            Capabilities::default(),
            false,
            crate::theme::builtin_themes(),
            0,
            providers,
            None,
            None,
            Vec::new(),
        )
    }

    fn api_key_profile(name: &str, model: &str) -> ProviderProfile {
        ProviderProfile {
            name: name.into(),
            kind: grain_llm_genai::ProviderKind::Anthropic,
            base_url: None,
            model: model.into(),
            auth: grain_llm_genai::ProviderAuth::ApiKey {
                env: "ANTHROPIC_API_KEY".into(),
            },
        }
    }

    fn oauth_profile(name: &str) -> ProviderProfile {
        ProviderProfile {
            name: name.into(),
            kind: grain_llm_genai::ProviderKind::Anthropic,
            base_url: None,
            model: "anthropic/claude-sonnet-4-5".into(),
            auth: grain_llm_genai::ProviderAuth::AnthropicOauth,
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn new_state_has_welcome_line_and_input_focus() {
        let s = fresh();
        assert_eq!(s.focus, Focus::Input);
        assert!(!s.transcript.is_empty(), "welcome banner present");
        assert!(s.transcript[0].text.contains("grain-tui"));
    }

    #[test]
    fn typing_appends_to_input_at_cursor() {
        let mut s = fresh();
        let _ = s.on_event(TuiEvent::Key(press(KeyCode::Char('h'))));
        let _ = s.on_event(TuiEvent::Key(press(KeyCode::Char('i'))));
        assert_eq!(s.input, "hi");
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn enter_promotes_input_to_user_prompt_and_emits_command() {
        let mut s = fresh();
        for c in "look".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert_eq!(cmds, vec![Command::SendPrompt("look".into())]);
        assert!(s.input.is_empty());
        assert_eq!(s.cursor, 0);
        // Last transcript line is the user prompt.
        assert_eq!(
            s.transcript.last().unwrap().kind,
            TranscriptKind::UserPrompt
        );
    }

    #[test]
    fn empty_enter_is_a_noop() {
        let mut s = fresh();
        let before_len = s.transcript.len();
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(cmds.is_empty());
        assert_eq!(s.transcript.len(), before_len);
    }

    #[test]
    fn slash_help_opens_overlay_without_command() {
        let mut s = fresh();
        for c in "/help".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(cmds.is_empty());
        assert!(matches!(s.overlay, Some(Overlay::Help)));
    }

    #[test]
    fn slash_clear_emits_reset_command_and_logs_info() {
        let mut s = fresh();
        for c in "/clear".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert_eq!(cmds, vec![Command::Reset]);
        assert!(
            s.transcript
                .iter()
                .any(|l| l.text.contains("transcript cleared"))
        );
    }

    #[test]
    fn slash_unknown_logs_info_no_command() {
        let mut s = fresh();
        for c in "/bogus".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(cmds.is_empty());
        assert!(
            s.transcript
                .iter()
                .any(|l| l.text.contains("unknown command"))
        );
    }

    #[test]
    fn esc_closes_overlay_first_then_clears_input_then_quits() {
        let mut s = fresh();
        s.overlay = Some(Overlay::Help);
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Esc)));
        assert!(cmds.is_empty(), "Esc on overlay shouldn't quit");
        assert!(s.overlay.is_none());

        // With text in the input, Esc clears it (doesn't quit).
        for c in "hello world".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Esc)));
        assert!(cmds.is_empty(), "first Esc with input should not quit");
        assert!(s.input.is_empty());
        assert_eq!(s.cursor, 0);
        assert!(!s.should_quit);

        // Now Esc with an empty input quits.
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Esc)));
        assert_eq!(cmds, vec![Command::Quit]);
        assert!(s.should_quit);
    }

    fn submit(s: &mut AppState, text: &str) {
        for c in text.chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
    }

    #[test]
    fn submitted_prompts_record_into_history_dedup_immediate_repeats() {
        let mut s = fresh();
        submit(&mut s, "ls");
        submit(&mut s, "ls");
        submit(&mut s, "ps");
        submit(&mut s, "ps");
        submit(&mut s, "ls");
        assert_eq!(s.history, vec!["ls".to_string(), "ps".into(), "ls".into()]);
    }

    #[test]
    fn up_recalls_previous_prompts_in_reverse_order() {
        let mut s = fresh();
        submit(&mut s, "first");
        submit(&mut s, "second");
        submit(&mut s, "third");
        // Type a draft we want preserved.
        for c in "draft".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        s.on_event(TuiEvent::Key(press(KeyCode::Up)));
        assert_eq!(s.input, "third");
        s.on_event(TuiEvent::Key(press(KeyCode::Up)));
        assert_eq!(s.input, "second");
        s.on_event(TuiEvent::Key(press(KeyCode::Up)));
        assert_eq!(s.input, "first");
        // At the top, more Up is a no-op.
        s.on_event(TuiEvent::Key(press(KeyCode::Up)));
        assert_eq!(s.input, "first");
    }

    #[test]
    fn down_after_up_walks_forward_then_restores_draft() {
        let mut s = fresh();
        submit(&mut s, "first");
        submit(&mut s, "second");
        for c in "wip".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        s.on_event(TuiEvent::Key(press(KeyCode::Up))); // second
        s.on_event(TuiEvent::Key(press(KeyCode::Up))); // first
        s.on_event(TuiEvent::Key(press(KeyCode::Down))); // second
        assert_eq!(s.input, "second");
        s.on_event(TuiEvent::Key(press(KeyCode::Down))); // draft restored
        assert_eq!(s.input, "wip");
        assert!(s.history_cursor.is_none());
    }

    #[test]
    fn editing_after_recall_drops_history_cursor() {
        let mut s = fresh();
        submit(&mut s, "echo hi");
        s.on_event(TuiEvent::Key(press(KeyCode::Up)));
        assert_eq!(s.input, "echo hi");
        // User edits the recalled line.
        s.on_event(TuiEvent::Key(press(KeyCode::Char('!'))));
        assert_eq!(s.input, "echo hi!");
        assert!(
            s.history_cursor.is_none(),
            "editing detaches from history cursor"
        );
        // Down should now be a no-op (no draft to restore either).
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        assert_eq!(s.input, "echo hi!");
    }

    fn open_doctor(s: &mut AppState, report: &str) {
        s.overlay = Some(Overlay::Doctor {
            report: report.to_string(),
            query: String::new(),
            scroll: 0,
        });
    }

    #[test]
    fn doctor_typing_appends_to_query_and_resets_scroll() {
        let mut s = fresh();
        open_doctor(&mut s, "irrelevant");
        // Pre-scroll so we can assert reset.
        if let Some(Overlay::Doctor { scroll, .. }) = &mut s.overlay {
            *scroll = 5;
        }
        for c in "API".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let Some(Overlay::Doctor { query, scroll, .. }) = &s.overlay else {
            panic!("doctor still open");
        };
        assert_eq!(query, "API");
        assert_eq!(*scroll, 0, "typing must reset scroll to top");
    }

    #[test]
    fn doctor_backspace_pops_query() {
        let mut s = fresh();
        open_doctor(&mut s, "x");
        for c in "abc".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        s.on_event(TuiEvent::Key(press(KeyCode::Backspace)));
        let Some(Overlay::Doctor { query, .. }) = &s.overlay else {
            panic!();
        };
        assert_eq!(query, "ab");
    }

    #[test]
    fn doctor_up_down_scrolls_body() {
        let mut s = fresh();
        open_doctor(&mut s, "many\nlines\nhere\n");
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        s.on_event(TuiEvent::Key(press(KeyCode::PageDown)));
        let Some(Overlay::Doctor { scroll, .. }) = s.overlay else {
            panic!();
        };
        assert_eq!(scroll, 12, "2 + 10 down steps");
    }

    #[test]
    fn doctor_esc_closes_without_quitting() {
        let mut s = fresh();
        open_doctor(&mut s, "anything");
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Esc)));
        assert!(cmds.is_empty());
        assert!(s.overlay.is_none());
        assert!(!s.should_quit);
    }

    #[test]
    fn doctor_overlay_doctor_event_preserves_user_query() {
        let mut s = fresh();
        open_doctor(&mut s, "placeholder");
        for c in "key".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        // Simulate the worker delivering the real report — should
        // swap `report` but keep the user's typed query intact.
        s.on_event(TuiEvent::OverlayDoctor("=== report ===\nfoo".into()));
        let Some(Overlay::Doctor { report, query, .. }) = &s.overlay else {
            panic!();
        };
        assert!(report.contains("=== report ==="));
        assert_eq!(query, "key");
    }

    #[test]
    fn slash_provider_with_no_profiles_logs_hint() {
        let mut s = fresh();
        for c in "/provider".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(cmds.is_empty());
        assert!(s.overlay.is_none(), "no overlay when no profiles");
        assert!(
            s.transcript
                .iter()
                .any(|l| l.text.contains("providers.toml")),
            "user gets a setup hint"
        );
    }

    #[test]
    fn slash_provider_opens_picker_when_profiles_loaded() {
        let providers = vec![
            api_key_profile("work", "openai/gpt-4o"),
            api_key_profile("personal", "anthropic/claude-sonnet-4-5"),
        ];
        let mut s = fresh_with_providers(providers);
        for c in "/provider".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(cmds.is_empty(), "opening the picker shouldn't dispatch");
        assert!(matches!(s.overlay, Some(Overlay::ProviderPicker { .. })));
    }

    #[test]
    fn provider_picker_enter_on_api_key_dispatches_apply() {
        let providers = vec![
            api_key_profile("work", "openai/gpt-4o"),
            api_key_profile("personal", "anthropic/claude-sonnet-4-5"),
        ];
        let mut s = fresh_with_providers(providers);
        s.overlay = Some(Overlay::ProviderPicker { focused: 1 });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert_eq!(cmds, vec![Command::ApplyProvider(1)]);
        assert!(s.overlay.is_none(), "picker closes on apply");
    }

    #[test]
    fn provider_picker_enter_on_oauth_surfaces_phase_2_hint() {
        let providers = vec![oauth_profile("claude-pro")];
        let mut s = fresh_with_providers(providers);
        s.overlay = Some(Overlay::ProviderPicker { focused: 0 });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(cmds.is_empty(), "OAuth must not dispatch ApplyProvider");
        assert!(s.overlay.is_none(), "picker still closes");
        assert!(
            s.transcript
                .iter()
                .any(|l| l.text.contains("OAuth") && l.text.contains("login flow not yet wired")),
            "user is told why nothing happened"
        );
    }

    #[test]
    fn provider_applied_event_updates_active_marker_and_model() {
        let providers = vec![
            api_key_profile("work", "openai/gpt-4o"),
            api_key_profile("personal", "anthropic/claude-sonnet-4-5"),
        ];
        let mut s = fresh_with_providers(providers);
        s.on_event(TuiEvent::ProviderApplied {
            profile: "personal".into(),
            model: "anthropic/claude-sonnet-4-5".into(),
            cost: Cost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
                total: 0.0,
            },
        });
        assert_eq!(s.current_provider_idx, Some(1));
        assert_eq!(s.model_id, "anthropic/claude-sonnet-4-5");
        // Pricing carried by the event must reach `model_cost` so the
        // footer chip reflects the new model on the next frame.
        assert_eq!(s.model_cost.input, 3.0);
        assert_eq!(s.model_cost.cache_read, 0.3);
    }

    #[test]
    fn down_with_no_recall_is_a_noop() {
        let mut s = fresh();
        submit(&mut s, "foo");
        // Fresh input pane, no Up pressed yet.
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        assert!(s.input.is_empty());
    }

    #[test]
    fn ctrl_c_aborts_when_streaming_and_quits_when_idle() {
        let mut s = fresh();

        // While streaming, Ctrl-C aborts the current turn — does NOT
        // quit (so the user can redirect the agent without losing
        // their session).
        s.streaming = true;
        let cmds = s.on_event(TuiEvent::Key(ctrl(KeyCode::Char('c'))));
        assert_eq!(cmds, vec![Command::AbortCurrentTurn]);
        assert!(!s.should_quit, "abort while streaming should not quit");

        // Idle Ctrl-C is a hard exit. (Necessary under raw mode where
        // the kernel won't deliver SIGINT for us.)
        let mut s = fresh();
        let cmds = s.on_event(TuiEvent::Key(ctrl(KeyCode::Char('c'))));
        assert_eq!(cmds, vec![Command::Quit]);
        assert!(s.should_quit);
    }

    #[test]
    fn tab_without_palette_does_not_toggle_focus() {
        // Regression guard for the "input goes gray and stops
        // responding" bug: Tab used to flip focus to Transcript,
        // after which typed chars were dropped. Now Tab is inert
        // unless the palette is open (in which case it completes
        // the selected command).
        let mut s = fresh();
        assert_eq!(s.focus, Focus::Input);
        s.on_event(TuiEvent::Key(press(KeyCode::Tab)));
        assert_eq!(s.focus, Focus::Input, "Tab must NOT strand focus");
        // Typing still works after Tab.
        s.on_event(TuiEvent::Key(press(KeyCode::Char('a'))));
        assert_eq!(s.input, "a");
    }

    #[test]
    fn tab_completes_palette_selection_without_submitting() {
        let mut s = fresh();
        // Type "/the" → only /theme matches → focused is 0.
        for c in "/the".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Tab)));
        assert!(cmds.is_empty(), "Tab must not submit");
        assert_eq!(s.input, "/theme");
        assert_eq!(s.cursor, "/theme".len());
        // The palette is still open and the input still starts with /,
        // so a follow-up Enter will dispatch normally.
        assert!(s.palette_visible());
        // No transcript change — Tab is purely a complete, not a submit.
        assert!(
            !s.transcript
                .iter()
                .any(|l| l.kind == TranscriptKind::UserPrompt),
            "Tab must not log a user prompt"
        );
    }

    #[test]
    fn page_up_freezes_view_then_end_reengages_tail_follow() {
        let mut s = fresh();
        // Simulate a render with 50 wrapped rows in a 20-row pane.
        s.render_metrics.set(RenderMetrics {
            total_rows: 50,
            visible_rows: 20,
            ..Default::default()
        });
        assert!(s.follow_bottom, "default is tail follow");

        s.on_event(TuiEvent::Key(press(KeyCode::PageUp)));
        assert!(!s.follow_bottom, "PgUp freezes the view");
        // The anchor jumped to the live bottom (30) and stepped 10 back.
        assert_eq!(s.scroll_offset, 20);

        s.on_event(TuiEvent::Key(press(KeyCode::PageUp)));
        assert_eq!(s.scroll_offset, 10);

        // PgDn walks forward 10.
        s.on_event(TuiEvent::Key(press(KeyCode::PageDown)));
        assert_eq!(s.scroll_offset, 20);

        // PgDn that catches up to live bottom re-engages tail follow.
        s.on_event(TuiEvent::Key(press(KeyCode::PageDown)));
        assert!(s.follow_bottom, "catching up to bottom re-engages follow");

        // End from anywhere returns to tail.
        s.on_event(TuiEvent::Key(press(KeyCode::Home)));
        assert!(!s.follow_bottom);
        assert_eq!(s.scroll_offset, 0);
        s.on_event(TuiEvent::Key(press(KeyCode::End)));
        assert!(s.follow_bottom);
    }

    #[test]
    fn frozen_view_does_not_drift_when_new_content_arrives() {
        // The bug this guards against: when scrolled up reading
        // history, new assistant chunks shouldn't push the user's
        // viewport.
        let mut s = fresh();
        s.render_metrics.set(RenderMetrics {
            total_rows: 100,
            visible_rows: 20,
            ..Default::default()
        });
        s.on_event(TuiEvent::Key(press(KeyCode::PageUp)));
        let frozen = s.scroll_offset;
        // New content arrives — total_rows grows.
        s.render_metrics.set(RenderMetrics {
            total_rows: 130,
            visible_rows: 20,
            ..Default::default()
        });
        // No key event — scroll_offset is unchanged. Ui renders
        // from this same anchor regardless of where the new bottom is.
        assert_eq!(s.scroll_offset, frozen);
        assert!(!s.follow_bottom);
    }

    #[test]
    fn push_does_not_drift_scroll_when_view_is_frozen() {
        // Regression: `push()` used to reset `scroll_offset = 0`
        // whenever `focus == Input`, which yanked the user's
        // viewport to the top of the buffer every time a new
        // assistant chunk / tool result / info line arrived. The
        // fix is to leave `scroll_offset` alone — `follow_bottom`
        // already does the auto-scroll work when needed.
        let mut s = fresh();
        s.render_metrics.set(RenderMetrics {
            total_rows: 100,
            visible_rows: 20,
            ..Default::default()
        });
        s.on_event(TuiEvent::Key(press(KeyCode::PageUp)));
        let frozen = s.scroll_offset;
        assert!(!s.follow_bottom);
        // Now exercise the path that used to corrupt the anchor:
        // various callers funnel through `push`.
        s.push(TranscriptKind::Info, "(some chrome line)".into());
        s.push(TranscriptKind::AssistantText, "fresh chunk".into());
        s.push(TranscriptKind::ToolCallStart, "(tool: read)".into());
        assert_eq!(s.scroll_offset, frozen, "frozen view drifted on push");
        assert!(!s.follow_bottom, "push must not re-engage tail follow");
    }

    #[test]
    fn push_while_following_bottom_keeps_follow_bottom_engaged() {
        // Sister of the prior test: when the user *is* at the tail,
        // pushing must NOT flip them off tail-follow either. (push
        // never touches follow_bottom in either direction.)
        let mut s = fresh();
        assert!(s.follow_bottom, "default is tail follow");
        s.push(TranscriptKind::Info, "ok".into());
        assert!(s.follow_bottom);
    }

    fn mk_line(kind: TranscriptKind, text: &str) -> TranscriptLine {
        TranscriptLine {
            kind,
            text: text.into(),
        }
    }

    #[test]
    fn build_blocks_groups_consecutive_thinking_lines() {
        let lines = vec![
            mk_line(TranscriptKind::UserPrompt, "hi"),
            mk_line(TranscriptKind::ThinkingText, "a"),
            mk_line(TranscriptKind::ThinkingText, "b"),
            mk_line(TranscriptKind::ThinkingText, "c"),
            mk_line(TranscriptKind::AssistantText, "ok"),
        ];
        let blocks = build_transcript_blocks(&lines);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].kind, BlockKind::Plain);
        assert_eq!(blocks[1].kind, BlockKind::Thinking);
        assert_eq!(blocks[1].first_line, 1);
        assert_eq!(blocks[1].last_line, 3);
        assert_eq!(blocks[1].line_count(), 3);
        assert_eq!(blocks[2].kind, BlockKind::Plain);
    }

    #[test]
    fn build_blocks_pairs_tool_call_start_and_end() {
        let lines = vec![
            mk_line(TranscriptKind::AssistantText, "I'll read"),
            mk_line(TranscriptKind::ToolCallStart, "→ read(...)"),
            mk_line(TranscriptKind::ToolCallEnd, "← read ok"),
            mk_line(TranscriptKind::AssistantText, "done"),
        ];
        let blocks = build_transcript_blocks(&lines);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[1].kind, BlockKind::ToolCall);
        assert_eq!(blocks[1].first_line, 1);
        assert_eq!(blocks[1].last_line, 2);
    }

    #[test]
    fn build_blocks_in_flight_tool_call_runs_to_buffer_tail() {
        // Worker hasn't emitted ToolCallEnd yet — the block
        // stretches to the last available line so the user sees
        // the call's pending state as a coherent group.
        let lines = vec![
            mk_line(TranscriptKind::ToolCallStart, "→ bash(...)"),
            mk_line(TranscriptKind::Info, "(running...)"),
        ];
        let blocks = build_transcript_blocks(&lines);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::ToolCall);
        assert_eq!(blocks[0].first_line, 0);
        assert_eq!(blocks[0].last_line, 1);
    }

    #[test]
    fn edit_diff_preview_uses_unified_context() {
        let args = serde_json::json!({
            "old": "fn run() {\n    old_call();\n}\n",
            "new": "fn run() {\n    new_call();\n}\n",
        });

        let text = edit_diff_preview(&args, 20).join("\n");

        assert!(text.contains("@@ edit @@"));
        assert!(text.contains(" fn run()"));
        assert!(text.contains("-    old_call();"));
        assert!(text.contains("+    new_call();"));
    }

    #[test]
    fn tool_diff_preview_prefers_git_diff_details() {
        let result = grain_agent_core::AgentToolResult {
            content: vec![],
            details: serde_json::json!({
                "uiDiff": "diff --git a/src/lib.rs b/src/lib.rs\nindex abc..def 100644\n@@ -1 +1 @@\n-old\n+new\n"
            }),
            terminate: None,
        };

        let lines = tool_diff_preview("edit", &serde_json::json!({}), &result, None, 8);
        let joined = lines.join("\n");

        assert!(!joined.contains("diff --git"));
        assert!(!joined.contains("index abc"));
        assert!(joined.contains("@@ -1 +1 @@"));
        assert!(joined.contains("-old"));
        assert!(joined.contains("+new"));
    }

    #[test]
    fn tool_diff_preview_uses_git_diff_for_tracked_files() {
        let tmp = tempfile::tempdir().unwrap();
        if std::process::Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .arg("init")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }
        std::fs::write(
            tmp.path().join("file.rs"),
            "fn value() -> i32 {\n    1\n}\n",
        )
        .unwrap();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .arg("add")
            .arg("file.rs")
            .output();
        std::fs::write(
            tmp.path().join("file.rs"),
            "fn value() -> i32 {\n    2\n}\n",
        )
        .unwrap();

        let result = grain_agent_core::AgentToolResult {
            content: vec![],
            details: serde_json::json!({ "path": "file.rs" }),
            terminate: None,
        };
        let args = serde_json::json!({ "path": "file.rs" });

        let joined = tool_diff_preview("write", &args, &result, tmp.path().to_str(), 20).join("\n");

        assert!(joined.contains("@@"));
        assert!(joined.contains("-    1"));
        assert!(joined.contains("+    2"));
    }

    #[test]
    fn write_created_file_gets_addition_preview_without_git() {
        let result = grain_agent_core::AgentToolResult {
            content: vec![],
            details: serde_json::json!({
                "path": "src/main.rs",
                "created": true,
            }),
            terminate: None,
        };
        let args = serde_json::json!({
            "path": "src/main.rs",
            "content": "fn main() {\n    println!(\"hi\");\n}\n",
        });

        let joined = tool_diff_preview("write", &args, &result, None, 10).join("\n");

        assert!(joined.contains("@@ new file: src/main.rs @@"));
        assert!(joined.contains("+fn main()"));
        assert!(joined.contains("+    println!"));
    }

    #[test]
    fn fold_defaults_collapse_tool_calls_and_thinking() {
        let s = fresh();
        let tool_block = TranscriptBlock {
            first_line: 0,
            last_line: 1,
            kind: BlockKind::ToolCall,
        };
        let thinking_block = TranscriptBlock {
            first_line: 5,
            last_line: 9,
            kind: BlockKind::Thinking,
        };
        let plain_block = TranscriptBlock {
            first_line: 3,
            last_line: 3,
            kind: BlockKind::Plain,
        };
        assert!(!s.is_block_expanded(&tool_block));
        assert!(!s.is_block_expanded(&thinking_block));
        assert!(s.is_block_expanded(&plain_block));
    }

    #[test]
    fn toggle_fold_round_trips() {
        let mut s = fresh();
        let block = TranscriptBlock {
            first_line: 0,
            last_line: 1,
            kind: BlockKind::ToolCall,
        };
        assert!(!s.is_block_expanded(&block));
        s.toggle_block_fold(&block);
        assert!(s.is_block_expanded(&block));
        s.toggle_block_fold(&block);
        assert!(!s.is_block_expanded(&block));
    }

    #[test]
    fn toggle_is_noop_on_plain_blocks() {
        let mut s = fresh();
        let plain = TranscriptBlock {
            first_line: 0,
            last_line: 0,
            kind: BlockKind::Plain,
        };
        s.toggle_block_fold(&plain);
        assert!(s.fold_overrides.is_empty());
    }

    #[test]
    fn cursor_navigation_initializes_and_walks_foldable_blocks() {
        let mut s = fresh();
        s.push(TranscriptKind::AssistantText, "intro".into());
        s.push(TranscriptKind::ThinkingText, "t1".into());
        s.push(TranscriptKind::ThinkingText, "t2".into());
        s.push(TranscriptKind::ToolCallStart, "→ read".into());
        s.push(TranscriptKind::ToolCallEnd, "← read ok".into());
        // 3 blocks: Plain(intro), Thinking(t1/t2), ToolCall(start/end).
        // First Ctrl-K from None lands on the *last* foldable
        // block (ToolCall).
        let _ = s.on_key(ctrl(KeyCode::Char('k')));
        let cursor1 = s.transcript_cursor;
        assert!(cursor1.is_some());
        // Ctrl-K again steps to the previous foldable block
        // (Thinking).
        let _ = s.on_key(ctrl(KeyCode::Char('k')));
        let cursor2 = s.transcript_cursor.expect("cursor still set");
        assert_ne!(Some(cursor2), cursor1);
        // Ctrl-J steps back forward to the ToolCall block.
        let _ = s.on_key(ctrl(KeyCode::Char('j')));
        assert_eq!(s.transcript_cursor, cursor1);
    }

    #[test]
    fn space_toggles_focused_block_when_input_empty() {
        let mut s = fresh();
        s.push(TranscriptKind::ToolCallStart, "→ read".into());
        s.push(TranscriptKind::ToolCallEnd, "← ok".into());
        // Establish cursor.
        let _ = s.on_key(ctrl(KeyCode::Char('k')));
        let block_id = s.transcript_cursor.unwrap();
        let blocks = build_transcript_blocks(&s.transcript);
        let block = blocks.iter().find(|b| b.id() == block_id).unwrap();
        let initial = s.is_block_expanded(block);
        // Plain Space.
        let _ = s.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let blocks = build_transcript_blocks(&s.transcript);
        let block = blocks.iter().find(|b| b.id() == block_id).unwrap();
        assert_ne!(s.is_block_expanded(block), initial);
    }

    #[test]
    fn space_with_nonempty_input_inserts_into_input() {
        let mut s = fresh();
        s.push(TranscriptKind::ToolCallStart, "→ read".into());
        s.push(TranscriptKind::ToolCallEnd, "← ok".into());
        let _ = s.on_key(ctrl(KeyCode::Char('k')));
        // User is in cursor mode but starts typing — Space lands
        // in the input buffer, not as a fold toggle.
        s.input = "ab".into();
        s.cursor = s.input.len();
        let block_id = s.transcript_cursor.unwrap();
        let blocks = build_transcript_blocks(&s.transcript);
        let block = blocks.iter().find(|b| b.id() == block_id).unwrap();
        let before = s.is_block_expanded(block);
        let _ = s.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let blocks = build_transcript_blocks(&s.transcript);
        let block = blocks.iter().find(|b| b.id() == block_id).unwrap();
        assert_eq!(s.is_block_expanded(block), before, "fold state untouched");
        assert!(s.input.contains(' '), "space went into input buffer");
    }

    #[test]
    fn esc_clears_cursor_before_clearing_input() {
        let mut s = fresh();
        s.push(TranscriptKind::ToolCallStart, "→ read".into());
        s.push(TranscriptKind::ToolCallEnd, "← ok".into());
        let _ = s.on_key(ctrl(KeyCode::Char('k')));
        assert!(s.transcript_cursor.is_some());
        s.input = "draft".into();
        let _ = s.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(s.transcript_cursor.is_none(), "cursor cleared first");
        assert_eq!(s.input, "draft", "input preserved on first Esc");
        // Second Esc proceeds to the legacy clear-input path.
        let _ = s.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(s.input.is_empty());
    }

    #[test]
    fn navigation_is_safe_on_empty_transcript() {
        let mut s = fresh();
        // No transcript content yet.
        let _ = s.on_key(ctrl(KeyCode::Char('k')));
        assert!(s.transcript_cursor.is_none());
        let _ = s.on_key(ctrl(KeyCode::Char('j')));
        assert!(s.transcript_cursor.is_none());
    }

    #[test]
    fn mouse_click_on_chrome_row_toggles_fold() {
        // Seed the transcript with a tool-call block so it has a
        // chrome row, then fake the post-render state the mouse
        // path expects: a `rendered_rows` snapshot + a
        // transcript_area covering the click.
        let mut s = fresh();
        s.push(TranscriptKind::ToolCallStart, "→ read".into());
        s.push(TranscriptKind::ToolCallEnd, "← ok".into());
        // `fresh()` pushes a Plain boot-banner line as
        // transcript[0]; the tool-call block lands later. Find it
        // explicitly instead of assuming `[0]`.
        let blocks = build_transcript_blocks(&s.transcript);
        let block_id = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolCall)
            .map(|b| b.id())
            .expect("tool-call block present");
        s.transcript_area.set(Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
        });
        s.rendered_rows.replace(vec![RenderedRow {
            text: " ▸ tool: read (2 lines)".into(),
            kind: TranscriptKind::ToolCallStart,
            chrome_for_block: Some(block_id),
            md_spans: None,
        }]);
        let pre_blocks = build_transcript_blocks(&s.transcript);
        let pre = pre_blocks.iter().find(|b| b.id() == block_id).unwrap();
        let initial_expanded = s.is_block_expanded(pre);
        // Click row 0.
        let _ = s.on_event(TuiEvent::MouseDown { row: 0, col: 5 });
        let post_blocks = build_transcript_blocks(&s.transcript);
        let post = post_blocks.iter().find(|b| b.id() == block_id).unwrap();
        assert_ne!(s.is_block_expanded(post), initial_expanded);
        // Click should also park the cursor on the clicked block.
        assert_eq!(s.transcript_cursor, Some(block_id));
        // And NOT start a selection.
        assert!(s.selection.is_none());
    }

    #[test]
    fn mouse_click_on_body_row_still_starts_selection() {
        // A click in the **expanded body** of a block (a row with
        // `chrome_for_block: None`) keeps the legacy drag-to-copy
        // behavior — fold-click only fires on chrome rows.
        let mut s = fresh();
        s.push(TranscriptKind::AssistantText, "some body text".into());
        s.transcript_area.set(Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
        });
        s.rendered_rows.replace(vec![RenderedRow {
            text: "  some body text".into(),
            kind: TranscriptKind::AssistantText,
            chrome_for_block: None,
            md_spans: None,
        }]);
        let _ = s.on_event(TuiEvent::MouseDown { row: 0, col: 5 });
        assert!(s.selection.is_some());
    }

    #[test]
    fn text_delta_appends_to_last_assistant_line() {
        let mut s = fresh();
        use grain_agent_core::{
            AssistantContent, AssistantMessage, StopReason, TextContent, Usage,
        };

        let make = |text: &str| AssistantMessage {
            content: vec![AssistantContent::Text(TextContent { text: text.into() })],
            api: "x".into(),
            provider: "x".into(),
            model: "x".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };

        // Each TextDelta carries the *canonical* partial — the handler
        // pulls the full assistant text from `partial.content`.
        s.on_event(TuiEvent::Agent(Box::new(AgentEvent::MessageUpdate {
            message: make("Hello, "),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                partial: make("Hello, "),
                content_index: 0,
                delta: "Hello, ".into(),
            },
        })));
        s.on_event(TuiEvent::Agent(Box::new(AgentEvent::MessageUpdate {
            message: make("Hello, world!"),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                partial: make("Hello, world!"),
                content_index: 0,
                delta: "world!".into(),
            },
        })));

        let last = s.transcript.last().expect("text line");
        assert_eq!(last.kind, TranscriptKind::AssistantText);
        assert_eq!(last.text, "Hello, world!");
    }

    #[test]
    fn tool_call_events_increment_then_decrement_pending() {
        let mut s = fresh();
        s.on_event(TuiEvent::Agent(Box::new(AgentEvent::ToolExecutionStart {
            tool_call_id: "1".into(),
            tool_name: "read".into(),
            args: serde_json::json!({ "path": "x" }),
        })));
        assert_eq!(s.pending_tool_calls, 1);
        s.on_event(TuiEvent::Agent(Box::new(AgentEvent::ToolExecutionEnd {
            tool_call_id: "1".into(),
            tool_name: "read".into(),
            result: grain_agent_core::AgentToolResult {
                content: vec![UserContent::text("ok")],
                details: serde_json::Value::Null,
                terminate: None,
            },
            is_error: false,
        })));
        assert_eq!(s.pending_tool_calls, 0);
        assert!(
            s.transcript
                .iter()
                .any(|l| l.kind == TranscriptKind::ToolCallStart)
        );
        assert!(
            s.transcript
                .iter()
                .any(|l| l.kind == TranscriptKind::ToolCallEnd)
        );
    }

    #[test]
    fn slash_theme_opens_picker_focused_on_current() {
        let mut s = fresh();
        s.current_theme_idx = 2;
        for c in "/theme".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(cmds.is_empty());
        assert!(
            matches!(s.overlay, Some(Overlay::ThemePicker { focused }) if focused == 2),
            "picker should open focused on current theme"
        );
    }

    #[test]
    fn theme_picker_arrows_clamp_within_bounds() {
        let mut s = fresh();
        s.overlay = Some(Overlay::ThemePicker { focused: 0 });
        // Up at top stays at 0
        s.on_event(TuiEvent::Key(press(KeyCode::Up)));
        let Some(Overlay::ThemePicker { focused }) = s.overlay else {
            panic!("picker should still be open");
        };
        assert_eq!(focused, 0);

        // Down moves
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        let Some(Overlay::ThemePicker { focused }) = s.overlay else {
            panic!();
        };
        assert_eq!(focused, 1);

        // End jumps to last
        s.on_event(TuiEvent::Key(press(KeyCode::End)));
        let Some(Overlay::ThemePicker { focused }) = s.overlay else {
            panic!();
        };
        assert_eq!(focused, s.themes.len() - 1);

        // Down at bottom stays at last
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        let Some(Overlay::ThemePicker { focused }) = s.overlay else {
            panic!();
        };
        assert_eq!(focused, s.themes.len() - 1);
    }

    #[test]
    fn theme_picker_enter_applies_and_closes() {
        let mut s = fresh();
        let target = 3;
        let target_name = s.themes[target].name.clone();
        s.overlay = Some(Overlay::ThemePicker { focused: target });
        s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(s.overlay.is_none(), "overlay should close on apply");
        assert_eq!(s.current_theme_idx, target);
        assert_eq!(s.theme().name, target_name);
        assert!(s.transcript.iter().any(|l| l.text.contains(&target_name)));
    }

    #[test]
    fn theme_picker_esc_cancels_without_applying() {
        let mut s = fresh();
        let original = s.current_theme_idx;
        s.overlay = Some(Overlay::ThemePicker { focused: 5 });
        s.on_event(TuiEvent::Key(press(KeyCode::Esc)));
        assert!(s.overlay.is_none());
        assert_eq!(s.current_theme_idx, original, "Esc must not apply");
    }

    #[test]
    fn palette_visible_only_when_input_starts_with_slash_and_focused() {
        let mut s = fresh();
        assert!(!s.palette_visible(), "empty input is not a slash prefix");
        s.on_event(TuiEvent::Key(press(KeyCode::Char('/'))));
        assert!(s.palette_visible());
        s.focus = Focus::Transcript;
        assert!(
            !s.palette_visible(),
            "hidden when input pane is not focused"
        );
        s.focus = Focus::Input;
        s.overlay = Some(Overlay::Help);
        assert!(!s.palette_visible(), "hidden when an overlay covers UI");
    }

    #[test]
    fn palette_matches_narrow_as_user_types() {
        let mut s = fresh();
        for c in "/".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let initial = s.palette_matches();
        assert_eq!(
            initial.len(),
            SLASH_CATALOG.len(),
            "bare slash matches every command (no skills loaded)"
        );

        for c in "the".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let narrowed = s.palette_matches();
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].trigger, "/theme");
        assert!(matches!(narrowed[0].action, PaletteAction::DispatchSlash));
    }

    // ---- Selection primitives -----------------------------------

    fn rrows(items: &[(&str, TranscriptKind)]) -> Vec<RenderedRow> {
        items
            .iter()
            .map(|(t, k)| RenderedRow {
                text: (*t).to_string(),
                kind: *k,
                chrome_for_block: None,
                md_spans: None,
            })
            .collect()
    }

    #[test]
    fn selection_normalized_orders_by_lexicographic_row_col() {
        let s = Selection {
            anchor: (3, 4),
            active: (1, 9),
            dragging: false,
        };
        // (1,9) < (3,4) → min comes from active.
        assert_eq!(s.normalized(), (1, 3, 9, 4));
    }

    #[test]
    fn col_range_returns_none_outside_selected_rows() {
        let s = Selection {
            anchor: (2, 0),
            active: (4, 5),
            dragging: false,
        };
        assert!(s.col_range_for_row(1, 100).is_none());
        assert!(s.col_range_for_row(5, 100).is_none());
    }

    #[test]
    fn col_range_single_row_uses_min_max_col() {
        let s = Selection {
            anchor: (2, 8),
            active: (2, 3),
            dragging: false,
        };
        assert_eq!(s.col_range_for_row(2, 50), Some((3, 8)));
    }

    #[test]
    fn col_range_multirow_first_row_extends_to_end() {
        let s = Selection {
            anchor: (2, 4),
            active: (5, 10),
            dragging: false,
        };
        assert_eq!(s.col_range_for_row(2, 80), Some((4, 80)));
        assert_eq!(s.col_range_for_row(3, 80), Some((0, 80)));
        assert_eq!(s.col_range_for_row(5, 80), Some((0, 10)));
    }

    #[test]
    fn col_range_empty_selection_returns_none() {
        let s = Selection {
            anchor: (2, 5),
            active: (2, 5),
            dragging: false,
        };
        assert!(s.col_range_for_row(2, 50).is_none());
    }

    #[test]
    fn extract_selection_single_row_substring() {
        let rendered = rrows(&[("hello world", TranscriptKind::AssistantText)]);
        let s = Selection {
            anchor: (0, 6),
            active: (0, 11),
            dragging: false,
        };
        assert_eq!(extract_selection(&rendered, s), "world");
    }

    #[test]
    fn extract_selection_multirow_joins_with_newlines() {
        let rendered = rrows(&[
            ("first line", TranscriptKind::AssistantText),
            ("middle row", TranscriptKind::AssistantText),
            ("last row", TranscriptKind::AssistantText),
        ]);
        let s = Selection {
            anchor: (0, 6),
            active: (2, 4),
            dragging: false,
        };
        // Row 0: "line" (from col 6 to end "first line".len()=10)
        // Row 1: "middle row" whole
        // Row 2: "last" (cols 0..4)
        assert_eq!(extract_selection(&rendered, s), "line\nmiddle row\nlast");
    }

    #[test]
    fn visual_col_zero_returns_zero_byte() {
        assert_eq!(visual_col_to_byte_idx("hello", 0), 0);
        assert_eq!(visual_col_to_byte_idx("中文", 0), 0);
    }

    #[test]
    fn visual_col_ascii_one_to_one() {
        let s = "hello";
        assert_eq!(visual_col_to_byte_idx(s, 3), 3);
        assert_eq!(visual_col_to_byte_idx(s, 5), 5);
        // past end clamps to len
        assert_eq!(visual_col_to_byte_idx(s, 999), 5);
    }

    #[test]
    fn visual_col_wide_chars_walk_two_per_glyph() {
        // 3 Han chars: each 1 display col-pair wide (= 2), 3 bytes UTF-8 (= 3 each).
        let s = "中文测";
        // After 0 visual cols we're at byte 0.
        assert_eq!(visual_col_to_byte_idx(s, 0), 0);
        // After 2 visual cols (one full Han), we're past the first 3 bytes.
        assert_eq!(visual_col_to_byte_idx(s, 2), 3);
        // After 4 visual cols (two Han), byte 6.
        assert_eq!(visual_col_to_byte_idx(s, 4), 6);
        // After 6 visual cols (three Han = whole string), byte 9.
        assert_eq!(visual_col_to_byte_idx(s, 6), 9);
        // Anything past the rendered width clamps to len.
        assert_eq!(visual_col_to_byte_idx(s, 100), 9);
    }

    #[test]
    fn visual_col_mid_glyph_snaps_to_glyph_start() {
        // Mouse-click landed in the middle of a wide char: snap to
        // the start of that glyph so the selection covers it.
        let s = "中a";
        assert_eq!(visual_col_to_byte_idx(s, 0), 0); // left half of 中
        assert_eq!(visual_col_to_byte_idx(s, 1), 0); // right half of 中
        assert_eq!(visual_col_to_byte_idx(s, 2), 3); // 'a' starts
        assert_eq!(visual_col_to_byte_idx(s, 3), 4); // past 'a' → len
    }

    #[test]
    fn extract_selection_handles_multibyte_chars() {
        // 4 Han chars × 3 bytes each = 12 bytes total.
        let rendered = rrows(&[("中文测试", TranscriptKind::AssistantText)]);
        // Ask for byte-range that lands in the middle of a char;
        // clamp must keep the slice valid (and shrink to the boundary).
        let s = Selection {
            anchor: (0, 2),
            active: (0, 7),
            dragging: false,
        };
        let out = extract_selection(&rendered, s);
        // Must be valid UTF-8 (i.e. not panic).
        assert!(out.is_empty() || out.chars().count() > 0);
    }

    #[test]
    fn mouse_down_starts_selection_when_inside_transcript() {
        let mut s = fresh();
        // Pretend the transcript pane sits at (x=0, y=1) with size 80x20
        // and that one row is rendered.
        s.transcript_area.set(Rect::new(0, 1, 80, 20));
        *s.rendered_rows.borrow_mut() = rrows(&[("abcdef", TranscriptKind::AssistantText)]);
        s.on_event(TuiEvent::MouseDown { row: 1, col: 2 });
        let sel = s.selection.expect("selection must be set");
        assert!(sel.dragging);
        assert_eq!(sel.anchor, sel.active);
        assert_eq!(sel.anchor, (0, 2));
    }

    #[test]
    fn mouse_drag_extends_active_only_while_dragging() {
        let mut s = fresh();
        s.transcript_area.set(Rect::new(0, 0, 80, 20));
        *s.rendered_rows.borrow_mut() = rrows(&[
            ("row0", TranscriptKind::AssistantText),
            ("row1 longer text", TranscriptKind::AssistantText),
        ]);
        s.on_event(TuiEvent::MouseDown { row: 0, col: 1 });
        s.on_event(TuiEvent::MouseDrag { row: 1, col: 10 });
        let sel = s.selection.expect("present");
        assert_eq!(sel.anchor, (0, 1));
        assert_eq!(sel.active, (1, 10));
        assert!(sel.dragging);
    }

    #[test]
    fn mouse_drag_outside_transcript_is_ignored() {
        let mut s = fresh();
        s.transcript_area.set(Rect::new(0, 0, 80, 20));
        *s.rendered_rows.borrow_mut() = rrows(&[("row0", TranscriptKind::AssistantText)]);
        s.on_event(TuiEvent::MouseDown { row: 0, col: 1 });
        // y=99 is past the bottom of the 20-row area.
        s.on_event(TuiEvent::MouseDrag { row: 99, col: 50 });
        let sel = s.selection.expect("present");
        // Active wasn't updated.
        assert_eq!(sel.active, (0, 1));
    }

    #[test]
    fn mouse_up_with_empty_drag_clears_selection() {
        let mut s = fresh();
        s.transcript_area.set(Rect::new(0, 0, 80, 20));
        *s.rendered_rows.borrow_mut() = rrows(&[("row0", TranscriptKind::AssistantText)]);
        s.on_event(TuiEvent::MouseDown { row: 0, col: 1 });
        s.on_event(TuiEvent::MouseUp);
        assert!(s.selection.is_none(), "click-without-drag clears selection");
    }

    #[test]
    fn f6_toggles_mouse_capture_flag() {
        let mut s = fresh();
        assert!(s.mouse_capture_on, "default is capture-on");
        s.on_event(TuiEvent::Key(press(KeyCode::F(6))));
        assert!(!s.mouse_capture_on);
        s.on_event(TuiEvent::Key(press(KeyCode::F(6))));
        assert!(s.mouse_capture_on);
    }

    #[test]
    fn f6_pushes_info_line_into_transcript() {
        let mut s = fresh();
        let before = s.transcript.len();
        s.on_event(TuiEvent::Key(press(KeyCode::F(6))));
        assert!(s.transcript.len() > before);
        assert_eq!(s.transcript.last().unwrap().kind, TranscriptKind::Info);
    }

    // ---- /log overlay -------------------------------------------

    #[test]
    fn request_logged_event_pushes_into_ring_and_caps_at_max() {
        let mut s = fresh();
        for i in 0..(MAX_REQUEST_LOG + 5) {
            s.on_event(TuiEvent::RequestLogged {
                body: format!("entry {i}"),
            });
        }
        assert_eq!(s.request_log.len(), MAX_REQUEST_LOG);
        // Oldest dropped → first survivor starts at index 5.
        assert!(s.request_log.front().unwrap().contains("entry 5"));
        // Most recent at the back.
        assert!(s.request_log.back().unwrap().contains("entry 24"));
    }

    #[test]
    fn request_logged_event_truncates_large_entries() {
        let mut s = fresh();
        let body = "记".repeat((MAX_REQUEST_LOG_ENTRY_BYTES / "记".len()) + 1000);
        let original_len = body.len();

        s.on_event(TuiEvent::RequestLogged { body });

        let stored = s.request_log.back().unwrap();
        assert!(stored.contains("Request log truncated"));
        assert!(stored.len() <= MAX_REQUEST_LOG_ENTRY_BYTES);
        assert!(stored.len() < original_len);
    }

    #[test]
    fn slash_log_with_empty_buffer_logs_hint_not_overlay() {
        let mut s = fresh();
        s.input = "/log".into();
        s.cursor = s.input.len();
        let _ = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(s.overlay.is_none());
        assert!(
            s.transcript
                .iter()
                .any(|l| l.kind == TranscriptKind::Info && l.text.contains("--debug-log"))
        );
    }

    #[test]
    fn slash_log_opens_overlay_when_buffer_has_entries() {
        let mut s = fresh();
        s.on_event(TuiEvent::RequestLogged {
            body: "{\"k\":1}".into(),
        });
        s.input = "/log".into();
        s.cursor = s.input.len();
        let _ = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(matches!(s.overlay, Some(Overlay::Log { .. })));
    }

    #[test]
    fn wheel_scrolls_log_overlay_when_open() {
        let mut s = fresh();
        s.overlay = Some(Overlay::Log { scroll: 5 });
        s.on_event(TuiEvent::ScrollUp { amount: 3 });
        if let Some(Overlay::Log { scroll }) = s.overlay {
            assert_eq!(scroll, 2);
        } else {
            panic!("overlay should still be Log");
        }
        s.on_event(TuiEvent::ScrollDown { amount: 10 });
        if let Some(Overlay::Log { scroll }) = s.overlay {
            assert_eq!(scroll, 12);
        } else {
            panic!("overlay should still be Log");
        }
    }

    // ---- /resume overlay ---------------------------------------

    fn fake_session_meta(id: &str, secs_ago: u64) -> grain_ai_agent_headless::SessionMeta {
        grain_ai_agent_headless::SessionMeta {
            id: id.into(),
            path: std::path::PathBuf::from(format!("/tmp/{id}.jsonl")),
            title: Some(format!("first prompt of {id}")),
            model: Some("anthropic/claude-sonnet-4-5".into()),
            message_count: 3,
            modified_at: std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago),
            locked: false,
        }
    }

    #[test]
    fn slash_resume_opens_overlay_and_emits_return_sessions() {
        let mut s = fresh();
        s.input = "/resume".into();
        s.cursor = s.input.len();
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert_eq!(cmds, vec![Command::ReturnSessions]);
        assert!(matches!(s.overlay, Some(Overlay::SessionResume { .. })));
    }

    #[test]
    fn sessions_listed_event_populates_open_picker() {
        let mut s = fresh();
        s.overlay = Some(Overlay::SessionResume {
            focused: 0,
            sessions: Vec::new(),
            confirm_delete: false,
        });
        s.on_event(TuiEvent::SessionsListed(vec![
            fake_session_meta("a", 30),
            fake_session_meta("b", 60),
        ]));
        if let Some(Overlay::SessionResume { sessions, .. }) = &s.overlay {
            assert_eq!(sessions.len(), 2);
        } else {
            panic!("overlay should still be SessionResume");
        }
    }

    #[test]
    fn sessions_listed_at_boot_opens_resume_picker() {
        let mut s = fresh();
        assert!(s.overlay.is_none());
        s.on_event(TuiEvent::SessionsListed(vec![fake_session_meta("a", 0)]));
        // Boot-list auto-opens the picker.
        assert!(matches!(s.overlay, Some(Overlay::SessionResume { .. })));
    }

    #[test]
    fn sessions_listed_when_overlay_closed_but_not_boot_is_noop() {
        let mut s = fresh();
        // Simulate post-boot state — transcript has grown.
        s.push(TranscriptKind::UserPrompt, "hi".into());
        s.push(TranscriptKind::AssistantText, "hello".into());
        s.push(TranscriptKind::UserPrompt, "more".into());
        s.push(TranscriptKind::AssistantText, "stuff".into());
        assert!(s.transcript.len() > 3);
        assert!(s.overlay.is_none());
        s.on_event(TuiEvent::SessionsListed(vec![fake_session_meta("a", 0)]));
        // Still none — not boot anymore.
        assert!(s.overlay.is_none());
    }

    #[test]
    fn resume_picker_arrows_navigate_within_bounds() {
        let mut s = fresh();
        s.overlay = Some(Overlay::SessionResume {
            focused: 0,
            sessions: vec![fake_session_meta("a", 0), fake_session_meta("b", 60)],
            confirm_delete: false,
        });
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        if let Some(Overlay::SessionResume { focused, .. }) = s.overlay.clone() {
            assert_eq!(focused, 1);
        }
        // End clamps to last entry.
        s.on_event(TuiEvent::Key(press(KeyCode::End)));
        if let Some(Overlay::SessionResume { focused, .. }) = s.overlay {
            assert_eq!(focused, 1);
        }
    }

    #[test]
    fn resume_picker_enter_dispatches_resume_command_and_closes() {
        let mut s = fresh();
        let meta = fake_session_meta("xyz", 10);
        let path = meta.path.clone();
        s.overlay = Some(Overlay::SessionResume {
            focused: 0,
            sessions: vec![meta],
            confirm_delete: false,
        });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(s.overlay.is_none());
        assert_eq!(cmds, vec![Command::ResumeSession(path)]);
        assert!(s.transcript.iter().any(|l| l.kind == TranscriptKind::Info
            && l.text.contains("resuming session")
            && l.text.contains("xyz.jsonl")));
    }

    #[test]
    fn resume_picker_delete_arms_then_fires_on_second_press() {
        let mut s = fresh();
        let meta = fake_session_meta("doomed", 5);
        let path = meta.path.clone();
        s.overlay = Some(Overlay::SessionResume {
            focused: 0,
            sessions: vec![meta],
            confirm_delete: false,
        });
        // First Delete arms but doesn't fire.
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Delete)));
        assert!(cmds.is_empty());
        if let Some(Overlay::SessionResume { confirm_delete, .. }) = &s.overlay {
            assert!(*confirm_delete, "first Delete should arm confirm_delete");
        } else {
            panic!("overlay should still be SessionResume");
        }
        // Second Delete fires.
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Delete)));
        assert_eq!(cmds, vec![Command::DeleteSession(path)]);
        // Arm flag resets — overlay stays open for further deletes.
        if let Some(Overlay::SessionResume { confirm_delete, .. }) = &s.overlay {
            assert!(!*confirm_delete);
        } else {
            panic!("overlay should still be SessionResume");
        }
    }

    #[test]
    fn resume_picker_navigation_cancels_pending_delete() {
        let mut s = fresh();
        s.overlay = Some(Overlay::SessionResume {
            focused: 0,
            sessions: vec![fake_session_meta("a", 0), fake_session_meta("b", 60)],
            confirm_delete: false,
        });
        s.on_event(TuiEvent::Key(press(KeyCode::Delete)));
        // Down should disarm.
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        if let Some(Overlay::SessionResume { confirm_delete, .. }) = &s.overlay {
            assert!(!*confirm_delete);
        }
        // And the follow-up Delete only re-arms, never fires.
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Delete)));
        assert!(cmds.is_empty());
    }

    #[test]
    fn resume_picker_enter_while_armed_resumes_not_deletes() {
        let mut s = fresh();
        let meta = fake_session_meta("xyz", 10);
        let path = meta.path.clone();
        s.overlay = Some(Overlay::SessionResume {
            focused: 0,
            sessions: vec![meta],
            confirm_delete: true,
        });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert_eq!(cmds, vec![Command::ResumeSession(path)]);
    }

    #[test]
    fn resume_picker_delete_on_empty_list_is_noop() {
        let mut s = fresh();
        s.overlay = Some(Overlay::SessionResume {
            focused: 0,
            sessions: Vec::new(),
            confirm_delete: false,
        });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Delete)));
        assert!(cmds.is_empty());
        if let Some(Overlay::SessionResume { confirm_delete, .. }) = &s.overlay {
            assert!(!*confirm_delete);
        }
    }

    #[test]
    fn sessions_listed_event_resets_pending_delete_confirm() {
        let mut s = fresh();
        s.overlay = Some(Overlay::SessionResume {
            focused: 0,
            sessions: vec![fake_session_meta("a", 0)],
            confirm_delete: true,
        });
        s.on_event(TuiEvent::SessionsListed(vec![fake_session_meta("b", 0)]));
        if let Some(Overlay::SessionResume { confirm_delete, .. }) = &s.overlay {
            assert!(!*confirm_delete);
        }
    }

    #[test]
    fn resume_picker_enter_on_locked_opens_conflict_overlay() {
        let mut s = fresh();
        let mut meta = fake_session_meta("held", 1);
        meta.locked = true;
        let locked_path = meta.path.clone();
        s.overlay = Some(Overlay::SessionResume {
            focused: 0,
            sessions: vec![meta],
            confirm_delete: false,
        });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        // No ResumeSession — locked rows divert to the dialog.
        assert!(cmds.is_empty());
        match &s.overlay {
            Some(Overlay::SessionLockConflict {
                source,
                locked_path: lp,
                choices,
                focused,
            }) => {
                assert_eq!(*source, SessionLockSource::Resume);
                assert_eq!(lp, &locked_path);
                assert_eq!(*focused, 0);
                assert_eq!(choices[0], SessionConflictChoice::Fresh);
                assert!(matches!(choices[3], SessionConflictChoice::Cancel));
            }
            other => panic!("expected SessionLockConflict, got {other:?}"),
        }
    }

    #[test]
    fn boot_locked_event_opens_overlay_with_fresh_default() {
        let mut s = fresh();
        let locked_path = std::path::PathBuf::from("/tmp/held.jsonl");
        s.on_event(TuiEvent::SessionLockedAtBoot {
            locked_path: locked_path.clone(),
        });
        match &s.overlay {
            Some(Overlay::SessionLockConflict {
                source,
                locked_path: lp,
                choices,
                focused,
            }) => {
                assert_eq!(*source, SessionLockSource::Boot);
                assert_eq!(lp, &locked_path);
                assert_eq!(*focused, 0, "Fresh must be preselected at boot");
                assert_eq!(choices[0], SessionConflictChoice::Fresh);
                assert!(matches!(choices[3], SessionConflictChoice::Quit));
            }
            other => panic!("expected SessionLockConflict, got {other:?}"),
        }
    }

    #[test]
    fn lock_conflict_enter_fresh_at_boot_is_noop() {
        let mut s = fresh();
        s.overlay = Some(Overlay::SessionLockConflict {
            source: SessionLockSource::Boot,
            locked_path: std::path::PathBuf::from("/tmp/x.jsonl"),
            choices: vec![
                SessionConflictChoice::Fresh,
                SessionConflictChoice::Fork,
                SessionConflictChoice::Quit,
            ],
            focused: 0,
        });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(
            cmds.is_empty(),
            "boot Fresh = stay on already-fresh session"
        );
        assert!(s.overlay.is_none());
    }

    #[test]
    fn lock_conflict_enter_fresh_at_resume_emits_reset() {
        let mut s = fresh();
        s.overlay = Some(Overlay::SessionLockConflict {
            source: SessionLockSource::Resume,
            locked_path: std::path::PathBuf::from("/tmp/x.jsonl"),
            choices: vec![
                SessionConflictChoice::Fresh,
                SessionConflictChoice::Fork,
                SessionConflictChoice::Cancel,
            ],
            focused: 0,
        });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert_eq!(cmds, vec![Command::Reset]);
    }

    #[test]
    fn lock_conflict_enter_fork_emits_fork_command() {
        let mut s = fresh();
        let path = std::path::PathBuf::from("/tmp/forkme.jsonl");
        s.overlay = Some(Overlay::SessionLockConflict {
            source: SessionLockSource::Resume,
            locked_path: path.clone(),
            choices: vec![
                SessionConflictChoice::Fresh,
                SessionConflictChoice::Fork,
                SessionConflictChoice::Cancel,
            ],
            focused: 1,
        });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert_eq!(cmds, vec![Command::ForkSession(path)]);
    }

    #[test]
    fn lock_conflict_enter_quit_at_boot_sets_quit() {
        let mut s = fresh();
        s.overlay = Some(Overlay::SessionLockConflict {
            source: SessionLockSource::Boot,
            locked_path: std::path::PathBuf::from("/tmp/x.jsonl"),
            choices: vec![
                SessionConflictChoice::Fresh,
                SessionConflictChoice::Fork,
                SessionConflictChoice::Quit,
            ],
            focused: 2,
        });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert_eq!(cmds, vec![Command::Quit]);
        assert!(s.should_quit);
    }

    #[test]
    fn lock_conflict_enter_cancel_just_closes() {
        let mut s = fresh();
        s.overlay = Some(Overlay::SessionLockConflict {
            source: SessionLockSource::Resume,
            locked_path: std::path::PathBuf::from("/tmp/x.jsonl"),
            choices: vec![
                SessionConflictChoice::Fresh,
                SessionConflictChoice::Fork,
                SessionConflictChoice::Cancel,
            ],
            focused: 2,
        });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(cmds.is_empty());
        assert!(s.overlay.is_none());
    }

    #[test]
    fn lock_conflict_arrows_navigate_within_bounds() {
        let mut s = fresh();
        s.overlay = Some(Overlay::SessionLockConflict {
            source: SessionLockSource::Boot,
            locked_path: std::path::PathBuf::from("/tmp/x.jsonl"),
            choices: vec![
                SessionConflictChoice::Fresh,
                SessionConflictChoice::Fork,
                SessionConflictChoice::Quit,
            ],
            focused: 0,
        });
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        s.on_event(TuiEvent::Key(press(KeyCode::Down))); // clamped
        if let Some(Overlay::SessionLockConflict { focused, .. }) = &s.overlay {
            assert_eq!(*focused, 2);
        }
        s.on_event(TuiEvent::Key(press(KeyCode::Up)));
        if let Some(Overlay::SessionLockConflict { focused, .. }) = &s.overlay {
            assert_eq!(*focused, 1);
        }
    }

    #[test]
    fn slash_resume_appears_in_catalog() {
        assert!(SLASH_CATALOG.iter().any(|c| c.trigger == "/resume"));
    }

    #[test]
    fn slash_log_appears_in_catalog() {
        assert!(SLASH_CATALOG.iter().any(|c| c.trigger == "/log"));
    }

    #[test]
    fn slash_compact_appears_in_catalog() {
        assert!(SLASH_CATALOG.iter().any(|c| c.trigger == "/compact"));
    }

    #[test]
    fn slash_compact_dispatches_compact_command() {
        let mut s = fresh();
        s.input = "/compact".into();
        s.cursor = s.input.len();
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(matches!(
            cmds.as_slice(),
            [Command::Compact { keep_recent: 4 }]
        ));
    }

    #[test]
    fn session_resumed_event_clears_transcript_and_shows_history() {
        let mut s = fresh();
        // Populate some existing transcript lines.
        s.push(TranscriptKind::UserPrompt, "old prompt".into());
        s.push(TranscriptKind::AssistantText, "old answer".into());
        let old_len = s.transcript.len();
        assert!(old_len >= 2, "expected at least 2 old lines, got {old_len}");

        // Simulate a /resume that loads a session with one user
        // message and one assistant message.
        let prior_user = AgentMessage::user(grain_agent_core::UserMessage {
            content: vec![UserContent::Text(grain_agent_core::TextContent {
                text: "how do I read a file?".into(),
            })],
            timestamp: 0,
        });
        let prior_assistant = AgentMessage::assistant(grain_agent_core::AssistantMessage {
            content: vec![grain_agent_core::AssistantContent::Text(
                grain_agent_core::TextContent {
                    text: "Use the read tool".into(),
                },
            )],
            api: "openai".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            usage: Default::default(),
            stop_reason: grain_agent_core::StopReason::Stop,
            error_message: None,
            timestamp: 0,
        });

        s.on_event(TuiEvent::SessionResumed {
            path: "/tmp/test.jsonl".into(),
            messages: vec![prior_user, prior_assistant],
        });

        // Old transcript is gone; new history is rendered.
        // The assistant text "Use the read tool" is a single line,
        // so we expect one UserPrompt + one AssistantText = 2 lines.
        let kinds: Vec<_> = s.transcript.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![TranscriptKind::UserPrompt, TranscriptKind::AssistantText],
            "transcript lines after session resume: {:#?}",
            s.transcript.iter().map(|l| &l.text).collect::<Vec<_>>()
        );

        // Streaming state is reset.
        assert!(!s.streaming);
        assert!(s.streaming_started_at.is_none());
    }

    #[test]
    fn resume_populates_input_history_from_user_messages() {
        let mut s = fresh();
        // Pre-resume history that should be wiped.
        s.push_history("stale draft 1");
        s.push_history("stale draft 2");

        let user1 = AgentMessage::user(grain_agent_core::UserMessage {
            content: vec![UserContent::Text(grain_agent_core::TextContent {
                text: "first prompt from prior session".into(),
            })],
            timestamp: 0,
        });
        let assistant1 = AgentMessage::assistant(grain_agent_core::AssistantMessage {
            content: vec![grain_agent_core::AssistantContent::Text(
                grain_agent_core::TextContent { text: "ok".into() },
            )],
            api: "openai".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            usage: Default::default(),
            stop_reason: grain_agent_core::StopReason::Stop,
            error_message: None,
            timestamp: 0,
        });
        let user2 = AgentMessage::user(grain_agent_core::UserMessage {
            content: vec![UserContent::Text(grain_agent_core::TextContent {
                text: "second prompt".into(),
            })],
            timestamp: 0,
        });

        s.on_event(TuiEvent::SessionResumed {
            path: "/tmp/test.jsonl".into(),
            messages: vec![user1, assistant1, user2],
        });

        assert_eq!(
            s.history,
            vec![
                "first prompt from prior session".to_string(),
                "second prompt".to_string(),
            ],
            "history should reflect the resumed session's user prompts only"
        );
        assert!(
            s.history_cursor.is_none(),
            "history cursor reset so first Up lands on the newest entry"
        );

        // Up arrow walks the new history.
        s.input = String::new();
        s.cursor = 0;
        s.history_up();
        assert_eq!(s.input, "second prompt");
        s.history_up();
        assert_eq!(s.input, "first prompt from prior session");
    }

    #[test]
    fn compaction_clears_render_and_markdown_caches() {
        let mut s = fresh();
        s.push(TranscriptKind::AssistantText, "**old** transcript".into());
        let _ = s.cached_blocks();
        let old_spans: Arc<[crate::md_render::MdStyledSpan]> =
            Arc::from(crate::md_render::render_md_to_spans("**old**").into_boxed_slice());
        s.markdown_cache.entries.push(Some(old_spans));
        s.block_summary_cache.insert(42, "stale".into());
        s.fold_overrides.insert(42, true);
        s.rendered_rows.replace(vec![RenderedRow {
            text: "stale rendered row".into(),
            kind: TranscriptKind::AssistantText,
            chrome_for_block: None,
            md_spans: None,
        }]);

        s.on_event(TuiEvent::SessionCompacted {
            messages: Vec::new(),
        });

        assert!(s.transcript.is_empty());
        assert!(s.markdown_cache.entries.is_empty());
        assert!(s.cached_blocks.is_empty());
        assert!(s.block_summary_cache.is_empty());
        assert!(s.fold_overrides.is_empty());
        assert!(s.rendered_rows.borrow().is_empty());
    }

    #[test]
    fn tui_info_event_pushes_info_transcript_line() {
        let mut s = fresh();
        s.on_event(TuiEvent::Info("(resumed: foo.jsonl)".into()));
        assert!(
            s.transcript
                .iter()
                .any(|l| l.kind == TranscriptKind::Info && l.text.contains("resumed"))
        );
    }

    #[test]
    fn slash_provider_appears_in_palette_catalog() {
        // Regression: `/provider` was handled in `on_slash_command`
        // but missing from `SLASH_CATALOG`, so the dropdown didn't
        // surface it. Lock it in.
        assert!(
            SLASH_CATALOG.iter().any(|c| c.trigger == "/provider"),
            "/provider must be discoverable via the slash palette"
        );
    }

    #[test]
    fn palette_down_navigates_and_typing_resets_focused() {
        let mut s = fresh();
        s.on_event(TuiEvent::Key(press(KeyCode::Char('/'))));
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        assert_eq!(s.palette_focused, 2);
        // Typing narrows the filter — focused must snap back to 0.
        s.on_event(TuiEvent::Key(press(KeyCode::Char('t'))));
        assert_eq!(s.palette_focused, 0);
    }

    #[test]
    fn palette_enter_snaps_input_to_focused_command_and_dispatches() {
        let mut s = fresh();
        for c in "/the".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        // Only /theme matches; focused is 0; Enter should open the
        // theme picker (no SendPrompt fired).
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(cmds.is_empty(), "slash command does not produce SendPrompt");
        assert!(matches!(s.overlay, Some(Overlay::ThemePicker { .. })));
        // And the user-prompt line in the transcript is the expanded form.
        let last_prompt = s
            .transcript
            .iter()
            .rev()
            .find(|l| l.kind == TranscriptKind::UserPrompt)
            .expect("user prompt logged");
        assert_eq!(last_prompt.text, "/theme");
    }

    #[test]
    fn palette_hidden_when_input_is_freeform_text() {
        let mut s = fresh();
        for c in "hello".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        assert!(!s.palette_visible());
        // Down should NOT navigate the (hidden) palette — it stays at 0
        // and we still consume the key as a no-op.
        s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        assert_eq!(s.palette_focused, 0);
    }

    #[test]
    fn agent_end_resets_streaming_and_pending() {
        let mut s = fresh();
        s.streaming = true;
        s.pending_tool_calls = 3;
        s.on_event(TuiEvent::Agent(Box::new(AgentEvent::AgentEnd {
            messages: vec![],
        })));
        assert!(!s.streaming);
        assert_eq!(s.pending_tool_calls, 0);
    }

    // ---- cache drop detection -----------------------------------

    #[test]
    fn update_cache_drop_zero_input_is_inert() {
        let (s, d) = update_cache_drop_state(2, false, 0, 0);
        assert_eq!(s, 2);
        assert!(!d);
    }

    #[test]
    fn update_cache_drop_high_rate_increments_streak() {
        let (s, d) = update_cache_drop_state(2, false, 1000, 900);
        assert_eq!(s, 3);
        assert!(!d);
    }

    #[test]
    fn update_cache_drop_neutral_rate_leaves_state_alone() {
        // 70% — between high and low → nothing changes.
        let (s, d) = update_cache_drop_state(4, false, 1000, 700);
        assert_eq!(s, 4);
        assert!(!d);
    }

    #[test]
    fn update_cache_drop_low_rate_without_baseline_just_resets_streak() {
        // No prior healthy streak → low rate just resets streak,
        // doesn't trip the alarm.
        let (s, d) = update_cache_drop_state(1, false, 1000, 100);
        assert_eq!(s, 0);
        assert!(!d);
    }

    #[test]
    fn update_cache_drop_low_rate_after_baseline_arms_alarm() {
        // Healthy streak ≥ threshold → next low-rate turn sticks
        // `dropped = true` forever (this session).
        let (s, d) = update_cache_drop_state(3, false, 1000, 100);
        assert_eq!(s, 0);
        assert!(d);
    }

    #[test]
    fn update_cache_drop_sticky_once_set() {
        // High rate after the alarm fires does *not* unset it —
        // only `AgentStart` clears the flag.
        let (s, d) = update_cache_drop_state(0, true, 1000, 999);
        assert_eq!(s, 1);
        assert!(d, "dropped flag must stick once tripped");
    }

    #[test]
    fn agent_start_clears_cache_drop_state() {
        let mut s = fresh();
        s.cache_high_streak = 5;
        s.cache_dropped = true;
        s.on_event(TuiEvent::Agent(Box::new(AgentEvent::AgentStart)));
        assert_eq!(s.cache_high_streak, 0);
        assert!(!s.cache_dropped);
    }

    // ---- session cumulative usage --------------------------------

    // ---- CNY rate resolution ------------------------------------

    #[test]
    fn resolve_cny_rate_explicit_cli_wins() {
        assert_eq!(resolve_cny_rate(Some(6.85), None), Some(6.85));
        // CLI also wins over a zh locale.
        assert_eq!(
            resolve_cny_rate(Some(6.85), Some("zh_CN.UTF-8")),
            Some(6.85)
        );
    }

    #[test]
    fn resolve_cny_rate_auto_from_zh_locale() {
        assert_eq!(
            resolve_cny_rate(None, Some("zh_CN.UTF-8")),
            Some(DEFAULT_CNY_RATE)
        );
        assert_eq!(
            resolve_cny_rate(None, Some("zh_TW")),
            Some(DEFAULT_CNY_RATE)
        );
        assert_eq!(
            resolve_cny_rate(None, Some("ZH_HK")),
            Some(DEFAULT_CNY_RATE)
        );
    }

    #[test]
    fn resolve_cny_rate_none_for_non_zh_locale() {
        assert_eq!(resolve_cny_rate(None, Some("en_US.UTF-8")), None);
        assert_eq!(resolve_cny_rate(None, Some("ja_JP.UTF-8")), None);
        assert_eq!(resolve_cny_rate(None, None), None);
    }

    #[test]
    fn resolve_cny_rate_rejects_non_positive_cli_override() {
        // Zero / negative explicit override falls through to locale
        // detection rather than being honored (defensive — `--cny-rate 0`
        // is almost certainly a typo).
        assert_eq!(
            resolve_cny_rate(Some(0.0), Some("zh_CN")),
            Some(DEFAULT_CNY_RATE)
        );
        assert_eq!(resolve_cny_rate(Some(-1.0), None), None);
    }

    #[test]
    fn agent_start_does_not_reset_session_usage() {
        // Session usage tracks across prompts; per-run counters reset.
        let mut s = fresh();
        s.session_usage.input = 5_000;
        s.session_usage.output = 1_200;
        s.tokens_in = 5_000;
        s.tokens_out = 1_200;

        s.on_event(TuiEvent::Agent(Box::new(AgentEvent::AgentStart)));

        // Per-run counters reset to 0.
        assert_eq!(s.tokens_in, 0);
        assert_eq!(s.tokens_out, 0);
        // Session-cumulative counters survive.
        assert_eq!(s.session_usage.input, 5_000);
        assert_eq!(s.session_usage.output, 1_200);
    }

    // ---- F5 thinking visibility toggle --------------------------

    #[test]
    fn f5_toggles_show_thinking_flag() {
        let mut s = fresh();
        // Default from fresh() is `show_thinking = false`.
        assert!(!s.show_thinking);
        s.on_event(TuiEvent::Key(press(KeyCode::F(5))));
        assert!(s.show_thinking, "F5 must flip the flag on");
        s.on_event(TuiEvent::Key(press(KeyCode::F(5))));
        assert!(!s.show_thinking, "second F5 must flip it back off");
    }

    #[test]
    fn f5_pushes_info_line_into_transcript() {
        let mut s = fresh();
        let before = s.transcript.len();
        s.on_event(TuiEvent::Key(press(KeyCode::F(5))));
        assert!(s.transcript.len() > before);
        let last = s.transcript.last().unwrap();
        assert_eq!(last.kind, TranscriptKind::Info);
        assert!(last.text.contains("thinking"));
    }

    #[test]
    fn thinking_deltas_are_always_pushed_regardless_of_show_thinking() {
        // After the F5 redesign, thinking deltas always land in the
        // transcript so they're available to render when the user
        // later flips visibility on.
        use grain_agent_core::{AssistantContent, ThinkingContent};
        let mut s = fresh();
        assert!(!s.show_thinking);
        let am = AssistantMessage {
            content: vec![AssistantContent::Thinking(ThinkingContent {
                thinking: "thinking-text".into(),
                signature: None,
                provider_metadata: None,
            })],
            api: "x".into(),
            provider: "x".into(),
            model: "x".into(),
            usage: Default::default(),
            stop_reason: grain_agent_core::StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        s.on_event(TuiEvent::Agent(Box::new(AgentEvent::MessageUpdate {
            message: am.clone(),
            assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                partial: am,
                content_index: 0,
                delta: "thinking-text".into(),
            },
        })));
        // Even with show_thinking off, the underlying transcript
        // holds the line — the renderer is what filters it out.
        assert!(
            s.transcript
                .iter()
                .any(|l| l.kind == TranscriptKind::ThinkingText
                    && l.text.contains("thinking-text"))
        );
    }

    // ---- skill palette injection -----------------------------------

    #[test]
    fn skills_loaded_event_populates_skills_field() {
        let mut s = fresh();
        assert!(s.skills.is_empty());
        let test_skill = grain_agent_harness::Skill {
            name: "test-skill".into(),
            description: "A test skill".into(),
            file_path: "/skills/test/SKILL.md".into(),
            disable_model_invocation: false,
            body: "you are a test skill".into(),
        };
        s.on_event(TuiEvent::SkillsLoaded(vec![test_skill]));
        assert_eq!(s.skills.len(), 1);
        assert_eq!(s.skills[0].name, "test-skill");
        assert_eq!(s.skills[0].body, "you are a test skill");
    }

    #[test]
    fn palette_includes_skills_alongside_commands() {
        let mut s = fresh();
        let test_skill = grain_agent_harness::Skill {
            name: "test-skill".into(),
            description: "A test skill".into(),
            file_path: String::new(),
            disable_model_invocation: false,
            body: "you are a test skill".into(),
        };
        s.skills = vec![test_skill];

        s.on_event(TuiEvent::Key(press(KeyCode::Char('/'))));
        let matches = s.palette_matches();
        // Should have both SLASH_CATALOG entries and the skill.
        assert!(matches.len() > SLASH_CATALOG.len());
        let skill_item = matches
            .iter()
            .find(|m| m.trigger == "skill: test-skill")
            .expect("skill must appear in palette");
        assert!(
            matches!(skill_item.action, PaletteAction::InjectBody(ref b) if b == "you are a test skill")
        );
    }

    #[test]
    fn palette_enter_on_skill_injects_body_and_does_not_dispatch() {
        let mut s = fresh();
        let test_skill = grain_agent_harness::Skill {
            name: "test-skill".into(),
            description: "A test skill".into(),
            file_path: String::new(),
            disable_model_invocation: false,
            body: "## skill prompt body\n\nDo the thing.".into(),
        };
        s.skills = vec![test_skill];

        // Type "/" to open the palette, then press Enter on the first
        // match (which is the first SLASH_CATALOG item, not the skill).
        // First move down past the commands to land on the skill.
        s.on_event(TuiEvent::Key(press(KeyCode::Char('/'))));
        // Move palette_focused past the commands to the skill.
        for _ in 0..SLASH_CATALOG.len() {
            s.on_event(TuiEvent::Key(press(KeyCode::Down)));
        }

        // Now Enter should inject the body and NOT send anything.
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(cmds.is_empty(), "skill injection should not dispatch");
        assert_eq!(s.input, "## skill prompt body\n\nDo the thing.");
        assert_eq!(s.cursor, s.input.len());
        // Palette should be gone because input no longer starts with /.
        assert!(!s.palette_visible());
    }

    #[test]
    fn disabled_skills_do_not_appear_in_palette() {
        let mut s = fresh();
        let visible_skill = grain_agent_harness::Skill {
            name: "visible".into(),
            description: "visible skill".into(),
            file_path: String::new(),
            disable_model_invocation: false,
            body: "visible body".into(),
        };
        let disabled_skill = grain_agent_harness::Skill {
            name: "hidden".into(),
            description: "hidden skill".into(),
            file_path: String::new(),
            disable_model_invocation: true,
            body: "hidden body".into(),
        };
        s.skills = vec![visible_skill, disabled_skill];

        s.on_event(TuiEvent::Key(press(KeyCode::Char('/'))));
        let matches = s.palette_matches();
        assert!(
            matches.iter().any(|m| m.trigger.contains("visible")),
            "visible skill should appear"
        );
        assert!(
            !matches.iter().any(|m| m.trigger.contains("hidden")),
            "disabled skill should not appear"
        );
    }

    // ── Cached blocks ──────────────────────────────────────────

    #[test]
    fn cached_blocks_rebuilds_when_transcript_grows() {
        let mut s = fresh();
        // Initial cache is empty; first access builds it.
        let blocks = s.cached_blocks().to_vec();
        let initial_len = blocks.len();

        // Append a user prompt line.
        s.push(TranscriptKind::UserPrompt, "hello".into());

        // Cache should rebuild since transcript grew.
        let new_blocks = s.cached_blocks().to_vec();
        assert!(
            new_blocks.len() > initial_len,
            "cache must rebuild on growth"
        );
    }

    #[test]
    fn cached_blocks_is_stable_when_transcript_unchanged() {
        let mut s = fresh();
        let blocks1 = s.cached_blocks().to_vec();
        let blocks2 = s.cached_blocks().to_vec();
        assert_eq!(blocks1, blocks2, "same transcript => same blocks");
    }

    #[test]
    fn block_summary_tool_call_includes_line_count() {
        let mut s = fresh();
        s.push(TranscriptKind::ToolCallStart, "● read(\"foo.rs\")".into());
        s.push(TranscriptKind::AssistantText, "result line".into());
        s.push(TranscriptKind::ToolCallEnd, "⎿ read complete".into());

        let blocks = s.cached_blocks().to_vec();
        // Find the tool-call block.
        let tc = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolCall)
            .unwrap();
        let summary = s.block_summary(tc);
        assert!(summary.contains("tool:"), "summary identifies tool");
        assert!(
            summary.contains("(3 lines)"),
            "summary includes line count: {summary}"
        );
    }

    #[test]
    fn block_summary_thinking() {
        let mut s = fresh();
        s.push(TranscriptKind::ThinkingText, "Hmm...".into());
        s.push(TranscriptKind::ThinkingText, "Let me think more...".into());

        let blocks = s.cached_blocks().to_vec();
        let th = blocks
            .iter()
            .find(|b| b.kind == BlockKind::Thinking)
            .unwrap();
        let summary = s.block_summary(th);
        assert!(
            summary.contains("thinking"),
            "summary identifies thinking: {summary}"
        );
        assert!(
            summary.contains("2 lines"),
            "summary includes line count: {summary}"
        );
    }
}
