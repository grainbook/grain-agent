//! Headless auto-compaction policy wiring.
//!
//! UI crates should not know provider-specific compaction strategy
//! details. They pass user/config knobs here and receive a generic
//! harness policy plus displayable metadata.

use std::sync::Arc;

use grain_agent_harness::{
    ActiveModelHandle, CompactionPolicy, CompactionSettings, DEFAULT_COMPACTION_SETTINGS,
    TokenBudgetPolicy, TokenEstimator,
};
use grain_llm_models::Registry;

#[derive(Debug, Clone, Copy)]
pub struct AutoCompactionConfig {
    pub enabled: bool,
    pub threshold_tokens: Option<i64>,
    pub threshold_percent: Option<i32>,
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
    pub deepseek_cache_first: bool,
}

impl Default for AutoCompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_tokens: None,
            threshold_percent: None,
            reserve_tokens: None,
            keep_recent_tokens: None,
            deepseek_cache_first: true,
        }
    }
}

impl AutoCompactionConfig {
    pub fn apply_to_settings(self, settings: &mut CompactionSettings) {
        settings.enabled = self.enabled;
        if let Some(v) = self.threshold_tokens {
            settings.threshold_tokens = v;
        }
        if let Some(v) = self.threshold_percent {
            settings.threshold_percent = v;
        }
        if let Some(v) = self.reserve_tokens {
            settings.reserve_tokens = v;
        }
        if let Some(v) = self.keep_recent_tokens {
            settings.keep_recent_tokens = v;
        }
    }
}

pub struct AutoCompactionPolicy {
    pub policy: Arc<dyn CompactionPolicy>,
    pub base_settings: CompactionSettings,
    pub mode_label: &'static str,
}

pub fn build_auto_compaction_policy(
    registry: Arc<Registry>,
    active_model_handle: ActiveModelHandle,
    config: AutoCompactionConfig,
) -> AutoCompactionPolicy {
    let mut base_settings = DEFAULT_COMPACTION_SETTINGS;
    config.apply_to_settings(&mut base_settings);

    let mut token_budget_policy = TokenBudgetPolicy::new(
        registry,
        active_model_handle,
        base_settings.clone(),
        TokenEstimator::approximate(),
    );
    let mode_label = if config.deepseek_cache_first {
        let deepseek_resolver = grain_deepseek_pack::cache_first_settings_resolver();
        token_budget_policy =
            token_budget_policy.with_settings_resolver(Arc::new(move |active_model, base| {
                let mut settings = deepseek_resolver(active_model, base);
                config.apply_to_settings(&mut settings);
                settings
            }));
        "auto + DeepSeek cache-first"
    } else {
        "auto"
    };

    AutoCompactionPolicy {
        policy: Arc::new(token_budget_policy),
        base_settings,
        mode_label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_harness::ActiveModelInfo;
    use std::sync::RwLock;

    #[test]
    fn config_applies_to_base_settings() {
        let mut settings = DEFAULT_COMPACTION_SETTINGS;
        AutoCompactionConfig {
            enabled: false,
            threshold_tokens: Some(123),
            threshold_percent: Some(77),
            reserve_tokens: Some(4096),
            keep_recent_tokens: Some(8192),
            deepseek_cache_first: false,
        }
        .apply_to_settings(&mut settings);

        assert!(!settings.enabled);
        assert_eq!(settings.threshold_tokens, 123);
        assert_eq!(settings.threshold_percent, 77);
        assert_eq!(settings.reserve_tokens, 4096);
        assert_eq!(settings.keep_recent_tokens, 8192);
    }

    #[test]
    fn builder_reports_deepseek_cache_first_mode() {
        let policy = build_auto_compaction_policy(
            Arc::new(Registry::default()),
            Arc::new(RwLock::new(ActiveModelInfo::new(
                "deepseek/deepseek-chat",
                64_000,
            ))),
            AutoCompactionConfig::default(),
        );

        assert_eq!(policy.mode_label, "auto + DeepSeek cache-first");
        assert!(policy.base_settings.enabled);
    }
}
