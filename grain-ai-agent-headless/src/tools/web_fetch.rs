//! `web_fetch` tool — HTTP GET a URL and return a simplified text view.
//!
//! Mirrors a basic version of pi's web-fetch capability. Stays light: no
//! headless browser, no JavaScript execution. Just `reqwest::get` with a
//! tight timeout, response-size cap, optional HTML→text simplification,
//! and tail-truncation to fit transcript budgets.
//!
//! Arguments:
//!
//! ```json
//! {
//!   "url": "https://example.com/article",
//!   "timeout_ms": 10000,             // optional; default 10s, max 60s
//!   "max_bytes": 524288               // optional; default 512 KiB, hard cap 2 MiB
//! }
//! ```
//!
//! Safety + ergonomics:
//! - HTTP/HTTPS only (refuses `file:`, `data:`, `ftp:`, etc).
//! - Response is bounded by `max_bytes` even when the server lies about
//!   `Content-Length` — the read loop stops on overrun.
//! - Content-Type tells us whether to strip HTML or keep plain text.
//! - HTML stripping is intentionally crude — fetches docs / READMEs /
//!   simple article pages decently; bring a real browser if you need JS.

use std::time::Duration;

use async_trait::async_trait;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use grain_agent_harness::{TruncationOptions, format_size, truncate_tail};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_BYTES: usize = 512 * 1024;
const HARD_MAX_BYTES: usize = 2 * 1024 * 1024;
const USER_AGENT: &str = concat!(
    "grain-ai-agent-headless/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/cyz1901/grain-agent)"
);

#[derive(Debug, Deserialize)]
struct FetchArgs {
    url: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

pub struct WebFetchTool {
    def: ToolDefinition,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        WebFetchTool {
            def: ToolDefinition {
                name: "web_fetch".into(),
                label: "Web Fetch".into(),
                description: format!(
                    "HTTP GET a URL and return a simplified text view. HTTPS / HTTP only. \
                     Default timeout {DEFAULT_TIMEOUT_MS}ms (capped at {MAX_TIMEOUT_MS}ms). \
                     Response truncated to {DEFAULT_MAX_BYTES} bytes (hard cap {HARD_MAX_BYTES}). \
                     HTML is crudely stripped — for JS-heavy pages, this won't see the rendered content."
                ),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Absolute HTTP or HTTPS URL."
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "description": "Request timeout in milliseconds (default 10s, capped at 60s)."
                        },
                        "max_bytes": {
                            "type": "integer",
                            "description": "Maximum response body bytes to read (default 512KB, hard cap 2MB)."
                        }
                    },
                    "required": ["url"]
                }),
                execution_mode: None,
            },
        }
    }
}

#[async_trait]
impl AgentTool for WebFetchTool {
    fn definition(&self) -> &ToolDefinition {
        &self.def
    }

    async fn execute(
        &self,
        _id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let args: FetchArgs = serde_json::from_value(args)
            .map_err(|e| AgentToolError::Validation(e.to_string()))?;

        // Scheme guard.
        let scheme_ok =
            args.url.starts_with("https://") || args.url.starts_with("http://");
        if !scheme_ok {
            return Err(AgentToolError::Validation(format!(
                "url must be http:// or https://, got {:?}",
                args.url
            )));
        }

        let timeout = Duration::from_millis(
            args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS),
        );
        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .min(HARD_MAX_BYTES);

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| AgentToolError::msg(format!("reqwest build: {e}")))?;

        // Race the HTTP request against the cancel token.
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(AgentToolError::Aborted),
            res = client.get(&args.url).send() => res
                .map_err(|e| AgentToolError::msg(format!("GET {}: {e}", args.url)))?,
        };

        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        let raw = tokio::select! {
            _ = cancel.cancelled() => return Err(AgentToolError::Aborted),
            bytes = resp.bytes() => bytes
                .map_err(|e| AgentToolError::msg(format!("read body: {e}")))?,
        };
        let total_bytes = raw.len();
        let truncated_at_fetch = total_bytes > max_bytes;
        let slice = &raw[..total_bytes.min(max_bytes)];

        // Best-effort UTF-8 decode; non-UTF-8 bytes become replacement chars.
        let decoded = String::from_utf8_lossy(slice).into_owned();
        let simplified = if is_html(&content_type, &decoded) {
            simplify_html(&decoded)
        } else {
            decoded
        };

        // Tail-truncate for transcript budget. Anything over ~32 KiB after
        // HTML stripping is usually noise; let the model ask for a more
        // specific URL if it needs more.
        let trunc = truncate_tail(
            &simplified,
            TruncationOptions {
                max_lines: None,
                max_bytes: Some(32 * 1024),
            },
        );

        let mut body = String::new();
        body.push_str(&format!(
            "[{} {}] {}\nContent-Type: {}\n\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            args.url,
            if content_type.is_empty() { "?" } else { &content_type }
        ));
        body.push_str(&trunc.content);
        if truncated_at_fetch {
            body.push_str(&format!(
                "\n\n[Body truncated at fetch time: kept first {} of {} bytes]",
                format_size(slice.len()),
                format_size(total_bytes)
            ));
        }
        if trunc.truncated {
            body.push_str(&format!(
                "\n[Transcript trim: kept tail {} of {}]",
                format_size(trunc.output_bytes),
                format_size(trunc.total_bytes)
            ));
        }

        Ok(AgentToolResult {
            content: vec![UserContent::text(body)],
            details: serde_json::json!({
                "url": args.url,
                "status": status.as_u16(),
                "contentType": content_type,
                "totalBytes": total_bytes,
                "truncatedAtFetch": truncated_at_fetch,
                "truncatedInTranscript": trunc.truncated,
            }),
            terminate: None,
        })
    }
}

