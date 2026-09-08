//! Child capability contract: why a child exists, what it may do, and what
//! it must return.
//!
//! This is the single resolve point for delegated-agent capability. It used
//! to be a `Copy` bag of flags on [`AgentRole`]. The flags still exist as
//! methods (`read_only`, `requires_scope`, …) derived from the policies
//! below, so call sites cannot drift from the named contract.

use serde::{Deserialize, Serialize};

use leveler_tools::ToolRegistry;

/// A sub-agent's role: its toolset and how it is prompted. Delegation is
/// CC-style star topology — the parent spawns focused workers/explorers and
/// collects their reports; sub-agents don't talk to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentRole {
    /// Full toolset (default when unspecified).
    Default,
    /// Read-only: investigates and reports; cannot modify the workspace.
    Explorer,
    /// Writes code, pinned to an explicit set of owned files.
    Worker,
    /// Read-only independent review of a change the parent already made.
    /// Launched by the harness when policy says the change warrants one, so it
    /// is the one role a model cannot ask for.
    Reviewer,
}

impl AgentRole {
    /// Model-facing parse. `"reviewer"` is *not* accepted: that role is
    /// harness-only. Unknown / omitted → Default (the historical behaviour).
    pub(crate) fn parse(s: Option<&str>) -> Self {
        match s.map(str::trim) {
            Some("explorer") => AgentRole::Explorer,
            Some("worker") => AgentRole::Worker,
            _ => AgentRole::Default,
        }
    }

    /// Strict parse of a known role label, including `reviewer`.
    pub(crate) fn from_label(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "default" => AgentRole::Default,
            "explorer" => AgentRole::Explorer,
            "worker" => AgentRole::Worker,
            "reviewer" => AgentRole::Reviewer,
            _ => return None,
        })
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            AgentRole::Default => "default",
            AgentRole::Explorer => "explorer",
            AgentRole::Worker => "worker",
            AgentRole::Reviewer => "reviewer",
        }
    }
}

/// Which tool *class* the child holds. Maps onto existing registry subsets;
/// never a per-role × tool-name matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolAccess {
    /// Observe-class tools (read / search). Explorer.
    ReadSearch,
    /// Same physical registry as [`Self::ReadSearch`]. Named separately so
    /// the contract can say "this child judges tests". `run_command` is a
    /// mutation surface and is *not* admitted — verification is host-owned.
    ReadTest,
    /// Full non-MCP tools, serialised, pinned to an exclusive scope. Worker.
    WriteScoped,
    /// Full non-MCP tools, late-bound claim. Default child.
    Inherit,
}

/// Tool permission policy. Enforced by the existing registry + ownership
/// fence, not by prompt text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolPolicy {
    pub access: ToolAccess,
}

/// How the child may touch the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkspaceMode {
    ReadOnly,
    /// Default child: no files at spawn; `claim_write_scope` after reading.
    LateBound,
    /// Worker: exclusive `files` required at spawn admission.
    ControlledMutation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspacePolicy {
    pub mode: WorkspaceMode,
    /// Worker: non-empty `files` required at spawn.
    pub requires_explicit_scope: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RuntimePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    pub serial_tools: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BudgetPolicy {
    /// Children share the parent's residual spend (already true).
    pub inherits_residual: bool,
    /// Wall-clock cap in seconds. Must match [`crate::sub_agent::SUB_AGENT_MAX_DURATION`].
    pub max_duration_secs: u64,
}

/// Wall-clock cap every child inherits. Keep in lockstep with
/// [`crate::sub_agent::SUB_AGENT_MAX_DURATION`].
pub(crate) const CHILD_MAX_DURATION_SECS: u64 = 20 * 60;

/// The capability contract of one child. Resolved in exactly one place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChildProfile {
    pub id: String,
    pub name: String,
    pub role: AgentRole,
    pub tool_policy: ToolPolicy,
    pub workspace_policy: WorkspacePolicy,
    pub runtime_policy: RuntimePolicy,
    pub budget_policy: BudgetPolicy,
}

