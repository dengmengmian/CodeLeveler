//! Risk classification and permission profiles.

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

/// User-facing three-tier permission profile.
///
/// | Profile | Filesystem | Approval |
/// |---------|------------|----------|
/// | [`RequestApproval`] | read broad, write confined | network and high-risk actions are usually prompted |
/// | [`Assisted`] | read broad, write confined | irreversible or boundary-crossing actions are prompted |
/// | [`FullAccess`] | unrestricted | prompts are exceptional (memory remains protected) |
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
            // Confined profiles: no destructive / privileged tools at all.
            Self::RequestApproval | Self::Assisted => {
                !matches!(risk, RiskLevel::Privileged | RiskLevel::Destructive)
            }
            Self::FullAccess => true,
        }
    }

    /// OS write confinement and absolute-path preflight for `run_command`.
    pub fn confines_workspace(self) -> bool {
        !matches!(self, Self::FullAccess)
    }
}

impl std::fmt::Display for PermissionProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PermissionProfile {
    /// Stable in-memory code for [`SharedPermissionProfile`]. Not a wire
    /// value — `as_str` owns persistence, and nothing outside this module
    /// may depend on these numbers.
    const fn code(self) -> u8 {
        match self {
            Self::RequestApproval => 0,
            Self::Assisted => 1,
            Self::FullAccess => 2,
        }
    }

    /// Inverse of [`Self::code`]. An unrecognised code cannot come from
    /// `code` — but if one ever did, the answer is the STRICTEST profile, so
    /// a corrupt read can never widen what an agent may do.
    const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Assisted,
            2 => Self::FullAccess,
            _ => Self::RequestApproval,
        }
    }
}

/// The permission profile a task is running under, shared live by everything
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
/// Reads and writes are infallible (no lock to poison) and cheap enough to
/// sit on every authorization decision.
#[derive(Debug, Clone)]
pub struct SharedPermissionProfile(std::sync::Arc<std::sync::atomic::AtomicU8>);

impl SharedPermissionProfile {
    pub fn new(profile: PermissionProfile) -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            profile.code(),
        )))
    }

    /// The profile in force right now. Every authorization decision reads
    /// this, so a change lands on the NEXT decision — never retroactively on
    /// work already authorized and running.
    pub fn get(&self) -> PermissionProfile {
        PermissionProfile::from_code(self.0.load(std::sync::atomic::Ordering::Acquire))
    }

    pub fn set(&self, profile: PermissionProfile) {
        self.0
            .store(profile.code(), std::sync::atomic::Ordering::Release);
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

    #[test]
    fn assisted_blocks_destructive_at_registry() {
        assert!(PermissionProfile::Assisted.permits(RiskLevel::WorkspaceWrite));
        assert!(!PermissionProfile::Assisted.permits(RiskLevel::Destructive));
    }

    #[test]
    fn request_approval_same_registry_gate_as_assisted() {
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
        for code in [3u8, 7, 255] {
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
}
