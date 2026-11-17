//! Per-call execution policy (PR 5).
//!
//! Authorization happens in exactly one place — the ToolHost's admission —
//! and produces one of three things: an immutable
//! [`ResolvedExecutionPolicy`] the call executes under, a decision still to
//! be asked for, or a denial. Nothing downstream reads the live profile or a
//! turn-scoped flag: the tool sees the frozen policy, so a profile switch or a
//! grant made after admission lands on the NEXT call, never this one.

use crate::NetworkScope;
use crate::approval::ApprovalRequest;
use crate::risk::{PermissionProfile, WriteScope};
use leveler_core::{Capability, GrantRequest, GrantScope, ResourceIdentity};

/// Why this call is allowed to run. Recorded on the admitted call so a reader
/// can tell a profile auto-allow from a human decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationEvidence {
    /// The profile policy allowed it without asking.
    Policy { profile: PermissionProfile },
    /// A standing permission rule matched.
    Rule,
    /// The auto-reviewer allowed it.
    Reviewer,
    /// The user approved this one call.
    ApprovedOnce,
    /// An earlier "approve for the session" decision covered it.
    SessionGrant { signature: String },
    /// The user approved it and asked for a standing rule.
    ApprovedAlways,
    /// Exact resource bindings approved for reuse; never global authority.
    ResourceGrant {
        request: GrantRequest,
        scope: GrantScope,
    },
}

/// The immutable policy an admitted call executes under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedExecutionPolicy {
    /// The profile, a matching rule, an automated reviewer and a reusable
    /// resource grant retain their execution boundaries. Full and consent to
    /// the entire exact call lift the workspace write fence.
    pub write: WriteScope,
    /// The destination boundary decided at admission. An approval may widen a
    /// profile default (the prompt carries that decision) but never clears an
    /// explicit `--deny-network`.
    pub network_scope: NetworkScope,
    pub authorization: AuthorizationEvidence,
}

impl ResolvedExecutionPolicy {
    /// Whether this admitted call carries the user's full-execution authority.
    ///
    /// Only the Full profile and a decision on the exact call do. A standing
    /// rule and an automated reviewer remove the question, but they are not a
    /// grant of the filesystem or the network: their call keeps the boundary
    /// it was resolved under.
    pub fn unrestricted_execution(&self) -> bool {
        match self.authorization {
            AuthorizationEvidence::ResourceGrant { .. } => false,
            AuthorizationEvidence::Policy { profile } => profile == PermissionProfile::FullAccess,
            AuthorizationEvidence::Rule | AuthorizationEvidence::Reviewer => false,
            AuthorizationEvidence::ApprovedOnce
            | AuthorizationEvidence::SessionGrant { .. }
            | AuthorizationEvidence::ApprovedAlways => true,
        }
    }

    /// Whether this call runs with the Full profile's no-confinement contract.
    ///
    /// Exact-call consent is deliberately NOT this: it may lift the write fence
    /// for the approved effect, but the process still runs under the frozen
    /// network boundary. The OS backend uses this to decide whether to skip its
    /// wrapper entirely. A frozen `None` is an explicit run-level network
    /// denial, so even Full needs the network-only wrapper in that case.
    pub fn full_execution(&self) -> bool {
        matches!(
            self.authorization,
            AuthorizationEvidence::Policy {
                profile: PermissionProfile::FullAccess
            }
        ) && matches!(self.network_scope, NetworkScope::Internet)
    }

    /// Frozen bindings matched by admission for this action only.
    pub fn resource_grant(&self) -> Option<&GrantRequest> {
        match &self.authorization {
            AuthorizationEvidence::ResourceGrant { request, .. } => Some(request),
            _ => None,
        }
    }

    /// Check one exact capability and current host-resolved identity.
    pub fn allows_resource(&self, capability: Capability, resource: &ResourceIdentity) -> bool {
        self.resource_grant().is_some_and(|request| {
            request
                .bindings
                .iter()
                .any(|binding| binding.capability == capability && &binding.resource == resource)
        })
    }

