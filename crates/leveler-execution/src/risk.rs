//! Risk classification and permission profiles.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How dangerous a tool action is. The permission layer gates on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Read-only, no side effects.
    Safe,
    /// Writes inside the workspace.
    WorkspaceWrite,
    /// Requires network access.
    Network,
    /// Potentially destructive (deletes, resets).
    Destructive,
    /// Requires elevated privileges.
    Privileged,
}

/// The one write boundary every execution runs under.
///
/// Reads are not modeled here on purpose: the target design lets any tool
/// read any path, and confines only what may be *changed*. The three-tier
/// [`PermissionProfile`] is a preset over this (see
/// [`PermissionProfile::write_scope`]); a late-bound child before
/// `claim_write_scope` is [`WriteScope::None`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteScope {
    /// Nothing may be written — the workspace is mounted read-only at the OS
    /// boundary (scratch and toolchain caches stay writable so builds work).
    None,
    /// Writes confined to this root plus the standard temp / cache dirs.
    Workspace { root: PathBuf },
    /// Writes confined to this root, including its Git metadata.
    WorkspaceWithGit { root: PathBuf },
    /// An execution owns only these workspace paths. Exclusions remain read-only.
    ScopedWorkspace {
        root: PathBuf,
        allowed: Vec<PathBuf>,
        excluded: Vec<PathBuf>,
    },
    /// No write confinement (完全访问, or an approved elevation).
    Unrestricted,
}

impl WriteScope {
    /// Whether an OS write fence is in force at all.
    pub fn confines(&self) -> bool {
        !matches!(self, Self::Unrestricted)
    }

    /// The writable root, when there is one.
    pub fn root(&self) -> Option<&Path> {
        match self {
            Self::Workspace { root }
            | Self::WorkspaceWithGit { root }
            | Self::ScopedWorkspace { root, .. } => Some(root),
            Self::None | Self::Unrestricted => None,
        }
    }
}

/// User-facing three-tier permission profile.
///
/// | Profile | Filesystem | Approval |
/// |---------|------------|----------|
/// | [`RequestApproval`] | read broad, write confined | network and high-risk actions are usually prompted |
/// | [`Assisted`] | read broad, write confined | irreversible or boundary-crossing actions are prompted |
/// | [`FullAccess`] | unrestricted | no permission prompts or sandbox restrictions |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionProfile {
    /// 请求批准 — ask when touching external paths or the network.
    RequestApproval,
    /// 替我审批 — default daily profile.
    #[default]
    Assisted,
    /// 完全访问 — unrestricted execution.
    FullAccess,
}

impl PermissionProfile {
    /// Wire / CLI / session persistence value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RequestApproval => "request_approval",
            Self::Assisted => "assisted",
            Self::FullAccess => "full_access",
        }
    }

    /// Parse a stored or CLI wire value.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "request_approval" | "request-approval" => Some(Self::RequestApproval),
            "assisted" => Some(Self::Assisted),
            "full_access" | "full-access" => Some(Self::FullAccess),
            // Legacy names from migration 0003 (mapped in 0012 for existing rows,
            // but the column DEFAULT still produces them for new rows).
            "plan" => Some(Self::RequestApproval),
            "workspace_write" => Some(Self::Assisted),
            _ => None,
        }
    }

    /// Whether this profile permits a tool of the given risk without blocking
    /// it at the registry (approval may still be required).
    pub fn permits(self, risk: RiskLevel) -> bool {
        match self {
            // Restricted can refuse inherently privileged tools. Auto routes
            // high-risk tools to approval instead of hard denial.
            Self::RequestApproval => {
                !matches!(risk, RiskLevel::Privileged | RiskLevel::Destructive)
            }
            Self::Assisted | Self::FullAccess => true,
        }
    }

    /// OS write confinement and absolute-path preflight for `run_command`.
    pub fn confines_workspace(self) -> bool {
        !matches!(self, Self::FullAccess)
    }

    /// Whether commands under this profile reach the network without a grant.
    /// The network half of the profile, beside [`Self::write_scope`]: 请求批准
    /// runs commands with the network denied by the OS sandbox until the user
    /// grants it for a call or the session. Approval decides; the sandbox
    /// enforces.
    pub fn network_by_default(self) -> bool {
        !matches!(self, Self::RequestApproval)
    }

    /// The profile as a preset over [`WriteScope`]. Same answer as
    /// [`Self::confines_workspace`], spelled as the boundary itself.
    pub fn write_scope(self, workspace_root: &Path) -> WriteScope {
        if self.confines_workspace() {
            WriteScope::Workspace {
                root: workspace_root.to_path_buf(),
            }
        } else {
            WriteScope::Unrestricted
        }
    }
}

