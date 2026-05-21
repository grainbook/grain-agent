//! Ratatui renderer. Single entry point: [`draw`].
//!
//! Borderless layout — no `Block::borders(...)` anywhere. Sections are
//! separated by accent-colored title rows + blank padding lines, which
//! reads less like a "form" and more like a chat surface (closer to
//! the upstream Claude Code TUI's look).
//!
//! When `state.input` starts with `/` and the input pane has focus, a
//! transient command-palette dropdown is rendered just above the input
//! line. Up/Down navigate; Enter snaps the input to the highlighted
//! trigger before submitting.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
};

use crate::app::{
    AppState, Focus, Overlay, SLASH_CATALOG, TranscriptKind, TranscriptLine,
};
use grain_llm_genai::{ProviderKind, ProviderProfile};
use crate::theme::{Palette, Theme, ThemeSource};

/// Cap on visible palette rows. Beyond this we slide a window of rows
/// around `palette_focused`.
const PALETTE_MAX_ROWS: u16 = 12;

pub fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    let palette = &state.theme().palette;

    let palette_rows = palette_height(state);
    let constraints: Vec<Constraint> = if palette_rows > 0 {
        vec![
            Constraint::Length(1), // header
            Constraint::Min(1),    // transcript
            Constraint::Length(palette_rows),
            Constraint::Length(1), // input
            Constraint::Length(1), // footer
        ]
    } else {
        vec![
            Constraint::Length(1), // header
            Constraint::Min(1),    // transcript
            Constraint::Length(1), // input
            Constraint::Length(1), // footer
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_header(frame, chunks[0], state, palette);
    draw_transcript(frame, chunks[1], state, palette);
    if palette_rows > 0 {
        draw_palette(frame, chunks[2], state, palette);
        draw_input(frame, chunks[3], state, palette);
        draw_footer(frame, chunks[4], state, palette);
    } else {
        draw_input(frame, chunks[2], state, palette);
        draw_footer(frame, chunks[3], state, palette);
    }

    if let Some(overlay) = &state.overlay {
        draw_overlay(frame, area, overlay, state, palette);
    }
}

/// Number of vertical cells reserved for the palette this frame.
/// Returns 0 when the palette is hidden.
fn palette_height(state: &AppState) -> u16 {
    if !state.palette_visible() {
        return 0;
    }
    let n = state.palette_matches().len() as u16;
    if n == 0 {
        // Reserve one row so we can render a "no matches" hint.
        1
    } else {
        n.min(PALETTE_MAX_ROWS)
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, state: &AppState, palette: &Palette) {
    let mut caps = Vec::new();
    if state.capabilities.allow_write {
        caps.push("write");
    }
    if state.capabilities.allow_bash {
        caps.push("bash");
    }
    if state.capabilities.allow_web {
        caps.push("web");
    }
    if state.capabilities.allow_semantic_search {
        caps.push("semantic");
    }
    let caps_str = if caps.is_empty() {
        "read-only".to_string()
    } else {
        caps.join("+")
    };

    let line = Line::from(vec![
        Span::styled(
            "grain-tui ",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(&state.model_id, Style::default().fg(palette.info)),
        Span::raw("  "),
        Span::styled(&state.workspace_display, Style::default().fg(palette.muted)),
        Span::raw("  ["),
        Span::styled(caps_str, Style::default().fg(palette.warning)),
        Span::raw("]  "),
        Span::styled(
            format!("theme:{}", state.theme().name),
            Style::default().fg(palette.secondary),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, state: &AppState, palette: &Palette) {
    let lines: Vec<Line> = state
        .transcript
        .iter()
        .flat_map(|line| split_for_render(line, palette))
        .collect();

    let visible = area.height as usize;
    let total = lines.len();
    let start = total
        .saturating_sub(visible)
        .saturating_sub(state.scroll_offset);
    let end = start.saturating_add(visible).min(total);
    let slice: Vec<Line> = lines[start..end].to_vec();

    frame.render_widget(
        Paragraph::new(Text::from(slice)).wrap(Wrap { trim: false }),
        area,
    );
}

fn split_for_render(line: &TranscriptLine, palette: &Palette) -> Vec<Line<'static>> {
    let style = style_for_kind(line.kind, palette);
    let prefix = prefix_for_kind(line.kind);
    let mut lines = Vec::new();
    for (i, segment) in line.text.split('\n').enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), style.add_modifier(Modifier::BOLD)),
                Span::styled(segment.to_string(), style),
            ]));
        } else {
            lines.push(Line::from(Span::styled(format!("  {segment}"), style)));
        }
    }
    lines
}

