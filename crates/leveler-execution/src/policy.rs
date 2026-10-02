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
    /// Auto and resource-granted calls retain their execution boundaries.
    /// Full and approval of an entire exact call execute unrestricted; reusable
    /// resource consent only authorizes its listed capabilities and identities.
    pub write: WriteScope,
    pub network_scope: NetworkScope,
    pub authorization: AuthorizationEvidence,
}

impl ResolvedExecutionPolicy {
    /// Full and exact-call consent authorize unrestricted execution.
    /// Reusable resource consent never widens filesystem or network scopes.
    pub fn unrestricted_execution(&self) -> bool {
        match self.authorization {
            AuthorizationEvidence::ResourceGrant { .. } => false,
            AuthorizationEvidence::Policy { profile } => profile == PermissionProfile::FullAccess,
            _ => true,
        }
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
            policy.write = WriteScope::Unrestricted;
            policy.network_scope = NetworkScope::Internet;
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
}

impl PendingApproval {
    /// The policy this call runs under once `evidence` says it may.
    pub fn allowed(&self, authorization: AuthorizationEvidence) -> ResolvedExecutionPolicy {
        ResolvedExecutionPolicy::new(
            self.write.clone(),
            self.network_scope.clone(),
            authorization,
        )
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
}
