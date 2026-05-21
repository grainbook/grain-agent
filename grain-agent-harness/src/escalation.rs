//! Auto-escalation hook — swaps to a stronger model when failure
//! signals accumulate inside a turn.
//!
//! Inspired by DeepSeek-Reasonix's Pillar 3 "Failure-Signal
//! Auto-Escalation" mechanism, but written provider-agnostically so it
//! works for any model pair (Claude Haiku → Sonnet, GPT-4o-mini → GPT-4o,
//! DeepSeek flash → pro, …).
//!
//! Wires into the loop via [`grain_agent_core::PrepareNextTurnFn`].
//! Recovery (`reset_on_recovery = true`) flips the swap back on the
//! next failure-free turn so long sessions don't ratchet up cost.

use std::sync::Arc;

use grain_agent_core::{
    AgentLoopTurnUpdate, AssistantMessage, Model, PrepareNextTurnFn, ToolResultMessage,
};
use tokio::sync::Mutex;

/// Configuration for [`failure_escalation_hook`].
#[derive(Debug, Clone)]
pub struct EscalationConfig {
    /// Number of failure signals (assistant errors + tool errors)
    /// required within the current session before escalation kicks in.
    pub threshold: u32,
    /// Model to swap in when `threshold` is reached. Typically the
    /// "pro" tier of the current model family.
    pub target: Model,
    /// When `true` (default), a turn that produces zero new failure
    /// signals while in the escalated state resets the counter and
    /// flips the swap back, so the next turn runs on the original
    /// (cheaper) model again.
    pub reset_on_recovery: bool,
}

impl EscalationConfig {
    pub fn new(threshold: u32, target: Model) -> Self {
        EscalationConfig {
            threshold,
            target,
            reset_on_recovery: true,
        }
    }
}

/// Build a [`PrepareNextTurnFn`] that returns a model swap when the
/// running failure-signal count crosses `config.threshold`.
pub fn failure_escalation_hook(config: EscalationConfig) -> PrepareNextTurnFn {
    let state: Arc<Mutex<EscalationState>> = Arc::new(Mutex::new(EscalationState::default()));
    Arc::new(move |ctx| {
        let state = state.clone();
        let config = config.clone();
        Box::pin(async move {
            let new_failures = count_failures(&ctx.message, &ctx.tool_results);
            let mut state = state.lock().await;
            if decide_escalation(
                &mut state,
                new_failures,
                config.threshold,
                config.reset_on_recovery,
            ) {
                eprintln!(
                    "[info] failure-escalation: failures={} ≥ {} — switching to {}",
                    state.failures_seen, config.threshold, config.target.id,
                );
                Some(AgentLoopTurnUpdate {
                    model: Some(config.target.clone()),
                    ..Default::default()
                })
            } else {
                None
            }
        })
    })
}

/// Running counter state. Public for callers that want to wire their
/// own hook on top of [`decide_escalation`].
#[derive(Debug, Default, Clone)]
pub struct EscalationState {
    pub failures_seen: u32,
    pub escalated: bool,
}

/// Pure decision: returns `true` exactly on the turn the swap should
/// happen. Caller is responsible for actually returning the
/// `AgentLoopTurnUpdate` afterwards.
pub fn decide_escalation(
    state: &mut EscalationState,
    new_failures: u32,
    threshold: u32,
    reset_on_recovery: bool,
) -> bool {
    if new_failures > 0 {
        state.failures_seen = state.failures_seen.saturating_add(new_failures);
    } else if reset_on_recovery && state.escalated {
        // Successful recovery while escalated → roll back.
        state.failures_seen = 0;
        state.escalated = false;
    }
    if state.failures_seen >= threshold && !state.escalated {
        state.escalated = true;
        true
    } else {
        false
    }
}

