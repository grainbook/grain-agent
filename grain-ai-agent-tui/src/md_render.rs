//! Streaming markdown renderer for ratatui.
//!
//! Inspired by [streamdown](https://github.com/vercel/streamdown), this
//! module incrementally parses CommonMark text with `pulldown-cmark` and
//! produces `ratatui::text::Span` sequences suitable for terminal
//! rendering.
//!
//! ## Streaming behaviour
//!
//! pulldown-cmark is a pull parser — it yields events as soon as they
//! become unambiguous. Unterminated constructs (unclosed `**bold`,
//! dangling `` `code`, unfinished ` ``` ` fences) are handled
//! naturally: the parser either defers the event or emits the best
//! partial result. This matches streamdown's "unterminated block
//! parsing" strategy.
//!
//! ## Architecture
//!
//! - [`MarkdownCache`] sits in [`AppState`] and holds styled span
//!   output for completed (non-streaming) transcript lines.
//! - On each frame, the last streaming line is re-parsed; all prior
//!   lines are served from cache.
//! - [`render_md_to_spans`] converts pulldown-cmark events into flat
//!   `Vec<MdStyledSpan>`, tracking link destinations and list nesting.
//! - [`spans_for_fragment`] maps a wrapped-text fragment back to the
//!   styled spans, producing ratatui `Span`s with correct style for
//!   each sub-segment.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::theme::Palette;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One styled span produced by the markdown parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdStyledSpan {
    pub text: String,
    pub style_kind: MdStyleKind,
}

/// Semantic style tag — palette-independent. The caller maps each
/// variant to a concrete `ratatui::Style` via the active [`Palette`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdStyleKind {
    /// Regular body text — inherits the line's base style.
    Body,
    /// Strong emphasis (`**text**` or `__text__`).
    Bold,
    /// Emphasis (`*text*` or `_text_`).
    Italic,
    /// Inline code (`` `code` ``).
    InlineCode,
    /// Fenced or indented code block.
    CodeBlock,
    /// Heading (`# ` … `###### `).
    Heading,
    /// Blockquote continuation (`> `).
    Blockquote,
    /// Link text — rendered underlined so the user knows it's
    /// clickable (though terminals can't actually follow links).
    Link,
    /// Image alt-text (the terminal can't render the image, so we
    /// show the alt text with a dimmed "img:" prefix).
    Image,
    /// Unordered list bullet.
    ListBullet,
}

// ---------------------------------------------------------------------------
// Markdown cache
// ---------------------------------------------------------------------------

/// Caches pre-parsed styled spans for completed transcript lines so
/// they don't get re-parsed every frame. Invalidated when the line
/// index is the streaming tail.
#[derive(Debug, Clone, Default)]
pub struct MarkdownCache {
    /// `cache[i]` holds the parsed spans for `transcript[i]`, or
    /// `None` when the entry hasn't been computed yet or is the
    /// currently-streaming line (which changes each frame).
    pub entries: Vec<Option<Vec<MdStyledSpan>>>,
}

impl MarkdownCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensure the cache has capacity for `total_transcript_len` lines.
    pub fn resize(&mut self, len: usize) {
        self.entries.resize(len, None);
    }

    /// Parse + cache `transcript[idx]`. Returns the parsed spans.
    pub fn get_or_parse(
        &mut self,
        idx: usize,
        line: &str,
        is_streaming: bool,
        palette: &Palette,
    ) -> Vec<Span<'static>> {
        // Never cache the streaming tail — it changes each frame.
        if !is_streaming {
            if let Some(Some(cached)) = self.entries.get(idx) {
                return cached.iter().map(|s| s.to_ratatui_span(palette)).collect();
            }
        }
        let spans = render_md_to_spans(line);
        let rat_spans: Vec<Span<'static>> =
            spans.iter().map(|s| s.to_ratatui_span(palette)).collect();
        if !is_streaming {
            if idx < self.entries.len() {
                self.entries[idx] = Some(spans);
            }
        }
        rat_spans
    }

    /// Drop cached entries from `idx` onward (the streaming tail
    /// shifted or the line was finalized into a new form).
    pub fn invalidate_from(&mut self, idx: usize) {
        if idx < self.entries.len() {
            self.entries.truncate(idx);
        }
    }
}

