//! `leveler-core` — the stable foundation shared across every CodeLeveler crate.
//!
//! This crate deliberately holds *only* primitives that almost never change:
//! typed identifiers, timestamps, resource budgets, and a couple of base
//! traits. Per the product spec (§8.3) business types must **not** accumulate
//! here — they belong to the crate that owns that concern.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod budget;
pub mod environment;
pub mod ids;
pub mod text;
pub mod time;

pub use budget::ResourceBudget;
pub use environment::{
    EnvSnapshot, environment, git_stdout, install_environment, is_credential_env_name,
    leveler_home_dir, leveler_home_dir_from, scrubbed_environment,
};
pub use ids::{
    ApprovalId, ArtifactId, CheckpointId, ClarificationId, CommandId, EventId, RequestId,
    SessionId, TaskId, TaskNodeId, ToolCallId, TurnId, new_uuid_string,
};
pub use text::{
    ceil_char_boundary, floor_char_boundary, redact_secrets, sanitize_terminal_output,
    truncate_head_bytes, truncate_tail_bytes,
};
pub use time::{Timestamp, now};