/// Count failure signals attributable to the turn that just ended.
/// Pure — testable without async.
pub fn count_failures(msg: &AssistantMessage, tool_results: &[ToolResultMessage]) -> u32 {
    let mut n = 0u32;
    if msg.error_message.is_some() {
        n = n.saturating_add(1);
    }
    let tool_errs = tool_results.iter().filter(|t| t.is_error).count() as u32;
    n.saturating_add(tool_errs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_agent_core::{StopReason, TextContent, UserContent};

    fn ok_assistant() -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: "openai".into(),
            provider: "openai".into(),
            model: "gpt-x".into(),
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
    }

    fn err_assistant() -> AssistantMessage {
        AssistantMessage {
            error_message: Some("boom".into()),
            stop_reason: StopReason::Error,
            ..ok_assistant()
        }
    }

    fn tool_result(is_error: bool) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: "id".into(),
            tool_name: "grep".into(),
            content: vec![UserContent::Text(TextContent {
                text: "x".into(),
            })],
            details: serde_json::Value::Null,
            is_error,
            timestamp: 0,
        }
    }

    // -------- count_failures ----------------------------------------

    #[test]
    fn count_failures_zero_when_clean() {
        assert_eq!(count_failures(&ok_assistant(), &[]), 0);
    }

    #[test]
    fn count_failures_picks_up_assistant_error() {
        assert_eq!(count_failures(&err_assistant(), &[]), 1);
    }

    #[test]
    fn count_failures_picks_up_tool_errors() {
        let results = vec![tool_result(false), tool_result(true), tool_result(true)];
        assert_eq!(count_failures(&ok_assistant(), &results), 2);
    }

    #[test]
    fn count_failures_sums_both_sources() {
        let results = vec![tool_result(true)];
        assert_eq!(count_failures(&err_assistant(), &results), 2);
    }

    // -------- decide_escalation -------------------------------------

    #[test]
    fn does_not_escalate_below_threshold() {
        let mut s = EscalationState::default();
        assert!(!decide_escalation(&mut s, 1, 3, true));
        assert!(!decide_escalation(&mut s, 1, 3, true));
        assert_eq!(s.failures_seen, 2);
        assert!(!s.escalated);
    }

    #[test]
    fn escalates_when_threshold_reached_first_time() {
        let mut s = EscalationState::default();
        decide_escalation(&mut s, 1, 3, true);
        decide_escalation(&mut s, 1, 3, true);
        let swap = decide_escalation(&mut s, 1, 3, true);
        assert!(swap, "third failure should swap");
        assert!(s.escalated);
    }

    #[test]
    fn does_not_re_escalate_while_still_escalated() {
        let mut s = EscalationState::default();
        decide_escalation(&mut s, 3, 3, true); // trip immediately
        assert!(s.escalated);
        // More failures don't cause a second swap.
        let swap = decide_escalation(&mut s, 5, 3, true);
        assert!(!swap);
    }

    #[test]
    fn recovery_resets_counter_when_enabled() {
        let mut s = EscalationState::default();
        decide_escalation(&mut s, 3, 3, true); // escalate
        // Failure-free turn → reset.
        let swap = decide_escalation(&mut s, 0, 3, true);
        assert!(!swap);
        assert!(!s.escalated);
        assert_eq!(s.failures_seen, 0);
        // Now we can escalate again later.
        decide_escalation(&mut s, 3, 3, true);
        assert!(s.escalated);
    }

    #[test]
    fn recovery_off_keeps_escalation_sticky() {
        let mut s = EscalationState::default();
        decide_escalation(&mut s, 3, 3, false);
        assert!(s.escalated);
        decide_escalation(&mut s, 0, 3, false);
        assert!(s.escalated, "without recovery, escalation must stick");
    }

    #[test]
    fn jumping_past_threshold_in_one_turn_still_only_swaps_once() {
        let mut s = EscalationState::default();
        let swap = decide_escalation(&mut s, 99, 3, true);
        assert!(swap);
        let swap = decide_escalation(&mut s, 99, 3, true);
        assert!(!swap);
    }
}