// ---------------------------------------------------------------------------
// Core conversion: pulldown-cmark events → MdStyledSpan list
// ---------------------------------------------------------------------------

/// Parse `source` (CommonMark) and return a flat list of styled spans.
///
/// The returned spans are **logical** — they don't know about terminal
/// wrapping. The caller is responsible for wrapping and then mapping
/// wrapped fragments back via [`spans_for_fragment`].
pub fn render_md_to_spans(source: &str) -> Vec<MdStyledSpan> {
    let mut out = Vec::new();
    let mut options = Options::all();
    // We don't need the full table/autolink/footnote power for
    // typical LLM output. Keep tables and tasklists but drop
    // features that add noise in a terminal.
    options.remove(Options::ENABLE_FOOTNOTES);
    options.remove(Options::ENABLE_DEFINITION_LIST);

    let parser = Parser::new_ext(source, options);

    // State stack for currently open inline styles.
    #[derive(Debug, Clone, Copy)]
    enum StackEntry {
        Bold,
        Italic,
        Heading,
        CodeBlock,
        Blockquote,
        Link,
        Image,
        ListItem,
    }

    let mut stack: Vec<StackEntry> = Vec::new();

    /// Current effective style considering the stack.
    fn current_style(stack: &[StackEntry]) -> MdStyleKind {
        // Last-in wins for inline styles; structural entries like
        // CodeBlock / Blockquote add their own layer.
        for entry in stack.iter().rev() {
            match entry {
                StackEntry::CodeBlock => return MdStyleKind::CodeBlock,
                StackEntry::Heading => return MdStyleKind::Heading,
                StackEntry::Blockquote => return MdStyleKind::Blockquote,
                StackEntry::Link => return MdStyleKind::Link,
                StackEntry::Image => return MdStyleKind::Image,
                StackEntry::Bold | StackEntry::Italic => {}
                StackEntry::ListItem => {}
            }
        }
        // Check for bold+italic combo.
        let has_bold = stack.iter().any(|e| matches!(e, StackEntry::Bold));
        let has_italic = stack.iter().any(|e| matches!(e, StackEntry::Italic));
        match (has_bold, has_italic) {
            (true, true) => MdStyleKind::Bold, // ratatui doesn't have bold+italic; bold wins
            (true, false) => MdStyleKind::Bold,
            (false, true) => MdStyleKind::Italic,
            (false, false) => MdStyleKind::Body,
        }
    }

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {} // no-op: paragraphs are implicit
                Tag::Heading { .. } => {
                    // Push a newline before headings for visual separation.
                    if !out.is_empty() && !out.last().map(|s: &MdStyledSpan| s.text.ends_with('\n')).unwrap_or(true)
                    {
                        out.push(MdStyledSpan {
                            text: "\n".into(),
                            style_kind: MdStyleKind::Body,
                        });
                    }
                    stack.push(StackEntry::Heading);
                }
                Tag::CodeBlock(kind) => {
                    if !out.is_empty() && !out.last().map(|s: &MdStyledSpan| s.text.ends_with('\n')).unwrap_or(true)
                    {
                        out.push(MdStyledSpan {
                            text: "\n".into(),
                            style_kind: MdStyleKind::Body,
                        });
                    }
                    // Emit the info string as a code-block header.
                    if let CodeBlockKind::Fenced(lang) = &kind {
                        if !lang.is_empty() {
                            out.push(MdStyledSpan {
                                text: format!("```{lang}\n"),
                                style_kind: MdStyleKind::CodeBlock,
                            });
                        } else {
                            out.push(MdStyledSpan {
                                text: "```\n".into(),
                                style_kind: MdStyleKind::CodeBlock,
                            });
                        }
                    } else {
                        out.push(MdStyledSpan {
                            text: "```\n".into(),
                            style_kind: MdStyleKind::CodeBlock,
                        });
                    }
                    stack.push(StackEntry::CodeBlock);
                }
                Tag::List(_) => {} // no-op
                Tag::Item => {
                    stack.push(StackEntry::ListItem);
                    out.push(MdStyledSpan {
                        text: "  • ".into(),
                        style_kind: MdStyleKind::ListBullet,
                    });
                }
                Tag::BlockQuote(_) => {
                    stack.push(StackEntry::Blockquote);
                    out.push(MdStyledSpan {
                        text: "▌ ".into(),
                        style_kind: MdStyleKind::Blockquote,
                    });
                }
                Tag::Strong => stack.push(StackEntry::Bold),
                Tag::Emphasis => stack.push(StackEntry::Italic),
                Tag::Link { .. } => stack.push(StackEntry::Link),
                Tag::Image { .. } => stack.push(StackEntry::Image),
                // Inline HTML, tables, etc. — passthrough as body text.
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    out.push(MdStyledSpan {
                        text: "\n".into(),
                        style_kind: MdStyleKind::Body,
                    });
                }
                TagEnd::Heading(_) => {
                    stack.retain(|e| !matches!(e, StackEntry::Heading));
                    out.push(MdStyledSpan {
                        text: "\n".into(),
                        style_kind: MdStyleKind::Body,
                    });
                }
                TagEnd::CodeBlock => {
                    stack.retain(|e| !matches!(e, StackEntry::CodeBlock));
                    out.push(MdStyledSpan {
                        text: "```\n".into(),
                        style_kind: MdStyleKind::CodeBlock,
                    });
                }
                TagEnd::List(_) => {} // no-op
                TagEnd::Item => {
                    stack.retain(|e| !matches!(e, StackEntry::ListItem));
                    if !out
                        .last()
                        .map(|s: &MdStyledSpan| s.text.ends_with('\n'))
                        .unwrap_or(true)
                    {
                        out.push(MdStyledSpan {
                            text: "\n".into(),
                            style_kind: MdStyleKind::Body,
                        });
                    }
                }
                TagEnd::BlockQuote(_) => {
                    stack.retain(|e| !matches!(e, StackEntry::Blockquote));
                }
                TagEnd::Strong => {
                    if let Some(pos) = stack.iter().rposition(|e| matches!(e, StackEntry::Bold)) {
                        stack.remove(pos);
                    }
                }
                TagEnd::Emphasis => {
                    if let Some(pos) =
                        stack.iter().rposition(|e| matches!(e, StackEntry::Italic))
                    {
                        stack.remove(pos);
                    }
                }
                TagEnd::Link => {
                    if let Some(pos) = stack.iter().rposition(|e| matches!(e, StackEntry::Link)) {
                        stack.remove(pos);
                    }
                }
                TagEnd::Image => {
                    if let Some(pos) = stack.iter().rposition(|e| matches!(e, StackEntry::Image)) {
                        stack.remove(pos);
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                let style = current_style(&stack);
                out.push(MdStyledSpan {
                    text: text.into_string(),
                    style_kind: style,
                });
            }
            Event::Code(text) => {
                out.push(MdStyledSpan {
                    text: text.into_string(),
                    style_kind: MdStyleKind::InlineCode,
                });
            }
            Event::InlineHtml(html) | Event::InlineMath(html) => {
                out.push(MdStyledSpan {
                    text: html.into_string(),
                    style_kind: MdStyleKind::Body,
                });
            }
            Event::Html(html) => {
                out.push(MdStyledSpan {
                    text: html.into_string(),
                    style_kind: MdStyleKind::Body,
                });
            }
            Event::SoftBreak => {
                out.push(MdStyledSpan {
                    text: " ".into(),
                    style_kind: MdStyleKind::Body,
                });
            }
            Event::HardBreak => {
                out.push(MdStyledSpan {
                    text: "\n".into(),
                    style_kind: MdStyleKind::Body,
                });
            }
            Event::Rule => {
                out.push(MdStyledSpan {
                    text: "\n───\n".into(),
                    style_kind: MdStyleKind::Body,
                });
            }
            Event::DisplayMath(_) => {
                out.push(MdStyledSpan {
                    text: "[math]".into(),
                    style_kind: MdStyleKind::InlineCode,
                });
            }
            _ => {}
        }
    }

    // If the parser was in the middle of a streaming block (e.g.
    // unterminated code fence), output never got a trailing newline.
    // Add one so the next block starts fresh.
    if !out.is_empty() && !out.last().map(|s: &MdStyledSpan| s.text.ends_with('\n')).unwrap_or(true) {
        out.push(MdStyledSpan {
            text: "\n".into(),
            style_kind: MdStyleKind::Body,
        });
    }

    out
}

