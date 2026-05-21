//! Sliding-window storm suppressor: blocks runaway tool-call loops.
//!
//! When a model gets stuck calling the same tool with the same args
//! over and over (DeepSeek-Reasonix calls this "tool-call storm"), the
//! hook short-circuits the N-th repeat and injects a brief reflection
//! message back into the loop so the model has a chance to change
//! tack. Implemented as a [`BeforeToolCallFn`] — wire it into
//! `AgentOptions::before_tool_call` and you're done.
//!
//! The check is provider-agnostic: it only inspects the tool name and
//! the serialized args. No DeepSeek-specific assumptions.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use grain_agent_core::{BeforeToolCallFn, BeforeToolCallResult};
use tokio::sync::Mutex;

/// Configuration for [`storm_hook`].
#[derive(Debug, Clone)]
pub struct StormConfig {
    /// Sliding-window length. Calls older than this are forgotten.
    pub window: Duration,
    /// Number of prior identical `(name, args)` calls within `window`
    /// that triggers suppression. Default `2` ⇒ the 3rd identical
    /// call is blocked.
    pub max_repeats: usize,
}

impl Default for StormConfig {
    fn default() -> Self {
        StormConfig {
            window: Duration::from_secs(60),
            max_repeats: 2,
        }
    }
}

/// Build a [`BeforeToolCallFn`] that suppresses tool-call storms per
/// [`StormConfig`]. Owns its own sliding-window state — clone the
/// returned `Arc` if you need it on multiple agents (each agent then
/// shares the window; pass a fresh hook per-agent for isolated state).
pub fn storm_hook(config: StormConfig) -> BeforeToolCallFn {
    let ring: Arc<Mutex<VecDeque<Entry>>> = Arc::new(Mutex::new(VecDeque::new()));
    Arc::new(move |ctx, _cancel| {
        let ring = ring.clone();
        let config = config.clone();
        Box::pin(async move {
            let args_key = serde_json::to_string(&ctx.args).unwrap_or_default();
            let name = ctx.tool_call.name.clone();
            let now = Instant::now();
            let mut ring = ring.lock().await;
            decide_storm(&mut ring, now, &config, &name, &args_key)
        })
    })
}

/// One window entry: when the call landed + its identity key.
#[derive(Debug, Clone)]
struct Entry {
    at: Instant,
    name: String,
    args_key: String,
}

/// Pure decision function — testable without async / global clock.
///
/// Side effects: garbage-collects entries older than `config.window`
/// and pushes the current call (regardless of block decision, so the
/// window measures *attempted* repeats, not just executed ones).
fn decide_storm(
    ring: &mut VecDeque<Entry>,
    now: Instant,
    config: &StormConfig,
    name: &str,
    args_key: &str,
) -> Option<BeforeToolCallResult> {
    // GC: drop everything outside the window.
    while let Some(front) = ring.front() {
        if now.duration_since(front.at) > config.window {
            ring.pop_front();
        } else {
            break;
        }
    }
    let repeats = ring
        .iter()
        .filter(|e| e.name == name && e.args_key == args_key)
        .count();
    ring.push_back(Entry {
        at: now,
        name: name.to_string(),
        args_key: args_key.to_string(),
    });
    if repeats >= config.max_repeats {
        let preview = preview_args(args_key);
        let reason = format!(
            "Tool '{name}' called with identical args {n} times within {secs}s — \
             storm suppressed. Args: {preview}. Reflect on whether a different approach is needed.",
            n = repeats + 1,
            secs = config.window.as_secs().max(1),
        );
        Some(BeforeToolCallResult {
            block: true,
            reason: Some(reason),
        })
    } else {
        None
    }
}

