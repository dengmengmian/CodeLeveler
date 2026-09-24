//! Why the kernel loop stopped. Semantically neutral by design: the kernel
//! can say the model ended, the host cancelled, or a limit fired — never that
//! a task is complete, verified, or accepted. Those readings belong to the
//! host that embeds it.
//!
//! The two model-step reasons are deliberately distinct from
//! [`StopReason::BudgetExhausted`]: a model step is a mechanical loop
//! iteration, so running out of them is never a statement about a task's
//! remaining resources. Which of the two applies is the host's shape: a
//! `ceiling` is the unconditional safety breaker, a `window limit` is a
//! bounded unit of work the host deliberately pinned (an eval case, a
//! delegated agent's manifest budget).

use leveler_model::Message;

use crate::limits::BudgetExhaustion;

/// Mechanical reasons the loop ends on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The model produced a response with no tool calls and the host did not
    /// ask for another model step. This ends the loop; it proves nothing about
    /// the work.
    ModelEnd,
    /// Cancelled from outside the run.
    Cancelled,
    /// The model-step safety ceiling: the circuit breaker that fires
    /// regardless of progress or policy, so a runaway model↔tool loop cannot
    /// spin forever. It is not a task budget.
    ModelStepCeiling { ceiling: u32 },
    /// A host-pinned window of model steps was used up. The host asked for a
    /// bounded unit of work (an eval case, a delegated agent's budget) and it
    /// ended normally; the host decides whether another run opens.
    ModelStepWindowLimit { limit: u32 },
    /// A token, cost, or duration cap was exhausted. This is the only stop
    /// reason that reports a task-level resource budget.
    BudgetExhausted(BudgetExhaustion),
}

/// A neutral exit, handed to the harness so it can map it onto its own
/// outcome type with whatever state it holds.
#[derive(Debug)]
pub struct LoopStop {
    pub reason: StopReason,
    /// Model steps started when the loop stopped.
    pub model_steps: u32,
    /// The most recent non-empty assistant text, if any.
    pub last_text: String,
    /// The transcript as the model would see it next.
    pub messages: Vec<Message>,
}
