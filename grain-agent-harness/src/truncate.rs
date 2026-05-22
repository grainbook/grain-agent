//! Truncation utilities for tool output.
//!
//! Ports `packages/agent/src/harness/utils/truncate.ts`. Uses byte counts
//! computed on UTF-8 (Rust strings are guaranteed UTF-8 so no surrogate
//! handling is needed, unlike the TS implementation).

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

#[derive(Debug, Clone)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TruncationOptions {
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
}

impl TruncationOptions {
    fn resolve(&self) -> (usize, usize) {
        (
            self.max_lines.unwrap_or(DEFAULT_MAX_LINES),
            self.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
        )
    }
}

/// Human-friendly byte count: `"512B"`, `"1.5KB"`, `"2.0MB"`.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Truncate content from the head (keep first N lines/bytes).
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.resolve();
    let total_bytes = content.len();
    let lines: Vec<&str> = content.split('\n').collect();
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let first_line_bytes = lines.first().map(|l| l.len()).unwrap_or(0);
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut byte_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;

    for (i, line) in lines.iter().enumerate() {
        if i >= max_lines {
            break;
        }
        let line_bytes = line.len() + if i > 0 { 1 } else { 0 };
        if byte_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        kept.push(line);
        byte_count += line_bytes;
    }

    if kept.len() >= max_lines && byte_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output = kept.join("\n");
    let output_bytes = output.len();
    let output_lines = kept.len();
    TruncationResult {
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines,
        output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate content from the tail (keep last N lines/bytes).
pub fn truncate_tail(content: &str, options: TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.resolve();
    let total_bytes = content.len();
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let mut kept: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    let mut byte_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    let mut partial_owned: Option<String> = None;

    for (i, line) in lines.iter().enumerate().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + if !kept.is_empty() { 1 } else { 0 };
        if byte_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            // Edge case: first iteration and line alone exceeds maxBytes.
            if kept.is_empty() {
                let truncated_line = truncate_str_to_bytes_from_end(line, max_bytes);
                byte_count = truncated_line.len();
                partial_owned = Some(truncated_line);
                last_line_partial = true;
            }
            let _ = i;
            break;
        }
        kept.push_front(line);
        byte_count += line_bytes;
    }

    if kept.len() >= max_lines && byte_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output = if let Some(partial) = partial_owned {
        partial
    } else {
        let kept_vec: Vec<&str> = kept.into_iter().collect();
        kept_vec.join("\n")
    };
    let output_bytes = output.len();
    let output_lines = if last_line_partial { 1 } else { output.matches('\n').count() + 1 };
    TruncationResult {
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines,
        output_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

fn truncate_str_to_bytes_from_end(s: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    // Walk char boundaries from the end.
    let bytes = s.as_bytes();
    let mut start = bytes.len();
    while start > 0 {
        // Move back to the previous char boundary.
        let mut prev = start - 1;
        while prev > 0 && !s.is_char_boundary(prev) {
            prev -= 1;
        }
        if bytes.len() - prev > max_bytes {
            break;
        }
        start = prev;
        if prev == 0 {
            break;
        }
    }
    s[start..].to_string()
}

/// Truncate a single line, returning the original or a `[truncated]`-suffixed version.
pub fn truncate_line(line: &str, max_chars: Option<usize>) -> (String, bool) {
    let limit = max_chars.unwrap_or(GREP_MAX_LINE_LENGTH);
    if line.chars().count() <= limit {
        (line.to_string(), false)
    } else {
        let head: String = line.chars().take(limit).collect();
        (format!("{head}... [truncated]"), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_truncation_for_small_input() {
        let result = truncate_head("a\nb\nc", TruncationOptions::default());
        assert!(!result.truncated);
        assert_eq!(result.content, "a\nb\nc");
    }

    #[test]
    fn head_truncates_by_lines() {
        let input: String = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let result = truncate_head(
            &input,
            TruncationOptions {
                max_lines: Some(10),
                max_bytes: None,
            },
        );
        assert!(result.truncated);
        assert_eq!(result.output_lines, 10);
    }

    #[test]
    fn tail_truncates_by_lines() {
        let input: String = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let result = truncate_tail(
            &input,
            TruncationOptions {
                max_lines: Some(5),
                max_bytes: None,
            },
        );
        assert!(result.truncated);
        assert_eq!(result.output_lines, 5);
        // Last line of input should be present in tail output.
        assert!(result.content.ends_with("line 49"));
    }

    #[test]
    fn truncate_line_appends_suffix() {
        let (text, was) = truncate_line(&"x".repeat(600), None);
        assert!(was);
        assert!(text.ends_with("[truncated]"));
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(2048), "2.0KB");
        assert_eq!(format_size(2 * 1024 * 1024), "2.0MB");
    }
}
