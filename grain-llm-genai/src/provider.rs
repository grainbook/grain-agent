//! Provider profile model + TOML loader.
//!
//! A *profile* is a named configuration that bundles the LLM provider
//! kind, optional custom base URL, the default model id, and how to
//! authenticate. Profiles let the user have e.g. `openai-work` and
//! `openai-personal` pointing at the same vendor with different
//! `OPENAI_API_KEY_*` env vars, switch between MiniMax / DeepSeek /
//! Kimi at runtime, or (Phase 2) attach a Claude.ai Pro/Max OAuth
//! identity instead of an API key.
//!
//! Profile files live at:
//!
//! - `<workspace>/.grain/providers.toml` — per-project; takes precedence.
//! - `~/.config/grain/providers.toml` — user-wide fallback.
//! - `--providers-file <path>` CLI override — wins above everything.
//!
//! File format:
//!
//! ```toml
//! [[profile]]
//! name = "openai-work"
//! kind = "openai-compat"
//! base_url = "https://api.openai.com/v1"
//! model = "openai/gpt-4o"
//! auth = { kind = "api_key", env = "OPENAI_API_KEY_WORK" }
//!
//! [[profile]]
//! name = "claude-pro"
//! kind = "anthropic"
//! model = "anthropic/claude-sonnet-4-5"
//! auth = { kind = "anthropic_oauth" }
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One named provider configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProfile {
    /// User-facing label. Doubles as the genai provider id used for
    /// routing — make it unique across profiles. `anthropic` /
    /// `openai` / `gemini` route through genai's native adapters;
    /// any other name routes through the OpenAI-compat path (so a
    /// `base_url` + env var is required for those).
    pub name: String,
    pub kind: ProviderKind,
    /// Override base URL. Required when `kind` is [`ProviderKind::OpenAiCompat`].
    pub base_url: Option<String>,
    /// grain-llm-models registry id, e.g. `"anthropic/claude-sonnet-4-5"`.
    /// The model row in the registry is materialized at switch time;
    /// its provider field is then rewritten to [`Self::name`] so
    /// custom-named profiles route through the right endpoint.
    pub model: String,
    pub auth: ProviderAuth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    OpenAi,
    Gemini,
    OpenAiCompat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAuth {
    /// Read an API key from the named env var at use time.
    ApiKey { env: String },
    /// Anthropic Claude.ai Pro/Max subscription via OAuth — parsed
    /// today but not yet usable. Selecting one of these in `/provider`
    /// surfaces a clear "login flow not yet wired" message; the real
    /// implementation lands in a follow-up patch.
    AnthropicOauth,
}

impl ProviderAuth {
    /// Whether the profile is ready to actually drive an agent
    /// today. OAuth variants are not ready yet (Phase 2).
    pub fn is_usable(&self) -> bool {
        matches!(self, ProviderAuth::ApiKey { .. })
    }

    /// Short summary string for the picker UI.
    pub fn summary(&self) -> String {
        match self {
            ProviderAuth::ApiKey { env } => {
                let present = std::env::var(env)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .is_some();
                if present {
                    format!("env {env} ✓")
                } else {
                    format!("env {env} (missing)")
                }
            }
            ProviderAuth::AnthropicOauth => "oauth (login pending)".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// TOML schema
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProvidersFile {
    #[serde(default)]
    profile: Vec<ProfileEntry>,
}

#[derive(Debug, Deserialize)]
struct ProfileEntry {
    name: String,
    kind: String,
    #[serde(default)]
    base_url: Option<String>,
    model: String,
    auth: AuthEntry,
}

#[derive(Debug, Deserialize)]
struct AuthEntry {
    kind: String,
    #[serde(default)]
    env: Option<String>,
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Resolve which providers.toml file to load given the CLI override,
/// workspace root, and user home. Returns the first existing path or
/// `None` if no file is on the search path.
pub fn resolve_providers_file(
    cli_override: Option<&Path>,
    workspace_root: &Path,
) -> Option<PathBuf> {
    if let Some(p) = cli_override
        && p.exists()
    {
        return Some(p.to_path_buf());
    }
    let workspace_file = workspace_root.join(".grain").join("providers.toml");
    if workspace_file.exists() {
        return Some(workspace_file);
    }
    if let Some(home) = dirs_home() {
        let user_file = home.join(".config").join("grain").join("providers.toml");
        if user_file.exists() {
            return Some(user_file);
        }
    }
    None
}

/// Load profiles from `path`. Returns the parsed list plus per-entry
/// warning strings (one entry with a bad `kind` does not block the
/// rest from loading).
pub fn load_profiles(path: &Path) -> (Vec<ProviderProfile>, Vec<String>) {
    let mut profiles = Vec::new();
    let mut warnings = Vec::new();

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!("providers file {}: {e}", path.display()));
            return (profiles, warnings);
        }
    };
    let file: ProvidersFile = match toml::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            warnings.push(format!("providers file {} parse: {e}", path.display()));
            return (profiles, warnings);
        }
    };
    for entry in file.profile {
        match profile_from_entry(entry) {
            Ok(p) => profiles.push(p),
            Err(e) => warnings.push(format!("providers file {}: {e}", path.display())),
        }
    }
    (profiles, warnings)
}