impl std::fmt::Display for PermissionProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The permission contract a session was created under.
///
/// The tier names did not change between 1.x and 2.0, but their meanings did:
/// v1 `request_approval` was the strict profile, v1 `assisted` was the old
/// Auto, and in v2 `request_approval` inherits the old Auto while `assisted`
/// becomes the effect-oriented Auto. A stored `mode` alone therefore cannot be
/// interpreted without this version — doing so would silently widen an old
/// session on resume.
///
/// [`Default`] is V1 on purpose: a persisted row or a wire message that does
/// not carry the field is legacy data and must never be read as v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionContract {
    #[default]
    V1,
    V2,
}

impl PermissionContract {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "v1" | "1" => Some(Self::V1),
            "v2" | "2" => Some(Self::V2),
            _ => None,
        }
    }

    const fn code(self) -> u16 {
        match self {
            Self::V1 => 0,
            Self::V2 => 1,
        }
    }

    const fn from_code(code: u16) -> Self {
        match code {
            1 => Self::V2,
            // Any unreadable contract is the legacy one: a corrupt read can
            // make a session stricter, never more autonomous.
            _ => Self::V1,
        }
    }
}

/// The effective authorization policy for one `(contract, profile)` pair.
///
/// Four implementations cover the six stored combinations; the execution path
/// stays one chain. See `docs/dev/permission-model-v2/01-current-and-target-model.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyKind {
    /// v1 request_approval: the strict pre-V2 profile.
    LegacyRestricted,
    /// The old Auto: workspace write sandbox, dangerous-command/MCP/network asks.
    LegacyAuto,
    /// V2 Auto: only directly identifiable irreversible effects ask.
    EffectAuto,
    /// Unchanged Full contract.
    Full,
}

impl PolicyKind {
    pub const fn of(contract: PermissionContract, profile: PermissionProfile) -> Self {
        match (contract, profile) {
            (_, PermissionProfile::FullAccess) => Self::Full,
            (PermissionContract::V2, PermissionProfile::RequestApproval) => Self::LegacyAuto,
            (PermissionContract::V2, PermissionProfile::Assisted) => Self::EffectAuto,
            (PermissionContract::V1, PermissionProfile::Assisted) => Self::LegacyAuto,
            (PermissionContract::V1, PermissionProfile::RequestApproval) => Self::LegacyRestricted,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LegacyRestricted => "legacy-restricted",
            Self::LegacyAuto => "legacy-auto",
            Self::EffectAuto => "effect-auto",
            Self::Full => "full-access",
        }
    }

    /// Whether the registry admits a tool of this declared risk. Only the
    /// strict v1 profile refuses a class outright; the others route it to
    /// admission.
    pub const fn permits(self, risk: RiskLevel) -> bool {
        match self {
            Self::LegacyRestricted => {
                !matches!(risk, RiskLevel::Privileged | RiskLevel::Destructive)
            }
            Self::LegacyAuto | Self::EffectAuto | Self::Full => true,
        }
    }

    /// The profile preset over [`WriteScope`] at admission.
    ///
    /// Ownership (`ScopedWorkspace`/`None`) is applied before this preset in
    /// `ToolPolicy::write_scope`, so a delegated child's claim still narrows
    /// even an effect-oriented Auto.
    pub fn write_scope(self, workspace_root: &Path) -> WriteScope {
        match self {
            Self::LegacyRestricted | Self::LegacyAuto => WriteScope::Workspace {
                root: workspace_root.to_path_buf(),
            },
            Self::EffectAuto | Self::Full => WriteScope::Unrestricted,
        }
    }

    /// Whether commands reach the network without a call/session grant.
    pub const fn network_by_default(self) -> bool {
        !matches!(self, Self::LegacyRestricted)
    }

    /// Whether an OS write fence is in force for this policy preset.
    pub const fn confines_workspace(self) -> bool {
        !matches!(self, Self::EffectAuto | Self::Full)
    }
}

impl std::fmt::Display for PolicyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The complete live permission identity of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionState {
    pub contract: PermissionContract,
    pub profile: PermissionProfile,
}

