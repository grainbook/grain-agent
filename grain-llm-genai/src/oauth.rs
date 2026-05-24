//! OAuth 2.0 PKCE flows for provider subscription plans.
//!
//! Supports Anthropic Claude Pro / Max and OpenAI ChatGPT Plus / Pro / Team
//! via browser-based OAuth with PKCE.  Tokens are persisted to
//! `~/.config/grain/oauth/<profile>.json` (with `0o600` on Unix) and
//! automatically refreshed on expiry.
//!
//! # Usage (from the builder / streaming layer)
//!
//! ```rust,ignore
//! // Synchronously get a valid bearer token for an OAuth profile:
//! let token = grain_llm_genai::oauth::get_valid_access_token_sync("claude-pro")
//!     .expect("failed to get OAuth token");
//! // Pass token as `AuthData::from_single(token)` to genai.
//! ```
//!
//! # CLI login flow
//!
//! The module provides `start_login_flow(config, on_message)` which opens a
//! browser for the user to authorize, catches the `http://127.0.0.1:<port>`
//! redirect via a temporary Tokio listener, exchanges the code for tokens,
//! and persists them.  The `on_message` callback receives progress strings
//! (URLs, warnings) so a TUI host can route them to its event stream
//! instead of letting them corrupt the terminal.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// OAuth-related errors.
#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("token exchange HTTP error: {0}")]
    Http(#[from] reqwest13::Error),

    #[error("token store {0}: {1}")]
    Store(String, String),

    #[error("no refresh token available — re-login required")]
    NoRefreshToken,

    #[error("authorization server returned: {0}")]
    AuthServer(String),

    #[error("local callback HTTP parsing error: {0}")]
    CallbackParse(String),

    #[error("opening browser failed: {0}")]
    BrowserOpen(String),
}

// ---------------------------------------------------------------------------
// Well-known OAuth configs
// ---------------------------------------------------------------------------

/// OAuth endpoint configuration for a provider.
#[derive(Debug, Clone)]
pub struct OauthConfig {
    /// Provider name (used in token store path, logs).
    pub provider: String,
    /// Authorization endpoint URL.
    pub authorize_url: String,
    /// Token exchange endpoint URL.
    pub token_url: String,
    /// Client ID (public OAuth native client).
    pub client_id: String,
    /// Space-separated scopes.
    pub scopes: String,
}

/// Built-in configs.
fn builtin_configs() -> Vec<OauthConfig> {
    vec![
        // Anthropic Console OAuth — used by the official Claude Code CLI.
        OauthConfig {
            provider: "anthropic".to_string(),
            authorize_url: "https://console.anthropic.com/oauth/authorize".to_string(),
            token_url: "https://console.anthropic.com/oauth/token".to_string(),
            client_id: "f7a5c308-b193-4dc8-a21a-50d593a0f5b8".to_string(), // public client
            scopes: "openid profile email".to_string(),
        },
        // OpenAI OAuth — used by ChatGPT web and the Codex CLI OSS.
        OauthConfig {
            provider: "openai".to_string(),
            authorize_url: "https://auth.openai.com/authorize".to_string(),
            token_url: "https://auth.openai.com/oauth/token".to_string(),
            client_id: "TdS8dKq5y3GQ8xH2zV1mR7pB4cA9fL6o".to_string(), // Codex CLI public client
            scopes: "openid profile email offline_access".to_string(),
        },
    ]
}

/// Resolve a provider name to its OAuth config.
pub fn config_for_provider(provider: &str) -> Option<OauthConfig> {
    builtin_configs()
        .into_iter()
        .find(|c| c.provider == provider)
}

/// Resolve a genai adapter kind (e.g. `"anthropic"`, `"openai"`) to
/// its OAuth config. Used by the auth resolver at request time so it
/// doesn't need to know which profile name the user logged in under.
pub fn config_for_kind(kind: &str) -> Option<OauthConfig> {
    let provider = match kind {
        "anthropic" => "anthropic",
        "openai" => "openai",
        _ => return None,
    };
    config_for_provider(provider)
}

