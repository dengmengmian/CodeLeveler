//! `leveler-tools` — the tool system (spec §18).
//!
//! Defines the [`Tool`] trait, a [`ToolRegistry`] that validates arguments
//! against each tool's JSON schema before dispatch, and the built-in
//! tools: `read_file`, `list_files`, `grep`, `apply_patch`, `run_command`,
//! `git_status`, `git_diff`.
#![forbid(unsafe_code)]

pub mod answer_effect;
pub mod capabilities;
pub mod definition_trust;
pub mod mcp;
pub(crate) mod memory_access;
pub(crate) mod network;
pub(crate) mod network_budget;
pub mod recoverable;
pub mod registry;
pub mod search;
pub mod tool;
pub mod tools;
pub(crate) mod workspace;

pub use answer_effect::{
    BOOKKEEPING_TOOLS, acts_on_answer, is_silent_shell_probe, update_goal_is_blocked,
};
pub use capabilities::Capabilities;
pub use definition_trust::{
    DefinitionTrustAuthorizationOutcome, DefinitionTrustAuthorizationRequest,
    DefinitionTrustAuthorizer, DefinitionTrustDecision, MAX_AUTHORIZATION_PREVIEW_BYTES,
};
#[cfg(debug_assertions)]
pub use network::{clear_test_dns_rebind_for_tests, install_test_dns_rebind_for_tests};
pub use network_budget::NetworkTrace;
#[cfg(feature = "test-support")]
pub use network_budget::{
    LAYER_GLOBAL, LAYER_HOST, LAYER_PROVIDER, LAYER_PROVIDER_RATE, LAYER_SESSION,
    clear_budget_wait_samples, take_budget_wait_samples,
};
pub use registry::{
    CapabilityPacks, MODEL_SURFACE_TOOLS, OBSERVE_CLASS_TOOLS, ToolRegistry, core_surface,
    default_registry, is_observe_class_tool, model_surface,
};
pub use tool::{ParallelClass, Tool, ToolContext, ToolError, ToolOutput};

// Re-export the risk vocabulary so consumers need not depend on execution.
pub use leveler_execution::{PermissionProfile, RiskLevel};
