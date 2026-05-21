# `grain_agent_harness::truncate`

Tool-output truncation utilities. Corresponds to `packages/agent/src/harness/utils/truncate.ts` in the TS reference. All counts are UTF-8 byte counts — Rust `String` is guaranteed UTF-8, so no surrogate-pair handling is needed (unlike the TS source).

中文版：[zh/harness-truncate.md](./zh/harness-truncate.md).

## Constants & options

```rust
pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;     // 50 KiB
pub const GREP_MAX_LINE_LENGTH: usize = 500;        // default single-line cap

pub struct TruncationOptions {
    pub max_lines: Option<usize>,   // None → DEFAULT_MAX_LINES
    pub max_bytes: Option<usize>,   // None → DEFAULT_MAX_BYTES
}
```

## `TruncationResult`

```rust
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,    // Lines | Bytes
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,              // tail: when first iter's lone line was clipped
    pub first_line_exceeds_limit: bool,       // head: when first line alone exceeds max_bytes
    pub max_lines: usize,
    pub max_bytes: usize,
}
```

`truncated == false` means the input fits under both thresholds; `content` equals the input.

## `truncate_head`

Keep the first N lines / bytes (whichever fires first):

```rust
use grain_agent_harness::{TruncationOptions, truncate_head};

let result = truncate_head(&long_output, TruncationOptions {
    max_lines: Some(10),
    max_bytes: None,
});

if result.truncated {
    println!("kept first {} lines ({}/{} bytes)",
        result.output_lines, result.output_bytes, result.total_bytes);
}
```

Details:

- Input is split by `\n`; trailing `\n` doesn't appear in `lines[i]`.
- On rejoin, byte accounting uses `line.len() + (i > 0 ? 1 : 0)`.
- **First line > `max_bytes`**: returns `content = ""`, `first_line_exceeds_limit = true`, `truncated_by = Bytes`.
- `truncated_by = Lines` when line count was the cap; otherwise `Bytes`.

## `truncate_tail`

Keep the last N lines / bytes. Useful for log tails:

```rust
let result = truncate_tail(&long_log, TruncationOptions {
    max_lines: Some(5),
    max_bytes: None,
});
assert!(result.content.ends_with(/* last line */));
```

Details:

- A trailing empty line (from terminal `\n`) is stripped before counting; `"a\nb\nc\n"` → `total_lines = 3`.
- If even the last single line exceeds `max_bytes`: clipped at a UTF-8 char boundary, `last_line_partial = true`, `truncated_by = Bytes`. Output may not start on a complete field.
- `output_lines = 1` when `last_line_partial = true`, otherwise `\n` count + 1.

## `truncate_line`

Single-line truncation (grep / cell-style):

```rust
use grain_agent_harness::truncate_line;

let (text, was_truncated) = truncate_line(&long_line, None /* = 500 chars */);
let (text2, _) = truncate_line(&long_line, Some(120));
```

Counted in **chars**, not bytes. When over limit: `format!("{head}... [truncated]")` with `head` being the first N chars.

## `format_size`

Human-readable byte counts:

```rust
use grain_agent_harness::format_size;

assert_eq!(format_size(512), "512B");
assert_eq!(format_size(2_048), "2.0KB");
assert_eq!(format_size(2 * 1024 * 1024), "2.0MB");
```

Uses 1024-based units (binary KiB / MiB), 1 decimal place. Handy for telling the model how much of the original output you kept.

## Typical pattern inside a tool

```rust
use grain_agent_harness::{TruncationOptions, format_size, truncate_tail};

let raw = run_shell(cmd).await?;
let trunc = truncate_tail(&raw, TruncationOptions::default());

let body = if trunc.truncated {
    format!(
        "{}\n[Truncated by {}: kept last {} lines, {} of {}]",
        trunc.content,
        match trunc.truncated_by.unwrap() {
            grain_agent_harness::truncate::TruncatedBy::Lines => "lines",
            grain_agent_harness::truncate::TruncatedBy::Bytes => "bytes",
        },
        trunc.output_lines,
        format_size(trunc.output_bytes),
        format_size(trunc.total_bytes),
    )
} else {
    trunc.content
};

Ok(AgentToolResult::text(body))
```

`TruncatedBy` isn't re-exported from `lib.rs`; import via `use grain_agent_harness::truncate::TruncatedBy;` (or wrap once in your own helper).