fn style_for_kind(kind: TranscriptKind, palette: &Palette) -> Style {
    match kind {
        TranscriptKind::UserPrompt => Style::default()
            .fg(palette.success)
            .add_modifier(Modifier::BOLD),
        TranscriptKind::AssistantText => Style::default().fg(palette.fg),
        TranscriptKind::ThinkingText => Style::default()
            .fg(palette.muted)
            .add_modifier(Modifier::ITALIC),
        TranscriptKind::ToolCallStart => Style::default().fg(palette.warning),
        TranscriptKind::ToolCallEnd => Style::default().fg(palette.warning),
        TranscriptKind::Info => Style::default().fg(palette.muted),
        TranscriptKind::Error => Style::default()
            .fg(palette.error)
            .add_modifier(Modifier::BOLD),
    }
}

fn prefix_for_kind(kind: TranscriptKind) -> &'static str {
    match kind {
        TranscriptKind::UserPrompt => "› ",
        TranscriptKind::AssistantText => "",
        TranscriptKind::ThinkingText => "· ",
        TranscriptKind::ToolCallStart => "",
        TranscriptKind::ToolCallEnd => "",
        TranscriptKind::Info => "· ",
        TranscriptKind::Error => "✖ ",
    }
}

fn draw_palette(frame: &mut Frame<'_>, area: Rect, state: &AppState, palette: &Palette) {
    let matches = state.palette_matches();
    if matches.is_empty() {
        let line = Line::from(Span::styled(
            "  (no commands match)",
            Style::default().fg(palette.muted),
        ));
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    let visible = area.height as usize;
    let total = matches.len();
    let focused = state.palette_focused.min(total.saturating_sub(1));
    // Sliding window so focused row stays in view.
    let start = if total > visible {
        focused.saturating_sub(visible / 2).min(total - visible)
    } else {
        0
    };
    let end = (start + visible).min(total);

    // Column for the description so triggers align visually.
    let trigger_col_width: usize = matches
        .iter()
        .map(|m| m.trigger.chars().count())
        .max()
        .unwrap_or(0)
        + 2;

    let lines: Vec<Line> = matches[start..end]
        .iter()
        .enumerate()
        .map(|(offset, item)| {
            let i = start + offset;
            let is_focused = i == focused;
            let trigger_style = if is_focused {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.secondary)
            };
            let desc_style = if is_focused {
                Style::default().fg(palette.fg)
            } else {
                Style::default().fg(palette.muted)
            };
            let trigger_text = format!(
                " {} {:<width$}",
                if is_focused { "▶" } else { " " },
                item.trigger,
                width = trigger_col_width
            );
            Line::from(vec![
                Span::styled(trigger_text, trigger_style),
                Span::styled(item.description.to_string(), desc_style),
            ])
        })
        .collect();

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, state: &AppState, palette: &Palette) {
    let prefix_style = if state.focus == Focus::Input {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    let text_style = if state.focus == Focus::Input {
        Style::default().fg(palette.fg)
    } else {
        Style::default().fg(palette.muted)
    };
    let line = Line::from(vec![
        Span::styled("› ", prefix_style),
        Span::styled(state.input.as_str(), text_style),
    ]);
    frame.render_widget(Paragraph::new(line), area);

    if state.focus == Focus::Input && state.overlay.is_none() {
        // "› " is 2 cells wide (one glyph + space).
        let col_offset = 2 + state.input[..state.cursor.min(state.input.len())]
            .chars()
            .count() as u16;
        let cx = area
            .x
            .saturating_add(col_offset)
            .min(area.x + area.width.saturating_sub(1));
        frame.set_cursor_position((cx, area.y));
    }
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, state: &AppState, palette: &Palette) {
    let mut spans = Vec::new();
    if state.streaming {
        spans.push(Span::styled(
            "● streaming",
            Style::default()
                .fg(palette.success)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("  "));
    }
    if state.pending_tool_calls > 0 {
        spans.push(Span::styled(
            format!("⚙ {} tool", state.pending_tool_calls),
            Style::default().fg(palette.warning),
        ));
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(
        "↑↓ history  Tab complete  F1 help  F2 doctor  F3 skills  / commands  Ctrl-C abort  Esc clear/quit",
        Style::default().fg(palette.muted),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    overlay: &Overlay,
    state: &AppState,
    palette: &Palette,
) {
    // Fixed-size cards centered in the terminal. Cap dimensions so
    // overlays never expand to weird sizes on a very large window —
    // wider than ~80 columns hurts readability, and 22 rows comfortably
    // shows everything we render today.
    let (target_w, target_h) = match overlay {
        Overlay::Help => (62, 22),
        Overlay::Doctor { .. } => (84, 26),
        Overlay::Skills(_) => (66, 18),
        Overlay::ThemePicker { .. } => (60, 20),
        Overlay::ProviderPicker { .. } => (72, 18),
    };
    let popup = centered_rect_fixed(target_w, target_h, area);
    // Clear so the transcript underneath doesn't bleed through; then
    // paint the surface bg block so the card stands out against the
    // transcript area (which uses the terminal's default background).
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.surface).fg(palette.fg)),
        popup,
    );
    // Inset 1 cell on every side so content doesn't kiss the edge.
    let inner = popup.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    let (title, body): (&str, OverlayBody) = match overlay {
        Overlay::Help => ("help", OverlayBody::Text(HELP_TEXT.to_string())),
        Overlay::Doctor { report, query, scroll } => {
            return draw_doctor(frame, inner, report, query, *scroll, palette);
        }
        Overlay::Skills(skills) => ("skills", OverlayBody::Skills(skills)),
        Overlay::ThemePicker { focused } => {
            return draw_theme_picker(frame, inner, *focused, state, palette);
        }
        Overlay::ProviderPicker { focused } => {
            return draw_provider_picker(frame, inner, *focused, state, palette);
        }
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Length(1), // blank pad
            Constraint::Min(1),    // body
            Constraint::Length(1), // hint
        ])
        .split(inner);

    let title_line = Line::from(Span::styled(
        title.to_string(),
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    match body {
        OverlayBody::Text(text) => {
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().fg(palette.fg))
                    .wrap(Wrap { trim: false }),
                chunks[2],
            );
        }
        OverlayBody::Skills(skills) => {
            let lines: Vec<Line> = if skills.is_empty() {
                vec![Line::from(Span::styled(
                    "(loading or no skills found)",
                    Style::default().fg(palette.muted),
                ))]
            } else {
                skills
                    .iter()
                    .map(|(name, desc, disabled)| {
                        let mut spans = vec![Span::styled(
                            format!("• {name}"),
                            Style::default()
                                .fg(palette.fg)
                                .add_modifier(Modifier::BOLD),
                        )];
                        if *disabled {
                            spans.push(Span::styled(
                                " [disabled]",
                                Style::default().fg(palette.error),
                            ));
                        }
                        spans.push(Span::raw("  — "));
                        spans.push(Span::styled(
                            desc.clone(),
                            Style::default().fg(palette.muted),
                        ));
                        Line::from(spans)
                    })
                    .collect()
            };
            frame.render_widget(
                Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
                chunks[2],
            );
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "press Esc to close",
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

enum OverlayBody<'a> {
    Text(String),
    Skills(&'a [(String, String, bool)]),
}

fn draw_doctor(
    frame: &mut Frame<'_>,
    popup: Rect,
    report: &str,
    query: &str,
    scroll: usize,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // pad
            Constraint::Length(1), // search input
            Constraint::Length(1), // pad
            Constraint::Min(1),    // body
            Constraint::Length(1), // hint
        ])
        .split(popup);

    let title_line = Line::from(vec![
        Span::styled(
            "doctor",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("({} lines)", report.lines().count()),
            Style::default().fg(palette.muted),
        ),
    ]);
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    // Search bar with caret. Empty query shows a placeholder.
    let search_line = if query.is_empty() {
        Line::from(vec![
            Span::styled("⌕ ", Style::default().fg(palette.accent)),
            Span::styled(
                "type to filter (e.g. ANTHROPIC, deepseek, branch) …",
                Style::default().fg(palette.muted),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("⌕ ", Style::default().fg(palette.accent)),
            Span::styled(query.to_string(), Style::default().fg(palette.fg)),
            Span::styled("▌", Style::default().fg(palette.accent)),
        ])
    };
    frame.render_widget(Paragraph::new(search_line), chunks[2]);

    // Filter the report. Empty query → keep every line. Otherwise
    // case-insensitive substring match, except section headers
    // (lines starting with `===`) are always retained so the user
    // doesn't lose orientation while filtering.
    let needle = query.to_ascii_lowercase();
    let filtered: Vec<&str> = if needle.is_empty() {
        report.lines().collect()
    } else {
        report
            .lines()
            .filter(|line| {
                line.trim().starts_with("===")
                    || line.to_ascii_lowercase().contains(&needle)
            })
            .collect()
    };

    let body_area = chunks[4];
    let visible = body_area.height as usize;
    let total = filtered.len();
    let max_scroll = total.saturating_sub(visible);
    let start = scroll.min(max_scroll);
    let end = (start + visible).min(total);
    let slice = &filtered[start..end];

    let lines: Vec<Line> = slice
        .iter()
        .map(|line| {
            // Headers (=== … ===) get accent color so the filtered
            // view still reads like a structured report.
            let style = if line.trim().starts_with("===") {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else if needle.is_empty() {
                Style::default().fg(palette.fg)
            } else if line.to_ascii_lowercase().contains(&needle) {
                Style::default()
                    .fg(palette.fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.fg)
            };
            Line::from(Span::styled((*line).to_string(), style))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        body_area,
    );

    let hint = if total == 0 {
        format!("(no lines match \"{query}\") · Esc to close")
    } else if max_scroll > 0 {
        format!(
            "showing {}-{} of {} · ↑↓/PgUp/PgDn scroll · Esc close",
            start + 1,
            end,
            total
        )
    } else {
        "↑↓ scroll · Esc close".to_string()
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(palette.muted),
        ))),
        chunks[5],
    );
}

fn draw_provider_picker(
    frame: &mut Frame<'_>,
    popup: Rect,
    focused: usize,
    state: &AppState,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // pad
            Constraint::Min(1),    // list
            Constraint::Length(1), // hint
        ])
        .split(popup);

    let title_line = Line::from(vec![
        Span::styled(
            "provider",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("({} profiles)", state.providers.len()),
            Style::default().fg(palette.muted),
        ),
    ]);
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    let body_area = chunks[2];
    if state.providers.is_empty() {
        let line = Line::from(Span::styled(
            "(no profiles — create <workspace>/.grain/providers.toml)",
            Style::default().fg(palette.muted),
        ));
        frame.render_widget(Paragraph::new(line), body_area);
    } else {
        let lines: Vec<Line> = state
            .providers
            .iter()
            .enumerate()
            .map(|(i, p)| {
                provider_picker_row(
                    i,
                    p,
                    focused,
                    state.current_provider_idx,
                    palette,
                )
            })
            .collect();
        let visible = body_area.height as usize;
        let total = lines.len();
        let start = if total > visible {
            focused.saturating_sub(visible / 2).min(total - visible)
        } else {
            0
        };
        let end = (start + visible).min(total);
        let slice: Vec<Line> = lines[start..end].to_vec();
        frame.render_widget(
            Paragraph::new(Text::from(slice)).wrap(Wrap { trim: false }),
            body_area,
        );
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑↓ navigate · Enter apply · Esc cancel",
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

fn provider_picker_row(
    i: usize,
    profile: &ProviderProfile,
    focused: usize,
    current: Option<usize>,
    palette: &Palette,
) -> Line<'static> {
    let cursor = if i == focused { "▶ " } else { "  " };
    let mark = if Some(i) == current { "✓ " } else { "  " };
    let kind_label = match profile.kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenAi => "openai",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenAiCompat => "compat",
    };
    let usable = profile.auth.is_usable();
    let status_tag = if usable {
        match &profile.auth {
            grain_llm_genai::ProviderAuth::ApiKey { env } => {
                if std::env::var(env)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .is_some()
                {
                    "[ready]".to_string()
                } else {
                    "[no key]".to_string()
                }
            }
            _ => "[ready]".to_string(),
        }
    } else {
        "[needs login]".to_string()
    };
    let status_color = if !usable {
        palette.muted
    } else if status_tag == "[no key]" {
        palette.warning
    } else {
        palette.success
    };
    let row_style = if i == focused {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else if usable {
        Style::default().fg(palette.fg)
    } else {
        Style::default().fg(palette.muted)
    };
    Line::from(vec![
        Span::styled(format!("{cursor}{mark}"), row_style),
        Span::styled(profile.name.clone(), row_style),
        Span::raw("  "),
        Span::styled(
            format!("[{kind_label}]"),
            Style::default().fg(palette.secondary),
        ),
        Span::raw("  "),
        Span::styled(profile.model.clone(), Style::default().fg(palette.muted)),
        Span::raw("  "),
        Span::styled(status_tag, Style::default().fg(status_color)),
    ])
}

