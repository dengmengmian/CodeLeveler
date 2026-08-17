//! The execution lifecycle vocabulary.
//!
//! Session status, agent state, and task outcome are persisted by
//! `leveler-storage`, produced by `leveler-engine`, projected by clients, and
//! mapped by the app. They are one shared, typed vocabulary rather than strings
//! passed across layer boundaries — so every layer speaks the same language and
//! the low-level storage crate can persist them without a back-edge to a
//! high-level crate.
//!
//! ## Runtime lifecycle vs Coding workflow
//!
//! The crate is split along the runtime-evolution boundary:
//!
//! - [`runtime`] — the **generic runtime lifecycle**: [`SessionStatus`],
//!   [`TaskOutcome`], [`TurnOutcome`]. Domain-neutral; a future non-Coding
//!   domain depends on this module without pulling Coding semantics.
//! - [`workflow`] — the **Coding workflow**: [`AgentState`] and, over time,
//!   the other Coding-phase structures. Refines the runtime lifecycle, never
//!   redefines it. `runtime` must not reference `workflow`.
//!
//! The remaining modules (plan, progress, readiness, ledger, impact, axes,
//! contract, objective) are Coding/product vocabulary and conceptually sit on
//! the workflow side; they keep their top-level paths until a consumer needs
//! the split to be physical.
//!
//! Three axes are kept deliberately distinct (see the M1A ADR):
//! - [`SessionStatus`] — the *operational* position in the lifecycle.
//! - [`TaskOutcome`] — the *terminal* verdict; `Verified` is the only
//!   automation success.
//! - [`TurnOutcome`] — whether one engine turn completed, failed, or was
//!   interrupted, independent of the task's later verification verdict.
//!
//! Each enum round-trips through a lowercase wire string: `as_str` for
//! persistence, [`std::str::FromStr`] for decode. An unknown persisted value is
//! a named [`UnknownVariant`] error — never a guessed default.

#![forbid(unsafe_code)]

mod axes;
mod contract;
mod findings;
mod impact;
mod ledger;
mod objective;
mod plan;
mod progress;
mod readiness;
pub mod runtime;
pub mod workflow;

pub use axes::{CollaborationMode, DepthUseMetrics, WorkProfile};
pub use contract::TaskContract;
pub use findings::{FindingError, FindingKind, FindingRecord, FindingState, transition_allowed};
pub use impact::{ChangeImpact, is_build_relevant};
pub use ledger::{
    CompleteStepReceipt, EvidenceLedger, InterceptRecord, MutationRecord, VerifyRecord,
};
pub use objective::{ObjectiveAnchor, ObjectiveSource};
pub use plan::{PlanOrigin, PlanState, PlanStep};
pub use progress::{ProgressCaps, ProgressLedger, TurnPhase};
pub use readiness::{
    GateConfig, ReadinessFailure, TaskClass, check, classify_task, task_looks_like_implementation,
};
// Original top-level paths stay valid: the module split is semantic first,
// physical second — no consumer changes required.
pub use runtime::{SessionStatus, TaskOutcome, TurnOutcome, UnknownVariant};
pub use workflow::AgentState;