// ---------------------------------------------------------------------------
// Token representation
// ---------------------------------------------------------------------------

/// OAuth token set persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix timestamp at which `access_token` expires.
    pub expires_at: u64,
    pub token_type: String,
}

// ---------------------------------------------------------------------------
// PKCE helpers
// ---------------------------------------------------------------------------

fn generate_code_verifier() -> String {
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes);
    base64url_encode(&bytes)
}

fn compute_code_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    base64url_encode(&digest)
}

/// Base64url without padding (RFC 7636 Appendix A).
fn base64url_encode(data: &[u8]) -> String {
    base64_encode(data)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// Standard base64 encoding (helper, not re-exported).
fn base64_encode(data: &[u8]) -> String {
    fn char_from(b: u8) -> char {
        match b {
            0..=25 => (b + b'A') as char,
            26..=51 => (b - 26 + b'a') as char,
            52..=61 => (b - 52 + b'0') as char,
            62 => '+',
            63 => '/',
            _ => '=', // padding marker
        }
    }

    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);

        let triple = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);

        out.push(char_from(((triple >> 18) & 0x3F) as u8));
        out.push(char_from(((triple >> 12) & 0x3F) as u8));

        if chunk.len() > 1 {
            out.push(char_from(((triple >> 6) & 0x3F) as u8));
        } else {
            out.push('=');
        }

        if chunk.len() > 2 {
            out.push(char_from((triple & 0x3F) as u8));
        } else {
            out.push('=');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Token store (filesystem persistence)
// ---------------------------------------------------------------------------

/// Returns the path to the token JSON file for a profile.
pub fn token_store_path(profile_name: &str) -> PathBuf {
    let mut p = dirs_data_dir();
    p.push("oauth");
    p.push(format!("{}.json", profile_name));
    p
}

fn dirs_data_dir() -> PathBuf {
    // Prefer XDG data dir on Linux, standard dirs on macOS / Windows.
    if let Ok(d) = std::env::var("GRAIN_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            return PathBuf::from(xdg);
        }
    }
    let home = dirs_next();
    #[cfg(target_os = "macos")]
    {
        home.join("Library")
            .join("Application Support")
            .join("grain")
    }
    #[cfg(target_os = "linux")]
    {
        home.join(".config").join("grain")
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("grain");
        }
        home.join(".config").join("grain")
    }
}

fn dirs_next() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Load persisted tokens for a profile. Returns `None` if the file doesn't
/// exist or can't be parsed.
pub fn load_tokens(profile_name: &str) -> Result<Option<StoredTokens>, OauthError> {
    let path = token_store_path(profile_name);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| OauthError::Store(profile_name.to_string(), format!("read failed: {e}")))?;
    let tokens: StoredTokens = serde_json::from_str(&raw)
        .map_err(|e| OauthError::Store(profile_name.to_string(), format!("bad JSON: {e}")))?;
    Ok(Some(tokens))
}