impl PermissionState {
    pub const fn new(contract: PermissionContract, profile: PermissionProfile) -> Self {
        Self { contract, profile }
    }

    pub const fn policy_kind(self) -> PolicyKind {
        PolicyKind::of(self.contract, self.profile)
    }
}

impl PermissionProfile {
    /// Stable in-memory code for [`SharedPermissionProfile`]. Not a wire
    /// value — `as_str` owns persistence, and nothing outside this module
    /// may depend on these numbers.
    const fn code(self) -> u16 {
        match self {
            Self::RequestApproval => 0,
            Self::Assisted => 1,
            Self::FullAccess => 2,
        }
    }

    /// Inverse of [`Self::code`]. An unrecognised code cannot come from
    /// `code` — but if one ever did, the answer is the STRICTEST profile, so
    /// a corrupt read can never widen what an agent may do.
    const fn from_code(code: u16) -> Self {
        match code {
            1 => Self::Assisted,
            2 => Self::FullAccess,
            _ => Self::RequestApproval,
        }
    }
}

/// The permission identity a task is running under, shared live by everything
/// executing for it.
///
/// A turn used to copy the profile at startup, so a user who switched to
/// 完全访问 while a turn was running kept being prompted by that turn and by
/// every agent it had already delegated to: the UI and the session row said
/// one thing and the authorization path used another. The value belongs to
/// the session, and each execution context holds a REFERENCE to it —
/// `ToolContext` is cloned into every child, so one cell serves the whole
/// agent tree with no update to fan out.
///
/// Contract and profile share one atomic word: a reader must never observe a
/// v1 contract with a v2 profile (or the reverse) while a session adopts V2.
#[derive(Debug, Clone)]
pub struct SharedPermissionProfile(std::sync::Arc<std::sync::atomic::AtomicU16>);

impl SharedPermissionProfile {
    /// A context that does not know its session contract. It is the LEGACY
    /// contract: an unknown caller must not silently acquire v2 autonomy.
    /// Session construction uses [`Self::new_with_contract`].
    pub fn new(profile: PermissionProfile) -> Self {
        Self::new_with_contract(PermissionContract::V1, profile)
    }

