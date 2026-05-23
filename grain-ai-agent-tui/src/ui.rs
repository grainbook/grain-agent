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
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{AppState, Focus, Overlay, SLASH_CATALOG, TranscriptKind, TranscriptLine};
use crate::theme::{Palette, Theme, ThemeSource};
use grain_llm_genai::{ProviderKind, ProviderProfile};

/// Cap on visible palette rows. Beyond this we slide a window of rows
/// around `palette_focused`.
const PALETTE_MAX_ROWS: u16 = 12;

/// Cap on the input box's vertical growth. Past this we stop adding
/// rows even if the input keeps wrapping — the transcript needs the
/// room. Cursor stays parked on the last visible row; users who want
/// to see more can scroll back over the input contents with arrow
/// keys (the buffer itself is unbounded).
const INPUT_MAX_ROWS: u16 = 8;

/// Visual width of the input prompt prefix `"› "`. Two cells (one
/// glyph + one space).
const INPUT_PREFIX_COLS: u16 = 2;

/// Cap on header height when its content wraps (long model id +
/// workspace path can overflow narrow terminals). Past this we stop
/// growing; the transcript needs the room.
const HEADER_MAX_ROWS: u16 = 3;

/// Cap on footer height when the status chip row wraps. The hint
/// text now lives in `/help` (F1), so on standard-width terminals
/// everything fits on a single line; we still allow a second row
/// for very narrow widths or when many chips (spinner + tokens +
/// cost + tool count + Σ + ctx + msg + compact) are active at once.
const FOOTER_MAX_ROWS: u16 = 2;

