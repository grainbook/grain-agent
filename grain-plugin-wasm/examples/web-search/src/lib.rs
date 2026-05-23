//! Web-search plugin — Exa search + URL fetch as a grain WASM plugin.
//!
//! Inspired by the TypeScript pi-web-access plugin (Exa / Perplexity /
//! Gemini Web / GitHub / YouTube / PDFs). Our WASM Component Model
//! sandbox doesn't have native deps (ffmpeg, browser cookies, PDF
//! parsers), so we ship only the providers that are pure HTTP + JSON:
//!
//! - `web_search` — Exa's `/search` endpoint, auth via `EXA_API_KEY`.
//! - `web_fetch`  — Plain HTTP GET; returns body truncated to ~16 KiB.
//!
//! Capabilities the manifest must grant: `["http", "env", "log"]`.
//!
//! Build: `cargo component build --release` (needs `cargo-component`
//! + the `wasm32-wasip2` target).

#![allow(clippy::all)]

wit_bindgen::generate!({
    world: "grain-plugin",
    path: "wit",
});

use grain::plugin::host::{self, LogLevel};
use serde::{Deserialize, Serialize};

struct WebSearchPlugin;

// ---------------------------------------------------------------------------
// JSON Schemas the LLM sees when deciding to invoke either tool.
// ---------------------------------------------------------------------------

const WEB_SEARCH_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "query": {
      "type": "string",
      "description": "Natural-language search query."
    },
    "num_results": {
      "type": "integer",
      "minimum": 1,
      "maximum": 20,
      "default": 5,
      "description": "How many results to return (1-20)."
    }
  },
  "required": ["query"]
}"#;

const WEB_FETCH_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "url": {
      "type": "string",
      "description": "Absolute HTTP(S) URL to fetch."
    }
  },
  "required": ["url"]
}"#;

const FETCH_BODY_MAX_BYTES: usize = 16 * 1024;

// ---------------------------------------------------------------------------
// Tool argument / result shapes.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    num_results: Option<u32>,
}

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
}

#[derive(Serialize)]
struct SearchResultItem {
    title: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    published_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
}

#[derive(Serialize)]
struct SearchOutput {
    query: String,
    results: Vec<SearchResultItem>,
    /// Provider that produced the results — currently always "exa".
    provider: &'static str,
}

#[derive(Serialize)]
struct FetchOutput {
    url: String,
    status: u16,
    body: String,
    truncated: bool,
}

// ---------------------------------------------------------------------------
// Tool implementations.
// ---------------------------------------------------------------------------

fn err_result(msg: impl Into<String>) -> exports::grain::plugin::plugin::ToolResult {
    let msg = msg.into();
    host::log(LogLevel::Error, &msg);
    exports::grain::plugin::plugin::ToolResult {
        content_json: serde_json::json!({ "error": msg }).to_string(),
        is_error: true,
    }
}

fn ok_result(value: impl Serialize) -> exports::grain::plugin::plugin::ToolResult {
    match serde_json::to_string(&value) {
        Ok(s) => exports::grain::plugin::plugin::ToolResult {
            content_json: s,
            is_error: false,
        },
        Err(e) => err_result(format!("serialize result: {e}")),
    }
}

