//! Why the kernel loop stopped. Semantically neutral by design: the kernel
//! can say the model ended, the host cancelled, or a limit fired — never that
//! a task is complete, verified, or accepted. Those readings belong to the
//! host that embeds it.

use leveler_model::Message;

use crate::limits::BudgetExhaustion;

/// Mechanical reasons the loop ends on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The model produced a response with no tool calls and the host did not
    /// ask for another round. This ends the loop; it proves nothing about the
    /// work.
    ModelEnd,
    /// Cancelled from outside the run.
    Cancelled,
    /// The absolute round ceiling: the circuit breaker that fires regardless
    /// of progress or policy.
    RoundCeiling { ceiling: u32 },
    /// The host's pinned round limit was reached.
    WindowLimit { limit: u32 },
    /// A token, cost, or duration cap was exhausted.
    BudgetExhausted(BudgetExhaustion),
}

/// A neutral exit, handed to the harness so it can map it onto its own
/// outcome type with whatever state it holds.
#[derive(Debug)]
pub struct LoopStop {
    pub reason: StopReason,
    /// Rounds completed when the loop stopped.
    pub rounds: u32,
    /// The most recent non-empty assistant text, if any.
    pub last_text: String,
    /// The transcript as the model would see it next.
    pub messages: Vec<Message>,
}
