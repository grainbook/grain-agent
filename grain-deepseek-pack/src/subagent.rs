//! DeepSeek subagent completion-tag parser.
//!
//! When a DeepSeek subagent finishes, it emits a literal marker in the
//! assistant stream:
//!
//! ```text
//! <deepseek:subagent.done>
//! <deepseek:subagent.done>{"summary":"refactor complete","files":3}
//! ```
//!
//! The optional payload, when present, is a JSON object immediately
//! after the closing `>`. This module gives callers a typed view of
//! both — a passive parser, no I/O.

use serde_json::Value;

/// One subagent-done event recovered from assistant text.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentDoneEvent {
    /// JSON payload immediately after the tag. `None` when the tag
    /// appears bare (no payload).
    pub payload: Option<Value>,
    /// Byte offsets `[start, end)` of the matched marker (including
    /// the payload when present). Useful for stripping it out of the
    /// outgoing transcript so downstream callers don't see the tag.
    pub span: (usize, usize),
}

/// Marker we look for. Kept as a `pub` constant so callers can test
/// for its presence cheaply (`.contains(MARKER)`) before invoking
/// the full parser.
pub const MARKER: &str = "<deepseek:subagent.done>";

/// Parse every subagent-done marker in `text`. Returns events in
/// source order. Pure — no allocation beyond the result vec.
pub fn parse_subagent_done(text: &str) -> Vec<SubagentDoneEvent> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let marker_bytes = MARKER.as_bytes();
    let mut i = 0;
    while i + marker_bytes.len() <= bytes.len() {
        if &bytes[i..i + marker_bytes.len()] != marker_bytes {
            i += 1;
            continue;
        }
        let tag_end = i + marker_bytes.len();
        // Look for an optional JSON object glued to the tag (skipping
        // any whitespace between them).
        let mut cursor = tag_end;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let (payload, end) = if cursor < bytes.len() && bytes[cursor] == b'{' {
            if let Some(obj_end) = scan_balanced(bytes, cursor)
                && let Ok(parsed) = serde_json::from_str::<Value>(&text[cursor..obj_end])
                && parsed.is_object()
            {
                (Some(parsed), obj_end)
            } else {
                (None, tag_end)
            }
        } else {
            (None, tag_end)
        };
        out.push(SubagentDoneEvent {
            payload,
            span: (i, end),
        });
        i = end;
    }
    out
}

/// Same balanced-brace scanner as `reasoning_scavenge`. Duplicated
/// here (rather than imported) so the modules stay independently
/// testable — both are tiny.
fn scan_balanced(bytes: &[u8], start: usize) -> Option<usize> {
    debug_assert_eq!(bytes[start], b'{');
    let mut depth: u32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_match_yields_empty() {
        assert!(parse_subagent_done("nothing here").is_empty());
        assert!(parse_subagent_done("").is_empty());
    }

    #[test]
    fn bare_marker_has_no_payload() {
        let r = "All done. <deepseek:subagent.done>";
        let events = parse_subagent_done(r);
        assert_eq!(events.len(), 1);
        assert!(events[0].payload.is_none());
    }

    #[test]
    fn marker_with_payload() {
        let r = r#"work finished <deepseek:subagent.done>{"summary":"ok","files":3}"#;
        let events = parse_subagent_done(r);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].payload,
            Some(json!({"summary": "ok", "files": 3}))
        );
    }

    #[test]
    fn payload_after_whitespace_is_accepted() {
        let r = "<deepseek:subagent.done>\n   {\"k\":\"v\"}";
        let events = parse_subagent_done(r);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload, Some(json!({"k": "v"})));
    }

    #[test]
    fn invalid_json_payload_falls_back_to_bare() {
        let r = "<deepseek:subagent.done>{not json";
        let events = parse_subagent_done(r);
        assert_eq!(events.len(), 1);
        assert!(events[0].payload.is_none());
    }

    #[test]
    fn span_covers_marker_only_when_no_payload() {
        let r = "prefix <deepseek:subagent.done> tail";
        let events = parse_subagent_done(r);
        let (s, e) = events[0].span;
        assert_eq!(&r[s..e], "<deepseek:subagent.done>");
    }

    #[test]
    fn span_includes_payload_when_present() {
        let r = r#"<deepseek:subagent.done>{"x": 1}"#;
        let events = parse_subagent_done(r);
        let (s, e) = events[0].span;
        assert!(r[s..e].ends_with('}'));
    }

    #[test]
    fn multiple_markers_are_all_reported_in_order() {
        let r = concat!(
            "<deepseek:subagent.done>{\"id\":1} then ",
            "<deepseek:subagent.done>{\"id\":2}"
        );
        let events = parse_subagent_done(r);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload, Some(json!({"id": 1})));
        assert_eq!(events[1].payload, Some(json!({"id": 2})));
    }

    #[test]
    fn array_payload_is_not_attached() {
        // Only object payloads are accepted (mirrors scavenge_tool_calls).
        let r = r#"<deepseek:subagent.done>[1,2,3]"#;
        let events = parse_subagent_done(r);
        assert_eq!(events.len(), 1);
        assert!(events[0].payload.is_none());
    }

    #[test]
    fn marker_constant_is_what_we_search_for() {
        assert_eq!(MARKER, "<deepseek:subagent.done>");
    }
}
