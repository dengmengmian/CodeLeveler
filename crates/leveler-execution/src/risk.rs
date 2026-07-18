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

    /// Whether network is treated as freely allowed without a sandbox flag.
    pub fn network_unrestricted(self) -> bool {
        matches!(self, Self::FullAccess)
    }
}

impl std::fmt::Display for PermissionProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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
