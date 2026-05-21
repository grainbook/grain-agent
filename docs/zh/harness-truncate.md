# `grain_agent_harness::truncate`

工具输出截断工具。对应 TS 参考实现 `packages/agent/src/harness/utils/truncate.ts`。所有计算基于 UTF-8 字节数——Rust `String` 一定是有效 UTF-8，所以不需要像 TS 端那样处理代理对。

## 常量与配置

```rust
pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;     // 50 KiB
pub const GREP_MAX_LINE_LENGTH: usize = 500;        // 单行截断默认上限

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
    pub last_line_partial: bool,              // tail 模式下首迭代单行超界时为 true
    pub first_line_exceeds_limit: bool,       // head 模式下首行超 max_bytes 时为 true
    pub max_lines: usize,
    pub max_bytes: usize,
}
```

`truncated == false` 时表示原文未被任何阈值触发，`content` 等于输入。

## `truncate_head`

保留前 N 行（或前 N 字节，先到为准）。

```rust
use grain_agent_harness::{TruncationOptions, truncate_head};

let result = truncate_head(&long_output, TruncationOptions {
    max_lines: Some(10),
    max_bytes: None,                  // → 默认 50 KiB
});

if result.truncated {
    println!("kept first {} lines ({}/{} bytes)",
        result.output_lines, result.output_bytes, result.total_bytes);
}
```

行为细节：

- 输入按 `\n` 切；行末的 `\n` 不在 `lines[i]` 里（split 行为）。
- 重新拼接时用 `\n` 连接，每行字节占用计算为 `line.len() + (i > 0 ? 1 : 0)`。
- **首行超 `max_bytes`** 的极端情况：`first_line_exceeds_limit = true`，`content = ""`，`truncated_by = Some(Bytes)`。
- 如果是因为行数耗完而非字节，`truncated_by = Lines`；否则 `Bytes`。

## `truncate_tail`

保留最后 N 行（或最后 N 字节）。常用于截尾日志。

```rust
let result = truncate_tail(&long_log, TruncationOptions {
    max_lines: Some(5),
    max_bytes: None,
});
assert!(result.content.ends_with(/* 最后一行 */));
```

细节：

- 末尾如果有空行（trailing `\n` 导致），会被去掉再统计；这意味着对原文 `"a\nb\nc\n"`，`total_lines = 3`。
- 首迭代单行就超 `max_bytes` 时：截掉头部，从某个 UTF-8 char boundary 开始截到 `max_bytes` 内，`last_line_partial = true`、`truncated_by = Bytes`，输出可能不再以完整字段开头。
- `output_lines` 在 `last_line_partial = true` 时强制为 1，否则按 `\n` 计数 + 1。

## `truncate_line`

单行截断（grep/cell 风格）：

```rust
use grain_agent_harness::truncate_line;

let (text, was_truncated) = truncate_line(&long_line, None /* = 500 chars */);
let (text2, _) = truncate_line(&long_line, Some(120));
```

按 **char** 数（非字节）计；超限时返回 `format!("{head}... [truncated]")`，`head` 是按 chars 取前 N 个。

## `format_size`

人类可读字节数：

```rust
use grain_agent_harness::format_size;

assert_eq!(format_size(512), "512B");
assert_eq!(format_size(2_048), "2.0KB");
assert_eq!(format_size(2 * 1024 * 1024), "2.0MB");
```

阈值是 1024（二进制 KiB / MiB），输出小数固定 1 位。常用于给模型展示“原输出多大、保留了多大”。

## 在工具里使用的典型模式

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

注意 `TruncatedBy` 现在没有从 `lib.rs` 重新导出；要拿到它需要 `use grain_agent_harness::truncate::TruncatedBy;`（如要常用，可在调用方包一层）。