    pub fn new_with_contract(contract: PermissionContract, profile: PermissionProfile) -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU16::new(
            Self::code(contract, profile),
        )))
    }

    const fn code(contract: PermissionContract, profile: PermissionProfile) -> u16 {
        (contract.code() << 8) | profile.code()
    }

    fn decode(code: u16) -> PermissionState {
        PermissionState {
            contract: PermissionContract::from_code(code >> 8),
            profile: PermissionProfile::from_code(code & 0xff),
        }
    }

    /// The profile in force right now. Every authorization decision reads
    /// this, so a change lands on the NEXT decision — never retroactively on
    /// work already authorized and running.
    pub fn get(&self) -> PermissionProfile {
        self.state().profile
    }

    /// The complete identity in force right now.
    pub fn state(&self) -> PermissionState {
        Self::decode(self.0.load(std::sync::atomic::Ordering::Acquire))
    }

    /// The effective policy kind in force right now.
    pub fn policy_kind(&self) -> PolicyKind {
        self.state().policy_kind()
    }

    /// Change only the tier; the contract stays what it was.
    pub fn set(&self, profile: PermissionProfile) {
        let contract = self.state().contract;
        self.set_state(PermissionState::new(contract, profile));
    }

    /// Change contract and tier as one atomic fact.
    pub fn set_state(&self, state: PermissionState) {
        self.0.store(
            Self::code(state.contract, state.profile),
            std::sync::atomic::Ordering::Release,
        );
    }

    /// Whether two handles are the same cell — the property that makes a
    /// child observe its parent's changes.
    pub fn is_same_cell(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Default for SharedPermissionProfile {
    fn default() -> Self {
        Self::new(PermissionProfile::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The profile states the network default beside the write boundary:
    /// request-approval denies until the user grants it; the others allow.
    #[test]
    fn each_profile_states_its_network_default() {
        assert!(!PermissionProfile::RequestApproval.network_by_default());
        assert!(PermissionProfile::Assisted.network_by_default());
        assert!(PermissionProfile::FullAccess.network_by_default());
    }

    #[test]
    fn assisted_routes_destructive_tools_to_approval_instead_of_registry_denial() {
        assert!(PermissionProfile::Assisted.permits(RiskLevel::WorkspaceWrite));
        assert!(PermissionProfile::Assisted.permits(RiskLevel::Destructive));
    }

    #[test]
    fn restricted_registry_can_refuse_privileged_tools() {
        assert!(PermissionProfile::RequestApproval.permits(RiskLevel::Safe));
        assert!(PermissionProfile::RequestApproval.permits(RiskLevel::Network));
        assert!(!PermissionProfile::RequestApproval.permits(RiskLevel::Privileged));
    }

    /// The code mapping is only ever written by `code`, but a value that
    /// slipped through must land on the strictest profile — never on 完全访问.
    #[test]
    fn every_profile_round_trips_and_an_unknown_code_is_the_strictest() {
        for profile in [
            PermissionProfile::RequestApproval,
            PermissionProfile::Assisted,
            PermissionProfile::FullAccess,
        ] {
            assert_eq!(PermissionProfile::from_code(profile.code()), profile);
        }
        for code in [3u16, 7, 255] {
            assert_eq!(
                PermissionProfile::from_code(code),
                PermissionProfile::RequestApproval,
                "an unreadable profile must never widen authority"
            );
        }
    }

    /// One cell, many holders: this is what lets an already-delegated child
    /// see a change its parent never told it about.
    #[test]
    fn a_cloned_handle_observes_a_change_made_through_the_original() {
        let shared = SharedPermissionProfile::new(PermissionProfile::Assisted);
        let clone = shared.clone();
        assert!(shared.is_same_cell(&clone));

        shared.set(PermissionProfile::FullAccess);
        assert_eq!(clone.get(), PermissionProfile::FullAccess);

        // …and tightening is just as live as widening.
        clone.set(PermissionProfile::Assisted);
        assert_eq!(shared.get(), PermissionProfile::Assisted);
    }

    /// A separately constructed cell is a separate session.
    #[test]
    fn separate_cells_do_not_leak_into_each_other() {
        let a = SharedPermissionProfile::new(PermissionProfile::Assisted);
        let b = SharedPermissionProfile::new(PermissionProfile::Assisted);
        assert!(!a.is_same_cell(&b));
        a.set(PermissionProfile::FullAccess);
        assert_eq!(b.get(), PermissionProfile::Assisted);
    }

    #[test]
    fn full_access_permits_all() {
        assert!(PermissionProfile::FullAccess.permits(RiskLevel::Privileged));
        assert!(!PermissionProfile::FullAccess.confines_workspace());
    }

    #[test]
    fn parse_wire_values() {
        assert_eq!(
            PermissionProfile::parse("request-approval"),
            Some(PermissionProfile::RequestApproval)
        );
        assert_eq!(
            PermissionProfile::parse("assisted"),
            Some(PermissionProfile::Assisted)
        );
        assert_eq!(
            PermissionProfile::parse("full_access"),
            Some(PermissionProfile::FullAccess)
        );
        assert_eq!(
            PermissionProfile::parse("plan"),
            Some(PermissionProfile::RequestApproval)
        );
        assert_eq!(
            PermissionProfile::parse("workspace_write"),
            Some(PermissionProfile::Assisted)
        );
        assert_eq!(PermissionProfile::parse("yolo"), None);
    }

    /// The six stored combinations collapse to four policy implementations.
    /// This table is the compatibility contract; a new row here is a product
    /// change, not a refactor.
    #[test]
    fn policy_kind_matrix_is_the_compatibility_contract() {
        use PermissionContract::{V1, V2};
        use PermissionProfile::{Assisted, FullAccess, RequestApproval};
        use PolicyKind::{EffectAuto, Full, LegacyAuto, LegacyRestricted};
        for (contract, profile, expected) in [
            (V1, RequestApproval, LegacyRestricted),
            (V1, Assisted, LegacyAuto),
            (V1, FullAccess, Full),
            (V2, RequestApproval, LegacyAuto),
            (V2, Assisted, EffectAuto),
            (V2, FullAccess, Full),
        ] {
            assert_eq!(
                PolicyKind::of(contract, profile),
                expected,
                "{contract:?} + {profile:?}"
            );
        }
    }

    /// An absent persisted contract is legacy data. This is the property that
    /// stops an old session from being read as v2 on upgrade.
    #[test]
    fn an_absent_contract_is_v1() {
        #[derive(serde::Deserialize)]
        struct Persisted {
            #[serde(default)]
            contract: PermissionContract,
        }
        assert_eq!(PermissionContract::default(), PermissionContract::V1);
        let stored: Persisted = serde_json::from_str("{}").unwrap();
        assert_eq!(stored.contract, PermissionContract::V1);
        assert_eq!(
            serde_json::from_str::<PermissionContract>("\"v2\"").unwrap(),
            PermissionContract::V2
        );
        assert_eq!(PermissionContract::parse("2"), Some(PermissionContract::V2));
    }

    #[test]
    fn legacy_restricted_is_the_only_policy_that_refuses_a_risk_class() {
        assert!(!PolicyKind::LegacyRestricted.permits(RiskLevel::Destructive));
        assert!(!PolicyKind::LegacyRestricted.permits(RiskLevel::Privileged));
        for kind in [
            PolicyKind::LegacyAuto,
            PolicyKind::EffectAuto,
            PolicyKind::Full,
        ] {
            assert!(kind.permits(RiskLevel::Destructive), "{kind:?}");
            assert!(kind.permits(RiskLevel::Privileged), "{kind:?}");
        }
    }

    #[test]
    fn the_first_tier_keeps_the_old_auto_write_boundary() {
        let root = Path::new("/ws");
        assert_eq!(
            PolicyKind::of(PermissionContract::V2, PermissionProfile::RequestApproval)
                .write_scope(root),
            WriteScope::Workspace {
                root: root.to_path_buf()
            }
        );
        assert_eq!(
            PolicyKind::EffectAuto.write_scope(root),
            WriteScope::Unrestricted
        );
    }

    /// A tier switch must not adopt v2. The two fields live in one atomic
    /// word, so a concurrent reader never sees a mixed pair.
    #[test]
    fn changing_the_tier_preserves_the_contract() {
        let shared = SharedPermissionProfile::new_with_contract(
            PermissionContract::V1,
            PermissionProfile::Assisted,
        );
        shared.set(PermissionProfile::RequestApproval);
        assert_eq!(
            shared.state(),
            PermissionState::new(PermissionContract::V1, PermissionProfile::RequestApproval)
        );
        assert_eq!(shared.policy_kind(), PolicyKind::LegacyRestricted);

        shared.set_state(PermissionState::new(
            PermissionContract::V2,
            PermissionProfile::RequestApproval,
        ));
        assert_eq!(shared.policy_kind(), PolicyKind::LegacyAuto);
    }
}

#[cfg(test)]
mod write_scope_tests {
    use super::*;
    use std::path::Path;

    /// PR 1. The three profiles become presets over one write boundary. The
    /// table is the contract: nothing here may change what a profile can
    /// write today, only where that fact is spelled.
    #[test]
    fn profiles_map_onto_write_scope_presets() {
        let root = Path::new("/ws");
        assert_eq!(
            PermissionProfile::RequestApproval.write_scope(root),
            WriteScope::Workspace {
                root: root.to_path_buf()
            },
            "request-approval keeps writing the workspace"
        );
        assert_eq!(
            PermissionProfile::Assisted.write_scope(root),
            WriteScope::Workspace {
                root: root.to_path_buf()
            }
        );
        assert_eq!(
            PermissionProfile::FullAccess.write_scope(root),
            WriteScope::Unrestricted
        );
    }

    /// The preset must agree with the predicate it is replacing, for every
    /// profile, or the adapter changes behavior on the way in.
    #[test]
    fn write_scope_agrees_with_confines_workspace() {
        let root = Path::new("/ws");
        for profile in [
            PermissionProfile::RequestApproval,
            PermissionProfile::Assisted,
            PermissionProfile::FullAccess,
        ] {
            assert_eq!(
                profile.write_scope(root).confines(),
                profile.confines_workspace(),
                "{profile:?}"
            );
        }
    }

    #[test]
    fn write_scope_predicates() {
        assert!(!WriteScope::Unrestricted.confines());
        assert!(WriteScope::Workspace { root: "/ws".into() }.confines());
        assert!(WriteScope::None.confines());
        assert_eq!(
            WriteScope::Workspace { root: "/ws".into() }.root(),
            Some(Path::new("/ws"))
        );
        assert_eq!(WriteScope::None.root(), None);
        assert_eq!(WriteScope::Unrestricted.root(), None);
    }
}