impl ChildProfile {
    /// Physically read-only toolset (`read_only_subset`): no mutating tool is
    /// even advertised, so denial is structural, not behavioral.
    pub fn read_only(&self) -> bool {
        matches!(self.workspace_policy.mode, WorkspaceMode::ReadOnly)
    }

    /// Spawn admission requires a non-empty exclusive `files` scope.
    pub fn requires_scope(&self) -> bool {
        self.workspace_policy.requires_explicit_scope
    }

    pub fn max_rounds(&self) -> Option<u32> {
        self.runtime_policy.max_rounds
    }

    pub fn serial_tools(&self) -> bool {
        self.runtime_policy.serial_tools
    }

    /// `(profile_id, profile_role, read_only)` for events and projections.
    ///
    /// The third field used to be a list of capability labels
    /// (`repository_analysis`, `code_review`, …) — a taxonomy that described
    /// a child rather than bounding it, and that no runtime decision read.
    /// What every consumer actually wanted is the structural fact: may this
    /// child write? The TUI tried to derive it by scanning the labels for
    /// "write"/"edit"/"apply_patch", none of which a label ever was, so every
    /// profiled child — Workers included — rendered as read-only.
    pub fn trace_fields(&self) -> (String, String, bool) {
        (
            self.id.clone(),
            self.role.label().to_string(),
            self.read_only(),
        )
    }

    /// Apply this profile's tool class to an existing registry.
    ///
    /// Explorer / Reviewer get a registry that physically holds no write
    /// tools. Writers keep the full set except MCP proxies, whose effect
    /// cannot be bounded to a claimed scope.
    pub fn apply_to_registry(&self, registry: &ToolRegistry) -> ToolRegistry {
        match self.tool_policy.access {
            ToolAccess::ReadSearch | ToolAccess::ReadTest => registry.read_only_subset(),
            ToolAccess::WriteScoped | ToolAccess::Inherit => registry.without_mcp_tools(),
        }
    }

    /// The unnamed child `spawn_agent(task)` has always produced.
    pub fn default_profile() -> Self {
        Self {
            id: "default".into(),
            name: "Default".into(),
            role: AgentRole::Default,
            tool_policy: ToolPolicy {
                access: ToolAccess::Inherit,
            },
            workspace_policy: WorkspacePolicy {
                mode: WorkspaceMode::LateBound,
                requires_explicit_scope: false,
            },
            runtime_policy: RuntimePolicy {
                max_rounds: None,
                serial_tools: false,
            },
            budget_policy: BudgetPolicy {
                inherits_residual: true,
                max_duration_secs: CHILD_MAX_DURATION_SECS,
            },
        }
    }

    pub fn explorer() -> Self {
        Self {
            id: "explorer".into(),
            name: "Explorer".into(),
            role: AgentRole::Explorer,
            tool_policy: ToolPolicy {
                access: ToolAccess::ReadSearch,
            },
            workspace_policy: WorkspacePolicy {
                mode: WorkspaceMode::ReadOnly,
                requires_explicit_scope: false,
            },
            runtime_policy: RuntimePolicy {
                max_rounds: None,
                serial_tools: false,
            },
            budget_policy: BudgetPolicy {
                inherits_residual: true,
                max_duration_secs: CHILD_MAX_DURATION_SECS,
            },
        }
    }

    pub fn reviewer() -> Self {
        Self {
            id: "reviewer".into(),
            name: "Reviewer".into(),
            role: AgentRole::Reviewer,
            tool_policy: ToolPolicy {
                access: ToolAccess::ReadTest,
            },
            workspace_policy: WorkspacePolicy {
                mode: WorkspaceMode::ReadOnly,
                requires_explicit_scope: false,
            },
            runtime_policy: RuntimePolicy {
                max_rounds: Some(20),
                serial_tools: false,
            },
            budget_policy: BudgetPolicy {
                inherits_residual: true,
                max_duration_secs: CHILD_MAX_DURATION_SECS,
            },
        }
    }

