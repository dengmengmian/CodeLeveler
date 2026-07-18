//! `leveler-tools` — the tool system (spec §18).
//!
//! Defines the [`Tool`] trait, a [`ToolRegistry`] that validates arguments
//! against each tool's JSON schema before dispatch, and the built-in
//! tools: `read_file`, `list_files`, `grep`, `apply_patch`, `run_command`,
//! `git_status`, `git_diff`.
#![forbid(unsafe_code)]

pub mod mcp;
pub mod recoverable;
pub mod registry;
pub mod tool;
pub mod tools;

pub use registry::{
    ToolRegistry, core_registry, default_registry, expand_tool_category, full_registry,
};
pub use tool::{Tool, ToolContext, ToolError, ToolOutput};

// Re-export the risk vocabulary so consumers need not depend on execution.
pub use leveler_execution::{PermissionProfile, RiskLevel};
