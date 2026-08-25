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

/// What a child is for. Closed and composable — not a per-tool allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChildCapability {
    RepositoryAnalysis,
    CodeReview,
    Implementation,
    Testing,
    Verification,
}

impl ChildCapability {
    pub fn label(self) -> &'static str {
        match self {
            Self::RepositoryAnalysis => "repository_analysis",
            Self::CodeReview => "code_review",
            Self::Implementation => "implementation",
            Self::Testing => "testing",
            Self::Verification => "verification",
        }
    }

    #[allow(dead_code)] // inverse of `label`; used by tests and future callers
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "repository_analysis" => Self::RepositoryAnalysis,
            "code_review" => Self::CodeReview,
            "implementation" => Self::Implementation,
            "testing" => Self::Testing,
            "verification" => Self::Verification,
            _ => return None,
        })
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

/// Which existing structured-result channels a parent should expect.
///
/// Flags over artefacts that already exist (`FindingRecord`,
/// `EvidenceLedger.verifications`, the child's `modified_files`). No new
/// result types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutputContract {
    /// `FindingRecord[]` via `report_finding` / ledger adoption.
    pub findings: bool,
    /// Host verification records on the child's / parent's ledger.
    pub verification: bool,
    /// Files the child mutated.
    pub changed_files: bool,
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
    /// `report_finding(blocking=true)` is honoured. Only the Reviewer.
    pub may_report_blocking: bool,
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
    pub description: String,
    pub purpose: String,
    pub capabilities: Vec<ChildCapability>,
    pub tool_policy: ToolPolicy,
    pub output_contract: OutputContract,
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

    pub fn may_report_blocking(&self) -> bool {
        self.runtime_policy.may_report_blocking
    }

    pub fn max_rounds(&self) -> Option<u32> {
        self.runtime_policy.max_rounds
    }

    pub fn serial_tools(&self) -> bool {
        self.runtime_policy.serial_tools
    }

    pub fn capability_labels(&self) -> Vec<String> {
        self.capabilities
            .iter()
            .map(|c| c.label().to_string())
            .collect()
    }

    /// `(profile_id, profile_role, capabilities)` for events and projections.
    pub fn trace_fields(&self) -> (String, String, Vec<String>) {
        (
            self.id.clone(),
            self.role.label().to_string(),
            self.capability_labels(),
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
            description: "Generic child: claims its own write scope after reading.".into(),
            purpose: "Carry out a self-contained subtask with the parent's full \
                      non-MCP toolset, bounded by late-bound exclusive ownership."
                .into(),
            capabilities: vec![ChildCapability::Implementation],
            tool_policy: ToolPolicy {
                access: ToolAccess::Inherit,
            },
            output_contract: OutputContract {
                findings: true,
                verification: false,
                changed_files: true,
            },
            workspace_policy: WorkspacePolicy {
                mode: WorkspaceMode::LateBound,
                requires_explicit_scope: false,
            },
            runtime_policy: RuntimePolicy {
                max_rounds: None,
                serial_tools: false,
                may_report_blocking: false,
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
            description: "Read-only repository understanding.".into(),
            purpose: "Architecture discovery, dependency analysis, and code \
                      exploration. Reports FindingRecords; cannot mutate."
                .into(),
            capabilities: vec![ChildCapability::RepositoryAnalysis],
            tool_policy: ToolPolicy {
                access: ToolAccess::ReadSearch,
            },
            output_contract: OutputContract {
                findings: true,
                verification: false,
                changed_files: false,
            },
            workspace_policy: WorkspacePolicy {
                mode: WorkspaceMode::ReadOnly,
                requires_explicit_scope: false,
            },
            runtime_policy: RuntimePolicy {
                max_rounds: None,
                serial_tools: false,
                may_report_blocking: false,
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
            description: "Independent verification of a change already made.".into(),
            purpose: "Review changes, detect regressions, inspect security. \
                      Reports FindingRecords and verification judgment; cannot mutate."
                .into(),
            capabilities: vec![ChildCapability::CodeReview, ChildCapability::Verification],
            tool_policy: ToolPolicy {
                access: ToolAccess::ReadTest,
            },
            output_contract: OutputContract {
                findings: true,
                verification: true,
                changed_files: false,
            },
            workspace_policy: WorkspacePolicy {
                mode: WorkspaceMode::ReadOnly,
                requires_explicit_scope: false,
            },
            runtime_policy: RuntimePolicy {
                max_rounds: Some(20),
                serial_tools: false,
                may_report_blocking: true,
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
            description: "Scoped implementation in an exclusive file set.".into(),
            purpose: "Modify code, run tests, fix issues — only within the \
                      files it exclusively owns."
                .into(),
            capabilities: vec![
                ChildCapability::Implementation,
                ChildCapability::Testing,
                ChildCapability::Verification,
            ],
            tool_policy: ToolPolicy {
                access: ToolAccess::WriteScoped,
            },
            output_contract: OutputContract {
                findings: true,
                verification: true,
                changed_files: true,
            },
            workspace_policy: WorkspacePolicy {
                mode: WorkspaceMode::ControlledMutation,
                requires_explicit_scope: true,
            },
            runtime_policy: RuntimePolicy {
                max_rounds: None,
                serial_tools: true,
                may_report_blocking: false,
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
        if self.purpose.trim().is_empty() {
            return Err("profile purpose must not be empty".into());
        }
        if self.capabilities.is_empty() {
            return Err("profile must declare at least one capability".into());
        }
        let output = &self.output_contract;
        if !output.findings && !output.verification && !output.changed_files {
            return Err("output_contract must name at least one existing artefact \
                        (findings, verification, changed_files)"
                .into());
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
        if self.role == AgentRole::Reviewer && !self.runtime_policy.may_report_blocking {
            return Err("reviewer profile must be allowed to raise blocking findings".into());
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
pub fn child_profile_trace(role: &str) -> (String, String, Vec<String>) {
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

    #[test]
    fn default_profile_is_the_historical_child() {
        let p = ChildProfile::default_profile();
        assert_eq!(p.id, "default");
        assert_eq!(p.role, AgentRole::Default);
        assert!(!p.read_only());
        assert!(!p.requires_scope());
        assert!(!p.may_report_blocking());
        assert_eq!(p.max_rounds(), None);
        assert!(!p.serial_tools());
        assert_eq!(p.tool_policy.access, ToolAccess::Inherit);
        assert_eq!(p.workspace_policy.mode, WorkspaceMode::LateBound);
        assert!(p.output_contract.findings);
        assert!(p.output_contract.changed_files);
        assert!(!p.output_contract.verification);
        assert_eq!(p.capabilities, vec![ChildCapability::Implementation]);
    }

    #[test]
    fn explorer_is_read_only_analysis() {
        let p = ChildProfile::explorer();
        assert_eq!(p.id, "explorer");
        assert!(p.read_only());
        assert!(!p.requires_scope());
        assert!(!p.may_report_blocking());
        assert_eq!(p.tool_policy.access, ToolAccess::ReadSearch);
        assert!(p.output_contract.findings);
        assert!(!p.output_contract.changed_files);
        assert_eq!(p.capabilities, vec![ChildCapability::RepositoryAnalysis]);
    }

    #[test]
    fn reviewer_is_bounded_read_only_judgment() {
        let p = ChildProfile::reviewer();
        assert_eq!(p.id, "reviewer");
        assert!(p.read_only());
        assert!(p.may_report_blocking());
        assert_eq!(p.max_rounds(), Some(20));
        assert_eq!(p.tool_policy.access, ToolAccess::ReadTest);
        assert!(p.output_contract.findings);
        assert!(p.output_contract.verification);
        assert!(!p.output_contract.changed_files);
        assert!(!p.serial_tools());
    }

    #[test]
    fn worker_is_scoped_serial_writer() {
        let p = ChildProfile::worker();
        assert_eq!(p.id, "worker");
        assert!(!p.read_only());
        assert!(p.requires_scope());
        assert!(p.serial_tools());
        assert!(!p.may_report_blocking());
        assert_eq!(p.tool_policy.access, ToolAccess::WriteScoped);
        assert_eq!(p.workspace_policy.mode, WorkspaceMode::ControlledMutation);
        assert!(p.output_contract.changed_files);
        assert!(p.output_contract.verification);
    }

    #[test]
    fn lookup_accepts_short_and_builtin_ids() {
        for id in ["explorer", "builtin.explorer", "EXPLORER"] {
            let p = ChildProfile::lookup(id).expect(id);
            assert_eq!(p.id, "explorer");
        }
        assert!(ChildProfile::lookup("not-a-profile").is_none());
        assert!(ChildProfile::lookup("").is_none());
    }

    #[test]
    fn resolve_matches_lookup_by_role_label() {
        for role in [
            AgentRole::Default,
            AgentRole::Explorer,
            AgentRole::Worker,
            AgentRole::Reviewer,
        ] {
            let a = ChildProfile::resolve(role);
            let b = ChildProfile::lookup(role.label()).unwrap();
            assert_eq!(a, b, "{role:?}");
        }
    }

    #[test]
    fn builtins_round_trip_through_json() {
        for p in ChildProfile::builtins() {
            let json = serde_json::to_string(&p).unwrap();
            let back: ChildProfile = serde_json::from_str(&json).unwrap();
            assert_eq!(back, p, "{}", p.id);
            assert!(json.contains(&format!("\"{}\"", p.id)), "{json}");
        }
    }

    #[test]
    fn capabilities_round_trip_between_parse_and_label() {
        for cap in [
            ChildCapability::RepositoryAnalysis,
            ChildCapability::CodeReview,
            ChildCapability::Implementation,
            ChildCapability::Testing,
            ChildCapability::Verification,
        ] {
            assert_eq!(ChildCapability::parse(cap.label()), Some(cap));
        }
        assert_eq!(ChildCapability::parse("vibe"), None);
    }

    #[test]
    fn validation_rejects_incoherent_contracts() {
        let mut p = ChildProfile::explorer();
        p.id.clear();
        assert!(p.validate().unwrap_err().contains("id"));

        let mut p = ChildProfile::explorer();
        p.tool_policy.access = ToolAccess::WriteScoped;
        assert!(p.validate().unwrap_err().contains("read-only"));

        let mut p = ChildProfile::worker();
        p.workspace_policy.requires_explicit_scope = false;
        assert!(p.validate().unwrap_err().contains("scope"));

        let mut p = ChildProfile::explorer();
        p.output_contract = OutputContract {
            findings: false,
            verification: false,
            changed_files: false,
        };
        assert!(p.validate().unwrap_err().contains("output_contract"));
    }

    #[test]
    fn model_parse_still_does_not_accept_reviewer() {
        assert_eq!(AgentRole::parse(Some("reviewer")), AgentRole::Default);
        assert_eq!(AgentRole::parse(None), AgentRole::Default);
        assert_eq!(AgentRole::parse(Some("explorer")), AgentRole::Explorer);
        assert_eq!(AgentRole::from_label("reviewer"), Some(AgentRole::Reviewer));
    }

    #[test]
    fn a_worker_without_a_scope_is_refused_not_unleashed() {
        let err = ChildProfile::admit(AgentRole::Worker, &[]).unwrap_err();
        assert!(
            err.contains("files"),
            "denial must name the missing scope: {err}"
        );
        assert!(ChildProfile::admit(AgentRole::Worker, &["src/a.rs".into()]).is_ok());
    }

    #[test]
    fn a_read_only_role_asking_for_write_scope_is_refused() {
        let err = ChildProfile::admit(AgentRole::Explorer, &["src/a.rs".into()]).unwrap_err();
        assert!(
            err.contains("read-only"),
            "denial must say why, not silently ignore the request: {err}"
        );
        assert!(ChildProfile::admit(AgentRole::Explorer, &[]).is_ok());
        assert!(ChildProfile::admit(AgentRole::Default, &[]).is_ok());
    }

    #[test]
    fn omitted_profile_and_role_maps_to_default() {
        let p = ChildProfile::admit_spawn(None, None, &[]).unwrap();
        assert_eq!(p.id, "default");
        assert_eq!(p.role, AgentRole::Default);
    }

    #[test]
    fn spawn_with_explorer_profile() {
        let p = ChildProfile::admit_spawn(Some("explorer"), None, &[]).unwrap();
        assert_eq!(p.id, "explorer");
        assert!(p.read_only());
        let p = ChildProfile::admit_spawn(Some("builtin.explorer"), None, &[]).unwrap();
        assert_eq!(p.id, "explorer");
    }

    #[test]
    fn spawn_with_worker_profile_requires_files() {
        let err = ChildProfile::admit_spawn(Some("worker"), None, &[]).unwrap_err();
        assert!(err.contains("files"), "{err}");
        let p = ChildProfile::admit_spawn(Some("worker"), None, &["src/a.rs".into()]).unwrap();
        assert_eq!(p.id, "worker");
        assert!(p.requires_scope());
    }

    #[test]
    fn spawn_with_reviewer_profile_is_harness_only() {
        let err = ChildProfile::admit_spawn(Some("reviewer"), None, &[]).unwrap_err();
        assert!(
            err.contains("harness"),
            "the model must not be able to request a reviewer: {err}"
        );
        assert!(ChildProfile::lookup("reviewer").is_some());
        assert!(ChildProfile::admit(AgentRole::Reviewer, &[]).is_ok());
    }

    #[test]
    fn unknown_profile_is_an_honest_denial() {
        let err = ChildProfile::admit_spawn(Some("marketplace.cool-agent"), None, &[]).unwrap_err();
        assert!(err.contains("Unknown profile"), "{err}");
        assert!(err.contains("explorer"), "{err}");
        assert!(
            !err.contains("Available: marketplace"),
            "must not invent a marketplace: {err}"
        );
    }

    #[test]
    fn conflicting_profile_and_role_is_refused() {
        let err = ChildProfile::admit_spawn(Some("explorer"), Some("worker"), &[]).unwrap_err();
        assert!(err.contains("Pick one"), "{err}");
        let p = ChildProfile::admit_spawn(Some("explorer"), Some("explorer"), &[]).unwrap();
        assert_eq!(p.id, "explorer");
    }

    #[test]
    fn historical_role_argument_still_selects_the_profile() {
        let p = ChildProfile::admit_spawn(None, Some("explorer"), &[]).unwrap();
        assert_eq!(p.id, "explorer");
        let p = ChildProfile::admit_spawn(None, Some("worker"), &["a.rs".into()]).unwrap();
        assert_eq!(p.id, "worker");
    }

    #[test]
    fn explorer_and_reviewer_are_structurally_read_only() {
        for role in [AgentRole::Explorer, AgentRole::Reviewer] {
            let p = ChildProfile::resolve(role);
            assert!(p.read_only(), "{role:?} must hold no write tools");
            assert!(!p.requires_scope());
            assert!(!p.serial_tools());
        }
    }

    #[test]
    fn only_the_reviewer_may_raise_blocking_findings() {
        for role in [AgentRole::Default, AgentRole::Explorer, AgentRole::Worker] {
            assert!(
                !ChildProfile::resolve(role).may_report_blocking(),
                "{role:?}"
            );
        }
        assert!(ChildProfile::resolve(AgentRole::Reviewer).may_report_blocking());
    }

    #[test]
    fn apply_to_registry_strips_writes_from_read_only_profiles() {
        let registry = leveler_tools::default_registry();
        for p in [ChildProfile::explorer(), ChildProfile::reviewer()] {
            let subset = p.apply_to_registry(&registry);
            for forbidden in [
                "apply_patch",
                "replace",
                "write_file",
                "create_file",
                "run_command",
                "shell_command",
            ] {
                assert!(
                    subset.get(forbidden).is_none(),
                    "{} must not advertise {forbidden}",
                    p.id
                );
            }
            assert!(
                subset.get("read_file").is_some(),
                "{} must keep read_file",
                p.id
            );
        }
        let worker = ChildProfile::worker().apply_to_registry(&registry);
        assert!(worker.get("apply_patch").is_some());
        assert!(
            worker
                .definitions()
                .iter()
                .all(|d| !d.name.starts_with("mcp__")),
            "a writer child must not hold MCP proxies"
        );
    }

    #[test]
    fn child_profile_trace_matches_the_builtin() {
        let (id, role, caps) = child_profile_trace("reviewer");
        assert_eq!(id, "reviewer");
        assert_eq!(role, "reviewer");
        assert!(caps.contains(&"code_review".to_string()));
        assert!(caps.contains(&"verification".to_string()));
        let (id, role, _) = child_profile_trace("nope");
        assert_eq!(id, "default");
        assert_eq!(role, "default");
    }
}