    pub fn worker() -> Self {
        Self {
            id: "worker".into(),
            name: "Worker".into(),
            role: AgentRole::Worker,
            tool_policy: ToolPolicy {
                access: ToolAccess::WriteScoped,
            },
            workspace_policy: WorkspacePolicy {
                mode: WorkspaceMode::ControlledMutation,
                requires_explicit_scope: true,
            },
            runtime_policy: RuntimePolicy {
                max_rounds: None,
                serial_tools: true,
            },
            budget_policy: BudgetPolicy {
                inherits_residual: true,
                max_duration_secs: CHILD_MAX_DURATION_SECS,
            },
        }
    }

    /// The three product profiles plus the Default compatibility profile.
    pub fn builtins() -> [Self; 4] {
        [
            Self::default_profile(),
            Self::explorer(),
            Self::reviewer(),
            Self::worker(),
        ]
    }

    /// Resolve by id. Accepts `explorer` and `builtin.explorer`.
    pub fn lookup(id: &str) -> Option<Self> {
        let key = normalize_profile_id(id)?;
        Self::builtins().into_iter().find(|p| p.id == key)
    }

    /// The capability contract of one child role, resolved in exactly one place.
    pub(crate) fn resolve(role: AgentRole) -> Self {
        let profile = match role {
            AgentRole::Default => Self::default_profile(),
            AgentRole::Explorer => Self::explorer(),
            AgentRole::Worker => Self::worker(),
            AgentRole::Reviewer => Self::reviewer(),
        };
        debug_assert!(
            profile.validate().is_ok(),
            "built-in `{}` must validate: {:?}",
            profile.id,
            profile.validate()
        );
        profile
    }

    /// Structural checks. Built-ins must pass; a hand-built profile that
    /// contradicts itself is refused before it can be spawned.
    pub fn validate(&self) -> Result<(), String> {
        if !is_safe_profile_id(&self.id) {
            return Err(format!(
                "profile id `{}` is not a valid identifier \
                 (lowercase letters, digits, `_`, `-`, `.`; 1–64 chars)",
                self.id
            ));
        }
        if self.name.trim().is_empty() {
            return Err("profile name must not be empty".into());
        }
        let read_only = matches!(self.workspace_policy.mode, WorkspaceMode::ReadOnly);
        let writes = matches!(
            self.tool_policy.access,
            ToolAccess::WriteScoped | ToolAccess::Inherit
        );
        if read_only && writes {
            return Err(format!(
                "profile `{}` is workspace read-only but tool_policy admits writes",
                self.id
            ));
        }
        if !read_only
            && matches!(
                self.tool_policy.access,
                ToolAccess::ReadSearch | ToolAccess::ReadTest
            )
        {
            return Err(format!(
                "profile `{}` admits mutation but tool_policy is read-only",
                self.id
            ));
        }
        if self.role == AgentRole::Worker && !self.workspace_policy.requires_explicit_scope {
            return Err("worker profile must require an explicit file scope".into());
        }
        if read_only && self.workspace_policy.requires_explicit_scope {
            return Err(format!(
                "profile `{}` is read-only and cannot take a write scope",
                self.id
            ));
        }
        Ok(())
    }

    /// Minimal capability negotiation at spawn admission: the requested
    /// capabilities (role + file scope) against the role's profile.
    /// `Err` is an honest denial fed back to the model — never a silent
    /// downgrade.
    pub(crate) fn admit(role: AgentRole, files: &[String]) -> Result<Self, String> {
        let profile = Self::resolve(role);
        if profile.requires_scope() && files.is_empty() {
            return Err(format!(
                "role='{}' requires a non-empty `files` list naming the files it \
                 exclusively owns; an unscoped writer is not admitted.",
                role.label()
            ));
        }
        if profile.read_only() && !files.is_empty() {
            return Err(format!(
                "role='{}' is read-only and cannot take a `files` write scope. \
                 Use role='worker' for edits, or drop `files` to investigate.",
                role.label()
            ));
        }
        Ok(profile)
    }