// ---------------------------------------------------------------------------
// Mapping a wrapped fragment back to styled spans
// ---------------------------------------------------------------------------

/// Given a list of logical spans (from [`render_md_to_spans`]) and a
/// character range `[fragment_start, fragment_end)` into the
/// concatenated plain text of those spans, produce ratatui `Span`s
/// with the correct style for each sub-segment.
///
/// `fragment_start` / `fragment_end` are byte offsets into the
/// concatenated plain text of `all_spans` (which may not be
/// contiguous — each span's text is concatenated).
pub fn spans_for_fragment(
    all_spans: &[MdStyledSpan],
    fragment_start: usize,
    fragment_end: usize,
    base_style: Style,
    palette: &Palette,
) -> Vec<Span<'static>> {
    if all_spans.is_empty() {
        return vec![Span::styled("", base_style)];
    }

    let mut result: Vec<Span<'static>> = Vec::new();
    let mut byte_offset = 0usize;

    for span in all_spans {
        let span_start = byte_offset;
        let span_end = byte_offset + span.text.len();

        // Does this span overlap [fragment_start, fragment_end)?
        if span_end <= fragment_start {
            byte_offset = span_end;
            continue;
        }
        if span_start >= fragment_end {
            break;
        }

        // Compute the in-span byte range, then clamp to the span's
        // actual length and snap both ends to UTF-8 char boundaries.
        // The fragment offsets come from the markdown *source* but
        // we're slicing the markdown-stripped span text — when the
        // source has formatting tokens (`**bold**`, `## header`, …)
        // those byte counts diverge and a naive `&text[a..b]` slice
        // panics on bounds or mid-char-boundary. Streaming partial
        // markdown + CJK / emoji content hits this within a frame
        // or two.
        let len = span.text.len();
        let raw_start = span_start.max(fragment_start) - span_start;
        let raw_end = span_end.min(fragment_end) - span_start;
        let mut overlap_start = raw_start.min(len);
        let mut overlap_end = raw_end.min(len);
        while overlap_start < len && !span.text.is_char_boundary(overlap_start) {
            overlap_start += 1;
        }
        while overlap_end > overlap_start && !span.text.is_char_boundary(overlap_end) {
            overlap_end -= 1;
        }

        if overlap_start < overlap_end {
            let text = &span.text[overlap_start..overlap_end];
            let style = apply_md_style(base_style, span.style_kind, palette);
            result.push(Span::styled(text.to_string(), style));
        }

        byte_offset = span_end;
    }

    if result.is_empty() {
        result.push(Span::styled("", base_style));
    }

    result
}

