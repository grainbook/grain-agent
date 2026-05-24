//! `web_fetch` tool — HTTP GET a URL and return a simplified text view.
//!
//! Mirrors a basic version of pi's web-fetch capability. Stays light: no
//! headless browser, no JavaScript execution. Reqwest with chunked read,
//! private-IP rejection, redirect cap, and tail-truncation for the
//! transcript.
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
//! - **SSRF guard**: the host is resolved before the request fires and
//!   the resolved IPs are checked against loopback / private / link-
//!   local / CGNAT ranges. Each redirect target is re-validated through
//!   a custom redirect policy.
//! - Body is **streamed** chunk-by-chunk and capped at `max_bytes` — we
//!   never materialize more than that into a single buffer regardless
//!   of what the server promises.
//! - Content-Type tells us whether to strip HTML or keep plain text.
//! - HTML stripping is intentionally crude: entity decoding runs
//!   **before** tag stripping so escaped-text `&lt;script&gt;` doesn't
//!   masquerade as a real script tag in the output we feed the LLM.

use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use grain_agent_harness::{TruncationOptions, format_size, truncate_tail};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use url::Url;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_BYTES: usize = 512 * 1024;
const HARD_MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
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
                     Body read is streaming, hard-capped at {HARD_MAX_BYTES} bytes. \
                     Refuses private / loopback / link-local / CGNAT addresses to block SSRF; \
                     redirects are limited to {MAX_REDIRECTS} hops and each hop is re-validated."
                ),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Absolute HTTP or HTTPS URL. Internal addresses (loopback / private / link-local / CGNAT) are refused."
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
        let args: FetchArgs =
            serde_json::from_value(args).map_err(|e| AgentToolError::Validation(e.to_string()))?;
        if cancel.is_cancelled() {
            return Err(AgentToolError::Aborted);
        }

        // --- URL + SSRF guard (parsed URL first; scheme + host both validated) ---
        let parsed = Url::parse(&args.url)
            .map_err(|e| AgentToolError::Validation(format!("invalid url: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(AgentToolError::Validation(format!(
                "url must be http:// or https://, got {:?}",
                parsed.scheme()
            )));
        }
        validate_public_host(&parsed)?;

        let timeout = Duration::from_millis(
            args.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );
        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .min(HARD_MAX_BYTES);

        // --- Client: bounded redirects + custom hop validation ---
        let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            // Re-run the host guard on every redirect target so a public URL
            // can't redirect into the AWS metadata service or localhost.
            match validate_public_host(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(format!("redirect rejected: {e}")),
            }
        });
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(redirect_policy)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| AgentToolError::msg(format!("reqwest build: {e}")))?;

        // --- Race the initial request against cancel ---
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(AgentToolError::Aborted),
            res = client.get(parsed.as_str()).send() => res
                .map_err(|e| AgentToolError::msg(format!("GET {}: {e}", parsed)))?,
        };

        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        let final_url = resp.url().clone();

        // --- Streamed body read with hard cap ---
        // Never materialize more than `max_bytes` regardless of Content-Length
        // or server lying. We *also* track total streamed so the response
        // metadata is honest about how much arrived.
        let mut buf: Vec<u8> = Vec::with_capacity(max_bytes.min(64 * 1024));
        let mut total_streamed: usize = 0;
        let mut stream = resp.bytes_stream();
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => return Err(AgentToolError::Aborted),
                n = stream.next() => n,
            };
            let Some(chunk_res) = next else { break };
            let chunk = chunk_res.map_err(|e| AgentToolError::msg(format!("read body: {e}")))?;
            total_streamed = total_streamed.saturating_add(chunk.len());
            if buf.len() < max_bytes {
                let take = chunk.len().min(max_bytes - buf.len());
                buf.extend_from_slice(&chunk[..take]);
            }
            // Stop reading entirely once the over-cap excess is large; some
            // servers stream indefinitely, no point in burning bandwidth.
            if total_streamed > max_bytes.saturating_mul(4) {
                break;
            }
        }
        let truncated_at_fetch = total_streamed > buf.len();
        let total_bytes = total_streamed.max(buf.len());

        // Best-effort UTF-8 decode; non-UTF-8 bytes become replacement chars.
        let decoded = String::from_utf8_lossy(&buf).into_owned();
        let simplified = if is_html(&content_type, &decoded) {
            simplify_html(&decoded)
        } else {
            decoded
        };

        // Tail-truncate for transcript budget.
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
            final_url,
            if content_type.is_empty() {
                "?"
            } else {
                &content_type
            }
        ));
        body.push_str(&trunc.content);
        if truncated_at_fetch {
            body.push_str(&format!(
                "\n\n[Body truncated at fetch time: kept first {} of {} bytes]",
                format_size(buf.len()),
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
                "finalUrl": final_url.to_string(),
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

/// Validate that the URL's host is publicly routable. Resolves the host
/// and refuses if any returned address is loopback, RFC 1918 private,
/// link-local, CGNAT (100.64.0.0/10), or unspecified.
///
/// Returns `Err(AgentToolError::Validation)` so the failure is surfaced
/// as a tool validation error rather than a runtime msg.
fn validate_public_host(url: &Url) -> Result<(), AgentToolError> {
    let Some(host) = url.host_str() else {
        return Err(AgentToolError::Validation("url has no host".into()));
    };
    let port = url.port_or_known_default().unwrap_or(80);
    // `to_socket_addrs` does DNS resolution + parses IP literals. Blocking
    // syscall — but this runs once before the request fires, and the cost
    // of letting the tool burn an executor thread for a single getaddrinfo
    // is acceptable vs. the alternative of taking a DNS dep just for this.
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| AgentToolError::Validation(format!("resolve {host}: {e}")))?;
    let mut had_any = false;
    for addr in addrs {
        had_any = true;
        if is_private_ip(&addr.ip()) {
            return Err(AgentToolError::Validation(format!(
                "refusing private / internal address: {} resolves to {}",
                host,
                addr.ip()
            )));
        }
    }
    if !had_any {
        return Err(AgentToolError::Validation(format!(
            "host {host} did not resolve to any address"
        )));
    }
    Ok(())
}

/// True if the IP belongs to a non-public range that the agent should
/// not be able to reach. Conservative — anything not clearly public is
/// rejected.
pub(crate) fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || is_cgnat(v4)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // v6 unique-local / link-local
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // v6-mapped v4: check the embedded v4
                || v6.to_ipv4().is_some_and(|v4| is_private_ip(&IpAddr::V4(v4)))
        }
    }
}