    /// Model-facing spawn admission. `profile` is the new optional argument;
    /// `role` is the historical alias. Omit both → Default.
    ///
    /// `profile=reviewer` is refused: independent review is harness-launched.
    /// Unknown ids are refused. Conflicting `profile` + `role` is refused.
    pub(crate) fn admit_spawn(
        profile: Option<&str>,
        role: Option<&str>,
        files: &[String],
    ) -> Result<Self, String> {
        if let Some(raw) = profile.map(str::trim).filter(|s| !s.is_empty()) {
            let resolved = Self::lookup(raw).ok_or_else(|| unknown_profile_message(raw))?;
            if resolved.role == AgentRole::Reviewer {
                return Err(
                    "profile='reviewer' is harness-launched independent verification; \
                     spawn_agent cannot request it. Use profile='explorer' to investigate, \
                     or omit profile."
                        .into(),
                );
            }
            if let Some(role_raw) = role.map(str::trim).filter(|s| !s.is_empty())
                && let Some(explicit) = AgentRole::from_label(role_raw)
                && explicit != resolved.role
            {
                return Err(format!(
                    "profile='{raw}' is role '{}' but role='{role_raw}' was also set. \
                     Pick one.",
                    resolved.role.label()
                ));
            }
            return Self::admit(resolved.role, files);
        }
        Self::admit(AgentRole::parse(role), files)
    }
}

fn normalize_profile_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let key = lower.strip_prefix("builtin.").unwrap_or(&lower);
    if is_safe_profile_id(key) {
        Some(key.to_string())
    } else {
        None
    }
}

