//! Pure UI state — no I/O. Every state transition is a method on
//! [`AppState`] taking a [`crate::event::TuiEvent`] or a key event, and
//! returning zero-or-more [`Command`]s for the agent worker to execute.
//!
//! Keeping the state machine pure lets us unit-test render-relevant
//! behavior without touching a real terminal or LLM.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::time::Instant;

use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use grain_agent_core::{
    AgentEvent, AgentMessage, AssistantMessageEvent, Cost, Message, UserContent,
};

use crate::event::TuiEvent;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptKind {
    UserPrompt,
    AssistantText,
    ThinkingText,
    ToolCallStart,
    ToolCallEnd,
    Info,
    Error,
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
    /// Scan `sessions_dir` for past `<uuidv7>.jsonl` files. Worker
    /// returns the list via [`TuiEvent::SessionsListed`], which
    /// populates the `/resume` overlay.
    ReturnSessions,
    /// Tear down the current harness and re-build it on top of the
    /// JSONL transcript at this path. Worker re-installs all
    /// subscriptions (event fan-out, telemetry, session writer) and
    /// emits a [`TuiEvent::Info`] when the swap completes.
    ResumeSession(std::path::PathBuf),
    /// Run a compaction pass on the harness's session: summarize all
    /// but the last `keep_recent` messages. Worker emits a
    /// [`TuiEvent::Info`] on success or [`TuiEvent::AgentWorkerError`]
    /// on failure (e.g. empty transcript).
    Compact { keep_recent: usize },
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
    UpdatePlugin { name: String },
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
pub const SLASH_CATALOG: &[CommandCatalogItem] = &[
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

/// Everything the renderer needs to draw a frame. Pure data — no
/// `Arc<Mutex<...>>`, no `tokio` types — so it can be cloned cheaply for
/// snapshot testing.
#[derive(Debug, Clone)]
pub struct AppState {
    pub transcript: Vec<TranscriptLine>,
    pub input: String,
    pub cursor: usize,
    pub focus: Focus,
    pub overlay: Option<Overlay>,
    /// Scroll position when `!follow_bottom`. Counted as "rendered
    /// rows from the top of the wrapped transcript" so new content
    /// arriving doesn't shift the user's frozen view.
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
    pub pending_tool_calls: usize,
    pub model_id: String,
    /// Per-million-token pricing for the active model. Driven from the
    /// embedded `models.dev` snapshot at startup, refreshed on
    /// `TuiEvent::ProviderApplied` so a runtime provider switch keeps
    /// the cost chip accurate. `Cost::default()` (all zeros) when
    /// pricing is unknown — the footer suppresses the chip then.
    pub model_cost: Cost,
    pub workspace_display: String,
    pub capabilities: Capabilities,
    pub show_thinking: bool,
    pub last_error: Option<String>,
    /// Available themes (built-ins + user). Index 0 is the default
    /// chosen at startup; the picker walks this list.
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
    /// Plugin-contributed slash command overrides. When the user
    /// types `/<trigger>`, this list is consulted **before** the
    /// built-in slash table; a match dispatches into the plugin's
    /// Rhai handler via [`Command::InvokePluginUi`].
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
}

/// Cap on the in-memory prompt history. Old entries get truncated
/// from the front once we exceed this so long sessions don't grow
/// unbounded.
pub const MAX_HISTORY: usize = 200;

/// Cap on the in-memory request-body log ring buffer (entries kept
/// in [`AppState::request_log`]). Each entry is a pretty-printed
/// JSON Message[] array — typically 2–20 KB. 20 entries keeps the
/// ring well under 500 KB even on heavy turns.
pub const MAX_REQUEST_LOG: usize = 20;

/// Default USD → CNY rate when `--cny-rate` is unset but a `zh_*`
/// locale is detected. Picked as a stable round number; users in
/// rate-sensitive workflows should pass `--cny-rate` explicitly.
pub const DEFAULT_CNY_RATE: f64 = 7.20;

/// Resolve the CNY rate from CLI override + locale env var. Pure —
/// takes the env value as an argument so tests don't need to mutate
/// process state.
pub fn resolve_cny_rate(cli_override: Option<f64>, lang_env: Option<&str>) -> Option<f64> {
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
pub const CACHE_HIGH_RATE: f64 = 0.80;

/// Per-turn hit rate below which (after a healthy baseline) we flag
/// a "cache drop". A 30+ percentage-point drop relative to the
/// baseline is almost always a prefix-mutation bug.
pub const CACHE_LOW_RATE: f64 = 0.50;

/// Number of consecutive healthy turns required before drop detection
/// arms. Without this minimum, the first turn (mostly miss) would
/// trip the alarm.
pub const CACHE_HIGH_STREAK_THRESHOLD: u8 = 3;

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
pub fn update_cache_drop_state(
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

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model_id: String,
        model_cost: Cost,
        workspace_display: String,
        capabilities: Capabilities,
        show_thinking: bool,
        themes: Vec<Theme>,
        initial_theme_idx: usize,
        providers: Vec<ProviderProfile>,
        initial_provider_idx: Option<usize>,
        cny_rate: Option<f64>,
    ) -> Self {
        assert!(!themes.is_empty(), "AppState needs at least one theme");
        let current_theme_idx = initial_theme_idx.min(themes.len() - 1);
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
            model_id,
            model_cost,
            workspace_display,
            capabilities,
            show_thinking,
            last_error: None,
            themes,
            current_theme_idx,
            providers,
            current_provider_idx: initial_provider_idx,
            plugin_slashes: Vec::new(),
            skills: Vec::new(),
            palette_focused: 0,
            history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            should_quit: false,
            mouse_capture_on: true,
            rendered_rows: RefCell::new(Vec::new()),
            transcript_area: Cell::new(Rect::default()),
            selection: None,
            request_log: VecDeque::new(),
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

    /// Shared scroll-up step. Used by PgUp and mouse-wheel-up. Same
    /// semantics: tail-follow → freeze at current bottom, then step
    /// back `amount` rows. Already-frozen → just step back.
    pub fn scroll_up(&mut self, amount: usize) {
        if self.follow_bottom {
            let m = self.render_metrics.get();
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
                || skill
                    .name
                    .to_ascii_lowercase()
                    .contains(skill_filter)
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
    }

    /// Reset all per-run streaming counters. Called when the user
    /// `/resume`s into a different session so stale per-turn token
    /// counts don't bleed into the new transcript.
    fn reset_streaming_state(&mut self) {
        self.streaming = false;
        self.streaming_started_at = None;
        self.pending_tool_calls = 0;
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.tokens_cache_read = 0;
        self.cache_high_streak = 0;
        self.cache_dropped = false;
    }

    /// Push a single historical [`AgentMessage`] into the transcript
    /// as one or more lines. Used by [`Self::on_event`] for
    /// `TuiEvent::SessionResumed` so the user sees the loaded
    /// conversation in the scrollback.
    fn push_agent_message(&mut self, msg: &AgentMessage) {
        use grain_agent_core::AssistantContent;
        let AgentMessage::Standard(msg) = msg else { return };
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
                            // Multi-line assistant text: split into
                            // lines so soft-wrap and scroll work.
                            for line in t.text.lines() {
                                self.push(
                                    TranscriptKind::AssistantText,
                                    line.to_string(),
                                );
                            }
                        }
                        AssistantContent::Thinking(t) => {
                            for line in t.thinking.lines() {
                                self.push(
                                    TranscriptKind::ThinkingText,
                                    line.to_string(),
                                );
                            }
                        }
                        AssistantContent::ToolCall(tc) => {
                            let args_preview = preview_json(&tc.arguments, 120);
                            self.push(
                                TranscriptKind::ToolCallStart,
                                format!("→ {}({args_preview})", tc.name),
                            );
                            self.push(
                                TranscriptKind::ToolCallEnd,
                                format!("  [tool call id: {}]", tc.id),
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
                let label = if tr.is_error { "✖" } else { "✓" };
                let tool_name = &tr.tool_name;
                // Truncate for readability — tool results can be
                // very large.
                let preview = truncate(&text, 500);
                self.push(
                    TranscriptKind::Info,
                    format!("{label} {tool_name}: {preview}"),
                );
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
                self.on_agent_event(e);
                Vec::new()
            }
            TuiEvent::OverlayDoctor(text) => {
                // If the user already opened a doctor placeholder
                // (via F2 or /doctor), keep their typed query and
                // scroll position and just swap the report contents.
                if let Some(Overlay::Doctor { query, scroll, .. }) = &self.overlay {
                    let query = query.clone();
                    let scroll = *scroll;
                    self.overlay = Some(Overlay::Doctor {
                        report: text,
                        query,
                        scroll,
                    });
                } else {
                    self.overlay = Some(Overlay::Doctor {
                        report: text,
                        query: String::new(),
                        scroll: 0,
                    });
                }
                Vec::new()
            }
            TuiEvent::OverlaySkills(skills) => {
                self.overlay = Some(Overlay::Skills(skills));
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
                self.request_log.push_back(body);
                while self.request_log.len() > MAX_REQUEST_LOG {
                    self.request_log.pop_front();
                }
                Vec::new()
            }
            TuiEvent::SessionsListed(list) => {
                // Only swap when the overlay is still open (user may
                // have hit Esc while the scan was in flight).
                if let Some(Overlay::SessionResume { sessions, focused }) =
                    &mut self.overlay
                {
                    *sessions = list;
                    if *focused >= sessions.len() {
                        *focused = sessions.len().saturating_sub(1);
                    }
                }
                Vec::new()
            }
            TuiEvent::SessionResumed { path: _, messages } => {
                self.transcript.clear();
                self.reset_streaming_state();
                for msg in &messages {
                    self.push_agent_message(msg);
                }
                Vec::new()
            }
            TuiEvent::Info(text) => {
                self.push(TranscriptKind::Info, text);
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
                self.overlay = Some(match descriptor {
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
                });
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
                self.model_cost = cost;
                self.push(
                    TranscriptKind::Info,
                    format!("(provider: {profile} · {model})"),
                );
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.streaming || self.pending_tool_calls > 0 {
                return vec![Command::AbortCurrentTurn];
            }
            self.should_quit = true;
            return vec![Command::Quit];
        }
        if key.code == KeyCode::Esc {
            if self.overlay.is_some() {
                self.overlay = None;
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
                self.overlay = Some(Overlay::Help);
                Vec::new()
            }
            KeyCode::F(2) => {
                self.overlay = Some(Overlay::Doctor {
                    report: "Running diagnostics…".into(),
                    query: String::new(),
                    scroll: 0,
                });
                vec![Command::ReturnDoctor]
            }
            KeyCode::F(3) => {
                self.overlay = Some(Overlay::Skills(Vec::new()));
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
                    self.overlay = None;
                    return Vec::new();
                }
                let chosen = *focused;
                // Phase 1: API-key profiles apply via the worker;
                // OAuth profiles surface a clear "not wired" line so
                // the user knows what's happening.
                let profile = &self.providers[chosen];
                if !profile.auth.is_usable() {
                    self.overlay = None;
                    let name = profile.name.clone();
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
                self.overlay = None;
                vec![Command::ApplyProvider(chosen)]
            }
            _ => Vec::new(),
        }
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
        let Some(Overlay::SessionResume { focused, sessions }) = &mut self.overlay else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Up => {
                if *focused > 0 {
                    *focused -= 1;
                }
            }
            KeyCode::Down => {
                if *focused + 1 < sessions.len() {
                    *focused += 1;
                }
            }
            KeyCode::Home => {
                *focused = 0;
            }
            KeyCode::End => {
                *focused = sessions.len().saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(sel) = sessions.get(*focused) {
                    let path = sel.path.clone();
                    self.push(
                        TranscriptKind::Info,
                        format!("(resuming session: {})", path.display()),
                    );
                    self.overlay = None;
                    return vec![Command::ResumeSession(path)];
                }
                self.overlay = None;
            }
            _ => {}
        }
        Vec::new()
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
                    obj.insert(
                        f.name.clone(),
                        serde_json::Value::String(f.value.clone()),
                    );
                }
                self.overlay = None;
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
                    self.overlay = None;
                }
                Vec::new()
            }
            Some(Overlay::DynamicConfirm {
                on_yes, yes_args, ..
            }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let handler = on_yes.clone();
                    let args = yes_args.clone();
                    self.overlay = None;
                    vec![Command::InvokePluginUi { handler, args }]
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.overlay = None;
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
                self.overlay = None;
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
                self.overlay = None;
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
                self.overlay = None;
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
        if let Some(bound) = self.plugin_slashes.iter().find(|b| b.command.trigger == head) {
            let handler = bound.command.handler.clone();
            return vec![Command::InvokePluginUi {
                handler,
                args: serde_json::Value::Null,
            }];
        }
        match head {
            "help" | "?" => {
                self.overlay = Some(Overlay::Help);
                Vec::new()
            }
            "clear" | "reset" => {
                self.push(TranscriptKind::Info, "(transcript cleared)".into());
                vec![Command::Reset]
            }
            "doctor" => {
                self.overlay = Some(Overlay::Doctor {
                    report: "Running diagnostics…".into(),
                    query: String::new(),
                    scroll: 0,
                });
                vec![Command::ReturnDoctor]
            }
            "skills" => {
                self.overlay = Some(Overlay::Skills(Vec::new()));
                vec![Command::ReturnSkills]
            }
            "theme" | "themes" => {
                // Open the picker focused on the active theme so
                // up/down feel natural from the user's current pick.
                self.overlay = Some(Overlay::ThemePicker {
                    focused: self.current_theme_idx,
                });
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
                self.overlay = Some(Overlay::ProviderPicker { focused });
                Vec::new()
            }
            "resume" => {
                // Open the picker immediately with an empty list;
                // the worker scans disk and replies via
                // `TuiEvent::SessionsListed`, which swaps the list in.
                self.overlay = Some(Overlay::SessionResume {
                    focused: 0,
                    sessions: Vec::new(),
                });
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
                self.overlay = Some(Overlay::Log { scroll: 0 });
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
                self.overlay = Some(Overlay::Plugins {
                    plugins: Vec::new(),
                    ui_commands: Vec::new(),
                });
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
                    self.push(
                        TranscriptKind::Info,
                        "(usage: /update <name>)".into(),
                    );
                    return Vec::new();
                };
                self.push(
                    TranscriptKind::Info,
                    format!("(updating '{name}' …)"),
                );
                vec![Command::UpdatePlugin { name: name.into() }]
            }
            "reload" => {
                self.push(
                    TranscriptKind::Info,
                    "(reloading Rhai scripts…)".into(),
                );
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
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    self.append_streaming(TranscriptKind::AssistantText, &delta);
                }
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    // Always push into the underlying transcript; the
                    // render-side filter in `ui::draw_transcript`
                    // governs visibility via `state.show_thinking`,
                    // toggleable at runtime with F5.
                    self.append_streaming(TranscriptKind::ThinkingText, &delta);
                }
                _ => {}
            },
            AgentEvent::MessageEnd { message } => {
                if let AgentMessage::Standard(Message::Assistant(am)) = &message {
                    self.tokens_in = self.tokens_in.saturating_add(am.usage.input);
                    self.tokens_out = self.tokens_out.saturating_add(am.usage.output);
                    self.tokens_cache_read =
                        self.tokens_cache_read.saturating_add(am.usage.cache_read);
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
                tool_name, args, ..
            } => {
                self.pending_tool_calls = self.pending_tool_calls.saturating_add(1);
                let preview = preview_json(&args, 120);
                self.push(
                    TranscriptKind::ToolCallStart,
                    format!("→ {tool_name}({preview})"),
                );
            }
            AgentEvent::ToolExecutionUpdate { .. } => {}
            AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                result,
                ..
            } => {
                if self.pending_tool_calls > 0 {
                    self.pending_tool_calls -= 1;
                }
                let preview = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .next()
                    .map(|t| truncate(t, 200))
                    .unwrap_or_default();
                self.push(
                    TranscriptKind::ToolCallEnd,
                    format!(
                        "← {tool_name}{} {preview}",
                        if is_error { " [error]" } else { "" }
                    ),
                );
            }
            AgentEvent::TurnEnd { message, .. } => {
                if let Some(err) = &message.error_message {
                    self.last_error = Some(err.clone());
                    self.push(TranscriptKind::Error, format!("[turn error] {err}"));
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
            }
        }
    }

    fn append_streaming(&mut self, kind: TranscriptKind, delta: &str) {
        if let Some(last) = self.transcript.last_mut()
            && last.kind == kind
        {
            last.text.push_str(delta);
        } else {
            self.push(kind, delta.to_string());
        }
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
pub fn extract_selection(rendered: &[RenderedRow], selection: Selection) -> String {
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
pub fn write_clipboard(text: &str) -> Result<(), String> {
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
            "/tmp/proj".into(),
            Capabilities::default(),
            false,
            crate::theme::builtin_themes(),
            0,
            Vec::new(),
            None,
            None,
        )
    }

    fn fresh_with_providers(providers: Vec<ProviderProfile>) -> AppState {
        AppState::new(
            "deepseek/deepseek-chat".into(),
            Cost::default(),
            "/tmp/proj".into(),
            Capabilities::default(),
            false,
            crate::theme::builtin_themes(),
            0,
            providers,
            None,
            None,
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
        });
        s.on_event(TuiEvent::Key(press(KeyCode::PageUp)));
        let frozen = s.scroll_offset;
        // New content arrives — total_rows grows.
        s.render_metrics.set(RenderMetrics {
            total_rows: 130,
            visible_rows: 20,
        });
        // No key event — scroll_offset is unchanged. Ui renders
        // from this same anchor regardless of where the new bottom is.
        assert_eq!(s.scroll_offset, frozen);
        assert!(!s.follow_bottom);
    }

    #[test]
    fn text_delta_appends_to_last_assistant_line() {
        let mut s = fresh();
        use grain_agent_core::{AssistantMessage, StopReason, Usage};

        let dummy = AssistantMessage {
            content: vec![],
            api: "x".into(),
            provider: "x".into(),
            model: "x".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };

        s.on_event(TuiEvent::Agent(AgentEvent::MessageUpdate {
            message: dummy.clone(),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                partial: dummy.clone(),
                content_index: 0,
                delta: "Hello, ".into(),
            },
        }));
        s.on_event(TuiEvent::Agent(AgentEvent::MessageUpdate {
            message: dummy.clone(),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                partial: dummy,
                content_index: 0,
                delta: "world!".into(),
            },
        }));

        let last = s.transcript.last().expect("text line");
        assert_eq!(last.kind, TranscriptKind::AssistantText);
        assert_eq!(last.text, "Hello, world!");
    }

    #[test]
    fn tool_call_events_increment_then_decrement_pending() {
        let mut s = fresh();
        s.on_event(TuiEvent::Agent(AgentEvent::ToolExecutionStart {
            tool_call_id: "1".into(),
            tool_name: "read".into(),
            args: serde_json::json!({ "path": "x" }),
        }));
        assert_eq!(s.pending_tool_calls, 1);
        s.on_event(TuiEvent::Agent(AgentEvent::ToolExecutionEnd {
            tool_call_id: "1".into(),
            tool_name: "read".into(),
            result: grain_agent_core::AgentToolResult {
                content: vec![UserContent::text("ok")],
                details: serde_json::Value::Null,
                terminate: None,
            },
            is_error: false,
        }));
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
            modified_at: std::time::SystemTime::now()
                - std::time::Duration::from_secs(secs_ago),
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
    fn sessions_listed_event_no_op_when_overlay_closed() {
        let mut s = fresh();
        assert!(s.overlay.is_none());
        s.on_event(TuiEvent::SessionsListed(vec![fake_session_meta("a", 0)]));
        // Still none — user closed before scan completed.
        assert!(s.overlay.is_none());
    }

    #[test]
    fn resume_picker_arrows_navigate_within_bounds() {
        let mut s = fresh();
        s.overlay = Some(Overlay::SessionResume {
            focused: 0,
            sessions: vec![fake_session_meta("a", 0), fake_session_meta("b", 60)],
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
        });
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(s.overlay.is_none());
        assert_eq!(cmds, vec![Command::ResumeSession(path)]);
        assert!(
            s.transcript
                .iter()
                .any(|l| l.kind == TranscriptKind::Info
                    && l.text.contains("resuming session")
                    && l.text.contains("xyz.jsonl"))
        );
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
        s.on_event(TuiEvent::Agent(AgentEvent::AgentEnd { messages: vec![] }));
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
        s.on_event(TuiEvent::Agent(AgentEvent::AgentStart));
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

        s.on_event(TuiEvent::Agent(AgentEvent::AgentStart));

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
        let mut s = fresh();
        assert!(!s.show_thinking);
        let am = AssistantMessage {
            content: vec![],
            api: "x".into(),
            provider: "x".into(),
            model: "x".into(),
            usage: Default::default(),
            stop_reason: grain_agent_core::StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        s.on_event(TuiEvent::Agent(AgentEvent::MessageUpdate {
            message: am.clone(),
            assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                partial: am,
                content_index: 0,
                delta: "thinking-text".into(),
            },
        }));
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
}