    pub fn new(
        write: WriteScope,
        network_scope: NetworkScope,
        authorization: AuthorizationEvidence,
    ) -> Self {
        let mut policy = Self {
            write,
            network_scope,
            authorization,
        };
        if policy.unrestricted_execution() {
            // Exact-call consent lets the approved effect actually happen,
            // including a target outside the workspace; Full does the same by
            // contract. The destination scope is separate and is never
            // rewritten here: an explicit network denial stays denied, and a
            // profile-default network grant was already put on the approval.
            policy.write = WriteScope::Unrestricted;
        }
        policy
    }
}

/// A decision still to be made: the request to put to the reviewer / human,
/// and what the call runs under if they allow it.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request: ApprovalRequest,
    /// Stable "approve for the session" key for this action.
    pub signature: String,
    pub write: WriteScope,
    pub network_scope: NetworkScope,
    /// The rendered command line, for a durable "always" rule.
    pub command_line: Option<String>,
    /// Paths the call touches, for a durable "always" rule.
    pub scoped_paths: Vec<String>,
    /// Whether approving this call lifts the workspace write fence.
    ///
    /// Only an action that can itself write (a command, a file mutation, an
    /// unsandboxed MCP server) needs the fence lifted. Approving a network
    /// fetch, a native action or a definition write is consent to that exact
    /// action, not a grant of arbitrary filesystem writes. When false the
    /// admitted call keeps the scope it was resolved under even though the
    /// authorization evidence is `ApprovedOnce`.
    pub widens_write: bool,
}

impl PendingApproval {
    /// The policy this call runs under once `evidence` says it may.
    pub fn allowed(&self, authorization: AuthorizationEvidence) -> ResolvedExecutionPolicy {
        let mut policy = ResolvedExecutionPolicy::new(
            self.write.clone(),
            self.network_scope.clone(),
            authorization,
        );
        if !self.widens_write {
            // Consent to this call is not consent to the filesystem. Keep the
            // scope admission already resolved.
            policy.write = self.write.clone();
        }
        policy
    }
}

/// A refusal decided by policy, before anyone was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDenial {
    pub reason: String,
}

/// The only three outcomes of resolving a call's policy.
#[derive(Debug)]
pub enum PolicyResolution {
    Allow(ResolvedExecutionPolicy),
    /// Boxed: the pending request carries the full approval prompt.
    Ask(Box<PendingApproval>),
    Deny(PolicyDenial),
}

#[cfg(test)]
mod resource_grant_tests {
    use super::*;
    use leveler_core::GrantBinding;
    #[test]
    fn resource_consent_preserves_execution_boundaries() {
        let resource = ResourceIdentity::Repository {
            identity: "repo".into(),
        };
        let request = GrantRequest {
            project_identity: "p".into(),
            bindings: vec![GrantBinding {
                capability: Capability::RepositoryRead,
                resource: resource.clone(),
            }],
        };
        for scope in [GrantScope::Session, GrantScope::Project] {
            let policy = ResolvedExecutionPolicy::new(
                WriteScope::None,
                NetworkScope::Loopback,
                AuthorizationEvidence::ResourceGrant {
                    request: request.clone(),
                    scope,
                },
            );
            assert!(!policy.unrestricted_execution());
            assert_eq!(policy.write, WriteScope::None);
            assert_eq!(policy.network_scope, NetworkScope::Loopback);
            assert!(policy.allows_resource(Capability::RepositoryRead, &resource));
            assert!(!policy.allows_resource(Capability::RepositoryMutate, &resource));
            assert!(!policy.allows_resource(
                Capability::RepositoryRead,
                &ResourceIdentity::Repository {
                    identity: "other".into()
                }
            ));
        }
    }

    fn workspace() -> WriteScope {
        WriteScope::Workspace {
            root: std::path::PathBuf::from("/repo"),
        }
    }

    /// A standing rule removes the question; it does not widen the resource
    /// boundary the call was resolved under. The old mapping made every
    /// matching `Allow` an unrestricted filesystem and network grant.
    #[test]
    fn a_standing_rule_preserves_the_resolved_resource_boundaries() {
        let policy = ResolvedExecutionPolicy::new(
            workspace(),
            NetworkScope::Loopback,
            AuthorizationEvidence::Rule,
        );
        assert!(!policy.unrestricted_execution());
        assert_eq!(policy.write, workspace());
        assert_eq!(policy.network_scope, NetworkScope::Loopback);
    }

