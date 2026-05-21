//! Recover tool calls that DeepSeek-R1 forgot to emit structurally.
//!
//! R1's `reasoning_content` is supposed to be free-form thought, but
//! the model occasionally writes JSON-shaped "I'll call tool X with
//! args …" passages there *instead of* emitting a proper `tool_calls`
//! entry. Calling code that only reads `tool_calls` then proceeds as
//! if no tool was requested, and the loop stalls.
//!
//! The scavenger sweeps the reasoning blob with a layered approach:
//!
//! 1. Find candidate JSON object substrings via a balanced-brace
//!    walker (regex alone can't match nested braces reliably).
//! 2. Parse each candidate as JSON and check for the shape
//!    `{ "name": string, "arguments": object|string }`.
//! 3. Yield the survivors as [`ScavengedToolCall`]. The caller
//!    decides whether to actually re-issue them — this module is a
//!    pure detector.

use serde_json::Value;

/// One tool call recovered from a `reasoning_content` blob.
#[derive(Debug, Clone, PartialEq)]
pub struct ScavengedToolCall {
    /// The `name` field — must be a non-empty string to be reported.
    pub name: String,
    /// The `arguments` field, normalized to a JSON `Object`. When the
    /// model wrote it as a stringified JSON object we parse the
    /// string; when it wrote a plain object we pass it through.
    pub arguments: Value,
    /// Byte offsets into the input where the JSON candidate lived.
    /// Useful for the caller to log / highlight / strip.
    pub span: (usize, usize),
}

/// Scan `reasoning_content` and return every plausible recoverable
/// tool call. Pure function — no I/O, no allocation beyond the
/// returned vec.
///
/// "Plausible" means: balanced JSON object containing a string
/// `"name"` and an `"arguments"` field that is either an object or a
/// stringified object. Anything else is dropped silently.
pub fn scavenge_tool_calls(reasoning: &str) -> Vec<ScavengedToolCall> {
    let mut out = Vec::new();
    for (start, end) in find_balanced_objects(reasoning) {
        let slice = &reasoning[start..end];
        let Ok(parsed) = serde_json::from_str::<Value>(slice) else {
            continue;
        };
        let Some(obj) = parsed.as_object() else { continue };
        let Some(name) = obj.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let Some(raw_args) = obj.get("arguments") else {
            continue;
        };
        let args = match raw_args {
            Value::Object(_) => raw_args.clone(),
            Value::String(s) => {
                // Stringified JSON object — common when the model
                // hand-rolls the call in prose.
                let Ok(reparsed) = serde_json::from_str::<Value>(s) else {
                    continue;
                };
                if !reparsed.is_object() {
                    continue;
                }
                reparsed
            }
            _ => continue,
        };
        out.push(ScavengedToolCall {
            name: name.to_string(),
            arguments: args,
            span: (start, end),
        });
    }
    out
}

/// Walk `s` left-to-right and return every byte range `[start, end)`
/// containing a balanced top-level `{ … }`. Strings and escapes are
/// respected so braces inside JSON values don't terminate the walk.
fn find_balanced_objects(s: &str) -> Vec<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = scan_balanced(bytes, i)
        {
            out.push((i, end));
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

/// Given `bytes[start] == b'{'`, return the index just past the
/// matching `}` (or `None` if unbalanced).
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
    fn scavenges_nothing_from_empty_reasoning() {
        assert!(scavenge_tool_calls("").is_empty());
    }

    #[test]
    fn scavenges_nothing_from_pure_prose() {
        let r = "I think we should grep for the offending TODO line and then maybe edit it.";
        assert!(scavenge_tool_calls(r).is_empty());
    }

    #[test]
    fn recovers_call_with_object_arguments() {
        let r = r#"I should call: {"name": "grep", "arguments": {"pattern": "TODO"}} and proceed."#;
        let calls = scavenge_tool_calls(r);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "grep");
        assert_eq!(calls[0].arguments, json!({"pattern": "TODO"}));
    }

    #[test]
    fn recovers_call_with_stringified_arguments() {
        // R1 sometimes serializes `arguments` as a JSON-encoded string.
        let r = r#"plan: {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}"#;
        let calls = scavenge_tool_calls(r);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, json!({"path": "src/main.rs"}));
    }

    #[test]
    fn recovers_multiple_calls_in_order() {
        let r = r#"
        Step 1: {"name": "grep", "arguments": {"pattern": "foo"}}
        Step 2: {"name": "read", "arguments": {"path": "f.txt"}}
        "#;
        let calls = scavenge_tool_calls(r);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "grep");
        assert_eq!(calls[1].name, "read");
        // Spans are increasing — order preserved.
        assert!(calls[0].span.1 <= calls[1].span.0);
    }

    #[test]
    fn ignores_objects_without_name_field() {
        let r = r#"{"arguments": {"x": 1}}"#;
        assert!(scavenge_tool_calls(r).is_empty());
    }

    #[test]
    fn ignores_objects_with_non_string_name() {
        let r = r#"{"name": 42, "arguments": {}}"#;
        assert!(scavenge_tool_calls(r).is_empty());
    }

    #[test]
    fn ignores_objects_with_empty_name() {
        let r = r#"{"name": "", "arguments": {}}"#;
        assert!(scavenge_tool_calls(r).is_empty());
    }

    #[test]
    fn ignores_objects_without_arguments_field() {
        let r = r#"{"name": "grep"}"#;
        assert!(scavenge_tool_calls(r).is_empty());
    }

    #[test]
    fn ignores_arguments_that_are_arrays_or_numbers() {
        let r1 = r#"{"name": "grep", "arguments": [1, 2, 3]}"#;
        let r2 = r#"{"name": "grep", "arguments": 42}"#;
        assert!(scavenge_tool_calls(r1).is_empty());
        assert!(scavenge_tool_calls(r2).is_empty());
    }

    #[test]
    fn handles_nested_braces_inside_arguments() {
        // The inner `{}` belongs to a value — must not split.
        let r = r#"{"name": "edit", "arguments": {"replace": {"old": "x", "new": "y"}}}"#;
        let calls = scavenge_tool_calls(r);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].arguments,
            json!({"replace": {"old": "x", "new": "y"}})
        );
    }

    #[test]
    fn handles_braces_inside_string_values() {
        // `}` inside a string must not close the object.
        let r = r#"{"name": "write", "arguments": {"text": "a}b{c"}}"#;
        let calls = scavenge_tool_calls(r);
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn handles_escaped_quotes_inside_string_values() {
        let r = r#"{"name": "write", "arguments": {"text": "say \"hi\""}}"#;
        let calls = scavenge_tool_calls(r);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].arguments,
            json!({"text": "say \"hi\""})
        );
    }

    #[test]
    fn unbalanced_object_yields_nothing() {
        let r = r#"plan: {"name": "grep", "arguments": {"pattern":"foo""#;
        assert!(scavenge_tool_calls(r).is_empty());
    }

    #[test]
    fn spans_point_at_recovered_substring() {
        let r = r#"Plan: {"name": "read", "arguments": {}} then continue."#;
        let calls = scavenge_tool_calls(r);
        assert_eq!(calls.len(), 1);
        let (s, e) = calls[0].span;
        assert!(r[s..e].starts_with('{'));
        assert!(r[s..e].ends_with('}'));
    }
}