fn draw_theme_picker(
    frame: &mut Frame<'_>,
    popup: Rect,
    focused: usize,
    state: &AppState,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // pad
            Constraint::Min(1),    // list
            Constraint::Length(1), // hint
        ])
        .split(popup);

    let title_line = Line::from(Span::styled(
        "theme",
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    let lines: Vec<Line> = state
        .themes
        .iter()
        .enumerate()
        .map(|(i, t)| theme_picker_row(i, t, focused, state.current_theme_idx, palette))
        .collect();
    let visible = chunks[2].height as usize;
    let total = lines.len();
    let start = if total > visible {
        focused.saturating_sub(visible / 2).min(total - visible)
    } else {
        0
    };
    let end = (start + visible).min(total);
    let slice: Vec<Line> = lines[start..end].to_vec();
    frame.render_widget(
        Paragraph::new(Text::from(slice)).wrap(Wrap { trim: false }),
        chunks[2],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑↓ navigate · Enter apply · Esc cancel",
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

fn theme_picker_row(
    i: usize,
    theme: &Theme,
    focused: usize,
    current: usize,
    palette: &Palette,
) -> Line<'static> {
    let cursor = if i == focused { "▶ " } else { "  " };
    let mark = if i == current { "✓ " } else { "  " };
    let source_tag = match theme.source {
        ThemeSource::BuiltIn => "[built-in]",
        ThemeSource::User => "[user]",
    };
    let row_style = if i == focused {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.fg)
    };
    Line::from(vec![
        Span::styled(format!("{cursor}{mark}"), row_style),
        Span::styled(theme.name.clone(), row_style),
        Span::raw("  "),
        Span::styled(source_tag, Style::default().fg(palette.muted)),
        Span::raw("  "),
        Span::styled("█", Style::default().fg(theme.palette.accent)),
        Span::styled("█", Style::default().fg(theme.palette.secondary)),
        Span::styled("█", Style::default().fg(theme.palette.success)),
        Span::styled("█", Style::default().fg(theme.palette.warning)),
        Span::styled("█", Style::default().fg(theme.palette.error)),
        Span::styled("█", Style::default().fg(theme.palette.info)),
    ])
}

const HELP_TEXT: &str = "\
  Enter           submit prompt (or accept slash palette pick)
  Esc             close overlay; else clear input; else quit
  Tab             complete the selected slash command (palette open)
  Ctrl-C          abort current turn while streaming; quit when idle
  F1 / F2 / F3    help · doctor · skills
  ←/→/Home/End    move cursor in input
  ↑/↓             history (no palette) · navigate (palette / picker)
  PgUp / PgDn     scroll transcript

slash commands

  /help, /?        open this overlay
  /clear, /reset   clear transcript
  /doctor          show doctor report
  /skills          list skills
  /theme           open theme picker
  /exit, /quit, /q quit
";

#[allow(dead_code)]
fn touch_unused() {
    // Keep import warnings honest for items only used in tests.
    let _ = SLASH_CATALOG;
}

/// Center a fixed-size popup inside `r`. If the terminal is smaller
/// than the requested size on either axis, the popup shrinks to fit
/// (rather than overflowing).
fn centered_rect_fixed(width: u16, height: u16, r: Rect) -> Rect {
    let w = width.min(r.width);
    let h = height.min(r.height);
    let x = r.x + (r.width.saturating_sub(w)) / 2;
    let y = r.y + (r.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}