fn is_cgnat(ip: &Ipv4Addr) -> bool {
    // 100.64.0.0/10
    let oct = ip.octets();
    oct[0] == 100 && (oct[1] & 0xc0) == 64
}

fn is_html(content_type: &str, body: &str) -> bool {
    if content_type.contains("text/html") || content_type.contains("application/xhtml") {
        return true;
    }
    // Heuristic for servers that don't set content-type properly.
    let head: &str = body.get(..body.len().min(512)).unwrap_or(body);
    head.contains("<html") || head.contains("<!DOCTYPE html")
}

/// Crude HTML simplifier. Entity decoding happens **first** so escaped
/// text like `&lt;script&gt;` becomes a literal `<script>` *before* we
/// strip tags — which then correctly removes it. The reverse order would
/// preserve fake-looking script tags in the LLM-visible output.
fn simplify_html(input: &str) -> String {
    let decoded = decode_entities(input);
    let stripped_scripts = strip_block(&decoded, "<script", "</script>");
    let stripped_styles = strip_block(&stripped_scripts, "<style", "</style>");
    let mut out = String::with_capacity(stripped_styles.len());
    let mut in_tag = false;
    let mut prev_ws = false;
    for ch in stripped_styles.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
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
    out.trim().to_string()
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
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
        assert!(out.contains("before"));
        assert!(!out.contains("let x"));
    }

    #[test]
    fn simplify_html_decodes_entities_before_stripping_tags() {
        // The literal text `&lt;script&gt;alert(1)&lt;/script&gt;` is escaped
        // HTML — i.e. the page wants to *display* "<script>alert(1)</script>"
        // as text, not execute it. After decoding entities the resulting
        // angle-bracketed content should be stripped, not surface as visible
        // pseudo-script in the LLM output.
        let input = "<p>safe&lt;script&gt;alert(1)&lt;/script&gt;safe</p>";
        let out = simplify_html(input);
        // The "safe" markers survive on both sides; the script-looking
        // payload (now actual angle-bracketed text after decoding) is
        // dropped by strip_block.
        assert!(out.contains("safe"));
        assert!(!out.contains("alert"));
        assert!(!out.contains("<script>"));
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
    }

    #[tokio::test]
    async fn rejects_unparseable_url() {
        let tool = WebFetchTool::new();
        let on_update: ToolUpdateCallback = Arc::new(|_| {});
        let err = tool
            .execute(
                "c",
                serde_json::json!({ "url": "not a url" }),
                CancellationToken::new(),
                on_update,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentToolError::Validation(_)));
    }

    #[tokio::test]
    async fn cancel_short_circuits_before_request() {
        let tool = WebFetchTool::new();
        let on_update: ToolUpdateCallback = Arc::new(|_| {});
        let token = CancellationToken::new();
        token.cancel();
        // Use a valid public URL so the validation passes; the cancel
        // check beats the actual request.
        let err = tool
            .execute(
                "c",
                serde_json::json!({ "url": "https://example.com" }),
                token,
                on_update,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentToolError::Aborted));
    }

    #[test]
    fn is_private_ip_blocks_loopback_and_rfc1918() {
        for s in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254", // AWS / GCP metadata service
            "100.64.0.1",      // CGNAT
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(is_private_ip(&ip), "should refuse {s}");
        }
    }

    #[test]
    fn is_private_ip_allows_public_addresses() {
        for s in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(!is_private_ip(&ip), "should allow {s}");
        }
    }

    #[tokio::test]
    async fn rejects_literal_private_ip_in_url() {
        let tool = WebFetchTool::new();
        let on_update: ToolUpdateCallback = Arc::new(|_| {});
        // Direct IP literal — no DNS lookup needed; the validator should
        // still catch it.
        let err = tool
            .execute(
                "c",
                serde_json::json!({ "url": "http://169.254.169.254/latest/meta-data/" }),
                CancellationToken::new(),
                on_update,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, AgentToolError::Validation(_)));
        assert!(
            msg.contains("private") || msg.contains("internal"),
            "msg: {msg}"
        );
    }

    #[tokio::test]
    async fn rejects_localhost() {
        let tool = WebFetchTool::new();
        let on_update: ToolUpdateCallback = Arc::new(|_| {});
        let err = tool
            .execute(
                "c",
                serde_json::json!({ "url": "http://localhost:6379/" }),
                CancellationToken::new(),
                on_update,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentToolError::Validation(_)));
    }
}