pub fn draw(frame: &mut Frame<'_>, state: &mut AppState, elapsed: crate::anim::FxDuration) {
    let area = frame.area();
    // Stash full frame area early so set_overlay can size effect rects.
    {
        let mut m = state.render_metrics.get();
        m.full_area = area;
        state.render_metrics.set(m);
    }
    let palette = state.theme().palette; // Palette is Copy

    let palette_rows = palette_height(state);
    // Dynamic heights. The ratatui "dynamic layout" recipe:
    // give the flex pane `Constraint::Min(1)` and every other chunk
    // a `Constraint::Length(known_height)`. We build header / footer
    // paragraphs once, measure them with `Paragraph::line_count(width)`
    // (gated on the `unstable-rendered-line-info` cargo feature, which
    // this crate already opts into), cap each, then render them at
    // their layout chunks below. Net effect: a narrower terminal makes
    // the long footer status line wrap into 2-3 rows instead of being
    // sliced off; the transcript shrinks to compensate.
    let header_rows = {
        let h = build_header_paragraph(state, &palette);
        (h.line_count(area.width) as u16).clamp(1, HEADER_MAX_ROWS)
    };
    let footer_rows = {
        let f = build_footer_paragraph(state, &palette);
        (f.line_count(area.width) as u16).clamp(1, FOOTER_MAX_ROWS)
    };
    let input_rows = input_height(state, area.width);
    // Ephemeral status slot — 1 row above the input box when set, 0
    // rows (no slot) when empty. Replace-in-place, never appended to
    // the transcript: keeps retry-on-overflow progress visible without
    // stacking N warns per turn.
    let status_rows: u16 = if state.ephemeral_status.is_some() { 1 } else { 0 };
    // Visual spacer between transcript and input when there is content.
    let spacer_rows: u16 = if state.transcript.len() > 1 { 1 } else { 0 };
    let constraints: Vec<Constraint> = if palette_rows > 0 {
        vec![
            Constraint::Length(header_rows),  // header (dynamic)
            Constraint::Min(1),               // transcript (flex)
            Constraint::Length(palette_rows), // slash palette
            Constraint::Length(status_rows),  // ephemeral status
            Constraint::Length(spacer_rows),  // spacer
            Constraint::Length(input_rows),   // input (dynamic)
            Constraint::Length(footer_rows),  // footer (dynamic)
        ]
    } else {
        vec![
            Constraint::Length(header_rows),  // header (dynamic)
            Constraint::Min(1),               // transcript (flex)
            Constraint::Length(status_rows),  // ephemeral status
            Constraint::Length(spacer_rows),  // spacer
            Constraint::Length(input_rows),   // input (dynamic)
            Constraint::Length(footer_rows),  // footer (dynamic)
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    // Header: rebuild + render (no mutable borrow here)
    frame.render_widget(build_header_paragraph(state, &palette), chunks[0]);
    // Transcript + mutable borrow (for markdown cache)
    draw_transcript(frame, chunks[1], state, &palette);
    if palette_rows > 0 {
        draw_palette(frame, chunks[2], state, &palette);
        draw_status(frame, chunks[3], state, &palette);
        // chunks[4] = spacer (blank)
        draw_input(frame, chunks[5], state, &palette);
        frame.render_widget(build_footer_paragraph(state, &palette), chunks[6]);
    } else {
        draw_status(frame, chunks[2], state, &palette);
        // chunks[3] = spacer (blank)
        draw_input(frame, chunks[4], state, &palette);
        frame.render_widget(build_footer_paragraph(state, &palette), chunks[5]);
    }

    if let Some(overlay) = &state.overlay {
        draw_overlay(frame, area, overlay, state, &palette);
    }

    // Process tachyonfx effects last so they paint on top of everything.
    state.effects.process_frame(frame.buffer_mut(), elapsed);
}

/// Compute the input box's vertical height in rows for this frame.
/// One row at minimum (the cursor always needs somewhere to live),
/// growing with char-wrapped content, capped at [`INPUT_MAX_ROWS`].
fn input_height(state: &AppState, area_width: u16) -> u16 {
    let lines = wrap_input_to_lines(&state.input, area_width).len() as u16;
    lines.clamp(1, INPUT_MAX_ROWS)
}

/// Char-wrap (not word-wrap) `input` to a column budget so the cursor
/// math stays predictable. The first row reserves
/// [`INPUT_PREFIX_COLS`] for the prompt prefix `"› "`; continuation
/// rows start flush left. Wide glyphs (CJK, emoji) cost two cells per
/// `UnicodeWidthChar::width`. Newline characters (`\n`) force a hard
/// break (we may not have multi-line input today, but keep the
/// invariant correct for future paste-into-input flows).
fn wrap_input_to_lines(input: &str, area_width: u16) -> Vec<String> {
    let width = area_width.max(INPUT_PREFIX_COLS + 1);
    let mut lines: Vec<String> = vec![String::new()];
    let mut col: u16 = INPUT_PREFIX_COLS;
    for ch in input.chars() {
        if ch == '\n' {
            lines.push(String::new());
            col = 0;
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if col + w > width {
            lines.push(String::new());
            col = 0;
        }
        lines.last_mut().expect("just pushed").push(ch);
        col += w;
    }
    lines
}

/// Map a byte cursor inside `state.input` to a wrapped `(row, col)`
/// position relative to the input area's top-left corner. `col`
/// includes the prefix offset on row 0. Returns `(0, INPUT_PREFIX_COLS)`
/// for an empty input. Caller is responsible for clamping when the
/// total row count exceeds [`INPUT_MAX_ROWS`] (the cursor pins to the
/// last visible row in that case).
fn input_cursor_offset(input: &str, byte_cursor: usize, area_width: u16) -> (u16, u16) {
    let width = area_width.max(INPUT_PREFIX_COLS + 1);
    let mut row: u16 = 0;
    let mut col: u16 = INPUT_PREFIX_COLS;
    let cursor = byte_cursor.min(input.len());
    let mut bytes_consumed = 0usize;
    for ch in input.chars() {
        if bytes_consumed >= cursor {
            break;
        }
        if ch == '\n' {
            row = row.saturating_add(1);
            col = 0;
            bytes_consumed += ch.len_utf8();
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if col + w > width {
            row = row.saturating_add(1);
            col = 0;
        }
        col += w;
        bytes_consumed += ch.len_utf8();
    }
    (row, col)
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

/// Construct the header paragraph. Wraps on `Wrap { trim: false }` so
/// `Paragraph::line_count(width)` returns the right height for the
/// dynamic layout in [`draw`]; under wide terminals the result is
/// always 1 row.
fn build_header_paragraph<'a>(state: &'a AppState, palette: &Palette) -> Paragraph<'a> {
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
    Paragraph::new(line).wrap(Wrap { trim: false })
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, state: &mut AppState, palette: &Palette) {
    // Stash the pane bounds so mouse handlers can translate event
    // coordinates back into rendered-row indices.
    state.transcript_area.set(area);
    let width = area.width as usize;

    // Pre-wrap with `textwrap::wrap` instead of relying on
    // `Paragraph::wrap`: we need the wrapped output as plain text so
    // mouse handlers + selection highlighting can compute precise
    // (row, col) coordinates. Doing our own wrap also lets us track
    // each row's `TranscriptKind` for per-row styling.
    //
    // The outer loop walks **blocks** (returned by
    // `build_transcript_blocks`) rather than raw lines. Foldable
    // blocks (tool calls, thinking) render either as one collapsed
    // summary line or as an expanded header + child lines, driven
    // by `AppState::is_block_expanded`.
    let blocks = state.cached_blocks();
    let mut rendered: Vec<crate::app::RenderedRow> = Vec::new();
    for block in blocks {
        // Hard-hide thinking blocks when the legacy `show_thinking`
        // toggle is off — that key (F5) historically removed them
        // from the buffer entirely; fold semantics still apply to
        // them when they're visible.
        if block.kind == crate::app::BlockKind::Thinking && !state.show_thinking {
            continue;
        }
        let foldable = block.is_foldable();
        let expanded = state.is_block_expanded(&block);
        let focused = state.transcript_cursor == Some(block.id());
        // Cursor mark renders as "▶" before the fold glyph so the
        // user can see at a glance which block the next Space
        // press will toggle.
        let cursor_mark = if focused { "▶" } else { " " };
        if foldable && !expanded {
            // Single summary line replaces the whole block.
            let summary = format!(
                "{cursor_mark}▸ {}",
                state.block_summary(&block)
            );
            wrap_one_line(
                &TranscriptLine {
                    kind: block_chrome_kind(block.kind),
                    text: summary,
                },
                width,
                Some(block.id()),
                &mut rendered,
                None,
            );
            continue;
        }
        if foldable && expanded {
            // Header row tells the user the block is open + how
            // many child lines fall under it. Child lines follow,
            // each indented.
            let header = format!(
                "{cursor_mark}▾ {}",
                state.block_summary(&block)
            );
            wrap_one_line(
                &TranscriptLine {
                    kind: block_chrome_kind(block.kind),
                    text: header,
                },
                width,
                Some(block.id()),
                &mut rendered,
                None,
            );
        }
        for idx in block.first_line..=block.last_line {
            let line = state.transcript.get(idx).cloned();
            if let Some(line) = line {
                let md = md_spans_for_line(&line, idx, &block, state);
                wrap_one_line(&line, width, None, &mut rendered, md.as_deref());
            }
        }
    }

    let total_rows = rendered.len();
    let visible = area.height as usize;
    state.render_metrics.set(crate::app::RenderMetrics {
        total_rows,
        visible_rows: visible,
        full_area: state.render_metrics.get().full_area,
    });
    let skip = if state.follow_bottom {
        total_rows.saturating_sub(visible)
    } else {
        state.scroll_offset.min(total_rows.saturating_sub(visible))
    };

    // Build the visible ratatui `Line`s with selection-aware
    // highlighting. Only the slice [skip, skip+visible) renders.
    let lines: Vec<Line> = rendered
        .iter()
        .enumerate()
        .skip(skip)
        .take(visible)
        .map(|(idx, row)| build_line(row, idx, state.selection, palette))
        .collect();

    // Hand the wrapped rows over to AppState — mouse handlers consume
    // them on the next event (translate / extract-on-mouse-up).
    state.rendered_rows.replace(rendered);

    // No `.wrap()` here — we already wrapped to `area.width` so
    // ratatui's word-wrap is unnecessary (and would re-wrap our
    // already-sized rows).
    let paragraph = Paragraph::new(Text::from(lines));
    frame.render_widget(paragraph, area);
}

/// Decide whether `line` should get markdown rendering and return
/// the parsed spans if so. Uses [`MarkdownCache`] so completed lines
/// aren't re-parsed every frame.
fn md_spans_for_line(
    line: &TranscriptLine,
    idx: usize,
    block: &crate::app::TranscriptBlock,
    state: &mut AppState,
) -> Option<Vec<crate::md_render::MdStyledSpan>> {
    use crate::md_render::{looks_like_markdown, render_md_to_spans};
    // Only render AssistText / ThinkingText through the md pipeline.
    if !matches!(
        line.kind,
        TranscriptKind::AssistantText | TranscriptKind::ThinkingText
    ) {
        return None;
    }
    // Skip empty lines and lines without markdown triggers.
    if line.text.is_empty() || !looks_like_markdown(&line.text) {
        return None;
    }
    // Is this the last line of a streaming block?
    let is_streaming =
        state.streaming && idx == block.last_line && idx == state.transcript.len().saturating_sub(1);
    if is_streaming {
        // Tail-buffer (streamdown-style): the streaming tail is the
        // line whose markdown state changes shape on every delta —
        // an unclosed `**` becomes bold once the closer arrives, a
        // partial code fence flips the whole rest of the document
        // into a code block until the second fence lands, and so
        // on. Parsing it every frame produces (a) source-vs-
        // rendered byte-offset drift that historically panicked
        // `spans_for_fragment`, and (b) visible flicker where
        // styles rapid-fire on/off as the stream catches up.
        //
        // Defer markdown styling until the line is *finalized*
        // (i.e. `state.streaming` flips false, typically on
        // `AgentEvent::MessageEnd`). On the first post-stream
        // frame the cache branch below parses + memoizes it.
        return None;
    } else {
        // Ensure cache capacity.
        state.markdown_cache.resize(state.transcript.len());
        // Check cache.
        if let Some(Some(cached)) = state.markdown_cache.entries.get(idx) {
            return Some(cached.clone());
        }
        // Parse and cache.
        let spans = render_md_to_spans(&line.text);
        state.markdown_cache.resize(state.transcript.len());
        if idx < state.markdown_cache.entries.len() {
            state.markdown_cache.entries[idx] = Some(spans.clone());
        }
        Some(spans)
    }
}

/// Wrap one `TranscriptLine` into 1+ rendered rows, identical to
/// what the legacy in-line loop did. Extracted so the block-aware
/// outer loop can reuse it both for raw transcript lines and for
/// synthetic fold-header / fold-summary lines.
///
/// `chrome_for_block` is propagated onto every wrapped fragment so
/// mouse handlers can recognize **any** sub-row of a chrome line as
/// "click here toggles the fold". (Most chrome lines fit on one
/// terminal row, but very wide terminals + long tool names can
/// still wrap.)
///
/// When `md_spans` is `Some`, each wrapped fragment also carries the
/// full markdown span list + its byte range so [`build_line`] can
/// produce styled ratatui `Span`s.
fn wrap_one_line(
    line: &TranscriptLine,
    width: usize,
    chrome_for_block: Option<usize>,
    rendered: &mut Vec<crate::app::RenderedRow>,
    md_spans: Option<&[crate::md_render::MdStyledSpan]>,
) {
    let prefix = prefix_for_kind(line.kind);
    let continuation = "  ";
    let prefix_width = UnicodeWidthStr::width(prefix);
    let cont_width = UnicodeWidthStr::width(continuation);

    if let Some(md_spans) = md_spans {
        wrap_markdown_line(
            line.kind,
            width,
            chrome_for_block,
            rendered,
            md_spans,
            prefix,
            continuation,
            prefix_width,
            cont_width,
        );
        return;
    }

    for (seg_i, segment) in line.text.split('\n').enumerate() {
        let initial_prefix = if seg_i == 0 { prefix } else { continuation };
        let initial_prefix_width = if seg_i == 0 { prefix_width } else { cont_width };

        let inner = width
            .saturating_sub(initial_prefix_width)
            .max(1);
        let wrapped: Vec<String> = if segment.is_empty() {
            vec![String::new()]
        } else {
            textwrap::wrap(segment, inner)
                .into_iter()
                .map(|c| c.into_owned())
                .collect()
        };

        for (frag_i, frag) in wrapped.into_iter().enumerate() {
            let p = if frag_i == 0 {
                initial_prefix
            } else {
                continuation
            };
            let text = format!("{p}{frag}");
            rendered.push(crate::app::RenderedRow {
                text,
                kind: line.kind,
                chrome_for_block,
                md_spans: None,
            });
        }
    }
}

fn wrap_markdown_line(
    kind: TranscriptKind,
    width: usize,
    chrome_for_block: Option<usize>,
    rendered: &mut Vec<crate::app::RenderedRow>,
    md_spans: &[crate::md_render::MdStyledSpan],
    prefix: &str,
    continuation: &str,
    prefix_width: usize,
    cont_width: usize,
) {
    let display_text = markdown_plain_text(md_spans);

    let mut segment_start = 0usize;
    for (seg_i, segment) in display_text.split('\n').enumerate() {
        let initial_prefix = if seg_i == 0 { prefix } else { continuation };
        let initial_prefix_width = if seg_i == 0 { prefix_width } else { cont_width };
        let inner = width
            .saturating_sub(initial_prefix_width)
            .max(1);
        let wrapped: Vec<String> = if segment.is_empty() {
            vec![String::new()]
        } else {
            textwrap::wrap(segment, inner)
                .into_iter()
                .map(|c| c.into_owned())
                .collect()
        };
        let ranges = wrapped_fragment_ranges(segment, &wrapped);

        for (frag_i, frag) in wrapped.into_iter().enumerate() {
            let p = if frag_i == 0 {
                initial_prefix
            } else {
                continuation
            };
            let (frag_start, frag_end) = ranges[frag_i];
            rendered.push(crate::app::RenderedRow {
                text: format!("{p}{frag}"),
                kind,
                chrome_for_block,
                md_spans: Some((
                    md_spans.to_vec(),
                    segment_start + frag_start,
                    segment_start + frag_end,
                )),
            });
        }

        segment_start = segment_start
            .saturating_add(segment.len())
            .saturating_add(1);
    }
}

fn markdown_plain_text(spans: &[crate::md_render::MdStyledSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn wrapped_fragment_ranges(segment: &str, wrapped: &[String]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::with_capacity(wrapped.len());
    let mut search_from = 0usize;
    for frag in wrapped {
        if frag.is_empty() {
            ranges.push((
                search_from.min(segment.len()),
                search_from.min(segment.len()),
            ));
            continue;
        }
        let start = segment
            .get(search_from..)
            .and_then(|tail| tail.find(frag).map(|rel| search_from + rel))
            .unwrap_or_else(|| search_from.min(segment.len()));
        let end = start.saturating_add(frag.len()).min(segment.len());
        ranges.push((start, end));
        search_from = end;
    }
    ranges
}


/// Pick a chrome `TranscriptKind` for a synthetic fold header /
/// summary line. We borrow an existing kind so style mapping +
/// prefix glyphs reuse the established palette.
fn block_chrome_kind(kind: crate::app::BlockKind) -> TranscriptKind {
    match kind {
        crate::app::BlockKind::ToolCall => TranscriptKind::ToolCallStart,
        crate::app::BlockKind::Thinking => TranscriptKind::ThinkingText,
        crate::app::BlockKind::Plain => TranscriptKind::Info,
    }
}

/// Build one rendered `Line` with the kind's base style + optional
/// selection-background highlight. When `row.md_spans` is `Some`,
/// renders styled sub-spans (bold, italic, code, etc.) instead of a
/// single plain-text span.
fn build_line(
    row: &crate::app::RenderedRow,
    idx: usize,
    selection: Option<crate::app::Selection>,
    palette: &Palette,
) -> Line<'static> {
    let style = style_for_kind(row.kind, palette);

    // If we have markdown spans, build styled sub-spans.
    if let Some((ref spans, frag_start, frag_end)) = row.md_spans {
        let mut sub_spans =
            crate::md_render::spans_for_fragment(spans, frag_start, frag_end, style, palette);
        let content_len: usize = sub_spans.iter().map(|span| span.content.len()).sum();
        let prefix_len = clamp_char_boundary(
            &row.text,
            row.text.len().saturating_sub(content_len),
        );
        if prefix_len > 0 {
            sub_spans.insert(
                0,
                Span::styled(row.text[..prefix_len].to_string(), style),
            );
        }
        // Apply selection highlight across the sub-spans if needed.
        if let Some(s) = selection {
            if let Some((lo, hi)) = s.col_range_for_row(idx, row.text.len()) {
                sub_spans = apply_highlight_to_spans(sub_spans, lo, hi, palette);
            }
        }
        return Line::from(sub_spans);
    }

    let highlight = selection.and_then(|s| s.col_range_for_row(idx, row.text.len()));
    let Some((lo, hi)) = highlight else {
        return Line::from(Span::styled(row.text.clone(), style));
    };
    // Snap to UTF-8 char boundaries; multi-byte chars must stay intact.
    let lo = clamp_char_boundary(&row.text, lo);
    let hi = clamp_char_boundary(&row.text, hi);
    let highlight_style = style.bg(palette.surface);
    Line::from(vec![
        Span::styled(row.text[..lo].to_string(), style),
        Span::styled(row.text[lo..hi].to_string(), highlight_style),
        Span::styled(row.text[hi..].to_string(), style),
    ])
}

/// Apply selection-background highlight to a list of styled spans.
fn apply_highlight_to_spans(
    spans: Vec<Span<'static>>,
    highlight_start: usize,
    highlight_end: usize,
    palette: &Palette,
) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut byte_offset = 0usize;
    for span in spans {
        let span_start = byte_offset;
        let span_end = byte_offset + span.content.len();
        if span_end <= highlight_start || span_start >= highlight_end {
            // No overlap — pass through unchanged.
            out.push(span);
        } else {
            let lo = highlight_start.saturating_sub(span_start);
            let hi = highlight_end.min(span_end) - span_start;
            let hi = hi.min(span.content.len());
            let lo = clamp_char_boundary(&span.content, lo);
            let hi = clamp_char_boundary(&span.content, hi);
            let highlight_style = span.style.bg(palette.surface);
            if lo > 0 {
                out.push(Span::styled(
                    span.content[..lo].to_string(),
                    span.style,
                ));
            }
            if hi > lo {
                out.push(Span::styled(
                    span.content[lo..hi].to_string(),
                    highlight_style,
                ));
            }
            if hi < span.content.len() {
                out.push(Span::styled(
                    span.content[hi..].to_string(),
                    span.style,
                ));
            }
        }
        byte_offset = span_end;
    }
    out
}

fn clamp_char_boundary(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    if s.is_char_boundary(idx) {
        idx
    } else {
        // Walk back to the nearest boundary.
        let mut i = idx.saturating_sub(1);
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
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
        // Claude-Code-style tool-call rendering. The `●` bullet is
        // already embedded in the line text, so coloring the whole
        // row tints the bullet too. Success row uses `success` so the
        // bullet reads as green; the `⎿` continuation rides on
        // `muted` for subdued read; failures get the full red error
        // palette via `ToolCallError`.
        TranscriptKind::ToolCallStart => Style::default()
            .fg(palette.success)
            .add_modifier(Modifier::BOLD),
        TranscriptKind::ToolCallEnd => Style::default().fg(palette.muted),
        TranscriptKind::ToolCallError => Style::default()
            .fg(palette.error)
            .add_modifier(Modifier::BOLD),
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
        // Tool-call rows already carry their own visual prefix
        // (`● ` / `  ⎿  `) in the line text, so no extra glyph here.
        TranscriptKind::ToolCallStart => "",
        TranscriptKind::ToolCallEnd => "",
        TranscriptKind::ToolCallError => "",
        TranscriptKind::Info => "· ",
        TranscriptKind::Error => "✖ ",
    }
}

fn draw_palette(frame: &mut Frame<'_>, area: Rect, state: &AppState, palette: &Palette) {
    let matches = state.palette_matches();
    if matches.is_empty() {
        let line = Line::from(Span::styled(
            "  (no matches)",
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

/// Render the single-row ephemeral status slot above the input box.
/// When `state.ephemeral_status` is `None`, `area` is given `0` rows
/// (no slot allocated) — see the constraint setup in [`draw`].
fn draw_status(frame: &mut Frame<'_>, area: Rect, state: &AppState, palette: &Palette) {
    let Some(text) = state.ephemeral_status.as_ref() else {
        return;
    };
    if area.height == 0 {
        return;
    }
    let style = Style::default()
        .fg(palette.warning)
        .add_modifier(Modifier::DIM);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text.clone(), style))),
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
    // Char-wrap so the cursor math stays in sync with what we render:
    // the helper returns one `String` per visual row, with the prefix
    // already accounted for on row 0.
    let wrapped = wrap_input_to_lines(&state.input, area.width);
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(wrapped.len());
    for (i, segment) in wrapped.iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled("› ", prefix_style),
                Span::styled(segment.clone(), text_style),
            ]));
        } else {
            lines.push(Line::from(Span::styled(segment.clone(), text_style)));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);

    if state.focus == Focus::Input && state.overlay.is_none() {
        // Cursor position depends on which wrapped row the byte cursor
        // lands on. `input_cursor_offset` returns `(row, col)` in the
        // input area's local coordinate space; we clamp the row to
        // [`INPUT_MAX_ROWS`] - 1 so the cursor pins to the bottom when
        // input has grown past the visible budget (rare — input rows
        // are dynamic, so the cap kicks in only when transcript would
        // be squeezed below 1 row).
        let (row, col) = input_cursor_offset(&state.input, state.cursor, area.width);
        let max_row = area.height.saturating_sub(1);
        let cursor_row = row.min(max_row);
        let cursor_col = col.min(area.width.saturating_sub(1));
        frame.set_cursor_position((
            area.x.saturating_add(cursor_col),
            area.y.saturating_add(cursor_row),
        ));
    }
}

