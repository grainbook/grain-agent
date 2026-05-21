//! Pure UI state — no I/O. Every state transition is a method on
//! [`AppState`] taking a [`crate::event::TuiEvent`] or a key event, and
//! returning zero-or-more [`Command`]s for the agent worker to execute.
//!
//! Keeping the state machine pure lets us unit-test render-relevant
//! behavior without touching a real terminal or LLM.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use grain_agent_core::{
    AgentEvent, AgentMessage, AssistantMessageEvent, Message, UserContent,
};

use crate::event::TuiEvent;
use grain_llm_genai::ProviderProfile;
use crate::theme::Theme;

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
    ThemePicker { focused: usize },
    /// Provider profile picker — `focused` is the index into
    /// [`AppState::providers`]. Same key model as ThemePicker.
    ProviderPicker { focused: usize },
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
    Quit,
}

/// One entry in the slash-command palette. The renderer matches on
/// `trigger` (always including the leading `/`) and prints
/// `description` to the right.
#[derive(Debug, Clone, Copy)]
pub struct CommandCatalogItem {
    pub trigger: &'static str,
    pub description: &'static str,
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
    pub scroll_offset: usize,
    pub streaming: bool,
    pub pending_tool_calls: usize,
    pub model_id: String,
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
}

/// Cap on the in-memory prompt history. Old entries get truncated
/// from the front once we exceed this so long sessions don't grow
/// unbounded.
pub const MAX_HISTORY: usize = 200;

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model_id: String,
        workspace_display: String,
        capabilities: Capabilities,
        show_thinking: bool,
        themes: Vec<Theme>,
        initial_theme_idx: usize,
        providers: Vec<ProviderProfile>,
        initial_provider_idx: Option<usize>,
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
            streaming: false,
            pending_tool_calls: 0,
            model_id,
            workspace_display,
            capabilities,
            show_thinking,
            last_error: None,
            themes,
            current_theme_idx,
            providers,
            current_provider_idx: initial_provider_idx,
            palette_focused: 0,
            history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            should_quit: false,
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

    /// The slash palette is visible while the input has focus, no
    /// overlay is open, and the input starts with `/`. Hidden once the
    /// user submits — re-appears when they type `/` again.
    pub fn palette_visible(&self) -> bool {
        self.focus == Focus::Input
            && self.overlay.is_none()
            && self.input.starts_with('/')
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
    /// row matches.
    pub fn palette_matches(&self) -> Vec<&'static CommandCatalogItem> {
        if !self.input.starts_with('/') {
            return Vec::new();
        }
        let needle = self.input.to_ascii_lowercase();
        SLASH_CATALOG
            .iter()
            .filter(|item| item.trigger.starts_with(&needle))
            .collect()
    }

    fn push(&mut self, kind: TranscriptKind, text: String) {
        self.transcript.push(TranscriptLine { kind, text });
        if self.focus == Focus::Input {
            self.scroll_offset = 0;
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
            TuiEvent::AgentWorkerError(msg) => {
                self.push(TranscriptKind::Error, msg);
                Vec::new()
            }
            TuiEvent::ProviderApplied { profile, model } => {
                // Mark this profile as the active one so the picker's
                // ✓ moves immediately. Match by name so applies that
                // happened out-of-band (e.g. CLI) still align.
                self.current_provider_idx = self
                    .providers
                    .iter()
                    .position(|p| p.name == profile);
                self.model_id = model.clone();
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
                        self.input = item.trigger.to_string();
                        self.cursor = self.input.len();
                    }
                }
                Vec::new()
            }
            // Transcript scroll keys work regardless of focus so the
            // user never needs to leave input focus to look back.
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                Vec::new()
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
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
                        self.palette_focused =
                            (self.palette_focused + 1).min(matches_len - 1);
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
                // match, snap the input to that command's trigger so
                // partial typing (`/the`) submits as the full command
                // (`/theme`).
                if self.palette_visible() {
                    let matches = self.palette_matches();
                    if let Some(item) = matches.get(self.palette_focused) {
                        self.input = item.trigger.to_string();
                        self.cursor = self.input.len();
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
                AssistantMessageEvent::ThinkingDelta { delta, .. } if self.show_thinking => {
                    self.append_streaming(TranscriptKind::ThinkingText, &delta);
                }
                _ => {}
            },
            AgentEvent::MessageEnd { message } => {
                if let AgentMessage::Standard(Message::Assistant(_)) = &message {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn fresh() -> AppState {
        AppState::new(
            "deepseek/deepseek-chat".into(),
            "/tmp/proj".into(),
            Capabilities::default(),
            false,
            crate::theme::builtin_themes(),
            0,
            Vec::new(),
            None,
        )
    }

    fn fresh_with_providers(providers: Vec<ProviderProfile>) -> AppState {
        AppState::new(
            "deepseek/deepseek-chat".into(),
            "/tmp/proj".into(),
            Capabilities::default(),
            false,
            crate::theme::builtin_themes(),
            0,
            providers,
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
        assert_eq!(s.transcript.last().unwrap().kind, TranscriptKind::UserPrompt);
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
        assert!(s
            .transcript
            .iter()
            .any(|l| l.text.contains("transcript cleared")));
    }

    #[test]
    fn slash_unknown_logs_info_no_command() {
        let mut s = fresh();
        for c in "/bogus".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let cmds = s.on_event(TuiEvent::Key(press(KeyCode::Enter)));
        assert!(cmds.is_empty());
        assert!(s
            .transcript
            .iter()
            .any(|l| l.text.contains("unknown command")));
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
                .any(|l| l.text.contains("OAuth")
                    && l.text.contains("login flow not yet wired")),
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
        });
        assert_eq!(s.current_provider_idx, Some(1));
        assert_eq!(s.model_id, "anthropic/claude-sonnet-4-5");
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
    fn page_up_and_page_down_scroll_transcript_from_input_focus() {
        let mut s = fresh();
        // Stuff the transcript so scrolling is meaningful.
        for i in 0..30 {
            s.push(TranscriptKind::Info, format!("line {i}"));
        }
        assert_eq!(s.scroll_offset, 0);
        s.on_event(TuiEvent::Key(press(KeyCode::PageUp)));
        assert_eq!(s.scroll_offset, 10);
        s.on_event(TuiEvent::Key(press(KeyCode::PageDown)));
        assert_eq!(s.scroll_offset, 0);
        // PageDown saturates at zero, doesn't underflow.
        s.on_event(TuiEvent::Key(press(KeyCode::PageDown)));
        assert_eq!(s.scroll_offset, 0);
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
        assert!(s
            .transcript
            .iter()
            .any(|l| l.kind == TranscriptKind::ToolCallStart));
        assert!(s
            .transcript
            .iter()
            .any(|l| l.kind == TranscriptKind::ToolCallEnd));
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
        assert!(!s.palette_visible(), "hidden when input pane is not focused");
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
            "bare slash matches every command"
        );

        for c in "the".chars() {
            s.on_event(TuiEvent::Key(press(KeyCode::Char(c))));
        }
        let narrowed = s.palette_matches();
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].trigger, "/theme");
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
}