// ---------------------------------------------------------------------------
// MdStyledSpan methods
// ---------------------------------------------------------------------------

impl MdStyledSpan {
    /// Convert to a ratatui `Span` using the given palette.
    pub fn to_ratatui_span(&self, palette: &Palette) -> Span<'static> {
        let style = apply_md_style(Style::default(), self.style_kind, palette);
        Span::styled(self.text.clone(), style)
    }
}

/// Map an [`MdStyleKind`] to a concrete ratatui [`Style`], layering
/// on top of `base`.
pub fn apply_md_style(base: Style, kind: MdStyleKind, palette: &Palette) -> Style {
    match kind {
        MdStyleKind::Body => base,
        MdStyleKind::Bold => base.add_modifier(Modifier::BOLD),
        MdStyleKind::Italic => base.add_modifier(Modifier::ITALIC),
        MdStyleKind::InlineCode => base.bg(palette.surface).fg(palette.info),
        MdStyleKind::CodeBlock => base.bg(palette.surface).fg(palette.subdued),
        MdStyleKind::Heading => base.fg(palette.accent).add_modifier(Modifier::BOLD),
        MdStyleKind::Blockquote => base.fg(palette.muted).add_modifier(Modifier::ITALIC),
        MdStyleKind::Link => base.fg(palette.info).add_modifier(Modifier::UNDERLINED),
        MdStyleKind::Image => base.fg(palette.muted).add_modifier(Modifier::ITALIC),
        MdStyleKind::ListBullet => base.fg(palette.secondary),
    }
}

