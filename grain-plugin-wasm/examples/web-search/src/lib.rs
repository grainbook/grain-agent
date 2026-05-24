//! Web-search plugin — multi-provider search + URL fetch as a grain WASM plugin.
//!
//! Supported search providers (select with the `provider` parameter):
//!
//! - `exa`    — Exa API (https://exa.ai), auth via `EXA_API_KEY`.
//! - `tavily` — Tavily API (https://tavily.com), auth via `TAVILY_API_KEY`.
//! - `searxng`— SearXNG (https://docs.searxng.org), self-hosted;
//!              set `SEARXNG_BASE_URL` (e.g. `http://my-host:8080`).
//!
//! - `web_fetch` — Plain HTTP GET; returns body truncated to ~16 KiB.
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
// JSON Schema the LLM sees when deciding to invoke the tool.
// ---------------------------------------------------------------------------

const WEB_SEARCH_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "query": {
      "type": "string",
      "description": "Natural-language search query."
    },
    "provider": {
      "type": "string",
      "enum": ["exa", "tavily", "searxng", "anysearch"],
      "default": "exa",
      "description": "Search provider: exa (cloud, needs EXA_API_KEY), tavily (cloud, needs TAVILY_API_KEY), searxng (self-hosted, needs SEARXNG_BASE_URL), anysearch (cloud, optional ANYSEARCH_API_KEY)."
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
    provider: Option<String>,
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
    provider: String,
}

#[derive(Serialize)]
struct FetchOutput {
    url: String,
    status: u16,
    body: String,
    truncated: bool,
}

// ---------------------------------------------------------------------------
// Helpers
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

/// Extract the first sentence-worth of text from a longer field.
fn extract_snippet(text: &str) -> String {
    // Take first ~280 chars, break at sentence boundary when possible.
    let preview = truncate(text, 280);
    if let Some(dot) = preview.rfind(". ") {
        if dot > 40 {
            return preview[..=dot].to_string();
        }
    }
    preview
}

// ---------------------------------------------------------------------------
// Per-provider search implementations.
// ---------------------------------------------------------------------------

fn search_exa(args: &SearchArgs) -> exports::grain::plugin::plugin::ToolResult {
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

    host::log(LogLevel::Info, &format!("exa search: q={:?} n={}", args.query, n));

    let resp = match host::http_post(
        "https://api.exa.ai/search",
        &[
            ("Content-Type".to_string(), "application/json".to_string()),
            ("x-api-key".to_string(), api_key),
        ],
        &payload.to_string(),
    ) {
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
                        .map(|s| extract_snippet(s))
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
        query: args.query.clone(),
        results,
        provider: "exa".into(),
    })
}

fn search_tavily(args: &SearchArgs) -> exports::grain::plugin::plugin::ToolResult {
    let api_key = match host::env_get("TAVILY_API_KEY") {
        Some(k) if !k.is_empty() => k,
        _ => return err_result("TAVILY_API_KEY not set in host environment"),
    };
    let n = args.num_results.unwrap_or(5).clamp(1, 20);
    let payload = serde_json::json!({
        "query": args.query,
        "search_depth": "basic",
        "max_results": n,
    });

    host::log(LogLevel::Info, &format!("tavily search: q={:?} n={}", args.query, n));

    let resp = match host::http_post(
        "https://api.tavily.com/search",
        &[
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Authorization".to_string(), format!("Bearer {api_key}")),
        ],
        &payload.to_string(),
    ) {
        Ok(r) => r,
        Err(e) => return err_result(format!("tavily http: {e}")),
    };
    if !(200..300).contains(&resp.status) {
        return err_result(format!(
            "tavily HTTP {} — body: {}",
            resp.status,
            truncate(&resp.body, 256)
        ));
    }

    let parsed: serde_json::Value = match serde_json::from_str(&resp.body) {
        Ok(v) => v,
        Err(e) => return err_result(format!("tavily json parse: {e}")),
    };
    let Some(items) = parsed.get("results").and_then(|v| v.as_array()) else {
        return err_result("tavily response missing `results` array");
    };

    let results: Vec<SearchResultItem> = items
        .iter()
        .filter_map(|item| {
            let title = item.get("title")?.as_str()?.to_string();
            let url = item.get("url")?.as_str()?.to_string();
            let snippet = item
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| extract_snippet(s));
            Some(SearchResultItem {
                title,
                url,
                published_date: None,
                author: None,
                snippet,
            })
        })
        .collect();

    ok_result(SearchOutput {
        query: args.query.clone(),
        results,
        provider: "tavily".into(),
    })
}