fn profile_from_entry(entry: ProfileEntry) -> Result<ProviderProfile, String> {
    let kind = match entry.kind.as_str() {
        "anthropic" => ProviderKind::Anthropic,
        "openai" => ProviderKind::OpenAi,
        "gemini" => ProviderKind::Gemini,
        "openai-compat" | "openai_compat" => ProviderKind::OpenAiCompat,
        other => {
            return Err(format!(
                "profile '{}': unknown kind '{}' (expected anthropic, openai, gemini, openai-compat)",
                entry.name, other
            ));
        }
    };
    if matches!(kind, ProviderKind::OpenAiCompat) && entry.base_url.is_none() {
        return Err(format!(
            "profile '{}': kind=openai-compat requires base_url",
            entry.name
        ));
    }
    let auth = match entry.auth.kind.as_str() {
        "api_key" => {
            let env = entry.auth.env.ok_or_else(|| {
                format!(
                    "profile '{}': auth.kind=api_key requires auth.env",
                    entry.name
                )
            })?;
            ProviderAuth::ApiKey { env }
        }
        "anthropic_oauth" => ProviderAuth::AnthropicOauth,
        other => {
            return Err(format!(
                "profile '{}': unknown auth.kind '{}' (expected api_key or anthropic_oauth)",
                entry.name, other
            ));
        }
    };
    Ok(ProviderProfile {
        name: entry.name,
        kind,
        base_url: entry.base_url,
        model: entry.model,
        auth,
    })
}

fn dirs_home() -> Option<PathBuf> {
    // std::env::home_dir was deprecated then un-deprecated; safest path
    // is to look for HOME on unix, USERPROFILE on windows.
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home));
    }
    if let Ok(home) = std::env::var("USERPROFILE")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_providers(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("providers.toml");
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn loads_an_api_key_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_providers(
            tmp.path(),
            r#"
[[profile]]
name = "openai-work"
kind = "openai-compat"
base_url = "https://api.openai.com/v1"
model = "openai/gpt-4o"
auth = { kind = "api_key", env = "OPENAI_API_KEY_WORK" }
"#,
        );
        let (profiles, warnings) = load_profiles(&path);
        assert!(warnings.is_empty(), "no warnings: {:?}", warnings);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "openai-work");
        assert_eq!(profiles[0].kind, ProviderKind::OpenAiCompat);
        assert_eq!(profiles[0].model, "openai/gpt-4o");
        assert!(matches!(
            &profiles[0].auth,
            ProviderAuth::ApiKey { env } if env == "OPENAI_API_KEY_WORK"
        ));
    }

    #[test]
    fn loads_oauth_profile_even_though_login_not_wired() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_providers(
            tmp.path(),
            r#"
[[profile]]
name = "claude-pro"
kind = "anthropic"
model = "anthropic/claude-sonnet-4-5"
auth = { kind = "anthropic_oauth" }
"#,
        );
        let (profiles, _) = load_profiles(&path);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].auth, ProviderAuth::AnthropicOauth);
        assert!(!profiles[0].auth.is_usable());
    }

    #[test]
    fn warns_on_compat_profile_missing_base_url() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_providers(
            tmp.path(),
            r#"
[[profile]]
name = "bad"
kind = "openai-compat"
model = "openai/gpt-4o"
auth = { kind = "api_key", env = "X" }
"#,
        );
        let (profiles, warnings) = load_profiles(&path);
        assert!(profiles.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("base_url"));
    }

    #[test]
    fn warns_on_unknown_auth_kind_but_keeps_other_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_providers(
            tmp.path(),
            r#"
[[profile]]
name = "ok"
kind = "anthropic"
model = "anthropic/claude-sonnet-4-5"
auth = { kind = "api_key", env = "ANTHROPIC_API_KEY" }

[[profile]]
name = "broken"
kind = "anthropic"
model = "anthropic/claude-sonnet-4-5"
auth = { kind = "wat" }
"#,
        );
        let (profiles, warnings) = load_profiles(&path);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "ok");
        assert!(warnings.iter().any(|w| w.contains("wat")));
    }

    #[test]
    fn auth_summary_marks_present_and_missing_env_vars() {
        let auth = ProviderAuth::ApiKey {
            env: "GRAIN_PROFILE_TEST_KEY_THAT_SHOULD_NOT_EXIST".into(),
        };
        assert!(auth.summary().contains("missing"));
        // SAFETY-ish: we set a unique key name that won't collide.
        // Skip the present-branch assertion in tests to avoid depending
        // on test parallelism and env mutation.
    }

    #[test]
    fn resolve_uses_cli_override_first() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_providers(tmp.path(), "");
        let resolved = resolve_providers_file(Some(&path), tmp.path());
        assert_eq!(resolved, Some(path));
    }

    #[test]
    fn resolve_falls_back_to_workspace_grain_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let grain_dir = tmp.path().join(".grain");
        fs::create_dir_all(&grain_dir).unwrap();
        let path = grain_dir.join("providers.toml");
        fs::write(&path, "").unwrap();
        let resolved = resolve_providers_file(None, tmp.path());
        assert_eq!(resolved, Some(path));
    }
}
