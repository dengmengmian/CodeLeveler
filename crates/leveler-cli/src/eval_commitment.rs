//! C1.5A ablation seam: a one-shot "commit to an edit" nudge, delivered
//! through the existing mid-turn steering channel.
//!
//! EVAL ONLY, OFF BY DEFAULT. Nothing here runs unless
//! `LEVELER_EVAL_COMMITMENT_NUDGE=<rounds>` is set for an ablation arm; a
//! normal `leveler eval run` (and every product path) is untouched.
//!
//! The trigger uses only facts the agent itself can see — a plan exists, N
//! model rounds have passed since it appeared, and nothing has been edited
//! yet. It never consults `relevant_paths`, `first_relevant_file_round`, or
//! any other hidden ground truth: an ablation that waits for the model to
//! stumble onto the right file and then tells it to edit would be measuring
//! the harness, not the agent.

use std::sync::Mutex;

use leveler_agent::{AgentEvent, SteeringSource};

/// The nudge text. Deliberately generic: no file, no symbol, no patch, and an
/// explicit escape hatch so a run facing a real blocker is not pushed into
/// guessing.
const COMMITMENT_NUDGE: &str = "You already have a plan and have spent several rounds gathering \
     evidence without changing anything. Unless a concrete blocker remains, make the smallest \
     plausible implementation now and use the verification feedback to refine it. Do not continue \
     broad exploration merely for completeness.";

#[derive(Default)]
struct State {
    /// Model rounds observed, proxied by stream attempts.
    rounds: u32,
    /// Round the first `update_plan` call landed on.
    plan_round: Option<u32>,
    /// Whether any edit tool has been called yet.
    edited: bool,
    /// The nudge is one-shot.
    fired: bool,
    pending: Option<String>,
}

/// Fires the nudge once, after `quiet_rounds` model rounds have passed since a
/// plan appeared with no edit in between.
pub(crate) struct CommitmentNudge {
    quiet_rounds: u32,
    state: Mutex<State>,
}

impl CommitmentNudge {
    /// The configured ablation, or `None` when the arm is off (the default).
    pub(crate) fn from_environment() -> Option<Self> {
        let quiet_rounds = std::env::var("LEVELER_EVAL_COMMITMENT_NUDGE")
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)?;
        Some(Self {
            quiet_rounds,
            state: Mutex::new(State::default()),
        })
    }

    /// Fold one agent event in. Called from the eval's existing observer.
    pub(crate) fn observe(&self, event: &AgentEvent) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            AgentEvent::StreamAttemptStarted => {
                state.rounds = state.rounds.saturating_add(1);
                let quiet_since_plan = state
                    .plan_round
                    .map(|plan| state.rounds.saturating_sub(plan));
                if !state.fired
                    && !state.edited
                    && quiet_since_plan.is_some_and(|quiet| quiet >= self.quiet_rounds)
                {
                    state.fired = true;
                    state.pending = Some(COMMITMENT_NUDGE.to_string());
                }
            }
            AgentEvent::ToolCall { name, .. } => match name.as_str() {
                "update_plan" => {
                    let round = state.rounds.max(1);
                    state.plan_round.get_or_insert(round);
                }
                "apply_patch" | "replace" => state.edited = true,
                _ => {}
            },
            _ => {}
        }
    }

    /// Whether the nudge was delivered, for the ablation report.
    #[allow(dead_code)]
    pub(crate) fn fired(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).fired
    }
}

impl SteeringSource for CommitmentNudge {
    fn take_pending(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .take()
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nudge(quiet_rounds: u32) -> CommitmentNudge {
        CommitmentNudge {
            quiet_rounds,
            state: Mutex::new(State::default()),
        }
    }

    fn call(name: &str) -> AgentEvent {
        AgentEvent::ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: "{}".into(),
            parallel: false,
        }
    }

    /// The trigger is "plan, then N quiet rounds" — and it fires exactly once.
    #[test]
    fn fires_once_after_quiet_rounds_following_a_plan() {
        let n = nudge(3);
        n.observe(&AgentEvent::StreamAttemptStarted); // round 1
        n.observe(&call("update_plan"));
        for _ in 0..2 {
            n.observe(&AgentEvent::StreamAttemptStarted); // rounds 2-3
            n.observe(&call("read_file"));
        }
        assert!(n.take_pending().is_empty(), "two quiet rounds is not three");
        n.observe(&AgentEvent::StreamAttemptStarted); // round 4
        let delivered = n.take_pending();
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].contains("smallest plausible implementation"));
        n.observe(&AgentEvent::StreamAttemptStarted);
        assert!(n.take_pending().is_empty(), "one-shot");
    }

    /// A run that is already editing is behaving; it must never be nudged.
    #[test]
    fn never_fires_once_an_edit_has_happened() {
        let n = nudge(2);
        n.observe(&AgentEvent::StreamAttemptStarted);
        n.observe(&call("update_plan"));
        n.observe(&call("apply_patch"));
        for _ in 0..10 {
            n.observe(&AgentEvent::StreamAttemptStarted);
        }
        assert!(n.take_pending().is_empty());
        assert!(!n.fired());
    }

    /// No plan yet means no opinion: exploration before a plan is not the
    /// behavior under test.
    #[test]
    fn never_fires_before_a_plan_exists() {
        let n = nudge(2);
        for _ in 0..10 {
            n.observe(&AgentEvent::StreamAttemptStarted);
            n.observe(&call("grep"));
        }
        assert!(n.take_pending().is_empty());
    }

    /// Off unless the arm is explicitly configured: a value that is not a
    /// positive round count leaves the ablation absent, so a stray or empty
    /// variable can never silently arm it.
    #[test]
    fn only_a_positive_round_count_arms_the_ablation() {
        for raw in ["", "0", "off", "-3", "two"] {
            assert!(
                raw.trim().parse::<u32>().ok().filter(|n| *n > 0).is_none(),
                "{raw:?} must not arm the ablation"
            );
        }
        assert_eq!("8".trim().parse::<u32>().ok().filter(|n| *n > 0), Some(8));
    }
}