fn search_searxng(args: &SearchArgs) -> exports::grain::plugin::plugin::ToolResult {
    let base_url = match host::env_get("SEARXNG_BASE_URL") {
        Some(u) if !u.is_empty() => u.trim_end_matches('/').to_string(),
        _ => return err_result("SEARXNG_BASE_URL not set in host environment"),
    };

    let n = args.num_results.unwrap_or(5).clamp(1, 20);
    // SearXNG supports `format=json` and `categories=general`.
    let url = format!(
        "{}/search?q={}&format=json&categories=general",
        base_url,
        urlencoding(&args.query)
    );

    host::log(
        LogLevel::Info,
        &format!("searxng search: q={:?} n={}", args.query, n),
    );

    let resp = match host::http_get(
        &url,
        &[(
            "User-Agent".to_string(),
            "grain-web-search-plugin/0.1".to_string(),
        )],
    ) {
        Ok(r) => r,
        Err(e) => return err_result(format!("searxng http: {e}")),
    };
    if !(200..300).contains(&resp.status) {
        return err_result(format!(
            "searxng HTTP {} — body: {}",
            resp.status,
            truncate(&resp.body, 256)
        ));
    }

    let parsed: serde_json::Value = match serde_json::from_str(&resp.body) {
        Ok(v) => v,
        Err(e) => return err_result(format!("searxng json parse: {e}")),
    };
    let Some(items) = parsed.get("results").and_then(|v| v.as_array()) else {
        return err_result("searxng response missing `results` array");
    };

    let results: Vec<SearchResultItem> = items
        .iter()
        .take(n as usize)
        .filter_map(|item| {
            let title = item.get("title")?.as_str()?.to_string();
            let url = item.get("url")?.as_str()?.to_string();
            let snippet = item
                .get("content")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("snippet").and_then(|v| v.as_str()))
                .map(|s| extract_snippet(s));
            Some(SearchResultItem {
                title,
                url,
                published_date: None,
                author: None,
                snippet,
            })
        })
        .collect();

    ok_result(SearchOutput {
        query: args.query.clone(),
        results,
        provider: "searxng".into(),
    })
}

/// Minimal url-encoding for the query parameter (SearXNG GET request).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push_str("%20"),
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}


fn search_anysearch(args: &SearchArgs) -> exports::grain::plugin::plugin::ToolResult {
    // AnySearch supports anonymous access (no key required).
    // When ANYSEARCH_API_KEY is set, use it for higher rate limits.
    let api_key = host::env_get("ANYSEARCH_API_KEY")
        .filter(|k| !k.is_empty());

    let n = args.num_results.unwrap_or(5).clamp(1, 20);
    let payload = serde_json::json!({
        "query": args.query,
        "num_results": n,
    });

    host::log(
        LogLevel::Info,
        &format!("anysearch search: q={:?} n={}", args.query, n),
    );

    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    if let Some(ref key) = api_key {
        headers.push(("x-api-key".to_string(), key.clone()));
    }

    let resp = match host::http_post(
        "https://api.anysearch.com/search",
        &headers,
        &payload.to_string(),
    ) {
        Ok(r) => r,
        Err(e) => return err_result(format!("anysearch http: {e}")),
    };
    if !(200..300).contains(&resp.status) {
        return err_result(format!(
            "anysearch HTTP {} — body: {}",
            resp.status,
            truncate(&resp.body, 256)
        ));
    }

    let parsed: serde_json::Value = match serde_json::from_str(&resp.body) {
        Ok(v) => v,
        Err(e) => return err_result(format!("anysearch json parse: {e}")),
    };
    let Some(items) = parsed.get("results").and_then(|v| v.as_array()) else {
        return err_result("anysearch response missing `results` array");
    };

    let results: Vec<SearchResultItem> = items
        .iter()
        .take(n as usize)
        .filter_map(|item| {
            let title = item.get("title")?.as_str()?.to_string();
            let url = item.get("url")?.as_str()?.to_string();
            let snippet = item
                .get("snippet")
                .or_else(|| item.get("content"))
                .and_then(|v| v.as_str())
                .map(|s| extract_snippet(s));
            Some(SearchResultItem {
                title,
                url,
                published_date: None,
                author: None,
                snippet,
            })
        })
        .collect();

    ok_result(SearchOutput {
        query: args.query.clone(),
        results,
        provider: "anysearch".into(),
    })
}

// ---------------------------------------------------------------------------
// Dispatcher.


fn do_search(args: SearchArgs) -> exports::grain::plugin::plugin::ToolResult {
    let provider = args.provider.as_deref().unwrap_or("exa");
    match provider {
        "exa" => search_exa(&args),
        "tavily" => search_tavily(&args),
        "searxng" => search_searxng(&args),
        "anysearch" => search_anysearch(&args),
        other => err_result(format!(
            "unknown provider '{other}'. Supported: exa, tavily, searxng, anysearch"
        )),
    }
}

// ---------------------------------------------------------------------------
// web_fetch
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Plugin entry points.
// ---------------------------------------------------------------------------

impl exports::grain::plugin::plugin::Guest for WebSearchPlugin {
    fn init() -> Result<exports::grain::plugin::plugin::PluginInfo, String> {
        host::log(LogLevel::Info, "web-search plugin loaded");
        Ok(exports::grain::plugin::plugin::PluginInfo {
            name: "web-search".to_string(),
            version: "0.2.0".to_string(),
        })
    }

    fn list_tools() -> Vec<exports::grain::plugin::plugin::ToolDef> {
        vec![
            exports::grain::plugin::plugin::ToolDef {
                name: "web_search".to_string(),
                label: "Web Search".to_string(),
                description:
                    "Search the live web via Exa / Tavily / SearXNG / AnySearch. \
Returns title / url / snippet for each hit. \
Set provider to 'exa' (needs EXA_API_KEY), 'tavily' (needs TAVILY_API_KEY), \
or 'searxng' (self-hosted, needs SEARXNG_BASE_URL), or 'anysearch' (cloud, optional ANYSEARCH_API_KEY). \
Defaults to 'exa' when provider is omitted."
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
