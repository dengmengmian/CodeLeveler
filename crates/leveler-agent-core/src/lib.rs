//! `leveler-agent-core` — the generic agent kernel.
//!
//! One authoritative model↔tool loop over the provider-neutral types in
//! `leveler-model`:
//!
//! ```text
//! model request → model response → tool calls → tool results → next request
//! ```
//!
//! The kernel owns what is *mechanically* generic about running an agent:
//! the round loop, the model-round assembly (streaming, retry, backoff), the
//! neutral stop reasons, the round/token/cost/duration limits, cancellation
//! and the deadline timer, usage accounting, and one tool-dispatch seam.
//!
//! It deliberately owns nothing product-shaped. A host embeds the kernel by
//! implementing [`AgentHarness`] — the seams the loop calls at each round —
//! and, for the common case of a plain tool-calling agent, a [`ToolRuntime`]
//! wrapped in [`BasicHarness`]. Permission, write scope, repository context,
//! prompts, persistence, delegation and every other CodeLeveler concern live
//! above this crate, in the harness that composes it.
//!
//! The kernel never judges whether the model's work is good, complete, or
//! acceptable: a run ends where the model stops, where the host stops it, or
//! where a mechanical limit stops it.
#![forbid(unsafe_code)]

mod agent;
mod error;
mod event;
mod harness;
mod limits;
mod model_round;
mod stop;
mod tool_runtime;
mod usage;

pub use agent::Agent;
pub use error::AgentCoreError;
pub use event::AgentEvent;
pub use harness::{AgentHarness, BasicHarness, Flow, LoopContext};
pub use limits::{
    BudgetDimension, BudgetExhaustion, DEFAULT_ROUND_CEILING, RoundAdmission, RoundAdmissionInput,
    RoundLimits, SpentBefore, admit_next_round,
};
pub use model_round::{ModelRound, run_model_round};
pub use stop::{LoopStop, StopReason};
pub use tool_runtime::{ToolOutcome, ToolRuntime, ToolRuntimeError, dispatch_calls};
pub use usage::{UsageProjection, estimate_tokens};
