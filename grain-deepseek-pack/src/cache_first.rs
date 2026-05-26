//! DeepSeek prefix-cache-friendly compaction defaults.
//!
//! DeepSeek's cache economics reward byte-stable, long-lived prefixes.
//! This module keeps the provider-specific choice out of the generic
//! harness: callers install the resolver only when they want DeepSeek-aware
//! behavior, and non-DeepSeek model ids pass through unchanged.

use std::sync::Arc;

use grain_agent_harness::{ActiveModelInfo, CompactionSettings, CompactionSettingsResolver};

/// Returns true for model ids that should use DeepSeek cache-first behavior.
pub fn is_deepseek_model_id(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.starts_with("deepseek/") || id.contains("deepseek") || id.starts_with("ds-")
}

/// Tune compaction to preserve DeepSeek prefix-cache efficiency.
///
/// Compared with the generic defaults, this delays automatic compaction until
/// the transcript is closer to the model window and keeps a slightly smaller
/// untouched tail. The result is fewer prefix rewrites during normal long
/// sessions, while still leaving enough response headroom before provider
/// overflow.
pub fn cache_first_compaction_settings(base: &CompactionSettings) -> CompactionSettings {
    let mut settings = base.clone();
    settings.threshold_tokens = -1;
    settings.threshold_percent = 92;
    settings.reserve_tokens = 8192;
    settings.keep_recent_tokens = settings.keep_recent_tokens.min(16_000);
    settings
}

/// Resolver suitable for [`grain_agent_harness::TokenBudgetPolicy`].
pub fn resolve_cache_first_compaction_settings(
    active_model: &ActiveModelInfo,
    base: &CompactionSettings,
) -> CompactionSettings {
    if is_deepseek_model_id(&active_model.id) {
        cache_first_compaction_settings(base)
    } else {
        base.clone()
    }
}

/// Pluggable resolver for generic harness token-budget policies.
pub fn cache_first_settings_resolver() -> CompactionSettingsResolver {
    Arc::new(resolve_cache_first_compaction_settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_harness::DEFAULT_COMPACTION_SETTINGS;

    #[test]
    fn detects_deepseek_model_ids() {
        assert!(is_deepseek_model_id("deepseek/deepseek-chat"));
        assert!(is_deepseek_model_id("deepseek-v4-pro"));
        assert!(is_deepseek_model_id("ds-r1"));
        assert!(!is_deepseek_model_id("openai/gpt-4o"));
    }

    #[test]
    fn cache_first_delays_threshold_and_preserves_enabled() {
        let mut base = DEFAULT_COMPACTION_SETTINGS;
        base.enabled = false;

        let tuned = cache_first_compaction_settings(&base);

        assert!(!tuned.enabled);
        assert_eq!(tuned.threshold_percent, 92);
        assert_eq!(tuned.reserve_tokens, 8192);
        assert!(tuned.keep_recent_tokens <= base.keep_recent_tokens);
    }

    #[test]
    fn resolver_passes_through_non_deepseek() {
        let base = DEFAULT_COMPACTION_SETTINGS;
        let active = ActiveModelInfo::new("openai/gpt-4o", 128_000);

        let resolved = resolve_cache_first_compaction_settings(&active, &base);

        assert_eq!(resolved.threshold_percent, base.threshold_percent);
        assert_eq!(resolved.reserve_tokens, base.reserve_tokens);
    }

    #[test]
    fn resolver_factory_is_pluggable() {
        let base = DEFAULT_COMPACTION_SETTINGS;
        let active = ActiveModelInfo::new("deepseek/deepseek-chat", 64_000);
        let resolver = cache_first_settings_resolver();

        let resolved = resolver(&active, &base);

        assert_eq!(resolved.threshold_percent, 92);
        assert_eq!(resolved.reserve_tokens, 8192);
    }
}