fn is_safe_profile_id(s: &str) -> bool {
    let len = s.len();
    (1..=64).contains(&len)
        && !s.starts_with('.')
        && !s.ends_with('.')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

fn unknown_profile_message(raw: &str) -> String {
    format!(
        "Unknown profile `{raw}`. Built-in: default, explorer, worker. \
         Reviewer is harness-launched. Omit profile to spawn the default child."
    )
}

/// Trace fields for a built-in (or parsed) profile, so the engine's
/// harness-launched reviewer emits the same shape as `spawn_agent` children.
pub fn child_profile_trace(role: &str) -> (String, String, bool) {
    let profile = ChildProfile::lookup(role)
        .or_else(|| AgentRole::from_label(role).map(ChildProfile::resolve))
        .unwrap_or_else(ChildProfile::default_profile);
    profile.trace_fields()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_validate() {
        for p in ChildProfile::builtins() {
            p.validate().unwrap_or_else(|e| panic!("{}: {e}", p.id));
        }
    }

    /// Each built-in is a bundle of STRUCTURAL bounds — what it may touch and
    /// for how long — with no semantic self-description. Every assertion here
    /// is something the runtime enforces.
    #[test]
    fn the_builtins_are_the_documented_bounds() {
        let d = ChildProfile::default_profile();
        assert_eq!(d.role, AgentRole::Default);
        assert!(!d.read_only());
        assert!(!d.requires_scope(), "the default child claims late");
        assert_eq!(d.tool_policy.access, ToolAccess::Inherit);
        assert_eq!(d.workspace_policy.mode, WorkspaceMode::LateBound);
        assert_eq!(d.max_rounds(), None);
        assert!(!d.serial_tools());

        let e = ChildProfile::explorer();
        assert!(e.read_only(), "an explorer holds no mutating tool at all");
        assert!(!e.requires_scope());
        assert_eq!(e.tool_policy.access, ToolAccess::ReadSearch);

        let r = ChildProfile::reviewer();
        assert!(r.read_only());
        assert_eq!(r.max_rounds(), Some(20), "a reviewer is bounded");

        let w = ChildProfile::worker();
        assert!(!w.read_only());
        assert!(w.requires_scope(), "an unscoped writer is not admitted");
        assert!(w.serial_tools(), "parallel writes conflict");
        assert_eq!(w.workspace_policy.mode, WorkspaceMode::ControlledMutation);
    }

    /// The bound the trace carries is the structural one. It used to be a list
    /// of capability labels, which the TUI then scanned for "write"/"edit" —
    /// words no label ever was, so every profiled child read as read-only.
    #[test]
    fn the_trace_carries_the_write_bound_not_a_taxonomy() {
        for (role, read_only) in [
            (AgentRole::Explorer, true),
            (AgentRole::Reviewer, true),
            (AgentRole::Worker, false),
            (AgentRole::Default, false),
        ] {
            let (id, role_label, traced) = ChildProfile::resolve(role).trace_fields();
            assert_eq!(role_label, role.label());
            assert_eq!(traced, read_only, "{id} must trace its real write bound");
        }
        assert!(!child_profile_trace("worker").2);
        assert!(child_profile_trace("explorer").2);
    }

    /// A profile that contradicts itself is refused before it can be spawned.
    #[test]
    fn a_self_contradictory_profile_is_refused() {
        let mut p = ChildProfile::explorer();
        p.tool_policy.access = ToolAccess::WriteScoped;
        assert!(p.validate().is_err(), "read-only workspace + write tools");

        let mut w = ChildProfile::worker();
        w.workspace_policy.requires_explicit_scope = false;
        assert!(w.validate().is_err(), "an unscoped worker is not a worker");

        let mut bad_id = ChildProfile::explorer();
        bad_id.id = "../etc".into();
        assert!(bad_id.validate().is_err());
    }

    #[test]
    fn a_read_only_role_is_refused_a_write_scope_and_a_writer_needs_one() {
        assert!(ChildProfile::admit(AgentRole::Worker, &[]).is_err());
        assert!(ChildProfile::admit(AgentRole::Worker, &["a.rs".into()]).is_ok());
        assert!(ChildProfile::admit(AgentRole::Explorer, &["a.rs".into()]).is_err());
        assert!(ChildProfile::admit(AgentRole::Explorer, &[]).is_ok());
    }

    /// The reviewer is harness-launched: a model cannot ask for one, and a
    /// conflicting profile/role pair is refused rather than silently resolved.
    #[test]
    fn spawn_admission_refuses_reviewer_unknown_and_conflicting_requests() {
        assert!(ChildProfile::admit_spawn(Some("reviewer"), None, &[]).is_err());
        assert!(ChildProfile::admit_spawn(Some("nonsense"), None, &[]).is_err());
        assert!(
            ChildProfile::admit_spawn(Some("explorer"), Some("worker"), &[]).is_err(),
            "profile and role must not disagree"
        );
        assert_eq!(
            ChildProfile::admit_spawn(Some("builtin.explorer"), None, &[])
                .unwrap()
                .role,
            AgentRole::Explorer,
            "the `builtin.` prefix resolves to the same profile"
        );
        assert_eq!(
            ChildProfile::admit_spawn(None, None, &[]).unwrap().role,
            AgentRole::Default
        );
    }

    /// `reviewer` is not a model-facing role word, but it IS a valid label
    /// when the harness resolves one by name.
    #[test]
    fn role_parsing_keeps_reviewer_harness_only() {
        assert_eq!(AgentRole::parse(Some("reviewer")), AgentRole::Default);
        assert_eq!(AgentRole::parse(Some("explorer")), AgentRole::Explorer);
        assert_eq!(AgentRole::parse(None), AgentRole::Default);
        assert_eq!(AgentRole::from_label("reviewer"), Some(AgentRole::Reviewer));
        assert_eq!(AgentRole::from_label("nonsense"), None);
    }

    #[test]
    fn every_child_shares_one_wall_clock_bound() {
        for p in ChildProfile::builtins() {
            assert_eq!(p.budget_policy.max_duration_secs, CHILD_MAX_DURATION_SECS);
            assert!(p.budget_policy.inherits_residual);
        }
    }
}