/// Construct the footer paragraph. Wraps with `Wrap { trim: false }`
/// so long status (spinner + tokens + cost + tool count + Σ + hint)
/// gracefully grows downward on narrow terminals instead of clipping
/// off-screen. Dynamic height is computed in [`draw`] via
/// `Paragraph::line_count(width)`.
fn build_footer_paragraph<'a>(state: &'a AppState, palette: &Palette) -> Paragraph<'a> {
    let mut spans = Vec::new();
    if state.streaming {
        // Build a Claude-Code-style spinner: rotating verb + elapsed
        // + cumulative token usage + cache-hit rate. Refreshes on
        // every tick because the elapsed counter ticks live.
        let elapsed = state
            .streaming_started_at
            .map(|t| t.elapsed())
            .unwrap_or_default();
        let verb = pick_thinking_verb(elapsed);
        let elapsed_str = format_elapsed(elapsed);
        let token_str = if state.tokens_in > 0 || state.tokens_out > 0 {
            format!(
                " · ↑ {} · ↓ {} tokens",
                format_tokens(state.tokens_in),
                format_tokens(state.tokens_out),
            )
        } else {
            String::new()
        };
        let cache_str = if state.tokens_in > 0 {
            let rate = format_cache_rate(state.tokens_cache_read, state.tokens_in);
            // Append a small "↓!" marker when drop detection flagged
            // a mid-session prefix mutation. The colored chip below
            // does the heavy lifting; this marker survives in plain-
            // text logs / screen scrapes that lose color.
            if state.cache_dropped {
                format!(" · cache {rate}↓!")
            } else {
                format!(" · cache {rate}")
            }
        } else {
            String::new()
        };
        let spinner = spinning_char(elapsed);
        spans.push(Span::styled(
            format!("{spinner} {verb}… ({elapsed_str}{token_str}{cache_str})"),
            Style::default()
                .fg(if state.cache_dropped {
                    palette.error
                } else {
                    palette.warning
                })
                .add_modifier(Modifier::BOLD),
        ));
        // Cost chip rendered as its own span so the color can swing
        // independently of the spinner (green / yellow / red).
        // Suppressed when pricing is unknown (all-zero `Cost`) or no
        // tokens have accrued yet.
        let usage_snapshot = grain_agent_core::Usage {
            input: state.tokens_in,
            output: state.tokens_out,
            cache_read: state.tokens_cache_read,
            ..grain_agent_core::Usage::default()
        };
        let cost_usd = state.model_cost.cost_for(&usage_snapshot);
        if cost_usd > 0.0 {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format_cost_localized(cost_usd, state.cny_rate),
                Style::default()
                    .fg(cost_color(cost_usd, palette))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::raw("  "));
    }
    if state.pending_tool_calls > 0 {
        spans.push(Span::styled(
            format!("⚙ {} tool", state.pending_tool_calls),
            Style::default().fg(palette.warning),
        ));
        spans.push(Span::raw("  "));
    }
    // Session-cumulative cost chip — survives between prompts so the
    // user can track "what has this whole TUI session cost me?"
    // without needing to do mental arithmetic across runs.
    let session_cost = state.model_cost.cost_for(&state.session_usage);
    if session_cost > 0.0 {
        spans.push(Span::styled(
            format!("Σ {}", format_cost_localized(session_cost, state.cny_rate)),
            Style::default().fg(palette.muted),
        ));
        spans.push(Span::raw("  "));
    }
    // Context-window usage chip: [ctx N%]. Color: green <=60, yellow 60-85, red >85.
    // Both input and output tokens occupy the physical context window, so include both in the numerator.
    if state.context_window > 0 && state.tokens_in > 0 {
        let total_tokens = state.tokens_in.saturating_add(state.tokens_out);
        let pct = (total_tokens as f64 / state.context_window as f64 * 100.0)
            .clamp(0.0, 100.0) as u64;
        let ctx_color = if pct <= 60 {
            palette.success
        } else if pct <= 85 {
            palette.warning
        } else {
            palette.error
        };
        spans.push(Span::styled(
            format!("[ctx {pct}%]"),
            Style::default().fg(ctx_color),
        ));
        spans.push(Span::raw("  "));
    }
    // Message count chip: [msg N].
    {
        let msg_count = state
            .transcript
            .iter()
            .filter(|l| matches!(
                l.kind,
                TranscriptKind::UserPrompt
                    | TranscriptKind::AssistantText
                    | TranscriptKind::ToolCallEnd
                    | TranscriptKind::ToolCallError
            ))
            .count();
        if msg_count > 0 {
            spans.push(Span::styled(
                format!("[msg {msg_count}]"),
                Style::default().fg(palette.subdued),
            ));
            spans.push(Span::raw("  "));
        }
    }
    // Compaction count chip: [compact N].
    if state.compaction_count > 0 {
        spans.push(Span::styled(
            format!("[compact {}]", state.compaction_count),
            Style::default().fg(palette.subdued),
        ));
        spans.push(Span::raw("  "));
    }
    // Minimal hint pointer — full keybind / slash-command catalog
    // lives in the `/help` overlay (F1). Keeping the footer compact
    // means status chips stay on one line even on narrow terminals,
    // and the user isn't drowning in shortcuts they already know.
    spans.push(Span::styled(
        "F1 help",
        Style::default().fg(palette.muted),
    ));
    Paragraph::new(Line::from(spans))
        .alignment(ratatui::layout::Alignment::Left)
        .wrap(Wrap { trim: false })
}