fn do_search(args: SearchArgs) -> exports::grain::plugin::plugin::ToolResult {
    let api_key = match host::env_get("EXA_API_KEY") {
        Some(k) if !k.is_empty() => k,
        _ => return err_result("EXA_API_KEY not set in host environment"),
    };
    let n = args.num_results.unwrap_or(5).clamp(1, 20);
    let payload = serde_json::json!({
        "query": args.query,
        "numResults": n,
        "type": "auto",
        "contents": { "highlights": { "numSentences": 2 } },
    });
    let payload_str = payload.to_string();

    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("x-api-key".to_string(), api_key),
    ];

    host::log(
        LogLevel::Info,
        &format!("exa search: q={:?} n={}", args.query, n),
    );

    let resp = match host::http_post("https://api.exa.ai/search", &headers, &payload_str) {
        Ok(r) => r,
        Err(e) => return err_result(format!("exa http: {e}")),
    };
    if !(200..300).contains(&resp.status) {
        return err_result(format!(
            "exa HTTP {} — body: {}",
            resp.status,
            truncate(&resp.body, 256)
        ));
    }

    // Parse Exa's response shape leniently — extract title/url/etc per item.
    let parsed: serde_json::Value = match serde_json::from_str(&resp.body) {
        Ok(v) => v,
        Err(e) => return err_result(format!("exa json parse: {e}")),
    };
    let Some(items) = parsed.get("results").and_then(|v| v.as_array()) else {
        return err_result("exa response missing `results` array");
    };

    let results: Vec<SearchResultItem> = items
        .iter()
        .filter_map(|item| {
            let title = item.get("title")?.as_str()?.to_string();
            let url = item.get("url")?.as_str()?.to_string();
            let published_date = item
                .get("publishedDate")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let author = item
                .get("author")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let snippet = item
                .get("highlights")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| {
                    item.get("text")
                        .and_then(|v| v.as_str())
                        .map(|s| truncate(s, 280))
                });
            Some(SearchResultItem {
                title,
                url,
                published_date,
                author,
                snippet,
            })
        })
        .collect();

    ok_result(SearchOutput {
        query: args.query,
        results,
        provider: "exa",
    })
}

fn do_fetch(args: FetchArgs) -> exports::grain::plugin::plugin::ToolResult {
    if !args.url.starts_with("http://") && !args.url.starts_with("https://") {
        return err_result("url must start with http:// or https://");
    }
    host::log(LogLevel::Info, &format!("fetch: {}", args.url));
    let resp = match host::http_get(
        &args.url,
        &[(
            "User-Agent".to_string(),
            "grain-web-search-plugin/0.1".to_string(),
        )],
    ) {
        Ok(r) => r,
        Err(e) => return err_result(format!("fetch http: {e}")),
    };
    let (body, truncated) = if resp.body.len() > FETCH_BODY_MAX_BYTES {
        // Char-boundary safe truncation.
        let mut cut = FETCH_BODY_MAX_BYTES;
        while cut > 0 && !resp.body.is_char_boundary(cut) {
            cut -= 1;
        }
        (resp.body[..cut].to_string(), true)
    } else {
        (resp.body, false)
    };
    ok_result(FetchOutput {
        url: args.url,
        status: resp.status,
        body,
        truncated,
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

// ---------------------------------------------------------------------------
// Plugin entry points.
// ---------------------------------------------------------------------------

impl exports::grain::plugin::plugin::Guest for WebSearchPlugin {
    fn init() -> Result<exports::grain::plugin::plugin::PluginInfo, String> {
        host::log(LogLevel::Info, "web-search plugin loaded");
        Ok(exports::grain::plugin::plugin::PluginInfo {
            name: "web-search".to_string(),
            version: "0.1.0".to_string(),
        })
    }

    fn list_tools() -> Vec<exports::grain::plugin::plugin::ToolDef> {
        vec![
            exports::grain::plugin::plugin::ToolDef {
                name: "web_search".to_string(),
                label: "Web Search".to_string(),
                description:
                    "Search the live web via Exa. Returns title / url / author / publish \
                     date / snippet for each hit. Requires `EXA_API_KEY` in the host \
                     environment."
                        .to_string(),
                parameters_json: WEB_SEARCH_SCHEMA.to_string(),
            },
            exports::grain::plugin::plugin::ToolDef {
                name: "web_fetch".to_string(),
                label: "Fetch URL".to_string(),
                description:
                    "HTTP GET an arbitrary URL and return its body (truncated to 16 KiB \
                     for safety). Useful for following links returned by `web_search`."
                        .to_string(),
                parameters_json: WEB_FETCH_SCHEMA.to_string(),
            },
        ]
    }

    fn call_tool(name: String, args_json: String) -> exports::grain::plugin::plugin::ToolResult {
        match name.as_str() {
            "web_search" => match serde_json::from_str::<SearchArgs>(&args_json) {
                Ok(args) => do_search(args),
                Err(e) => err_result(format!("web_search args: {e}")),
            },
            "web_fetch" => match serde_json::from_str::<FetchArgs>(&args_json) {
                Ok(args) => do_fetch(args),
                Err(e) => err_result(format!("web_fetch args: {e}")),
            },
            other => err_result(format!("unknown tool: {other}")),
        }
    }
}

export!(WebSearchPlugin);
