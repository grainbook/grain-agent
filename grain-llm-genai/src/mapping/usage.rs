//! Shared scalar conversions used by both directions.

use grain_agent_core::Usage;

/// Project genai's nullable token counters into grain's flat `Usage` struct.
pub fn map_usage(g: genai::chat::Usage) -> Usage {
    let input = g.prompt_tokens.unwrap_or(0).max(0) as u64;
    let output = g.completion_tokens.unwrap_or(0).max(0) as u64;
    let total = g.total_tokens.unwrap_or(0).max(0) as u64;

    let (cache_read, cache_write) = g
        .prompt_tokens_details
        .as_ref()
        .map(|d| {
            (
                d.cached_tokens.unwrap_or(0).max(0) as u64,
                d.cache_creation_tokens.unwrap_or(0).max(0) as u64,
            )
        })
        .unwrap_or((0, 0));

    Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: total,
        ..Usage::default()
    }
}