/// Braille-dot spinner glyph rotating at ~10 fps.
fn spinning_char(elapsed: std::time::Duration) -> char {
    const SPINNER: &[char] = &[
        '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}',
        '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
        '\u{2807}', '\u{280F}',
    ];
    let idx = (elapsed.as_millis() / 100) as usize % SPINNER.len();
    SPINNER[idx]
}

/// Pick a "thinking" word that rotates every 5 seconds. Variety
/// keeps the spinner from looking stuck during long turns.
fn pick_thinking_verb(elapsed: std::time::Duration) -> &'static str {
    const VERBS: &[&str] = &[
        "Marinating",
        "Pondering",
        "Cogitating",
        "Mulling",
        "Brewing",
        "Conjuring",
        "Imagining",
        "Crunching",
        "Composing",
        "Distilling",
        "Tinkering",
        "Plotting",
        "Synthesizing",
        "Mapping",
    ];
    let idx = (elapsed.as_secs() / 5) as usize % VERBS.len();
    VERBS[idx]
}

/// Format a wall-clock duration as `Xm Ys` (or `Xs` under a minute).
fn format_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// Format a token count with a `k` suffix once it crosses 1000.
fn format_tokens(n: u64) -> String {
    if n >= 1000 {
        let k = n as f64 / 1000.0;
        format!("{k:.1}k")
    } else {
        n.to_string()
    }
}

/// Cache hit rate as a whole-percent string. Returns `"-"` when the
/// denominator is zero so we don't render `0%` before any tokens
/// arrive. Truncated (not rounded) so `99.99%` displays as `99%` —
/// matches how prefix-cache dashboards display partial-window data.
fn format_cache_rate(cache_read: u64, input_total: u64) -> String {
    if input_total == 0 {
        return "-".into();
    }
    let pct = (cache_read as f64 / input_total as f64 * 100.0).clamp(0.0, 100.0);
    format!("{}%", pct as u64)
}