    /// An automated reviewer is not the person. It may skip the prompt, but it
    /// must not turn a confined call into an unrestricted one.
    #[test]
    fn an_automated_reviewer_preserves_the_resolved_resource_boundaries() {
        let policy = ResolvedExecutionPolicy::new(
            workspace(),
            NetworkScope::None,
            AuthorizationEvidence::Reviewer,
        );
        assert!(!policy.unrestricted_execution());
        assert_eq!(policy.write, workspace());
        assert_eq!(policy.network_scope, NetworkScope::None);
    }

    /// Exact-call consent executes the action the person saw, including an
    /// external write target. It must not also lift an explicit network denial
    /// the run owner set.
    #[test]
    fn exact_call_consent_keeps_an_explicit_network_denial() {
        for authorization in [
            AuthorizationEvidence::ApprovedOnce,
            AuthorizationEvidence::SessionGrant {
                signature: "s".into(),
            },
            AuthorizationEvidence::ApprovedAlways,
        ] {
            let policy =
                ResolvedExecutionPolicy::new(workspace(), NetworkScope::None, authorization);
            assert_eq!(policy.network_scope, NetworkScope::None, "{policy:?}");
            assert_eq!(policy.write, WriteScope::Unrestricted, "{policy:?}");
        }
    }

    /// A profile-default destination scope is not a denial: the approval prompt
    /// already decided whether this call may reach the Internet.
    #[test]
    fn exact_call_consent_keeps_an_approved_network_destination() {
        let policy = ResolvedExecutionPolicy::new(
            workspace(),
            NetworkScope::Internet,
            AuthorizationEvidence::ApprovedOnce,
        );
        assert_eq!(policy.network_scope, NetworkScope::Internet);
    }

    fn pending(widens_write: bool) -> PendingApproval {
        PendingApproval {
            request: ApprovalRequest {
                id: leveler_core::ApprovalId::new("approval-a"),
                turn_id: None,
                call_id: "call-a".into(),
                agent_id: None,
                action_fingerprint: "fp".into(),
                tool: "web_fetch".into(),
                risk: crate::risk::RiskLevel::Network,
                description: String::new(),
                command: None,
                paths: Vec::new(),
                grant: None,
                write_scope_expanded: widens_write,
            },
            signature: "sig".into(),
            write: workspace(),
            network_scope: NetworkScope::Loopback,
            command_line: None,
            scoped_paths: Vec::new(),
            widens_write,
        }
    }

    /// Consent to a network fetch is not consent to the filesystem. Only an
    /// action that can itself write outside the workspace needs the fence
    /// lifted; everything else keeps the scope admission resolved.
    #[test]
    fn exact_call_consent_lifts_the_write_fence_only_for_a_writing_action() {
        let read_only = pending(false).allowed(AuthorizationEvidence::ApprovedOnce);
        assert_eq!(read_only.write, workspace());
        assert_eq!(read_only.network_scope, NetworkScope::Loopback);

        let writer = pending(true).allowed(AuthorizationEvidence::ApprovedOnce);
        assert_eq!(writer.write, WriteScope::Unrestricted);
    }

    /// Full skips the wrapper only when it is also allowed to reach the
    /// network. A frozen `None` is the user's explicit run-level denial: the
    /// process still needs the network-only confinement wrapper, so the OS
    /// backend must not treat this call as "no confinement".
    #[test]
    fn full_execution_requires_an_open_network_destination() {
        let open = ResolvedExecutionPolicy::new(
            WriteScope::Unrestricted,
            NetworkScope::Internet,
            AuthorizationEvidence::Policy {
                profile: PermissionProfile::FullAccess,
            },
        );
        assert!(open.full_execution());

        let denied = ResolvedExecutionPolicy::new(
            WriteScope::Unrestricted,
            NetworkScope::None,
            AuthorizationEvidence::Policy {
                profile: PermissionProfile::FullAccess,
            },
        );
        assert!(!denied.full_execution());
    }
}
