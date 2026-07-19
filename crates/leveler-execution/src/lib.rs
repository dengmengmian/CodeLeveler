//! `leveler-execution` — workspace safety, risk classification, and process
//! execution (spec §8.12, §19, §20, §21).
//!
//! Provides a [`Workspace`] that resolves and validates every path so the
//! file/patch tools cannot escape the repository, the [`RiskLevel`] /
//! [`PermissionProfile`] vocabulary the tool layer tags itself with, a
//! [`CommandRunner`] with process-tree termination, the permission
//! [`ApprovalPolicy`]/[`Approver`], and a [`Checkpoint`] for rollback.
// `deny` (not `forbid`) with exactly one scoped allow: the Linux
// PR_SET_PDEATHSIG pre-exec hook in `command.rs`/`background.rs` — the only
// way to guarantee grandchildren die when the parent is force-killed. Any
// new unsafe block still fails the build unless explicitly allowed and
// justified like that one.
#![deny(unsafe_code)]

pub mod approval;
pub mod artifact;
pub mod background;
pub mod checkpoint;
pub mod command;
pub mod hooks;
pub mod permission_grants;
pub mod permission_rules;
pub mod risk;
mod shell_ast;
pub mod snapshot;
pub mod windows_acl;
pub mod windows_appcontainer;
pub mod windows_sandbox;
pub mod workspace;

pub use approval::{
    ApprovalDecision, ApprovalPolicy, ApprovalRequest, Approver, AutoApprove, AutoDeny,
    AutoReviewer, CommandClass, CommandView, NeedUserReviewer, Requirement, ReviewVerdict,
    classify_command, command_needs_host_escape, is_comment_only_acceptance_command,
    is_host_escape_program, is_memory_write_tool, is_remote_publish_command, is_shell_c_flag,
    is_shell_wrapper_program, is_trivial_acceptance_command, shell_c_script,
};
pub use artifact::{ArtifactRef, ArtifactStore};
pub use background::{
    BackgroundTaskRegistry, BackgroundTaskSnapshot, BackgroundTaskStatus, MutationBaseline,
};
pub use checkpoint::Checkpoint;
pub use command::{
    CommandRunner, ProcessError, ProcessOutput, ProcessRequest, VerifyNetworkPolicy,
    credential_env_names, first_absolute_arg_outside_roots, is_credential_env_name,
    looks_like_absolute_path_arg, process_request_for_verify_check,
};
pub use hooks::{HookRunner, PreHookResult};
pub use permission_grants::{
    GrantFile, grants_path, load_grants, remember_project_grant, signatures_from_file,
};
pub use permission_rules::{
    PermissionRule, PermissionRuleSet, RuleDecision, RuleEffect, RuleMatch, always_rules_for,
    append_project_rule, append_rule_file, clear_project_rules, load_merged_rules, load_rules_file,
    project_rules_path,
};
pub use risk::{PermissionProfile, RiskLevel};
pub use snapshot::{SnapshotError, SnapshotId, WorkspaceSnapshot};
pub use windows_sandbox::{
    FilesystemIntent, FsCapability, ProcessTreeCapability, SandboxBackend, SandboxCapabilities,
    WindowsSandboxError, assert_intent_spawn_allowed, assert_windows_spawn_allowed,
    doctor_sandbox_line, probe_sandbox_capabilities, process_tree_backend_available,
    validate_acl_root,
};
pub use workspace::{PathAccess, Workspace, WorkspaceError};