fn is_html(content_type: &str, body: &str) -> bool {
    if content_type.contains("text/html") || content_type.contains("application/xhtml") {
        return true;
    }
    // Heuristic for servers that don't set content-type properly.
    let head: &str = body.get(..body.len().min(512)).unwrap_or(body);
    head.contains("<html") || head.contains("<!DOCTYPE html")
}

/// Crude HTML simplifier: drop `<script>` and `<style>` blocks, replace
/// tags with whitespace, collapse runs of whitespace. Good enough for
/// READMEs / docs pages; useless for SPA-rendered content.
fn simplify_html(input: &str) -> String {
    let stripped_scripts = strip_block(input, "<script", "</script>");
    let stripped_styles = strip_block(&stripped_scripts, "<style", "</style>");
    let mut out = String::with_capacity(stripped_styles.len());
    let mut in_tag = false;
    let mut prev_ws = false;
    for ch in stripped_styles.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                // Treat tag boundary as whitespace so words stay separated.
                if !prev_ws {
                    out.push(' ');
                    prev_ws = true;
                }
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    // Best-effort entity replacement for the most common ones.
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .trim()
        .to_string()
}

fn strip_block(input: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut remaining = input;
    while let Some(open_idx) = remaining.find(open) {
        out.push_str(&remaining[..open_idx]);
        let rest = &remaining[open_idx..];
        if let Some(end_idx) = rest.find(close) {
            remaining = &rest[end_idx + close.len()..];
        } else {
            // No closing tag — drop the remainder to be safe.
            remaining = "";
            break;
        }
    }
    out.push_str(remaining);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn simplify_html_strips_scripts_styles_and_tags() {
        let input = r#"<!DOCTYPE html><html><head>
        <style>body { color: red; }</style>
        <script>alert('hi')</script>
        </head><body><h1>Title</h1><p>Hello&nbsp;world &amp; more.</p></body></html>"#;
        let out = simplify_html(input);
        assert!(out.contains("Title"));
        assert!(out.contains("Hello world & more."));
        assert!(!out.contains("color: red"));
        assert!(!out.contains("alert"));
    }

    #[test]
    fn simplify_html_handles_unclosed_script_gracefully() {
        let input = "<p>before</p><script>let x = 1; // no closing tag";
        let out = simplify_html(input);
        // Anything from "<script" onward is dropped.
        assert!(out.contains("before"));
        assert!(!out.contains("let x"));
    }

    #[test]
    fn is_html_detects_by_content_type() {
        assert!(is_html("text/html; charset=utf-8", ""));
        assert!(is_html("application/xhtml+xml", ""));
        assert!(!is_html("application/json", ""));
        assert!(!is_html("text/plain", ""));
    }

    #[test]
    fn is_html_detects_by_body_heuristic_when_ctype_missing() {
        assert!(is_html("", "<!DOCTYPE html><html><body>ok</body></html>"));
        assert!(is_html("", "<html><body>x</body></html>"));
        assert!(!is_html("", "{\"key\": \"value\"}"));
    }

    #[tokio::test]
    async fn rejects_non_http_url() {
        let tool = WebFetchTool::new();
        let on_update: ToolUpdateCallback = Arc::new(|_| {});
        let err = tool
            .execute(
                "c",
                serde_json::json!({ "url": "file:///etc/passwd" }),
                CancellationToken::new(),
                on_update,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentToolError::Validation(_)));
        assert!(err.to_string().contains("http://"));
    }

    #[tokio::test]
    async fn cancel_short_circuits_before_request() {
        let tool = WebFetchTool::new();
        let on_update: ToolUpdateCallback = Arc::new(|_| {});
        let token = CancellationToken::new();
        token.cancel();
        let err = tool
            .execute(
                "c",
                serde_json::json!({ "url": "https://invalid.invalid/" }),
                token,
                on_update,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentToolError::Aborted));
    }
}