/// Format a USD cost with adaptive precision so sub-cent runs are
/// readable: `$0.0012` under one cent, else `$0.01` / `$0.42` / `$12.34`.
fn format_cost_usd(usd: f64) -> String {
    if usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// Format a USD cost in CNY using the given conversion rate. Same
/// adaptive-precision rule as USD.
fn format_cost_cny(usd: f64, rate: f64) -> String {
    let cny = usd * rate;
    if cny < 0.01 {
        format!("¥{cny:.4}")
    } else {
        format!("¥{cny:.2}")
    }
}

/// Pick the cost-chip string based on the optional CNY rate. Single
/// place that decides $-vs-¥ so all chips render the same currency.
fn format_cost_localized(usd: f64, cny_rate: Option<f64>) -> String {
    match cny_rate {
        Some(rate) => format_cost_cny(usd, rate),
        None => format_cost_usd(usd),
    }
}

/// Map a USD cost to a stoplight color via the palette. Thresholds
/// mirror DeepSeek-TUI's chip (green <$0.05, yellow $0.05–0.20, red ≥$0.20).
fn cost_color(usd: f64, palette: &Palette) -> Color {
    if usd < 0.05 {
        palette.success
    } else if usd < 0.20 {
        palette.warning
    } else {
        palette.error
    }
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
        Overlay::ModelPicker { .. } => (72, 22),
        Overlay::Log { .. } => (96, 30),
        Overlay::SessionResume { .. } => (88, 24),
        Overlay::Plugins { .. } => (78, 22),
        Overlay::DynamicForm { fields, .. } => {
            // Grow vertically with field count: 4 chrome rows
            // (title + pad + footer + hint) plus 2 rows per field
            // (label + input).
            let h = 6u16.saturating_add((fields.len() as u16).saturating_mul(2)).min(24);
            (72, h.max(8))
        }
        Overlay::DynamicModal { .. } => (72, 12),
        Overlay::DynamicConfirm { .. } => (66, 11),
        Overlay::DynamicList { items, .. } => {
            // 4 rows chrome + 1 per item, capped.
            let h = 5u16.saturating_add(items.len() as u16).min(24);
            (66, h.max(8))
        }
        Overlay::DynamicTable { rows, .. } => {
            // chrome (4) + header (1) + per row (1), capped.
            let h = 6u16.saturating_add(rows.len() as u16).min(24);
            (88, h.max(8))
        }
        Overlay::DynamicTextPanel { lines, footer, .. } => {
            // chrome (3) + per line (1) + optional footer row.
            let chrome = if footer.is_some() { 4 } else { 3 };
            let h = (chrome + lines.len() as u16).min(28);
            (88, h.max(6))
        }
        Overlay::DynamicProgress { .. } => (60, 8),
        Overlay::DynamicStack { children, .. } => {
            // Rough estimate: 4 chrome + 6 rows per child (most
            // children fit). Capped at terminal height anyway.
            let h = (4u16 + children.len() as u16 * 6).min(28);
            (88, h.max(10))
        }
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
        Overlay::Doctor {
            report,
            query,
            scroll,
        } => {
            return draw_doctor(frame, inner, report, query, *scroll, palette);
        }
        Overlay::Skills(skills) => ("skills", OverlayBody::Skills(skills)),
        Overlay::ThemePicker { focused } => {
            return draw_theme_picker(frame, inner, *focused, state, palette);
        }
        Overlay::ProviderPicker { focused } => {
            return draw_provider_picker(frame, inner, *focused, state, palette);
        }
        Overlay::ModelPicker { focused, models, query } => {
            return draw_model_picker(frame, inner, *focused, models, query, state, palette);
        }
        Overlay::Log { scroll } => {
            return draw_log(frame, inner, *scroll, state, palette);
        }
        Overlay::SessionResume { focused, sessions } => {
            return draw_session_resume(frame, inner, *focused, sessions, palette);
        }
        Overlay::Plugins {
            plugins,
            ui_commands,
        } => {
            return draw_plugins(frame, inner, plugins, ui_commands, palette);
        }
        Overlay::DynamicForm {
            title,
            fields,
            focused,
            ..
        } => {
            return draw_dynamic_form(frame, inner, title, fields, *focused, palette);
        }
        Overlay::DynamicModal {
            title,
            body,
            severity,
        } => {
            return draw_dynamic_modal(frame, inner, title, body, *severity, palette);
        }
        Overlay::DynamicConfirm { title, body, .. } => {
            return draw_dynamic_confirm(frame, inner, title, body, palette);
        }
        Overlay::DynamicList {
            title,
            items,
            focused,
            ..
        } => {
            return draw_dynamic_list(frame, inner, title, items, *focused, palette);
        }
        Overlay::DynamicTable {
            title,
            columns,
            rows,
            focused,
            ..
        } => {
            return draw_dynamic_table(frame, inner, title, columns, rows, *focused, palette);
        }
        Overlay::DynamicTextPanel {
            title,
            lines,
            footer,
        } => {
            return draw_dynamic_text_panel(
                frame,
                inner,
                title,
                lines,
                footer.as_deref(),
                palette,
            );
        }
        Overlay::DynamicProgress {
            title,
            value,
            max,
            label,
        } => {
            return draw_dynamic_progress(frame, inner, title, *value, *max, label, palette);
        }
        Overlay::DynamicStack { title, children } => {
            return draw_dynamic_stack(frame, inner, title, children, palette);
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
                            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
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
                line.trim().starts_with("===") || line.to_ascii_lowercase().contains(&needle)
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
                Style::default().fg(palette.fg).add_modifier(Modifier::BOLD)
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

/// Render the `/log` overlay: title + scrollable body containing the
/// joined request-log entries (newest at the bottom — same as
/// transcript). Scroll wheel handlers in `app.rs` mutate the
/// `scroll` field directly.
fn draw_log(
    frame: &mut Frame<'_>,
    popup: Rect,
    scroll: usize,
    state: &AppState,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // pad
            Constraint::Min(1),    // body
            Constraint::Length(1), // hint
        ])
        .split(popup);

    let title_line = Line::from(vec![
        Span::styled(
            "request log",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("({} entries)", state.request_log.len()),
            Style::default().fg(palette.muted),
        ),
    ]);
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    // Join entries with a separator line, oldest → newest.
    let mut body = String::new();
    for (i, entry) in state.request_log.iter().enumerate() {
        if i > 0 {
            body.push_str("\n---\n");
        }
        body.push_str(&format!("# request #{}\n", i + 1));
        body.push_str(entry);
        body.push('\n');
    }
    if body.is_empty() {
        body.push_str("(no entries; start the TUI with --debug-log)");
    }

    frame.render_widget(
        Paragraph::new(body)
            .style(Style::default().fg(palette.fg))
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0)),
        chunks[2],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "PgUp/PgDn · wheel · Esc close",
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

/// Render the `/resume` picker: title + list of past sessions
/// (`title · model · mtime`), with the currently focused row reverse-
/// styled. Hint row at the bottom describes the keys.
fn draw_session_resume(
    frame: &mut Frame<'_>,
    popup: Rect,
    focused: usize,
    sessions: &[grain_ai_agent_headless::SessionMeta],
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
            "resume session",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("({} entries)", sessions.len()),
            Style::default().fg(palette.muted),
        ),
    ]);
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    let body = if sessions.is_empty() {
        Paragraph::new("(no past sessions found in sessions dir)")
            .style(Style::default().fg(palette.muted))
            .wrap(Wrap { trim: false })
    } else {
        let lines: Vec<Line> = sessions
            .iter()
            .enumerate()
            .flat_map(|(i, sess)| {
                let mtime_str = humanize_mtime(sess.modified_at);
                let model = sess.model.as_deref().unwrap_or("(unknown)");
                let title = sess.title_or_placeholder();
                let row = title.to_string();
                let meta = format!("    {model} · {mtime_str} · {} msgs", sess.message_count);
                let (row_style, meta_style) = if i == focused {
                    (
                        Style::default()
                            .fg(palette.surface)
                            .bg(palette.accent)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(palette.surface).bg(palette.accent),
                    )
                } else {
                    (
                        Style::default().fg(palette.fg),
                        Style::default().fg(palette.muted),
                    )
                };
                vec![
                    Line::from(Span::styled(row, row_style)),
                    Line::from(Span::styled(meta, meta_style)),
                ]
            })
            .collect();
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false })
    };
    frame.render_widget(body, chunks[2]);

    let hint = if sessions.is_empty() {
        "Esc close"
    } else {
        "↑↓ navigate · Enter pick · Esc close"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

/// Draw the `/plugins` overlay. Body lists every discovered plugin
/// (manifest metadata + content counts); footer mixes `Esc close`
/// with one chip per plugin-contributed `[[ui_command]]` so the
/// user can see at a glance what dynamic actions lazy-gagent (or
/// any other plugin) adds.
fn draw_plugins(
    frame: &mut Frame<'_>,
    popup: Rect,
    plugins: &[grain_ai_agent_headless::PluginInfo],
    ui_commands: &[grain_ai_agent_headless::BoundUiCommand],
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // pad
            Constraint::Min(1),    // body
            Constraint::Length(1), // footer
        ])
        .split(popup);

    let title_line = Line::from(Span::styled(
        "plugins",
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    let body_lines: Vec<Line> = if plugins.is_empty() {
        vec![Line::from(Span::styled(
            "(loading or no plugins found under .grain/plugins/)",
            Style::default().fg(palette.muted),
        ))]
    } else {
        plugins
            .iter()
            .flat_map(|p| {
                let mut header = vec![Span::styled(
                    format!("• {}", p.name),
                    Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
                )];
                if !p.version.is_empty() {
                    header.push(Span::styled(
                        format!(" v{}", p.version),
                        Style::default().fg(palette.secondary),
                    ));
                }
                header.push(Span::raw("  "));
                header.push(Span::styled(
                    format!(
                        "[skills: {} · themes: {} · scripts: {}]",
                        p.skills, p.themes, p.scripts
                    ),
                    Style::default().fg(palette.info),
                ));
                let detail = if p.description.is_empty() {
                    Line::from(Span::styled(
                        "    (no description)",
                        Style::default().fg(palette.muted),
                    ))
                } else {
                    Line::from(Span::styled(
                        format!("    {}", p.description),
                        Style::default().fg(palette.muted),
                    ))
                };
                vec![Line::from(header), detail]
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(Text::from(body_lines)).wrap(Wrap { trim: false }),
        chunks[2],
    );

    // Footer: built-in Esc hint plus one chip per contributed
    // ui_command. Each chip shows the bound key bracket + label,
    // attributed by plugin name in muted text.
    let mut footer_spans: Vec<Span> = vec![Span::styled(
        "Esc close",
        Style::default().fg(palette.muted),
    )];
    for cmd in ui_commands {
        if cmd.command.target != "plugins" {
            continue;
        }
        footer_spans.push(Span::raw("  "));
        footer_spans.push(Span::styled(
            format!("[{}]", cmd.command.key),
            Style::default().fg(palette.accent).add_modifier(Modifier::BOLD),
        ));
        footer_spans.push(Span::raw(" "));
        footer_spans.push(Span::styled(
            cmd.command.label.clone(),
            Style::default().fg(palette.fg),
        ));
        footer_spans.push(Span::styled(
            format!(" ({})", cmd.plugin_name),
            Style::default().fg(palette.muted),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(footer_spans)), chunks[3]);
}

/// Draw a plugin-contributed [`crate::app::Overlay::DynamicForm`].
/// One field per row pair (label above, editable buffer below);
/// the focused field's buffer gets an underscore cursor suffix and
/// accent-colored label.
fn draw_dynamic_form(
    frame: &mut Frame<'_>,
    popup: Rect,
    title: &str,
    fields: &[crate::app::DynamicFormFieldState],
    focused: usize,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // pad
            Constraint::Min(1),    // body
            Constraint::Length(1), // hint
        ])
        .split(popup);

    let title_line = Line::from(Span::styled(
        title.to_string(),
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    let mut lines: Vec<Line> = Vec::with_capacity(fields.len() * 2);
    for (i, f) in fields.iter().enumerate() {
        let focused = i == focused;
        let label_style = if focused {
            Style::default().fg(palette.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        };
        lines.push(Line::from(Span::styled(f.label.clone(), label_style)));
        let mut body = if f.value.is_empty() {
            Span::styled(
                if f.placeholder.is_empty() {
                    "  (empty)".to_string()
                } else {
                    format!("  {}", f.placeholder)
                },
                Style::default().fg(palette.muted),
            )
        } else {
            Span::styled(
                format!("  {}", f.value),
                Style::default().fg(palette.fg),
            )
        };
        if focused {
            body = Span::styled(
                format!("{}_", body.content),
                body.style.fg(palette.fg),
            );
        }
        lines.push(Line::from(body));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        chunks[2],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Tab next · Shift-Tab prev · Enter submit · Esc cancel",
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

/// Draw a plugin-contributed [`crate::app::Overlay::DynamicModal`].
/// Body is wrap-rendered; severity tints the title accent.
fn draw_dynamic_modal(
    frame: &mut Frame<'_>,
    popup: Rect,
    title: &str,
    body: &str,
    severity: grain_ai_agent_headless::ModalSeverity,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(popup);

    let accent = match severity {
        grain_ai_agent_headless::ModalSeverity::Info => palette.accent,
        grain_ai_agent_headless::ModalSeverity::Success => palette.info,
        grain_ai_agent_headless::ModalSeverity::Warn => palette.secondary,
        grain_ai_agent_headless::ModalSeverity::Error => palette.error,
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title.to_string(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(body.to_string())
            .style(Style::default().fg(palette.fg))
            .wrap(Wrap { trim: false }),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Enter / Esc close",
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

/// Draw a plugin-contributed [`crate::app::Overlay::DynamicConfirm`].
/// Body is wrap-rendered; footer shows `y / Enter confirm · n / Esc cancel`.
fn draw_dynamic_confirm(
    frame: &mut Frame<'_>,
    popup: Rect,
    title: &str,
    body: &str,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(popup);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(body.to_string())
            .style(Style::default().fg(palette.fg))
            .wrap(Wrap { trim: false }),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "y / Enter confirm · n / Esc cancel",
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

/// Map the plugin-facing semantic [`grain_ai_agent_headless::TextColor`]
/// to a concrete palette color. Keeps "success" / "error" / "accent"
/// consistent across themes.
fn map_text_color(c: grain_ai_agent_headless::TextColor, palette: &Palette) -> Color {
    use grain_ai_agent_headless::TextColor as T;
    match c {
        T::Fg => palette.fg,
        T::Muted => palette.muted,
        T::Accent => palette.accent,
        T::Secondary => palette.secondary,
        T::Info => palette.info,
        T::Error => palette.error,
        T::Success => palette.info,
        T::Warn => palette.secondary,
    }
}

/// Convert a plugin-supplied [`grain_ai_agent_headless::TextLine`] into
/// a ratatui `Line` with palette-mapped styling.
fn render_text_line(
    line: &grain_ai_agent_headless::TextLine,
    palette: &Palette,
) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .iter()
        .map(|s| {
            let mut style = Style::default()
                .fg(s.color.map(|c| map_text_color(c, palette)).unwrap_or(palette.fg));
            if s.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            Span::styled(s.text.clone(), style)
        })
        .collect();
    Line::from(spans)
}

/// Draw [`crate::app::Overlay::DynamicList`]. Highlighted row is
/// the focused entry; Esc / Enter footer hints below the body.
fn draw_dynamic_list(
    frame: &mut Frame<'_>,
    popup: Rect,
    title: &str,
    items: &[String],
    focused: usize,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(popup);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );
    let lines: Vec<Line> = if items.is_empty() {
        vec![Line::from(Span::styled(
            "(empty)",
            Style::default().fg(palette.muted),
        ))]
    } else {
        items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let style = if i == focused {
                    Style::default()
                        .fg(palette.surface)
                        .bg(palette.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(palette.fg)
                };
                Line::from(Span::styled(format!("  {it}"), style))
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑↓ navigate · Enter pick · Esc close",
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

/// Draw [`crate::app::Overlay::DynamicTable`]. Header row in accent
/// color, focused row highlighted, columns padded to longest cell.
fn draw_dynamic_table(
    frame: &mut Frame<'_>,
    popup: Rect,
    title: &str,
    columns: &[String],
    rows: &[Vec<String>],
    focused: usize,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(popup);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );
    // Per-column width = max(header, all rows).
    let mut widths: Vec<usize> = columns
        .iter()
        .map(|c| UnicodeWidthStr::width(c.as_str()))
        .collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
    }
    let pad = |s: &str, w: usize| {
        let cur = UnicodeWidthStr::width(s);
        if cur >= w {
            s.to_string()
        } else {
            let mut out = s.to_string();
            out.push_str(&" ".repeat(w - cur));
            out
        }
    };
    let header_text = columns
        .iter()
        .enumerate()
        .map(|(i, c)| pad(c, widths.get(i).copied().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join("  ");
    let header = Line::from(Span::styled(
        header_text,
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    ));
    let mut body: Vec<Line> = vec![header];
    if rows.is_empty() {
        body.push(Line::from(Span::styled(
            "(no rows)",
            Style::default().fg(palette.muted),
        )));
    } else {
        for (i, row) in rows.iter().enumerate() {
            let text = row
                .iter()
                .enumerate()
                .map(|(c, cell)| pad(cell, widths.get(c).copied().unwrap_or(0)))
                .collect::<Vec<_>>()
                .join("  ");
            let style = if i == focused {
                Style::default()
                    .fg(palette.surface)
                    .bg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.fg)
            };
            body.push(Line::from(Span::styled(text, style)));
        }
    }
    // No-wrap on purpose: with `Wrap { trim: false }` a single
    // long-source row (e.g. a wasm plugin with a deep src path) wraps
    // to 2-3 visible lines and pushes subsequent rows past the
    // popup's bottom edge — they then never render and the user
    // sees "2 plugins" in the title but only 1 row in the body.
    // Letting long cells clip at the right edge keeps every row
    // visible; the user can switch to `/plugin-details` for the
    // full src if they need it.
    frame.render_widget(Paragraph::new(Text::from(body)), chunks[2]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑↓ navigate · Enter pick · Esc close",
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

/// Draw [`crate::app::Overlay::DynamicTextPanel`]. Plugin owns the
/// row contents and styling; this just maps each TextLine into a
/// ratatui Line via the palette-aware [`render_text_line`].
fn draw_dynamic_text_panel(
    frame: &mut Frame<'_>,
    popup: Rect,
    title: &str,
    lines: &[grain_ai_agent_headless::TextLine],
    footer: Option<&str>,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(popup);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );
    let body: Vec<Line> = lines.iter().map(|l| render_text_line(l, palette)).collect();
    frame.render_widget(
        Paragraph::new(Text::from(body)).wrap(Wrap { trim: false }),
        chunks[2],
    );
    let footer_text = footer.unwrap_or("Esc close").to_string();
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer_text,
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

/// Draw [`crate::app::Overlay::DynamicProgress`]. Block-style fill
/// bar inside the body; the title row tracks percent.
fn draw_dynamic_progress(
    frame: &mut Frame<'_>,
    popup: Rect,
    title: &str,
    value: i64,
    max: i64,
    label: &str,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(popup);
    let pct = if max > 0 {
        ((value.max(0) as f32) / (max as f32) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                title.to_string(),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{pct:.0}%"),
                Style::default().fg(palette.muted),
            ),
        ])),
        chunks[0],
    );
    // Render a single-row fill bar. Body width drives the bar
    // length; clamp to body width to handle tiny terminals.
    let bar_w = chunks[2].width.max(1) as usize;
    let filled = ((pct / 100.0) * bar_w as f32).round() as usize;
    let mut bar = String::new();
    for _ in 0..filled {
        bar.push('█');
    }
    for _ in filled..bar_w {
        bar.push('░');
    }
    let bar_line = Line::from(Span::styled(bar, Style::default().fg(palette.accent)));
    let body: Vec<Line> = if label.is_empty() {
        vec![bar_line]
    } else {
        vec![
            Line::from(Span::styled(
                label.to_string(),
                Style::default().fg(palette.fg),
            )),
            bar_line,
        ]
    };
    frame.render_widget(Paragraph::new(Text::from(body)), chunks[2]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Esc close",
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

/// Draw [`crate::app::Overlay::DynamicStack`]. Children are
/// rendered top-to-bottom by recursing into a small slice of the
/// inner area per child; only TextPanel / Progress / List / Table
/// nested children draw correctly here (other variants assume
/// owning the full popup, so they'll truncate inside a stack).
fn draw_dynamic_stack(
    frame: &mut Frame<'_>,
    popup: Rect,
    title: &str,
    children: &[grain_ai_agent_headless::OverlayDescriptor],
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(popup);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );
    // Equal vertical share per child for v1; future revs may use
    // per-child weights.
    let n = children.len().max(1) as u16;
    let per_h = (chunks[2].height / n).max(1);
    let mut y = chunks[2].y;
    for (i, child) in children.iter().enumerate() {
        let h = if i == children.len() - 1 {
            chunks[2].y + chunks[2].height - y // last child gets the remainder
        } else {
            per_h
        };
        let area = Rect {
            x: chunks[2].x,
            y,
            width: chunks[2].width,
            height: h,
        };
        draw_stack_child(frame, area, child, palette);
        y += h;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Esc close",
            Style::default().fg(palette.muted),
        ))),
        chunks[3],
    );
}

/// Render one stacked child inline (no chrome — title goes inline
/// with the content). Only display-oriented children are
/// well-supported; interactive children (Form, Confirm, List with
/// on_select, Table with on_select) will render their visuals but
/// keys won't route to them.
fn draw_stack_child(
    frame: &mut Frame<'_>,
    area: Rect,
    descriptor: &grain_ai_agent_headless::OverlayDescriptor,
    palette: &Palette,
) {
    use grain_ai_agent_headless::OverlayDescriptor as D;
    match descriptor {
        D::TextPanel { title, lines, .. } => {
            let mut body: Vec<Line> = Vec::with_capacity(lines.len() + 1);
            if !title.is_empty() {
                body.push(Line::from(Span::styled(
                    title.clone(),
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            for line in lines {
                body.push(render_text_line(line, palette));
            }
            frame.render_widget(
                Paragraph::new(Text::from(body)).wrap(Wrap { trim: false }),
                area,
            );
        }
        D::Progress {
            title,
            value,
            max,
            label,
        } => {
            // Inline progress: title + bar + optional label.
            let pct = if *max > 0 {
                ((*value).max(0) as f32 / *max as f32 * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            };
            let bar_w = area.width.max(1) as usize;
            let filled = ((pct / 100.0) * bar_w as f32).round() as usize;
            let mut bar = String::new();
            for _ in 0..filled {
                bar.push('█');
            }
            for _ in filled..bar_w {
                bar.push('░');
            }
            let mut body: Vec<Line> = Vec::new();
            if !title.is_empty() {
                body.push(Line::from(Span::styled(
                    title.clone(),
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            if !label.is_empty() {
                body.push(Line::from(Span::styled(
                    label.clone(),
                    Style::default().fg(palette.fg),
                )));
            }
            body.push(Line::from(Span::styled(
                bar,
                Style::default().fg(palette.accent),
            )));
            frame.render_widget(Paragraph::new(Text::from(body)), area);
        }
        D::List { title, items, .. } => {
            let mut body: Vec<Line> = Vec::with_capacity(items.len() + 1);
            if !title.is_empty() {
                body.push(Line::from(Span::styled(
                    title.clone(),
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            for it in items {
                body.push(Line::from(Span::styled(
                    format!("  {it}"),
                    Style::default().fg(palette.fg),
                )));
            }
            frame.render_widget(
                Paragraph::new(Text::from(body)).wrap(Wrap { trim: false }),
                area,
            );
        }
        _ => {
            // For unsupported nested kinds, render a placeholder so
            // the layout doesn't collapse silently.
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("(nested {} not rendered)", descriptor.title()),
                    Style::default().fg(palette.muted),
                ))),
                area,
            );
        }
    }
}

/// Format a SystemTime as a human-friendly relative string ("3m ago",
/// "2h ago", "5d ago"). Falls back to a raw timestamp if the system
/// clock is somehow before UNIX epoch.
fn humanize_mtime(t: std::time::SystemTime) -> String {
    use std::time::SystemTime;
    let Ok(elapsed) = SystemTime::now().duration_since(t) else {
        return "future".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
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
            .map(|(i, p)| provider_picker_row(i, p, focused, state.current_provider_idx, palette))
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
                if std::env::var(env).ok().filter(|v| !v.is_empty()).is_some() {
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

fn draw_model_picker(
    frame: &mut Frame<'_>,
    popup: Rect,
    focused: usize,
    models: &[(String, String)],
    query: &str,
    state: &AppState,
    palette: &Palette,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // pad
            Constraint::Length(1), // search input
            Constraint::Length(1), // pad
            Constraint::Min(1),    // list
            Constraint::Length(1), // hint
        ])
        .split(popup);

    let title_line = Line::from(vec![
        Span::styled(
            "model",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("({} models)", models.len()),
            Style::default().fg(palette.muted),
        ),
    ]);
    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    // Search bar with caret. Empty query shows a placeholder.
    let search_line = if query.is_empty() {
        Line::from(vec![
            Span::styled("⌕ ", Style::default().fg(palette.accent)),
            Span::styled(
                "type to search models …",
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

    let body_area = chunks[4];

    // Filter, sort, and group by provider.
    let filtered = crate::app::filter_models(models, query);

    if filtered.is_empty() {
        let line = Line::from(Span::styled(
            if models.is_empty() {
                "(loading models…)"
            } else {
                "(no models match your search)"
            },
            Style::default().fg(palette.muted),
        ));
        frame.render_widget(Paragraph::new(line), body_area);
    } else {
        #[derive(Debug, Clone)]
        enum Row {
            Header(String),
            Item { idx: usize, id: String, name: String },
        }

        let mut rows: Vec<Row> = Vec::new();
        let mut last_provider = "";
        for (idx, (id, name)) in filtered.iter().enumerate() {
            let provider = id.split('/').next().unwrap_or("");
            if provider != last_provider {
                rows.push(Row::Header(provider.to_string()));
                last_provider = provider;
            }
            rows.push(Row::Item {
                idx,
                id: id.clone(),
                name: name.clone(),
            });
        }

        // Find the display row index of the focused item.
        let focused_row = rows
            .iter()
            .position(|r| matches!(r, Row::Item { idx, .. } if *idx == focused))
            .unwrap_or(0);

        let visible = body_area.height as usize;
        let total = rows.len();
        let start = if total > visible {
            focused_row.saturating_sub(visible / 2).min(total - visible)
        } else {
            0
        };
        let end = (start + visible).min(total);

        let lines: Vec<Line> = rows[start..end]
            .iter()
            .map(|row| match row {
                Row::Header(provider) => Line::from(vec![
                    Span::styled(
                        format!("{provider}/"),
                        Style::default()
                            .fg(palette.secondary)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Row::Item { idx, id, name } => {
                    let is_focused = *idx == focused;
                    let cursor = if is_focused { "▶ " } else { "  " };
                    let mark = if id == &state.model_id { "✓ " } else { "  " };
                    let row_style = if is_focused {
                        Style::default()
                            .fg(palette.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(palette.fg)
                    };
                    Line::from(vec![
                        Span::styled(format!("{cursor}{mark}"), row_style),
                        Span::styled(name.to_string(), row_style),
                        Span::raw("  "),
                        Span::styled(id.to_string(), Style::default().fg(palette.muted)),
                    ])
                }
            })
            .collect();

        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
            body_area,
        );
    }

    let hint = if query.is_empty() {
        "↑↓ navigate · Enter apply · Esc cancel".to_string()
    } else {
        format!("↑↓ navigate · Enter apply · Esc cancel · {} matches", filtered.len())
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(palette.muted),
        ))),
        chunks[5],
    );
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
  F5              toggle thinking visibility (show/hide reasoning lines)
  F6              toggle mouse capture (scroll wheel vs native text selection)
  ←/→/Home/End    move cursor in input
  ↑/↓             history (no palette) · navigate (palette / picker)
  PgUp / PgDn     scroll transcript (freezes view; PgDn catches up to tail)
  End             jump back to live transcript bottom (re-engage tail)
  Home            jump to top of transcript

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

#[cfg(test)]
mod ui_format_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn format_elapsed_under_a_minute_is_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(5)), "5s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_elapsed_at_or_above_a_minute_is_minutes_and_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_elapsed(Duration::from_secs(566)), "9m 26s");
        assert_eq!(format_elapsed(Duration::from_secs(3725)), "62m 5s");
    }

    #[test]
    fn format_tokens_uses_k_above_a_thousand() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(1234), "1.2k");
        assert_eq!(format_tokens(32_800), "32.8k");
        assert_eq!(format_tokens(100_500), "100.5k");
    }

    #[test]
    fn pick_thinking_verb_rotates_every_five_seconds() {
        let v0 = pick_thinking_verb(Duration::from_secs(0));
        let v5 = pick_thinking_verb(Duration::from_secs(5));
        let v10 = pick_thinking_verb(Duration::from_secs(10));
        assert_ne!(v0, v5);
        assert_ne!(v5, v10);
    }

    #[test]
    fn format_cache_rate_handles_zero_denominator() {
        assert_eq!(format_cache_rate(0, 0), "-");
        // cache_read without input_total is nonsensical; bucket it
        // with the empty case rather than dividing into infinity.
        assert_eq!(format_cache_rate(999, 0), "-");
    }

    #[test]
    fn format_cache_rate_truncates_partial_percent() {
        assert_eq!(format_cache_rate(0, 100), "0%");
        assert_eq!(format_cache_rate(50, 100), "50%");
        assert_eq!(format_cache_rate(9_998, 10_000), "99%"); // truncate, don't round
        assert_eq!(format_cache_rate(10_000, 10_000), "100%");
    }

    #[test]
    fn format_cache_rate_clamps_overshoot() {
        // Providers occasionally report cache_read > input_total; clamp to 100%.
        assert_eq!(format_cache_rate(15_000, 10_000), "100%");
    }

    #[test]
    fn format_cost_usd_switches_precision_at_one_cent() {
        assert_eq!(format_cost_usd(0.0001), "$0.0001");
        assert_eq!(format_cost_usd(0.0099), "$0.0099");
        assert_eq!(format_cost_usd(0.01), "$0.01");
        assert_eq!(format_cost_usd(0.4231), "$0.42");
        assert_eq!(format_cost_usd(12.345), "$12.35");
    }

    #[test]
    fn format_cost_cny_switches_precision_at_one_fen() {
        // 0.01 USD * 7.20 = ¥0.0720 → renders as "¥0.07"
        assert_eq!(format_cost_cny(0.01, 7.20), "¥0.07");
        // 0.0001 USD * 7.20 = ¥0.00072 → renders as "¥0.0007"
        assert_eq!(format_cost_cny(0.0001, 7.20), "¥0.0007");
        assert_eq!(format_cost_cny(1.0, 7.20), "¥7.20");
    }

    #[test]
    fn format_cost_localized_uses_usd_when_no_rate() {
        assert_eq!(format_cost_localized(0.42, None), "$0.42");
    }

    #[test]
    fn format_cost_localized_uses_cny_when_rate_set() {
        assert_eq!(format_cost_localized(0.10, Some(7.20)), "¥0.72");
    }

    #[test]
    fn cost_color_threshold_buckets() {
        // We don't care about absolute color values — just that the
        // three thresholds map to *different* palette slots.
        let p = &crate::theme::builtin_themes()[0].palette;
        assert_eq!(cost_color(0.00, p), p.success);
        assert_eq!(cost_color(0.049, p), p.success);
        assert_eq!(cost_color(0.05, p), p.warning);
        assert_eq!(cost_color(0.199, p), p.warning);
        assert_eq!(cost_color(0.20, p), p.error);
        assert_eq!(cost_color(99.0, p), p.error);
    }

    #[test]
    fn wrap_input_returns_one_line_for_empty_input() {
        let lines = wrap_input_to_lines("", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "");
    }

    #[test]
    fn wrap_input_keeps_short_input_on_one_line() {
        let lines = wrap_input_to_lines("hello", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "hello");
    }

    #[test]
    fn wrap_input_splits_at_width_boundary_accounting_for_prefix() {
        // Width 10, prefix occupies first 2 cells → row 0 fits 8 chars,
        // continuation rows fit 10 chars each.
        let lines = wrap_input_to_lines("abcdefghijklmnopqrstuvwxyz", 10);
        assert_eq!(lines[0], "abcdefgh"); // 8 chars after prefix
        assert_eq!(lines[1], "ijklmnopqr"); // 10 chars
        assert_eq!(lines[2], "stuvwxyz"); // remainder
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn wrap_input_treats_newline_as_hard_break() {
        let lines = wrap_input_to_lines("hi\nthere", 80);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hi");
        assert_eq!(lines[1], "there");
    }

    #[test]
    fn wrap_input_counts_wide_glyphs_as_two_cells() {
        // 中 = 2 cells. Width 10, prefix = 2, so row 0 fits 4 wide chars
        // (using 8 cells).
        let lines = wrap_input_to_lines("中文中文中", 10);
        assert_eq!(lines[0], "中文中文"); // 4 wide chars = 8 cells, fits after prefix
        assert_eq!(lines[1], "中"); // remainder
    }

    #[test]
    fn input_cursor_offset_origin_for_empty_input() {
        let (row, col) = input_cursor_offset("", 0, 80);
        assert_eq!((row, col), (0, INPUT_PREFIX_COLS));
    }

    #[test]
    fn input_cursor_offset_tracks_visual_width_after_wide_glyphs() {
        // After 2 wide chars, cursor is at prefix (2) + 4 = col 6.
        let s = "中文";
        let (row, col) = input_cursor_offset(s, s.len(), 80);
        assert_eq!((row, col), (0, INPUT_PREFIX_COLS + 4));
    }

    #[test]
    fn input_cursor_offset_jumps_to_next_row_on_wrap() {
        // Width 10, prefix 2 → row 0 ends at col 10 after 8 chars.
        // Cursor at byte 12 means 8 on row 0 + 4 on row 1.
        let s = "abcdefghijkl"; // 12 chars
        let (row, col) = input_cursor_offset(s, s.len(), 10);
        assert_eq!(row, 1);
        assert_eq!(col, 4);
    }

    #[test]
    fn wrap_input_caps_implicitly_at_max_rows_via_input_height() {
        // We can't construct a full `AppState` here without going through
        // the wider crate test seam, but `wrap_input_to_lines` is the
        // load-bearing helper; verify the cap arithmetic matches what
        // `input_height` would clamp to.
        let long = "x".repeat(500);
        let rows = wrap_input_to_lines(&long, 20).len() as u16;
        assert!(rows > INPUT_MAX_ROWS);
        // Cap kicks in on the consumer side.
        assert_eq!(rows.clamp(1, INPUT_MAX_ROWS), INPUT_MAX_ROWS);
    }

    #[test]
    fn wrap_input_minimum_width_does_not_panic() {
        // Width 0 / 1 / 2 would otherwise divide cleanly to "no room";
        // helper bumps to PREFIX + 1 internally so wrapping always
        // makes forward progress (at least one char per row).
        let lines = wrap_input_to_lines("abcd", 1);
        assert!(!lines.is_empty());
        // All characters preserved across the wrapped rows.
        let joined: String = lines.concat();
        assert_eq!(joined, "abcd");
    }

    #[test]
    fn markdown_wrapping_uses_rendered_offsets_after_hard_breaks() {
        let source = "有的。我有两个网络相关工具：\n\n- `web_search` — 通过 Exa 搜索引擎搜索网页\n- `web_fetch` — 获取网页内容\n";
        let md_spans = crate::md_render::render_md_to_spans(source);
        let line = TranscriptLine {
            kind: TranscriptKind::AssistantText,
            text: source.into(),
        };
        let mut rendered = Vec::new();

        wrap_one_line(&line, 160, None, &mut rendered, Some(&md_spans));

        let palette = crate::theme::builtin_themes()[0].palette;
        let rendered_text: Vec<String> = rendered
            .iter()
            .map(|row| {
                build_line(row, 0, None, &palette)
                    .spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect();

        assert_eq!(rendered_text[0], "有的。我有两个网络相关工具：");
        assert!(rendered_text[1].starts_with("    • web_search"));
        assert!(rendered_text[2].starts_with("    • web_fetch"));
        assert!(
            rendered_text[1..]
                .iter()
                .all(|row| !row.starts_with("有的。我有两个网络相关工具：")),
            "markdown rows must not restart from the first paragraph: {rendered_text:?}"
        );
    }

    #[test]
    fn markdown_line_preserves_non_empty_prefix() {
        let source = "**thinking**\n";
        let md_spans = crate::md_render::render_md_to_spans(source);
        let line = TranscriptLine {
            kind: TranscriptKind::ThinkingText,
            text: source.into(),
        };
        let mut rendered = Vec::new();

        wrap_one_line(&line, 80, None, &mut rendered, Some(&md_spans));

        let palette = crate::theme::builtin_themes()[0].palette;
        let first = build_line(&rendered[0], 0, None, &palette)
            .spans
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>();

        assert_eq!(first, "· thinking");
    }
}