/// Persist tokens with `0o600` permissions on Unix.
pub fn save_tokens(profile_name: &str, tokens: &StoredTokens) -> Result<(), OauthError> {
    let path = token_store_path(profile_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| OauthError::Store(profile_name.to_string(), format!("mkdir: {e}")))?;
    }
    let raw = serde_json::to_string_pretty(tokens)
        .map_err(|e| OauthError::Store(profile_name.to_string(), format!("serialize: {e}")))?;

    // Write to a temp file then rename for atomicity.
    let tmp = {
        let mut t = path.clone();
        t.set_extension("tmp");
        t
    };
    std::fs::write(&tmp, &raw)
        .map_err(|e| OauthError::Store(profile_name.to_string(), format!("write tmp: {e}")))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = tmp.metadata() {
            let mut perm = meta.permissions();
            perm.set_mode(0o600);
            let _ = std::fs::set_permissions(&tmp, perm);
        }
    }

    std::fs::rename(&tmp, &path)
        .map_err(|e| OauthError::Store(profile_name.to_string(), format!("rename: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Token HTTP helpers
// ---------------------------------------------------------------------------

/// Exchange an authorization code for tokens (POST to token endpoint).
async fn exchange_code(
    config: &OauthConfig,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<StoredTokens, OauthError> {
    let client = reqwest13::Client::new();
    let resp = client
        .post(&config.token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &config.client_id),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await?;

    let status = resp.status();
    let body = resp.text().await?;

    if !status.is_success() {
        return Err(OauthError::AuthServer(format!(
            "token endpoint returned {status}: {body}"
        )));
    }

    let raw: serde_json::Value = serde_json::from_str(&body)
        .map_err(|_| OauthError::AuthServer(format!("bad JSON from token endpoint: {body}")))?;

    let access_token = raw["access_token"]
        .as_str()
        .ok_or_else(|| OauthError::AuthServer(format!("missing access_token in: {body}")))?
        .to_string();

    let refresh_token = raw["refresh_token"]
        .as_str()
        .ok_or_else(|| OauthError::AuthServer(format!("missing refresh_token in: {body}")))?
        .to_string();

    let expires_in = raw["expires_in"].as_u64().unwrap_or(3600);
    let token_type = raw["token_type"].as_str().unwrap_or("Bearer").to_string();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Refresh 60s before actual expiry to avoid race.
    let expires_at = now + expires_in.saturating_sub(60);

    Ok(StoredTokens {
        access_token,
        refresh_token,
        expires_at,
        token_type,
    })
}

/// Refresh tokens using a stored `refresh_token`.
async fn refresh_tokens(
    config: &OauthConfig,
    refresh_token: &str,
) -> Result<StoredTokens, OauthError> {
    let client = reqwest13::Client::new();
    let resp = client
        .post(&config.token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &config.client_id),
        ])
        .send()
        .await?;

    let status = resp.status();
    let body = resp.text().await?;

    if !status.is_success() {
        return Err(OauthError::AuthServer(format!(
            "refresh endpoint returned {status}: {body}"
        )));
    }

    let raw: serde_json::Value = serde_json::from_str(&body)
        .map_err(|_| OauthError::AuthServer(format!("bad JSON from refresh: {body}")))?;

    let access_token = raw["access_token"]
        .as_str()
        .ok_or_else(|| {
            OauthError::AuthServer(format!("missing access_token in refresh response: {body}"))
        })?
        .to_string();

    // Some providers may issue a new refresh token; prefer it if present.
    let new_refresh = raw["refresh_token"].as_str();
    let refresh_token = new_refresh.unwrap_or(refresh_token).to_string();

    let expires_in = raw["expires_in"].as_u64().unwrap_or(3600);
    let token_type = raw["token_type"].as_str().unwrap_or("Bearer").to_string();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let expires_at = now + expires_in.saturating_sub(60);

    Ok(StoredTokens {
        access_token,
        refresh_token,
        expires_at,
        token_type,
    })
}

// ---------------------------------------------------------------------------
// High-level token API
// ---------------------------------------------------------------------------

/// Load tokens from store. If missing, return `None`. If present but expired,
/// refresh them in-place. Returns the valid access token.
pub async fn get_valid_access_token(profile_name: &str) -> Result<Option<String>, OauthError> {
    let Some(config) = config_for_provider(profile_name) else {
        return Ok(None);
    };

    let Some(mut tokens) = load_tokens(profile_name)? else {
        return Ok(None);
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Refresh if expired or about to expire (within 60s).
    if now >= tokens.expires_at {
        tokens = refresh_tokens(&config, &tokens.refresh_token).await?;
        save_tokens(profile_name, &tokens)?;
    }

    Ok(Some(tokens.access_token))
}

/// Like [`get_valid_access_token`] but takes the OAuth config explicitly
/// instead of looking it up by provider name. The `profile_name` is used
/// only to locate the persisted token file.
pub async fn get_valid_access_token_with_config(
    config: &OauthConfig,
    profile_name: &str,
) -> Result<Option<String>, OauthError> {
    let Some(mut tokens) = load_tokens(profile_name)? else {
        return Ok(None);
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Refresh if expired or about to expire (within 60s).
    if now >= tokens.expires_at {
        tokens = refresh_tokens(config, &tokens.refresh_token).await?;
        save_tokens(profile_name, &tokens)?;
    }

    Ok(Some(tokens.access_token))
}

/// Synchronous version of [`get_valid_access_token_with_config`].
pub fn get_valid_access_token_with_config_sync(
    config: &OauthConfig,
    profile_name: &str,
) -> Result<Option<String>, OauthError> {
    let handle = tokio::runtime::Handle::current();
    handle.block_on(get_valid_access_token_with_config(config, profile_name))
}

/// Synchronous version for use in genai auth resolvers.
/// Requires a Tokio runtime to be active on the current thread.
pub fn get_valid_access_token_sync(profile_name: &str) -> Result<Option<String>, OauthError> {
    let handle = tokio::runtime::Handle::current();
    handle.block_on(get_valid_access_token(profile_name))
}

// ---------------------------------------------------------------------------
// Browser login flow (CLI / TUI driven)
// ---------------------------------------------------------------------------

/// Start the OAuth PKCE login flow.  Opens the browser for the user to
/// authorize, starts a temporary `TcpListener` on a random port to catch the
/// redirect, exchanges the code for tokens, and persists them.
///
/// `on_message` is called with progress strings (the authorize URL, a hint
/// when `open::that` fails).  Pass `|m| println!("{m}")` from a CLI and a
/// closure that emits to your event channel from a TUI.
pub async fn start_login_flow(
    config: &OauthConfig,
    on_message: impl Fn(&str) + Send + Sync,
) -> Result<StoredTokens, OauthError> {
    let code_verifier = generate_code_verifier();
    let code_challenge = compute_code_challenge(&code_verifier);
    let state = generate_code_verifier(); // reuse the same random generation

    // Bind to a random port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(OauthError::Io)?;
    let local_addr = listener.local_addr().map_err(OauthError::Io)?;
    let port = local_addr.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    // Build the authorization URL.
    let mut auth_url = Url::parse(&config.authorize_url)
        .map_err(|e| OauthError::AuthServer(format!("bad authorize_url: {e}")))?;
    {
        let mut q = auth_url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", &config.client_id);
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("scope", &config.scopes);
        q.append_pair("code_challenge", &code_challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("state", &state);
    }

    // Open the browser.
    let url_str = auth_url.to_string();
    on_message(&format!(
        "Opening browser for {}. If it doesn't open, visit:\n  {url_str}",
        config.provider
    ));
    if let Err(e) = open::that(&url_str) {
        // Non-fatal — user can copy-paste the URL.
        on_message(&format!("Could not open browser: {e}"));
        on_message(&format!("Please open the URL manually:\n  {url_str}"));
    }

    // Accept exactly one connection from the listener, with a 5-minute
    // timeout.  Use tokio::net::TcpListener for async accept.
    listener
        .set_nonblocking(true)
        .map_err(OauthError::Io)?;
    let tokio_listener =
        tokio::net::TcpListener::from_std(listener).map_err(OauthError::Io)?;

    let timeout = tokio::time::sleep(Duration::from_secs(300));
    let accept = tokio_listener.accept();
    tokio::pin!(timeout);

    let (stream, _peer) = tokio::select! {
        r = accept => {
            match r {
                Ok((stream, peer)) => (stream, peer),
                Err(e) => return Err(OauthError::Io(e)),
            }
        }
        _ = &mut timeout => {
            return Err(OauthError::AuthServer(
                "timed out waiting for browser redirect".to_string(),
            ));
        }
    };

    // Parse the incoming HTTP request for the authorization code.  We pass
    // `&state` so the callback parser can reject mismatched values (CSRF).
    let code = parse_callback_request(stream, &state).await?;

    // Exchange the authorization code for tokens.
    let tokens = exchange_code(config, &code, &code_verifier, &redirect_uri).await?;

    // Persist.
    save_tokens(&config.provider, &tokens)?;

    Ok(tokens)
}

/// Parse the OAuth callback from the browser redirect stream and verify
/// the `state` parameter matches `expected_state` (CSRF protection — the
/// authorization server echoes back whatever opaque value we sent in the
/// authorize URL; if a third party tried to trick the browser into
/// hitting our redirect with a code of their choosing, their `state`
/// wouldn't match).
async fn parse_callback_request(
    stream: tokio::net::TcpStream,
    expected_state: &str,
) -> Result<String, OauthError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader_half);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .map_err(|e| OauthError::CallbackParse(format!("read request: {e}")))?;

    // Expect: GET /?code=...&state=... HTTP/1.1
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(OauthError::CallbackParse(format!(
            "malformed HTTP request line: {request_line}"
        )));
    }
    let path_and_query = parts[1];
    let query_start = path_and_query
        .find('?')
        .ok_or_else(|| OauthError::CallbackParse(format!("no query in path: {path_and_query}")))?;
    let query_str = &path_and_query[(query_start + 1)..];

    // Parse query params manually (avoid pulling in `url` for this).
    let params = parse_query_string(query_str);

    // Send a success page to the browser.
    {
        let body =
            "<html><body><h1>Login successful!</h1><p>You may close this window.</p></body></html>";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        writer
            .write_all(resp.as_bytes())
            .await
            .map_err(|e| OauthError::CallbackParse(format!("write response: {e}")))?;
        writer
            .flush()
            .await
            .map_err(|e| OauthError::CallbackParse(format!("flush response: {e}")))?;
    }

    if let Some(error) = params.get("error") {
        let desc = params.get("error_description").cloned().unwrap_or_default();
        return Err(OauthError::AuthServer(format!("{error}: {desc}")));
    }

    let code = params
        .get("code")
        .cloned()
        .ok_or_else(|| OauthError::CallbackParse("no 'code' parameter in redirect".to_string()))?;

    match params.get("state") {
        Some(s) if s == expected_state => {}
        Some(_) => {
            return Err(OauthError::AuthServer(
                "state mismatch — possible CSRF, refusing to exchange code".to_string(),
            ));
        }
        None => {
            return Err(OauthError::AuthServer(
                "authorization server did not echo state — refusing to exchange code".to_string(),
            ));
        }
    }

    Ok(code)
}

fn parse_query_string(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in s.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let key = url_decode(k);
            let val = url_decode(v);
            map.insert(key, val);
        }
    }
    map
}

fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(hex) = u8::from_str_radix(&s[(i + 1)..(i + 3)], 16) {
                    out.push(hex as char);
                }
                i += 3;
            }
            _ => {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_round_trip() {
        let verifier = generate_code_verifier();
        // Verifier must be 43-128 chars of unreserved chars.
        assert!(verifier.len() >= 43);
        assert!(verifier.len() <= 128);
        let challenge = compute_code_challenge(&verifier);
        assert!(!challenge.is_empty());
        // Challenge is base64url(SHA256(verifier)) — 43 chars (32 bytes digest
        // → 43 base64url chars).
        assert_eq!(challenge.len(), 43);
    }

    #[test]
    fn base64url_no_padding() {
        let encoded = base64url_encode(b"test");
        assert!(!encoded.contains('='));
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
        // "test" in standard base64 is "dGVzdA==" → base64url "dGVzdA"
        assert_eq!(encoded, "dGVzdA");
    }

    #[test]
    fn url_decode_basic() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("%3D"), "=");
    }

    #[test]
    fn query_parse_simple() {
        let m = parse_query_string("code=abc&state=xyz");
        assert_eq!(m.get("code").unwrap(), "abc");
        assert_eq!(m.get("state").unwrap(), "xyz");
    }

    #[test]
    fn config_for_known_provider() {
        let c = config_for_provider("anthropic").unwrap();
        assert!(c.authorize_url.contains("anthropic"));
        assert!(c.token_url.contains("anthropic"));
    }

    #[test]
    fn config_for_unknown_is_none() {
        assert!(config_for_provider("nonexistent").is_none());
    }
}