// ---------------------------------------------------------------------------
// Helper: check whether a transcript line likely contains markdown
// ---------------------------------------------------------------------------

/// Quick heuristic to decide whether to render `text` through the
/// markdown pipeline. Avoids unnecessary parsing for lines that are
/// clearly plain text (tool output, error messages, etc.).
///
/// Returns `true` for text containing CommonMark triggers: `*`, `_`,
/// `` ` ``, `#`, `>`, `-`, `[`.
pub fn looks_like_markdown(text: &str) -> bool {
    text.contains('*')
        || text.contains('`')
        || text.contains('#')
        || text.contains('>')
        || text.contains('[')
        || text.contains("__")
        || text.contains("~~")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn test_palette() -> Palette {
        Palette {
            accent: Color::Rgb(0xc6, 0x78, 0xdd),
            secondary: Color::Rgb(0x56, 0xb6, 0xc2),
            bg: Color::Rgb(0x0e, 0x0f, 0x16),
            fg: Color::Rgb(0xeb, 0xeb, 0xeb),
            subdued: Color::Rgb(0xab, 0xad, 0xb8),
            muted: Color::Rgb(0x6c, 0x70, 0x86),
            selection: Color::Rgb(0x2a, 0x2a, 0x3e),
            error: Color::Rgb(0xe0, 0x6c, 0x75),
            warning: Color::Rgb(0xe5, 0xc0, 0x7b),
            success: Color::Rgb(0x98, 0xc3, 0x79),
            info: Color::Rgb(0x61, 0xaf, 0xef),
            surface: Color::Rgb(0x1e, 0x1e, 0x2e),
        }
    }

    #[test]
    fn plain_text_passes_through() {
        let spans = render_md_to_spans("hello world");
        assert_eq!(spans.len(), 2); // "hello world" + trailing newline
        assert_eq!(spans[0].text, "hello world");
        assert_eq!(spans[0].style_kind, MdStyleKind::Body);
    }

    #[test]
    fn bold_text() {
        let spans = render_md_to_spans("hello **world** today");
        let non_newline: Vec<_> = spans.iter().filter(|s| s.text != "\n").collect();
        assert_eq!(non_newline.len(), 3);
        assert_eq!(non_newline[0].style_kind, MdStyleKind::Body); // "hello "
        assert_eq!(non_newline[1].style_kind, MdStyleKind::Bold); // "world"
        assert_eq!(non_newline[2].style_kind, MdStyleKind::Body); // " today"
    }

    #[test]
    fn italic_text() {
        let spans = render_md_to_spans("this is *very* nice");
        let non_newline: Vec<_> = spans.iter().filter(|s| s.text != "\n").collect();
        assert!(non_newline.iter().any(|s| s.style_kind == MdStyleKind::Italic));
    }

    #[test]
    fn inline_code() {
        let spans = render_md_to_spans("use `cargo build` now");
        let non_newline: Vec<_> = spans.iter().filter(|s| s.text != "\n").collect();
        let code_span = non_newline
            .iter()
            .find(|s| s.style_kind == MdStyleKind::InlineCode)
            .unwrap();
        assert_eq!(code_span.text, "cargo build");
    }

    #[test]
    fn code_block() {
        let spans = render_md_to_spans("```rust\nfn main() {}\n```\n");
        let code: Vec<_> = spans
            .iter()
            .filter(|s| s.style_kind == MdStyleKind::CodeBlock)
            .collect();
        assert!(
            code.iter().any(|s| s.text.contains("fn main")),
            "should contain code body: {spans:?}"
        );
    }

    #[test]
    fn tight_list_items_render_on_separate_lines() {
        let spans = render_md_to_spans("- `one`\n- `two`\n");
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("  • one\n  • two\n"), "{text:?}");
    }

    #[test]
    fn heading() {
        let spans = render_md_to_spans("## Hello");
        let heading_span = spans
            .iter()
            .find(|s| s.style_kind == MdStyleKind::Heading)
            .unwrap();
        assert!(heading_span.text.contains("Hello"));
    }

    #[test]
    fn unterminated_bold() {
        // Streaming: opening ** without closing **.
        let spans = render_md_to_spans("hello **world");
        // pulldown-cmark treats unclosed ** as two separate * chars
        // (literal text), waiting for the closing ** before emitting
        // Strong. The text still renders without panic.
        let non_newline: Vec<_> = spans.iter().filter(|s| s.text != "\n").collect();
        let all_text: String = non_newline.iter().map(|s| s.text.as_str()).collect();
        assert!(all_text.contains("world"), "should contain 'world': {all_text}");
        // The ** appears as literal * * in the output — that's
        // correct CommonMark behavior for unterminated delimiters.
    }

    #[test]
    fn unterminated_code_block() {
        // Streaming: open fence but no close.
        let spans = render_md_to_spans("```rust\nfn main() {\n");
        // Should contain the code content.
        let code: Vec<_> = spans
            .iter()
            .filter(|s| s.style_kind == MdStyleKind::CodeBlock)
            .collect();
        assert!(!code.is_empty(), "should have code block spans: {spans:?}");
    }

    #[test]
    fn looks_like_markdown_heuristic() {
        assert!(looks_like_markdown("**bold**"));
        assert!(looks_like_markdown("`code`"));
        assert!(looks_like_markdown("# heading"));
        assert!(looks_like_markdown("> quote"));
        assert!(looks_like_markdown("[link](url)"));
        assert!(!looks_like_markdown("plain text without triggers"));
    }

    #[test]
    fn fragment_mapping_middle_of_span() {
        let spans = vec![
            MdStyledSpan {
                text: "hello ".into(),
                style_kind: MdStyleKind::Body,
            },
            MdStyledSpan {
                text: "world".into(),
                style_kind: MdStyleKind::Bold,
            },
        ];
        let p = test_palette();
        // "hello world" → fragment "ello wo" (bytes 1..8)
        let out = spans_for_fragment(&spans, 1, 8, Style::default(), &p);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content, "ello "); // body
        assert_eq!(out[1].content, "wo"); // bold
        assert!(out[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn fragment_mapping_exact_span_boundary() {
        let spans = vec![
            MdStyledSpan {
                text: "aa".into(),
                style_kind: MdStyleKind::Body,
            },
            MdStyledSpan {
                text: "bb".into(),
                style_kind: MdStyleKind::Bold,
            },
            MdStyledSpan {
                text: "cc".into(),
                style_kind: MdStyleKind::Body,
            },
        ];
        let p = test_palette();
        // fragment = "abb" → bytes 1..4
        let out = spans_for_fragment(&spans, 1, 4, Style::default(), &p);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content, "a"); // body tail
        assert_eq!(out[1].content, "bb"); // full bold
        assert!(out[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn markdown_cache_hit() {
        let mut cache = MarkdownCache::new();
        cache.resize(3);
        let p = test_palette();
        // Parse once, cache.
        let _ = cache.get_or_parse(0, "**bold**", false, &p);
        assert!(cache.entries[0].is_some());
        // Streaming line at index 1 — should NOT be cached.
        let _ = cache.get_or_parse(1, "*italic*", true, &p);
        assert!(cache.entries[1].is_none(), "streaming must not cache");
    }

    #[test]
    fn markdown_cache_resize_keeps_existing() {
        let mut cache = MarkdownCache::new();
        cache.resize(1);
        let p = test_palette();
        let _ = cache.get_or_parse(0, "hi", false, &p);
        assert!(cache.entries[0].is_some());
        cache.resize(3);
        // First entry preserved.
        assert!(cache.entries[0].is_some());
        assert!(cache.entries[1].is_none());
        assert!(cache.entries[2].is_none());
    }
}
