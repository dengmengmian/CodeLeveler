//! `leveler-vcs` — git/GitHub workflow for shipping an agent's changes:
//! create a branch, commit, push, and open a pull request.
//!
//! All operations go through the `git`/`gh` CLIs via the execution layer's
//! [`CommandRunner`], so there is no libgit2 dependency and behavior matches
//! what a user would run by hand.
#![forbid(unsafe_code)]

pub mod parallel;
mod workflow;

pub use parallel::{MergeCandidate, MergeOutcome, worktree_path};
pub use workflow::{GitWorkflow, VcsError, WorkflowOptions, WorkflowOutcome, slugify};
