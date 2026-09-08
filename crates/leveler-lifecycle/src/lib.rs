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
//!   [`TaskOutcome`], [`VerificationStatus`], [`TurnOutcome`]. Domain-neutral; a future non-Coding
//!   domain depends on this module without pulling Coding semantics.
//! - [`workflow`] — the **Coding workflow**: [`AgentState`] and, over time,
//!   the other Coding-phase structures. Refines the runtime lifecycle, never
//!   redefines it. `runtime` must not reference `workflow`.
//!
//! The remaining modules (plan, progress, readiness, ledger, impact, axes,
//! objective) are Coding/product vocabulary and conceptually sit on
//! the workflow side; they keep their top-level paths until a consumer needs
//! the split to be physical.
//!
//! Four axes are kept deliberately distinct (see the M1A ADR):
//! - [`SessionStatus`] — the *operational* position in the lifecycle.
//! - [`TaskOutcome`] — how the task *ended* (completed / blocked / failed…).
//! - [`VerificationStatus`] — what the project's own checks said about the
//!   final tree. Orthogonal to the outcome: the runtime reports both and
//!   never folds them into one word.
//! - [`TurnOutcome`] — whether one engine turn completed, failed, or was
//!   interrupted, independent of the task's later verification verdict.
//!
//! ## Runtime authority boundary
//!
//! The runtime owns mechanical truth: which files changed, which commands ran
//! and how they exited, whether a check ran after the latest edit. The model
//! owns semantic interpretation of the goal, and the user owns acceptance.
//! Nothing in this crate claims that a green check proves the user's request
//! was satisfied.
//!
//! Each enum round-trips through a lowercase wire string: `as_str` for
//! persistence, [`std::str::FromStr`] for decode. An unknown persisted value is
//! a named [`UnknownVariant`] error — never a guessed default.

#![forbid(unsafe_code)]

mod axes;
mod checkpoint;
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
pub use checkpoint::{
    CheckpointChild, CheckpointFindings, CheckpointPlan, CheckpointReason, CheckpointVerification,
    CheckpointWorkspace, GOAL_CHECKPOINT_SCHEMA_VERSION, GoalCheckpoint,
};
pub use findings::{ChildResultProjection, FindingKind, FindingRecord};
pub use impact::{ChangeImpact, is_build_relevant};
pub use ledger::{EvidenceLedger, InterceptRecord, MutationRecord, VerifyRecord};
pub use objective::{ObjectiveAnchor, ObjectiveSource};
pub use plan::{PlanOrigin, PlanState, PlanStep};
pub use progress::{ProgressCaps, ProgressLedger, TurnPhase};
pub use readiness::{GateConfig, ReadinessFailure, check};
// Original top-level paths stay valid: the module split is semantic first,
// physical second — no consumer changes required.
pub use runtime::{SessionStatus, TaskOutcome, TurnOutcome, UnknownVariant, VerificationStatus};
pub use workflow::AgentState;