fn preview_args(args: &str) -> String {
    const MAX: usize = 80;
    if args.len() <= MAX {
        args.to_string()
    } else {
        // Round to a char boundary so we don't slice mid-multibyte.
        let mut end = MAX;
        while !args.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &args[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(window_secs: u64, max_repeats: usize) -> StormConfig {
        StormConfig {
            window: Duration::from_secs(window_secs),
            max_repeats,
        }
    }

    #[test]
    fn first_call_is_never_blocked() {
        let mut ring = VecDeque::new();
        let now = Instant::now();
        let r = decide_storm(&mut ring, now, &cfg(60, 2), "grep", "{}");
        assert!(r.is_none());
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn second_identical_call_passes_third_blocks_when_max_repeats_is_two() {
        let mut ring = VecDeque::new();
        let t0 = Instant::now();
        // 1st (record only).
        assert!(decide_storm(&mut ring, t0, &cfg(60, 2), "grep", "{\"q\":\"foo\"}").is_none());
        // 2nd (1 prior — under threshold).
        assert!(decide_storm(&mut ring, t0, &cfg(60, 2), "grep", "{\"q\":\"foo\"}").is_none());
        // 3rd (2 prior — at threshold — blocked).
        let blocked = decide_storm(&mut ring, t0, &cfg(60, 2), "grep", "{\"q\":\"foo\"}");
        let r = blocked.expect("third repeat should suppress");
        assert!(r.block);
        let reason = r.reason.expect("reason must be set");
        assert!(reason.contains("'grep'"));
        assert!(reason.contains("3 times"));
    }

    #[test]
    fn different_args_dont_count_as_repeats() {
        let mut ring = VecDeque::new();
        let t0 = Instant::now();
        for q in ["a", "b", "c", "d"] {
            let key = format!("{{\"q\":\"{q}\"}}");
            assert!(decide_storm(&mut ring, t0, &cfg(60, 2), "grep", &key).is_none());
        }
    }

    #[test]
    fn different_tool_names_dont_count_as_repeats() {
        let mut ring = VecDeque::new();
        let t0 = Instant::now();
        assert!(decide_storm(&mut ring, t0, &cfg(60, 2), "read", "{}").is_none());
        assert!(decide_storm(&mut ring, t0, &cfg(60, 2), "write", "{}").is_none());
        assert!(decide_storm(&mut ring, t0, &cfg(60, 2), "bash", "{}").is_none());
    }

    #[test]
    fn entries_outside_window_are_forgotten() {
        let mut ring = VecDeque::new();
        let t0 = Instant::now();
        // Two old entries (well outside the 1-second window).
        let old = t0;
        let fresh = old + Duration::from_secs(10);
        decide_storm(&mut ring, old, &cfg(1, 2), "grep", "{}");
        decide_storm(&mut ring, old, &cfg(1, 2), "grep", "{}");
        assert_eq!(ring.len(), 2);
        // A call 10s later — both old entries should be GC'd before
        // the count happens, so this fresh call sees 0 prior matches
        // and passes.
        let r = decide_storm(&mut ring, fresh, &cfg(1, 2), "grep", "{}");
        assert!(r.is_none());
        assert_eq!(ring.len(), 1, "old entries should have been pruned");
    }

    #[test]
    fn lower_max_repeats_blocks_sooner() {
        let mut ring = VecDeque::new();
        let t0 = Instant::now();
        // With max_repeats=1, the 2nd identical call blocks.
        assert!(decide_storm(&mut ring, t0, &cfg(60, 1), "grep", "{}").is_none());
        assert!(decide_storm(&mut ring, t0, &cfg(60, 1), "grep", "{}").is_some());
    }

    #[test]
    fn preview_args_truncates_with_ellipsis() {
        let long = "x".repeat(200);
        let p = preview_args(&long);
        // Truncated, but doesn't include the full string.
        assert!(p.len() < long.len());
        assert!(p.ends_with('…'));
    }

    #[test]
    fn preview_args_passes_through_short_strings() {
        assert_eq!(preview_args("{}"), "{}");
        assert_eq!(preview_args("{\"q\":\"foo\"}"), "{\"q\":\"foo\"}");
    }

    #[test]
    fn preview_args_handles_multibyte_boundary() {
        // 30 copies of a 3-byte char = 90 bytes; needs char-boundary
        // slicing to avoid panicking.
        let s = "中".repeat(30);
        let p = preview_args(&s);
        assert!(p.ends_with('…'));
    }
}
